//! Translating between IMAP flag atoms and the local flag bit sets.
//!
//! Two directions matter:
//!
//! * **reading**: a `FETCH` response carries system flags (`\Seen`) and
//!   user-defined keywords.  System flags map onto
//!   [`MessageFlags`]; keywords that HylMail itself uses (`$Done`,
//!   `$Forwarded`) map onto those bits as well, everything else stays a
//!   keyword.
//! * **writing**: the client sends `UID STORE <set> +FLAGS.SILENT (\Seen)` and
//!   must not send a flag the server did not list as permanent — a server that
//!   does not support keywords answers such a `STORE` with `NO`, which would
//!   make "mark as read" fail.

use std::collections::BTreeSet;

use async_imap::types::Flag;

use crate::models::message::{LocalFlags, MessageFlags};

/// The keyword HylMail uses to remember that a message was forwarded.
pub const KEYWORD_FORWARDED: &str = "$Forwarded";

/// The keyword HylMail uses for the "done" state, as introduced by Thunderbird
/// and supported by Dovecot, Exchange and GroupWise.
pub const KEYWORD_DONE: &str = "$Done";

/// The keyword HylMail uses for "completed", the variant Exchange writes.
pub const KEYWORD_COMPLETED: &str = "$Completed";

/// Converts an `async-imap` flag into its wire representation.
#[must_use]
pub fn flag_to_atom(flag: &Flag<'_>) -> String {
    match flag {
        Flag::Seen => "\\Seen".to_string(),
        Flag::Answered => "\\Answered".to_string(),
        Flag::Flagged => "\\Flagged".to_string(),
        Flag::Deleted => "\\Deleted".to_string(),
        Flag::Draft => "\\Draft".to_string(),
        Flag::Recent => "\\Recent".to_string(),
        Flag::MayCreate => "\\*".to_string(),
        Flag::Custom(keyword) => keyword.to_string(),
    }
}

/// The flags of one `FETCH` response as wire atoms.
pub fn atoms_from_flags<'a, I>(flags: I) -> Vec<String>
where
    I: IntoIterator<Item = Flag<'a>>,
{
    flags.into_iter().map(|flag| flag_to_atom(&flag)).collect()
}

/// Converts wire atoms into the local bit set.
#[must_use]
pub fn message_flags_from_atoms<'a, I>(atoms: I) -> MessageFlags
where
    I: IntoIterator<Item = &'a str>,
{
    MessageFlags::from_imap_atoms(atoms)
}

/// The user-defined keywords of a message, in a stable order.
///
/// Keywords that HylMail tracks as bits (`$Done`, `$Forwarded`) are excluded:
/// they live in [`MessageFlags`], and reporting them twice would make a
/// keyword list flicker in the user interface.
#[must_use]
pub fn keywords_from_atoms<'a, I>(atoms: I) -> Vec<String>
where
    I: IntoIterator<Item = &'a str>,
{
    let mut keywords = BTreeSet::new();
    for atom in atoms {
        let atom = atom.trim();
        if !atom.starts_with('\\') && !is_tracked_keyword(atom) {
            keywords.insert(atom.to_string());
        }
    }
    keywords.into_iter().collect()
}

/// `true` for the keywords HylMail keeps as flags.
#[must_use]
pub fn is_tracked_keyword(keyword: &str) -> bool {
    matches!(
        keyword.to_ascii_lowercase().as_str(),
        "$done" | "$completed" | "$forwarded"
    )
}

/// The `STORE` flag list for a set of local flags.
///
/// The result is what goes between the parentheses of
/// `UID STORE <set> +FLAGS.SILENT (...)`; an empty vector means "nothing to do".
#[must_use]
pub fn atoms_for_store(flags: MessageFlags, local: LocalFlags) -> Vec<String> {
    let mut atoms = Vec::new();
    if flags.contains(MessageFlags::SEEN) {
        atoms.push("\\Seen".to_string());
    }
    if flags.contains(MessageFlags::ANSWERED) {
        atoms.push("\\Answered".to_string());
    }
    if flags.contains(MessageFlags::FLAGGED) {
        atoms.push("\\Flagged".to_string());
    }
    if flags.contains(MessageFlags::DRAFT) {
        atoms.push("\\Draft".to_string());
    }
    if flags.contains(MessageFlags::DELETED) {
        atoms.push("\\Deleted".to_string());
    }
    if flags.contains(MessageFlags::FORWARDED) {
        atoms.push(KEYWORD_FORWARDED.to_string());
    }
    if flags.contains(MessageFlags::DONE) {
        atoms.push(KEYWORD_DONE.to_string());
    }
    if local.contains(LocalFlags::DONE) && !flags.contains(MessageFlags::DONE) {
        atoms.push(KEYWORD_DONE.to_string());
    }
    atoms
}

/// Splits a desired flag state into the atoms to add and the atoms to remove.
///
/// A `STORE` sets the whole flag set at once, but sending `-FLAGS` for a flag
/// the message does not carry is a wasted round trip on some servers and an
/// error on others, so only real changes are sent.
#[must_use]
pub fn diff_for_store(
    desired: MessageFlags,
    current: MessageFlags,
    local: LocalFlags,
) -> (Vec<String>, Vec<String>) {
    let wanted: BTreeSet<String> = atoms_for_store(desired, local).into_iter().collect();
    let present: BTreeSet<String> = atoms_for_store(current, LocalFlags::empty())
        .into_iter()
        .collect();

    let add = wanted.difference(&present).cloned().collect();
    let remove = present.difference(&wanted).cloned().collect();
    (add, remove)
}

/// Formats a `UID STORE` argument.
///
/// Returns `None` when nothing would change, which the caller should treat as
/// "already up to date" instead of sending an empty command.
#[must_use]
pub fn store_command(uid_set: &str, add: &[String], remove: &[String]) -> Option<String> {
    let mut parts = Vec::new();
    if !add.is_empty() {
        parts.push(format!("+FLAGS.SILENT ({})", add.join(" ")));
    }
    if !remove.is_empty() {
        parts.push(format!("-FLAGS.SILENT ({})", remove.join(" ")));
    }
    if parts.is_empty() {
        return None;
    }
    Some(format!("UID STORE {uid_set} {}", parts.join(" ")))
}

/// Formats a UID set, collapsing a contiguous range into `first:last`.
#[must_use]
pub fn uid_set(uids: &[u32]) -> String {
    if uids.is_empty() {
        return String::new();
    }
    let mut sorted: Vec<u32> = uids.to_vec();
    sorted.sort_unstable();
    sorted.dedup();

    let mut parts = Vec::new();
    let mut start = sorted[0];
    let mut previous = sorted[0];
    for uid in &sorted[1..] {
        if *uid == previous + 1 {
            previous = *uid;
            continue;
        }
        parts.push(render_range(start, previous));
        start = *uid;
        previous = *uid;
    }
    parts.push(render_range(start, previous));
    parts.join(",")
}

fn render_range(start: u32, end: u32) -> String {
    if start == end {
        start.to_string()
    } else {
        format!("{start}:{end}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_flags_round_trip() {
        let atoms = ["\\Seen", "\\Answered", "\\Flagged", "\\Draft", "\\Deleted"];
        let flags = message_flags_from_atoms(atoms);
        assert!(flags.is_seen());
        assert!(flags.is_answered());
        assert!(flags.is_flagged());
        assert!(flags.contains(MessageFlags::DRAFT));
        assert!(flags.contains(MessageFlags::DELETED));

        let stored = atoms_for_store(flags, LocalFlags::empty());
        assert_eq!(
            stored,
            vec!["\\Seen", "\\Answered", "\\Flagged", "\\Draft", "\\Deleted"]
        );
    }

    #[test]
    fn async_imap_flags_become_atoms() {
        assert_eq!(flag_to_atom(&Flag::Seen), "\\Seen");
        assert_eq!(flag_to_atom(&Flag::Recent), "\\Recent");
        assert_eq!(flag_to_atom(&Flag::MayCreate), "\\*");
        assert_eq!(flag_to_atom(&Flag::Custom("$Label1".into())), "$Label1");
        let atoms = atoms_from_flags([Flag::Seen, Flag::Custom("$Done".into())]);
        assert_eq!(atoms, vec!["\\Seen", "$Done"]);
    }

    #[test]
    fn only_untracked_keywords_are_reported() {
        let atoms = [
            "\\Seen",
            "$Done",
            "$Forwarded",
            "$Completed",
            "$Label1",
            "$Label1",
            "junk",
        ];
        assert_eq!(keywords_from_atoms(atoms), vec!["$Label1", "junk"]);
        assert!(is_tracked_keyword("$done"));
        assert!(!is_tracked_keyword("$label1"));
    }

    #[test]
    fn store_atoms_include_the_tracked_keywords() {
        let flags = MessageFlags::SEEN | MessageFlags::DONE | MessageFlags::FORWARDED;
        assert_eq!(
            atoms_for_store(flags, LocalFlags::empty()),
            vec!["\\Seen", "$Forwarded", "$Done"]
        );
    }

    #[test]
    fn a_locally_done_message_is_pushed_to_the_server() {
        let atoms = atoms_for_store(MessageFlags::empty(), LocalFlags::DONE);
        assert_eq!(atoms, vec!["$Done"]);
    }

    #[test]
    fn the_diff_only_contains_real_changes() {
        let current = MessageFlags::SEEN;
        let desired = MessageFlags::SEEN | MessageFlags::FLAGGED;

        let (add, remove) = diff_for_store(desired, current, LocalFlags::empty());
        assert_eq!(add, vec!["\\Flagged"]);
        assert!(remove.is_empty());

        let (add, remove) = diff_for_store(MessageFlags::empty(), current, LocalFlags::empty());
        assert!(add.is_empty());
        assert_eq!(remove, vec!["\\Seen"]);

        let (add, remove) = diff_for_store(current, current, LocalFlags::empty());
        assert!(add.is_empty() && remove.is_empty());
    }

    #[test]
    fn store_commands_are_well_formed() {
        assert_eq!(
            store_command("12", &["\\Seen".to_string()], &[]),
            Some("UID STORE 12 +FLAGS.SILENT (\\Seen)".to_string())
        );
        assert_eq!(
            store_command("12", &["\\Flagged".to_string()], &["\\Seen".to_string()]),
            Some("UID STORE 12 +FLAGS.SILENT (\\Flagged) -FLAGS.SILENT (\\Seen)".to_string())
        );
        assert_eq!(store_command("12", &[], &[]), None);
    }

    #[test]
    fn uid_sets_are_collapsed() {
        assert_eq!(uid_set(&[]), "");
        assert_eq!(uid_set(&[7]), "7");
        assert_eq!(uid_set(&[1, 2, 3]), "1:3");
        assert_eq!(uid_set(&[3, 1, 2]), "1:3");
        assert_eq!(uid_set(&[1, 2, 3, 7, 9, 10]), "1:3,7,9:10");
        assert_eq!(uid_set(&[5, 5, 5]), "5");
    }
}
