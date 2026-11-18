//! The vocabulary of user intents.
//!
//! Widgets never mutate the application directly.  They push an [`Action`]
//! into a list, and [`crate::app::App`] executes the list once per frame.
//! That keeps the drawing code free of side effects and makes the intent
//! mapping (especially of the keyboard) unit testable.

use std::path::PathBuf;

use hylmail_core::models::{AccountId, FolderId, MessageId};

/// The drag-and-drop payload of a message row.
///
/// Only the id travels: the folder drop target turns it into a move, and a
/// release that no folder accepts turns it into an `.eml` export.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DragMessage(pub MessageId);

/// Column the message list is sorted by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum SortKey {
    /// Newest first.
    #[default]
    Date,
    /// Sender, then date.
    From,
    /// Normalised subject.
    Subject,
    /// Read state, then date.
    Unread,
    /// Flag state, then date.
    Flagged,
    /// Attachment indicator, then date.
    Attachment,
}

impl SortKey {
    /// German column header.
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Date => "Datum",
            Self::From => "Von",
            Self::Subject => "Betreff",
            Self::Unread => "Gelesen",
            Self::Flagged => "Markiert",
            Self::Attachment => "Anhang",
        }
    }

    /// Every sort column, in header order.
    ///
    /// Only the tests use the list; the drawing code names its columns
    /// explicitly, so the constant is compiled for tests alone.
    #[cfg(test)]
    pub(crate) const ALL: [Self; 6] = [
        Self::Unread,
        Self::Flagged,
        Self::Attachment,
        Self::From,
        Self::Subject,
        Self::Date,
    ];
}

/// Something the user asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Action {
    /// A folder was clicked.
    SelectFolder(FolderId),
    /// The "all inboxes" entry was clicked; `None` clears the account filter.
    SelectAccount(Option<AccountId>),
    /// An account header was collapsed or expanded.
    ToggleAccount(AccountId),
    /// A message row was clicked.
    SelectMessage(MessageId),
    /// A list checkbox was toggled, adding or removing a bulk target.
    ToggleSelection(MessageId),
    /// The header checkbox ticked every loaded message.
    SelectAll,
    /// The header checkbox unticked every message.
    ClearSelection,
    /// Move the selection one row down.
    NextMessage,
    /// Move the selection one row up.
    PreviousMessage,
    /// The search box was submitted.
    Search(String),
    /// The search box was submitted with `Enter`, which skips the debounce.
    ForceSearch(String),
    /// The search box asks for the keyboard focus.
    FocusSearch,
    /// The search box was cleared.
    ClearSearch,
    /// The "only unread" filter was toggled.
    ToggleUnreadOnly,
    /// The "only flagged" filter was toggled.
    ToggleFlaggedOnly,
    /// The "only with attachment" filter was toggled.
    ToggleAttachmentOnly,
    /// A column header was clicked.
    SortBy(SortKey),
    /// Switch the message list between flat and conversation view.
    ToggleThreading,
    /// Show or hide the reading pane in the lower half of the main view.
    TogglePreview,
    /// Expand or collapse one conversation.
    ToggleThread {
        /// Conversation key, see [`hylmail_core::models::MessageHeader::conversation_key`].
        thread_id: String,
    },
    /// Load the next page into the list.
    LoadMore,
    /// Synchronise one account now.
    SyncAccount(AccountId),
    /// Synchronise every account now.
    SyncAll,
    /// Re-read the folder list of one account.
    ReloadFolders(AccountId),
    /// Open the composer for a new message.
    Compose,
    /// Answer the selected message.
    Reply,
    /// Answer the selected message to every recipient.
    ReplyAll,
    /// Forward the selected message.
    Forward,
    /// Move the selected message to the trash.
    Delete,
    /// Confirm a delete the confirmation prompt held back.
    ConfirmDelete,
    /// Dismiss the delete confirmation prompt.
    CancelDelete,
    /// Toggle the flag of the selected message.
    ToggleFlag,
    /// Mark the selected message as read.
    MarkRead,
    /// Mark the selected message as unread.
    MarkUnread,
    /// Apply the "done" action (`E`), per the configured [`DoneMode`].
    Done,
    /// Toggle the answered mark (`\Answered`) of the selected message.
    ToggleAnswered,
    /// Mark every message of the current folder as read.
    MarkAllRead,
    /// Load the blocked external images of the displayed message.
    ShowImages,
    /// Trust the displayed message's sender, so their images always load.
    TrustSenderImages,
    /// Toggle between the rendered body and its HTML source.
    ToggleSource,
    /// Write one listed attachment to a file the user picks.
    SaveAttachment {
        /// MIME part to write.
        part_id: String,
    },
    /// Write every attachment of the displayed message into one directory.
    SaveAllAttachments,
    /// Send the message in the composer.
    Send,
    /// Save the composer into the local drafts (`Ctrl+E`).
    SaveDraft,
    /// Pick files and stage them as attachments.
    Attach,
    /// Pick an image and embed it in the rich body (`FR-COMPOSE-3`).
    EmbedImage,
    /// Write the settings dialog back to disk.
    SaveSettings,
    /// Write the accounts dialog back to disk.
    SaveAccounts,
    /// Store the password typed into the accounts dialog.
    SavePassword,
    /// Move the selected message into another folder.
    MoveTo {
        /// Destination folder.
        target: FolderId,
    },
    /// Move one message into another folder, whoever happens to be selected.
    ///
    /// Produced by a drag from the message list onto a folder.
    MoveDragged {
        /// The dragged message.
        message_id: MessageId,
        /// Destination folder.
        target: FolderId,
    },
    /// Write one message to a `.eml` file the user picks.
    ///
    /// Produced when a message is dragged out of the list without a folder
    /// accepting it.
    ExportEml {
        /// The dragged message.
        message_id: MessageId,
    },
    /// Answer a calendar invitation with the given participation status.
    CalendarReply {
        /// The `PARTSTAT` to send back to the organiser.
        status: hylmail_core::calendar::PartStat,
    },
    /// Archive the selected message.
    Archive,
    /// Move the selected message into the junk folder.
    MarkJunk,
    /// Open the folder picker for a move.
    OpenMovePicker,
    /// Run the OAuth2 sign-in for the selected account in the browser.
    OAuthSignIn,
    /// Open the settings dialog.
    OpenSettings,
    /// Open the account dialog.
    OpenAccounts,
    /// Open the rule editor.
    OpenRules,
    /// Write the rule set back to disk.
    SaveRules,
    /// Add an empty rule to the working copy.
    AddRule,
    /// Remove one rule from the working copy.
    RemoveRule {
        /// The rule to drop.
        rule_id: hylmail_core::models::RuleId,
    },
    /// Run the enabled rules over the messages currently listed.
    RunRules,
    /// Open the address book.
    OpenContacts,
    /// Open the outgoing queue.
    OpenOutbox,
    /// Remove one entry from the outgoing queue.
    OutboxRemove {
        /// The queued message to drop.
        id: hylmail_core::models::OutboxId,
    },
    /// Re-arm one parked entry of the outgoing queue.
    OutboxRetry {
        /// The queued message to retry.
        id: hylmail_core::models::OutboxId,
    },
    /// Drain the outgoing queue now.
    OutboxSendNow,
    /// Write the address book back (new and edited contacts).
    SaveContacts,
    /// Delete one contact from the address book.
    DeleteContact {
        /// The contact to drop.
        contact_id: hylmail_core::models::ContactId,
    },
    /// Synchronise the address book from the configured CardDAV server.
    SyncContacts,
    /// Close every dialog.
    CloseDialog,
    /// Open a `.eml` file that arrived over the single-instance endpoint.
    OpenEml(PathBuf),
    /// Leave the application.
    Quit,
}

/// A keyboard gesture, already resolved against the platform modifiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Shortcut {
    /// `Ctrl+N`: write a new message.
    NewMessage,
    /// `Ctrl+R`: answer the sender.
    Reply,
    /// `Ctrl+Shift+R`: answer everyone.
    ReplyAll,
    /// `Ctrl+U` (or `Ctrl+L`): forward.
    Forward,
    /// `Delete`: move to trash.
    Delete,
    /// `Ctrl+Q`: mark read.
    MarkRead,
    /// `Ctrl+Shift+Q`: mark unread.
    MarkUnread,
    /// `Ctrl+Space`: toggle flag.
    ToggleFlag,
    /// `E`: the "done" action.
    Done,
    /// `Ctrl+Shift+W`: toggle the answered mark.
    Answered,
    /// `F5` or `Ctrl+Y`: synchronise.
    Sync,
    /// `Ctrl+F`: focus the search box.
    Search,
    /// `Ctrl+E`: save the composer as a draft.
    SaveDraft,
    /// `Ctrl+,`: settings.
    Settings,
    /// `Ctrl+Shift+A`: accounts.
    Accounts,
    /// `Ctrl+Alt+R`: the rule editor.
    Rules,
    /// `Down` / `J`.
    NextMessage,
    /// `Up` / `K`.
    PreviousMessage,
    /// `Escape`: close dialogs, leave the search.
    Escape,
}

/// Translates a shortcut into an action, given what is currently selected.
#[must_use]
pub(crate) fn from_shortcut(shortcut: Shortcut, has_selection: bool) -> Option<Action> {
    let needs_selection = |action: Action| if has_selection { Some(action) } else { None };
    Some(match shortcut {
        Shortcut::NewMessage => Action::Compose,
        Shortcut::Reply => needs_selection(Action::Reply)?,
        Shortcut::ReplyAll => needs_selection(Action::ReplyAll)?,
        Shortcut::Forward => needs_selection(Action::Forward)?,
        Shortcut::Delete => needs_selection(Action::Delete)?,
        Shortcut::MarkRead => needs_selection(Action::MarkRead)?,
        Shortcut::MarkUnread => needs_selection(Action::MarkUnread)?,
        Shortcut::ToggleFlag => needs_selection(Action::ToggleFlag)?,
        Shortcut::Done => needs_selection(Action::Done)?,
        Shortcut::Answered => needs_selection(Action::ToggleAnswered)?,
        Shortcut::Sync => Action::SyncAll,
        Shortcut::Search => Action::FocusSearch,
        Shortcut::Settings => Action::OpenSettings,
        Shortcut::SaveDraft => Action::SaveDraft,
        Shortcut::Accounts => Action::OpenAccounts,
        Shortcut::Rules => Action::OpenRules,
        Shortcut::NextMessage => Action::NextMessage,
        Shortcut::PreviousMessage => Action::PreviousMessage,
        Shortcut::Escape => Action::CloseDialog,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_sort_key_has_a_label() {
        for key in SortKey::ALL {
            assert!(!key.label().is_empty());
        }
    }

    #[test]
    fn sort_keys_are_unique() {
        let mut seen = Vec::new();
        for key in SortKey::ALL {
            assert!(!seen.contains(&key));
            seen.push(key);
        }
        assert_eq!(seen.len(), SortKey::ALL.len());
    }

    #[test]
    fn selection_actions_are_dropped_without_a_selection() {
        assert_eq!(from_shortcut(Shortcut::Reply, false), None);
        assert_eq!(from_shortcut(Shortcut::Reply, true), Some(Action::Reply));
        assert_eq!(from_shortcut(Shortcut::Delete, false), None);
        assert_eq!(from_shortcut(Shortcut::ToggleFlag, false), None);
    }

    #[test]
    fn global_actions_work_without_a_selection() {
        assert_eq!(
            from_shortcut(Shortcut::NewMessage, false),
            Some(Action::Compose)
        );
        assert_eq!(from_shortcut(Shortcut::Sync, false), Some(Action::SyncAll));
        assert_eq!(
            from_shortcut(Shortcut::Settings, false),
            Some(Action::OpenSettings)
        );
        assert_eq!(
            from_shortcut(Shortcut::NextMessage, false),
            Some(Action::NextMessage)
        );
    }
}
