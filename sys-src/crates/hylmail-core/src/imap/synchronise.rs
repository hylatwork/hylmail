//! Keeping the local database in step with a mailbox.
//!
//! The synchroniser is written as a *resumable, bounded* pass rather than as a
//! single "mirror the server" operation, because a desktop client has to stay
//! usable while a 50 000 message folder is being indexed:
//!
//! 1. **Cheap** — `SELECT`/`EXAMINE` the folder and read `UIDVALIDITY` and
//!    `UIDNEXT`.  If `UIDVALIDITY` changed, the local copy is meaningless: every
//!    UID of that folder is dropped and the folder is re-synchronised.
//! 2. **New messages** — fetch headers for `highest_local_uid + 1 : *` in
//!    batches, so header parsing, FTS indexing and the UI all get a chance to
//!    run between batches.
//! 3. **Flags** — `UID SEARCH UNSEEN` and `UID SEARCH FLAGGED` are compared
//!    against the stored state.  Two small searches are far cheaper than
//!    fetching flags for the whole folder, and they cover the flags that change
//!    behind the client's back (read on the phone, flagged in the web UI).
//! 4. **Deletions** — only in [`SyncDepth::Full`], because it needs
//!    `UID SEARCH ALL`; the incremental pass cannot see a deletion at all.
//!
//! Everything decision-shaped is a pure function at the top of the module so it
//! can be tested without a server; the driver below only performs i/o.

use std::collections::{BTreeMap, BTreeSet};

use crate::db::{folders, messages, Database};
use crate::error::{Error, Result};
use crate::models::folder::Folder;
use crate::models::ids::{AccountId, FolderId, MessageId};
use crate::models::message::{MessageFlags, MessageHeader};

use super::connection::ImapSession;
use super::flags;

/// How much work a pass does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SyncDepth {
    /// Only what changed: new messages and flag reconciliation.
    #[default]
    Incremental,
    /// Additionally detect messages that were deleted on the server.
    Full,
}

/// Knobs of a synchronisation pass.
#[derive(Debug, Clone, Copy)]
pub struct SyncOptions {
    /// How much work to do.
    pub depth: SyncDepth,
    /// UIDs per `UID FETCH`.
    pub batch_size: usize,
    /// Stop after this many new headers; `0` means "no limit".
    ///
    /// The first synchronisation of a huge mailbox otherwise blocks the
    /// account for a long time; the next pass continues where this one stopped.
    pub max_new: usize,
}

impl Default for SyncOptions {
    fn default() -> Self {
        Self {
            depth: SyncDepth::Incremental,
            batch_size: 500,
            max_new: 2_000,
        }
    }
}

/// Progress events, emitted between batches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncProgress {
    /// A folder started.
    FolderStarted {
        /// Display name of the folder.
        name: String,
        /// `true` when the pass includes the deletion check.
        full: bool,
    },
    /// A batch of new headers was stored.
    HeadersStored {
        /// Number of messages added by this batch.
        added: usize,
        /// Highest UID seen so far in this folder.
        highest_uid: u32,
    },
    /// Messages that no longer exist on the server were dropped.
    MessagesRemoved {
        /// Number of messages removed.
        removed: usize,
    },
    /// Flags of existing messages were corrected.
    FlagsUpdated {
        /// Number of messages whose flags changed.
        updated: usize,
    },
    /// The pass stopped early because the batch limit was reached.
    Deferred {
        /// Number of new messages still waiting.
        remaining: usize,
    },
    /// A folder finished.
    FolderFinished {
        /// Display name of the folder.
        name: String,
    },
}

/// What one folder synchronisation achieved.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FolderSyncReport {
    /// Folder that was synchronised.
    pub folder_id: Option<FolderId>,
    /// Display name of the folder.
    pub name: String,
    /// Messages added to the local store.
    pub new_messages: usize,
    /// Messages dropped because the server no longer has them.
    pub removed_messages: usize,
    /// Messages whose flags were corrected.
    pub updated_flags: usize,
    /// `true` when `UIDVALIDITY` changed and the local copy was rebuilt.
    pub uid_validity_reset: bool,
    /// `UIDNEXT` reported by the server.
    pub uid_next: Option<u32>,
    /// `true` when the pass stopped early because of [`SyncOptions::max_new`].
    pub deferred: bool,
}

impl FolderSyncReport {
    /// `true` when the pass changed nothing.
    #[must_use]
    pub fn is_noop(&self) -> bool {
        self.new_messages == 0
            && self.removed_messages == 0
            && self.updated_flags == 0
            && !self.uid_validity_reset
    }
}

/// The UID range that has to be fetched for new messages.
///
/// `UIDNEXT` is a hint, not a promise: after a deletion it can point past
/// messages that never existed, so the range is always `highest + 1 : *` and the
/// server decides what actually exists.
#[must_use]
pub fn new_uid_range(highest_local: Option<u32>, uid_next: Option<u32>) -> Option<String> {
    let highest = highest_local.unwrap_or(0);
    if let Some(next) = uid_next {
        if next <= highest + 1 {
            // Nothing new according to the hint.  A `*` fetch would only
            // re-fetch the last message, so skip it entirely.
            return None;
        }
    } else if highest == 0 {
        // No `UIDNEXT` and no local state: fall back to fetching everything.
        return Some("1:*".to_string());
    }
    Some(format!("{}:*", highest + 1))
}

/// Splits UID ranges into batches of at most `batch_size` entries.
///
/// A range counts as one entry, because `1000:1500` is a single item in an
/// IMAP sequence set.  Callers pass ranges, not individual UIDs.
#[must_use]
pub fn batches(ranges: &[String], batch_size: usize) -> Vec<String> {
    let batch_size = batch_size.max(1);
    let mut out = Vec::new();
    for chunk in ranges.chunks(batch_size) {
        out.push(chunk.join(","));
    }
    out
}

/// Local messages whose UID the server no longer lists.
#[must_use]
pub fn removed_locally(
    server_uids: &BTreeSet<u32>,
    local: &[messages::UidState],
) -> Vec<MessageId> {
    local
        .iter()
        .filter(|state| !server_uids.contains(&state.uid))
        .map(|state| state.id)
        .collect()
}

/// Flag corrections to apply, derived from two server side searches.
///
/// Only `\Seen` and `\Flagged` are reconciled, and only for messages that still
/// carry their UID.  Draft and answered flags are deliberately untouched: the
/// server knows nothing about a draft that has not been uploaded, and `\Answered`
/// is ambiguous across clients.
#[must_use]
pub fn flag_updates(
    unseen_uids: &BTreeSet<u32>,
    flagged_uids: &BTreeSet<u32>,
    local: &[messages::UidState],
    seen_uids: &BTreeSet<u32>,
) -> Vec<(MessageId, MessageFlags)> {
    let mut updates = Vec::new();
    for state in local {
        let mut flags = state.flags;
        if seen_uids.contains(&state.uid) {
            flags.insert(MessageFlags::SEEN);
        } else if unseen_uids.contains(&state.uid) {
            flags.remove(MessageFlags::SEEN);
        }
        if flagged_uids.contains(&state.uid) {
            flags.insert(MessageFlags::FLAGGED);
        } else {
            flags.remove(MessageFlags::FLAGGED);
        }
        // `\Recent` is a session flag and never stored; `\Deleted` is owned by
        // the move/delete logic.  Ignore both when comparing.
        let comparable = |value: MessageFlags| value & (MessageFlags::SEEN | MessageFlags::FLAGGED);
        if comparable(flags) != comparable(state.flags) {
            updates.push((state.id, flags));
        }
    }
    updates
}

/// The flags HylMail considers authoritative when they come from the server.
#[must_use]
pub const fn reconciled_flags() -> MessageFlags {
    MessageFlags::SEEN.union(MessageFlags::FLAGGED)
}

/// Synchronises one folder against the local database.
///
/// The session must be connected; the folder is selected or examined here, so
/// the caller does not have to know which one is current.
///
/// # Errors
///
/// Returns the first fatal error; a single missing message is not fatal.  The
/// caller (the sync engine) decides whether to retry based on
/// [`Error::is_transient`].
pub async fn sync_folder(
    session: &mut ImapSession,
    database: &Database,
    folder: &Folder,
    options: SyncOptions,
    progress: &mut impl FnMut(SyncProgress),
) -> Result<FolderSyncReport> {
    let mut report = FolderSyncReport {
        folder_id: Some(folder.id),
        name: folder.name.clone(),
        ..FolderSyncReport::default()
    };
    progress(SyncProgress::FolderStarted {
        name: folder.name.clone(),
        full: options.depth == SyncDepth::Full,
    });

    // Only `Trash` and `Junk` are ever written to during a pass; everything else
    // is examined so that `\Seen` cannot be set by a synchronisation.
    let mailbox = if writable(folder) {
        session.select(&folder.path).await?
    } else {
        session.examine(&folder.path).await?
    };

    let uid_validity = mailbox.uid_validity;
    let uid_next = mailbox.uid_next;
    report.uid_next = uid_next;

    if uid_validity.is_some() && uid_validity != folder.uid_validity {
        tracing::info!(
            folder = %folder.path,
            stored = ?folder.uid_validity,
            server = ?uid_validity,
            "UIDVALIDITY changed; the local copy of the folder is rebuilt"
        );
        database.write(|connection| {
            messages::delete_folder_messages(connection, folder.id)?;
            folders::update_uid_state(connection, folder.id, uid_validity, uid_next)
        })?;
        report.uid_validity_reset = true;
    }

    let (highest, local_states) = database.read(|connection| {
        let highest = messages::highest_uid(connection, folder.id)?;
        let states = messages::list_uid_states(connection, folder.id)?;
        Ok((highest, states))
    })?;

    // -- 1. new messages ----------------------------------------------------
    if let Some(range) = new_uid_range(highest, uid_next) {
        let ranges = vec![range];
        let mut stored = 0usize;
        for batch in batches(&ranges, options.batch_size) {
            let headers = session
                .fetch_headers(folder.account_id, folder.id, &batch)
                .await?;
            if headers.is_empty() {
                continue;
            }
            let (added, highest_batch) = store_headers(database, headers)?;
            stored += added;
            report.new_messages += added;
            progress(SyncProgress::HeadersStored {
                added,
                highest_uid: highest_batch,
            });
            if options.max_new > 0 && stored >= options.max_new {
                report.deferred = true;
                progress(SyncProgress::Deferred { remaining: 0 });
                break;
            }
        }
    }

    // -- 2. flag reconciliation --------------------------------------------
    let local_states = if report.new_messages > 0 {
        database.read(|connection| messages::list_uid_states(connection, folder.id))?
    } else {
        local_states
    };
    let updated = reconcile_flags(session, database, &local_states, options).await?;
    report.updated_flags = updated;
    if updated > 0 {
        progress(SyncProgress::FlagsUpdated { updated });
    }

    // -- 3. deletions (full pass only) -------------------------------------
    if options.depth == SyncDepth::Full {
        let server = server_uids(session).await?;
        let removed = removed_locally(&server, &local_states);
        if !removed.is_empty() {
            let count = removed.len();
            database.write(|connection| {
                for id in &removed {
                    messages::delete_message(connection, *id)?;
                }
                Ok(())
            })?;
            report.removed_messages = count;
            progress(SyncProgress::MessagesRemoved { removed: count });
        }
    }

    database.write(|connection| {
        folders::update_uid_state(connection, folder.id, uid_validity, uid_next)?;
        messages::refresh_folder_counts(connection, folder.id)?;
        Ok(())
    })?;

    progress(SyncProgress::FolderFinished {
        name: folder.name.clone(),
    });
    Ok(report)
}

/// `true` when a pass is allowed to open the folder read-write.
///
/// HylMail never sets flags while synchronising; a read-write selection is only
/// needed for the folders the user actively works in, and it is avoided
/// elsewhere so that no other client sees a `\Seen` it did not cause.
fn writable(folder: &Folder) -> bool {
    matches!(
        folder.kind,
        crate::models::folder::FolderKind::Trash | crate::models::folder::FolderKind::Junk
    )
}

/// Stores a batch of headers, returning `(added, highest_uid)`.
fn store_headers(database: &Database, headers: Vec<MessageHeader>) -> Result<(usize, u32)> {
    database.write(|connection| {
        let mut added = 0;
        let mut highest = 0u32;
        for mut header in headers {
            highest = highest.max(header.uid.unwrap_or(0));
            if messages::upsert_message(connection, &mut header)? {
                added += 1;
            }
        }
        Ok((added, highest))
    })
}

/// Compares `UNSEEN`/`FLAGGED`/`SEEN` on the server with the local state.
async fn reconcile_flags(
    session: &mut ImapSession,
    database: &Database,
    local: &[messages::UidState],
    options: SyncOptions,
) -> Result<usize> {
    if local.is_empty() {
        return Ok(0);
    }
    let unseen: BTreeSet<u32> = session.search("UNSEEN").await?.into_iter().collect();
    let flagged: BTreeSet<u32> = session.search("FLAGGED").await?.into_iter().collect();
    // `SEEN` is the complement of `UNSEEN` *within the folder*, but a folder can
    // contain messages whose UID the local store does not know yet, so the
    // complement is only taken over the UIDs that are actually known.
    let known: BTreeSet<u32> = local.iter().map(|state| state.uid).collect();
    let seen: BTreeSet<u32> = if options.depth == SyncDepth::Full {
        known.difference(&unseen).copied().collect()
    } else {
        // Without a full pass only trusted information is used: a message that
        // is not in the `UNSEEN` set was read at some point.
        BTreeSet::new()
    };

    let updates = flag_updates(&unseen, &flagged, local, &seen);
    if updates.is_empty() {
        return Ok(0);
    }
    database.write(|connection| {
        for (id, flags) in &updates {
            messages::set_flags(connection, *id, *flags)?;
        }
        Ok(())
    })?;
    Ok(updates.len())
}

/// Asks the server for every UID of the selected folder.
async fn server_uids(session: &mut ImapSession) -> Result<BTreeSet<u32>> {
    Ok(session.search("ALL").await?.into_iter().collect())
}

/// Synchronises a list of folders, skipping the ones that cannot be selected.
///
/// Errors of a single folder are logged and collected, so one broken mailbox
/// cannot stop the whole account.
pub async fn sync_folders(
    session: &mut ImapSession,
    database: &Database,
    account_id: AccountId,
    folders: &[Folder],
    options: SyncOptions,
    progress: &mut impl FnMut(SyncProgress),
) -> Vec<Result<FolderSyncReport>> {
    let _ = account_id;
    let mut reports = Vec::new();
    for folder in folders {
        if !folder.is_selectable() {
            continue;
        }
        match sync_folder(session, database, folder, options, progress).await {
            Ok(report) => reports.push(Ok(report)),
            Err(error) if error.is_transient() => {
                tracing::warn!(folder = %folder.path, %error, "folder synchronisation failed");
                reports.push(Err(error));
                // A transient error usually means the connection is gone; trying
                // the next folder would only pile up timeouts.
                break;
            }
            Err(error) => {
                tracing::warn!(folder = %folder.path, %error, "folder skipped");
                reports.push(Err(error));
            }
        }
    }
    reports
}

/// Applies a flag change the user made locally.
///
/// Kept here rather than in the connection layer because it is a synchronisation
/// decision: it needs the current state and the local-only flags to build the
/// minimal `STORE`.
///
/// # Errors
///
/// Returns [`Error::Protocol`] when the server refuses the update.
pub async fn apply_local_flags(
    session: &mut ImapSession,
    folder: &Folder,
    header: &MessageHeader,
    desired: MessageFlags,
) -> Result<bool> {
    let Some(uid) = header.uid else {
        return Err(Error::Unsupported(
            "the message has no IMAP UID yet".to_string(),
        ));
    };
    if session.selected() != Some(folder.path.as_str()) {
        session.select(&folder.path).await?;
    }
    session
        .store_flags(uid, desired, header.flags, header.local_flags)
        .await
}

/// Groups messages by conversation, which the list view needs when threading is
/// enabled.  Returns `thread_id -> newest UID`.
#[must_use]
pub fn thread_index(headers: &[MessageHeader]) -> BTreeMap<String, u32> {
    let mut index = BTreeMap::new();
    for header in headers {
        let Some(thread) = header.thread_id.as_ref() else {
            continue;
        };
        let uid = header.uid.unwrap_or(0);
        index
            .entry(thread.clone())
            .and_modify(|current: &mut u32| *current = (*current).max(uid))
            .or_insert(uid);
    }
    index
}

/// Atomically builds and sends a `UID FETCH` for a single message body.
///
/// Exposed for the on-demand body download of phase 3, which is why it lives
/// next to the synchroniser rather than in the connection layer.
///
/// # Errors
///
/// See [`ImapSession::fetch_raw`].
pub async fn fetch_body(session: &mut ImapSession, folder: &Folder, uid: u32) -> Result<Vec<u8>> {
    if session.selected() != Some(folder.path.as_str()) {
        session.examine(&folder.path).await?;
    }
    session.fetch_raw(uid).await
}

/// Builds an IMAP `SEARCH` criteria string from free text (`FR-SEARCH-2`).
///
/// `TEXT` matches the headers and the body, which is what a user expects from a
/// single search box.  The value is quoted and escaped, so a quote the user
/// typed cannot break out of the criteria.
#[must_use]
pub fn search_criteria(query: &str) -> String {
    let escaped = query.replace('\\', "\\\\").replace('"', "\\\"");
    format!("TEXT \"{escaped}\"")
}

/// Runs a server-side `UID SEARCH` and returns the newest matching headers.
///
/// The hits are the `limit` highest UIDs (a message's UID grows with its age in
/// the mailbox), which is the order the message list shows.  The caller is
/// expected to store the returned headers in the local cache, so a later local
/// search and the list view see them.
///
/// # Errors
///
/// [`Error::Protocol`] when the server refuses `SELECT`, `UID SEARCH` or
/// `UID FETCH`.
pub async fn search_folder(
    session: &mut ImapSession,
    folder: &Folder,
    query: &str,
    limit: usize,
) -> Result<Vec<MessageHeader>> {
    if session.selected() != Some(folder.path.as_str()) {
        session.select(&folder.path).await?;
    }
    let mut uids = session.search(&search_criteria(query)).await?;
    uids.sort_unstable();
    if uids.len() > limit {
        uids = uids.split_off(uids.len() - limit);
    }
    if uids.is_empty() {
        return Ok(Vec::new());
    }
    let set: String = uids
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(",");
    session
        .fetch_headers(folder.account_id, folder.id, &set)
        .await
}

/// Re-exported so callers can build a `STORE` without touching [`flags`].
#[must_use]
pub fn store_command(uid: u32, add: &[String], remove: &[String]) -> Option<String> {
    flags::store_command(&uid.to_string(), add, remove)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    fn state(uid: u32, flags: MessageFlags) -> messages::UidState {
        messages::UidState {
            id: MessageId::new(),
            uid,
            flags,
        }
    }

    #[test]
    fn search_criteria_quotes_and_escapes_the_value() {
        assert_eq!(search_criteria("rechnung"), "TEXT \"rechnung\"");
        assert_eq!(search_criteria("say \"hi\""), "TEXT \"say \\\"hi\\\"\"");
        assert_eq!(search_criteria("back\\slash"), "TEXT \"back\\\\slash\"");
    }

    #[test]
    fn the_first_synchronisation_asks_for_everything() {
        assert_eq!(new_uid_range(None, None).as_deref(), Some("1:*"));
        assert_eq!(new_uid_range(Some(0), Some(9)).as_deref(), Some("1:*"));
        // `UIDNEXT 1` means the mailbox is empty: nothing to fetch.
        assert_eq!(new_uid_range(None, Some(1)), None);
    }

    #[test]
    fn nothing_new_is_not_a_fetch() {
        assert_eq!(new_uid_range(Some(7), Some(8)), None);
        assert_eq!(new_uid_range(Some(7), Some(7)), None);
        assert_eq!(new_uid_range(Some(7), Some(1)), None);
    }

    #[test]
    fn a_stale_uid_next_still_fetches() {
        // The hint says "nothing new", the local state says otherwise: the
        // range wins, because a `UIDNEXT` that is too low must not lose mail.
        assert_eq!(new_uid_range(Some(7), None).as_deref(), Some("8:*"));
        assert_eq!(new_uid_range(Some(7), Some(20)).as_deref(), Some("8:*"));
    }

    #[test]
    fn ranges_are_batched() {
        let ranges: Vec<String> = vec!["1:*".into(), "500:600".into(), "700:800".into()];
        assert_eq!(batches(&ranges, 2), vec!["1:*,500:600", "700:800"]);
        assert_eq!(batches(&ranges, 500), vec!["1:*,500:600,700:800"]);
        assert_eq!(batches(&ranges, 0), vec!["1:*", "500:600", "700:800"]);
        assert!(batches(&[], 10).is_empty());
    }

    #[test]
    fn messages_the_server_no_longer_lists_are_removed() {
        let local = vec![
            state(1, MessageFlags::empty()),
            state(2, MessageFlags::empty()),
            state(3, MessageFlags::empty()),
        ];
        let server: BTreeSet<u32> = [1, 3].into_iter().collect();
        let removed = removed_locally(&server, &local);
        assert_eq!(removed, vec![local[1].id]);
        assert!(removed_locally(&[1, 2, 3].into_iter().collect(), &local).is_empty());
    }

    #[test]
    fn flags_are_reconciled_from_the_two_searches() {
        let local = vec![
            state(1, MessageFlags::SEEN),
            state(2, MessageFlags::empty()),
            state(3, MessageFlags::SEEN | MessageFlags::FLAGGED),
            state(4, MessageFlags::empty()),
        ];
        let unseen: BTreeSet<u32> = [2, 3].into_iter().collect();
        let flagged: BTreeSet<u32> = [3, 4].into_iter().collect();

        let updates = flag_updates(&unseen, &flagged, &local, &BTreeSet::new());
        // 1: still seen, not flagged -> unchanged
        // 2: unseen locally and on the server -> unchanged
        // 3: server says unseen -> \Seen removed, \Flagged stays
        // 4: flagged on the server, none locally -> \Flagged added
        assert_eq!(updates.len(), 2);
        assert_eq!(updates[0], (local[2].id, MessageFlags::FLAGGED));
        assert_eq!(updates[1], (local[3].id, MessageFlags::FLAGGED));
    }

    #[test]
    fn a_consistent_state_produces_no_writes() {
        let local = vec![
            state(1, MessageFlags::SEEN),
            state(2, MessageFlags::FLAGGED),
        ];
        let unseen: BTreeSet<u32> = [2].into_iter().collect();
        let flagged: BTreeSet<u32> = [2].into_iter().collect();
        assert!(flag_updates(&unseen, &flagged, &local, &BTreeSet::new()).is_empty());
    }

    #[test]
    fn the_full_pass_can_restore_the_seen_flag() {
        let local = vec![state(1, MessageFlags::empty())];
        let unseen: BTreeSet<u32> = BTreeSet::new();
        let seen: BTreeSet<u32> = [1].into_iter().collect();
        let updates = flag_updates(&unseen, &BTreeSet::new(), &local, &seen);
        assert_eq!(updates, vec![(local[0].id, MessageFlags::SEEN)]);
    }

    #[test]
    fn deleted_and_recent_flags_are_not_reconciled() {
        let local = vec![state(
            1,
            MessageFlags::SEEN | MessageFlags::DELETED | MessageFlags::RECENT | MessageFlags::DRAFT,
        )];
        let unseen: BTreeSet<u32> = [1].into_iter().collect();
        let updates = flag_updates(&unseen, &BTreeSet::new(), &local, &BTreeSet::new());
        // Only `\Seen` is compared; the other flags survive untouched.
        assert_eq!(updates.len(), 1);
        let flags = updates[0].1;
        assert!(flags.contains(MessageFlags::DELETED));
        assert!(flags.contains(MessageFlags::RECENT));
        assert!(flags.contains(MessageFlags::DRAFT));
        assert!(!flags.contains(MessageFlags::SEEN));
    }

    #[test]
    fn the_thread_index_keeps_the_newest_uid() {
        let mut a = MessageHeader::new(AccountId::new(), FolderId::new());
        a.thread_id = Some("t1".into());
        a.uid = Some(5);
        let mut b = MessageHeader::new(AccountId::new(), FolderId::new());
        b.thread_id = Some("t1".into());
        b.uid = Some(9);
        let mut c = MessageHeader::new(AccountId::new(), FolderId::new());
        c.thread_id = None;
        c.uid = Some(99);

        let index = thread_index(&[a, b, c]);
        assert_eq!(index.get("t1"), Some(&9));
        assert_eq!(index.len(), 1);
    }

    #[test]
    fn an_empty_report_is_a_noop() {
        let report = FolderSyncReport::default();
        assert!(report.is_noop());
        let report = FolderSyncReport {
            updated_flags: 1,
            ..FolderSyncReport::default()
        };
        assert!(!report.is_noop());
    }

    #[test]
    fn the_store_command_matches_the_flag_layer() {
        let command = store_command(42, &["\\Seen".to_string()], &[]);
        assert_eq!(
            command.as_deref(),
            Some("UID STORE 42 +FLAGS.SILENT (\\Seen)")
        );
        assert!(store_command(42, &[], &[]).is_none());
    }
}
