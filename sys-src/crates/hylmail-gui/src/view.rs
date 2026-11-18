//! The panels of the main window.
//!
//! Every function here is pure drawing: it reads [`UiState`] and appends to a
//! list of [`Action`]s.  Nothing in this module touches the network, the
//! database or the file system, which is what keeps a slow server from
//! freezing the window.

use egui::{Align, Layout, RichText, ScrollArea, TextStyle};
use egui_extras::{Column, TableBuilder};

use hylmail_core::models::{Account, Folder, FolderKind, MessageId};

use crate::action::{Action, DragMessage, SortKey};
use crate::format::{addresses_label, format_datetime, format_size, plural, truncate};
use crate::state::{folder_rows, unread_total, UiState};
use crate::theme::{icons, palette, subtitle};

/// Height of one message row.
const ROW_HEIGHT: f32 = 26.0;
/// Height of the clickable column header above the message rows.
const HEADER_HEIGHT: f32 = 30.0;
/// Width of the folder panel.
const SIDEBAR_WIDTH: f32 = 240.0;

/// The raised surface of the toolbar and the status bar.
fn bar_frame() -> egui::Frame {
    egui::Frame::new()
        .fill(palette().panel_alt)
        .inner_margin(egui::Margin::symmetric(8, 5))
}

/// A clickable column title that carries the sort arrow when it is the active
/// column (`FR-LIST-5`).
fn sort_header(
    ui: &mut egui::Ui,
    actions: &mut Vec<Action>,
    sort: SortKey,
    ascending: bool,
    key: SortKey,
    label: &str,
) {
    let active = sort == key;
    let text = if active {
        RichText::new(format!(
            "{label} {}",
            if ascending {
                icons::SORT_ASC
            } else {
                icons::SORT_DESC
            }
        ))
    } else {
        RichText::new(label)
    }
    .color(if active {
        palette().text
    } else {
        palette().text_dim
    });
    if ui
        .selectable_label(active, text)
        .on_hover_text("Nach dieser Spalte sortieren")
        .clicked()
    {
        actions.push(Action::SortBy(key));
    }
}

/// Draws the whole window.
pub(crate) fn draw(
    ctx: &egui::Context,
    state: &mut UiState,
    actions: &mut Vec<Action>,
    links: &mut Vec<String>,
) {
    toolbar(ctx, state, actions);
    status_bar(ctx, state);
    folder_panel(ctx, state, actions);
    egui::CentralPanel::default().show(ctx, |ui| {
        if state.preview_visible {
            // The list occupies the upper half, the reading preview the lower
            // half, so at start-up the preview is half the height of the
            // full-height folder tree.  This is only the first-frame default, so
            // a drag on the divider still wins from the second frame on.
            let list_height = (ui.available_height() * 0.5).max(160.0);
            list_panel(ui, state, actions, list_height);
            reader_panel(ui, state, actions, links);
        } else {
            // The preview is hidden: the list uses the whole area.
            let height = ui.available_height();
            list_panel(ui, state, actions, height);
        }
    });
    // A message that was dragged and released without a folder accepting it is
    // "dragged out of the list" and is exported as a `.eml` (`FR-DND-2`).  The
    // folder panel runs first, so a real drop has already consumed the payload.
    if ctx.input(|input| input.pointer.any_released()) {
        if let Some(payload) = egui::DragAndDrop::take_payload::<DragMessage>(ctx) {
            actions.push(Action::ExportEml {
                message_id: payload.0,
            });
        }
    }
}

/// The toolbar and the filter row.
fn toolbar(ctx: &egui::Context, state: &mut UiState, actions: &mut Vec<Action>) {
    egui::TopBottomPanel::top("hylmail-toolbar")
        .frame(bar_frame())
        .show(ctx, |ui| {
            ui.horizontal_wrapped(|ui| {
                let has_message = state.selected.is_some();

                // The actions an everyday user needs, always visible.
                if ui
                    .button(format!("{}  Neu", icons::COMPOSE))
                    .on_hover_text("Neue Nachricht (Strg+N)")
                    .clicked()
                {
                    actions.push(Action::Compose);
                }

                ui.add_enabled_ui(has_message, |ui| {
                    if ui
                        .button(format!("{}  Antworten", icons::REPLY))
                        .on_hover_text("Antworten (Strg+R)")
                        .clicked()
                    {
                        actions.push(Action::Reply);
                    }
                    if ui
                        .button(format!("{}  Allen", icons::REPLY_ALL))
                        .on_hover_text("Allen antworten (Strg+Umschalt+R)")
                        .clicked()
                    {
                        actions.push(Action::ReplyAll);
                    }
                    if ui
                        .button(format!("{}  Weiterleiten", icons::FORWARD))
                        .on_hover_text("Weiterleiten (Strg+L)")
                        .clicked()
                    {
                        actions.push(Action::Forward);
                    }

                    ui.separator();

                    // The button says what it does: a message that already is in
                    // the trash cannot be moved there again, so it is deleted for good.
                    let (label, hint) = if state.selection_is_in_trash() {
                        (
                            format!("{}  Endgültig löschen", icons::DELETE),
                            "Endgültig löschen - der Papierkorb wird nicht benutzt (Entf)",
                        )
                    } else {
                        (
                            format!("{}  Löschen", icons::DELETE),
                            "In den Papierkorb verschieben (Entf)",
                        )
                    };
                    if ui.button(label).on_hover_text(hint).clicked() {
                        actions.push(Action::Delete);
                    }

                    ui.separator();

                    // The per-message marks, compact: the icon carries the
                    // meaning, the tooltip the wording.
                    if ui
                        .button(icons::FLAG)
                        .on_hover_text("Markieren (Strg+Leer)")
                        .clicked()
                    {
                        actions.push(Action::ToggleFlag);
                    }
                    if ui
                        .button(icons::READ)
                        .on_hover_text("Als gelesen markieren (Strg+Q)")
                        .clicked()
                    {
                        actions.push(Action::MarkRead);
                    }
                    if ui
                        .button(icons::UNREAD)
                        .on_hover_text("Als ungelesen markieren (Strg+Umschalt+Q)")
                        .clicked()
                    {
                        actions.push(Action::MarkUnread);
                    }
                    if ui
                        .button(icons::DONE)
                        .on_hover_text("Erledigt (E)")
                        .clicked()
                    {
                        actions.push(Action::Done);
                    }
                });

                // Everything administrative is one click away in the overflow
                // menu, so the bar itself stays calm and easy to scan.
                ui.separator();
                ui.menu_button("Menü", |ui| {
                    ui.add_enabled_ui(has_message, |ui| {
                        if ui.button(format!("{}  Verschieben", icons::MOVE)).clicked() {
                            actions.push(Action::OpenMovePicker);
                            ui.close();
                        }
                        if ui
                            .button(format!("{}  Archivieren", icons::ARCHIVE))
                            .clicked()
                        {
                            actions.push(Action::Archive);
                            ui.close();
                        }
                        if ui.button(format!("{}  Spam", icons::JUNK)).clicked() {
                            actions.push(Action::MarkJunk);
                            ui.close();
                        }
                        if ui
                            .button("Alles gelesen")
                            .on_hover_text("Jede Nachricht dieser Liste als gelesen markieren")
                            .clicked()
                        {
                            actions.push(Action::MarkAllRead);
                            ui.close();
                        }
                    });
                    ui.separator();
                    if ui
                        .button(format!("{}  Adressbuch", icons::CONTACTS))
                        .clicked()
                    {
                        actions.push(Action::OpenContacts);
                        ui.close();
                    }
                    if ui
                        .button(format!("{}  Postausgang", icons::OUTBOX))
                        .clicked()
                    {
                        actions.push(Action::OpenOutbox);
                        ui.close();
                    }
                    if ui.button(format!("{}  Regeln", icons::RULES)).clicked() {
                        actions.push(Action::OpenRules);
                        ui.close();
                    }
                    if ui.button(format!("{}  Konten", icons::ACCOUNTS)).clicked() {
                        actions.push(Action::OpenAccounts);
                        ui.close();
                    }
                    if ui
                        .button(format!("{}  Einstellungen", icons::SETTINGS))
                        .clicked()
                    {
                        actions.push(Action::OpenSettings);
                        ui.close();
                    }
                    ui.separator();
                    if ui.button(format!("{}  Beenden", icons::QUIT)).clicked() {
                        actions.push(Action::Quit);
                        ui.close();
                    }
                });
                if ui
                    .button(format!("{}  Abrufen", icons::SYNC))
                    .on_hover_text("Jetzt synchronisieren (F5)")
                    .clicked()
                {
                    actions.push(Action::SyncAll);
                }
            });

            ui.horizontal_wrapped(|ui| {
                ui.label("Suche:");
                let response = ui.add(
                    egui::TextEdit::singleline(&mut state.search)
                        .desired_width(260.0)
                        .hint_text("Betreff, Absender, Text (ab 3 Zeichen)"),
                );
                if state.focus_search {
                    response.request_focus();
                    state.focus_search = false;
                }
                let submitted =
                    response.lost_focus() && ui.input(|input| input.key_pressed(egui::Key::Enter));
                if ui.button("Suchen").clicked() {
                    actions.push(Action::Search(state.search.clone()));
                }
                if submitted {
                    actions.push(Action::ForceSearch(state.search.clone()));
                }
                if ui.button("Zurücksetzen").clicked() {
                    actions.push(Action::ClearSearch);
                }
                ui.separator();
                if ui
                    .selectable_label(state.unread_only, "Nur ungelesene")
                    .clicked()
                {
                    actions.push(Action::ToggleUnreadOnly);
                }
                if ui
                    .selectable_label(state.flagged_only, "Nur markierte")
                    .clicked()
                {
                    actions.push(Action::ToggleFlaggedOnly);
                }
                if ui
                    .selectable_label(state.attachment_only, "Nur mit Anhang")
                    .clicked()
                {
                    actions.push(Action::ToggleAttachmentOnly);
                }
                ui.separator();
                if ui
                    .selectable_label(state.threaded, "Konversationen")
                    .on_hover_text("Nachrichten zu Gesprächsfäden gruppieren")
                    .clicked()
                {
                    actions.push(Action::ToggleThreading);
                }
                if ui
                    .selectable_label(state.preview_visible, "Vorschau")
                    .on_hover_text("Die Lesevorschau in der unteren Hälfte zeigen oder ausblenden")
                    .clicked()
                {
                    actions.push(Action::TogglePreview);
                }
            });
        });
}

/// The status bar.
fn status_bar(ctx: &egui::Context, state: &mut UiState) {
    egui::TopBottomPanel::bottom("hylmail-status")
        .frame(bar_frame())
        .show(ctx, |ui| {
            ui.horizontal_wrapped(|ui| {
                if let Some(error) = state.error.clone() {
                    ui.colored_label(palette().error, format!("⚠ {error}"));
                    if ui.button("Ausblenden").clicked() {
                        state.error = None;
                    }
                } else if !state.status.is_empty() {
                    ui.colored_label(palette().success, &state.status);
                } else {
                    ui.colored_label(palette().text_dim, "Bereit");
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.colored_label(
                        palette().text_dim,
                        format!(
                            "{} Nachrichten · {} ungelesen",
                            state.total,
                            unread_total(&state.folders)
                        ),
                    );
                    if !state.selected_ids.is_empty() {
                        ui.colored_label(
                            palette().accent,
                            format!("{} ausgewählt", state.selected_ids.len()),
                        );
                    }
                    if state.new_mail > 0 {
                        ui.colored_label(palette().unread, format!("⬇ {} neu", state.new_mail));
                    }
                    for (account_id, connection) in &state.connections {
                        let name = state
                            .account(*account_id)
                            .map(|account| account.display_name.clone())
                            .unwrap_or_else(|| "Konto".to_string());
                        ui.separator();
                        ui.colored_label(palette().text_dim, format!("{name}: {connection}"));
                    }
                });
            });
        });
}

/// The folder tree on the left.
fn folder_panel(ctx: &egui::Context, state: &mut UiState, actions: &mut Vec<Action>) {
    egui::SidePanel::left("hylmail-folders")
        .resizable(true)
        .default_width(SIDEBAR_WIDTH)
        .width_range(160.0..=460.0)
        .show(ctx, |ui| {
            ui.add_space(6.0);
            let unread = unread_total(&state.folders);
            let active = state.all_inboxes && state.active_account.is_none();
            let label = if unread > 0 {
                format!("Alle Posteingänge  ({unread})")
            } else {
                "Alle Posteingänge".to_string()
            };
            if ui
                .selectable_label(active, RichText::new(label).text_style(subtitle()))
                .clicked()
            {
                actions.push(Action::SelectAccount(None));
            }
            ui.separator();

            let accounts = state.accounts.clone();
            ScrollArea::vertical()
                .id_salt("hylmail-folder-tree")
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    for account in &accounts {
                        account_section(ui, state, account, actions);
                    }
                    if accounts.is_empty() {
                        ui.add_space(8.0);
                        ui.colored_label(
                            palette().text_dim,
                            "Es ist noch kein Konto eingerichtet. Über „Konten“ lässt sich eines anlegen.",
                        );
                    }
                });
        });
}

/// One account and its folder tree.
fn account_section(
    ui: &mut egui::Ui,
    state: &mut UiState,
    account: &Account,
    actions: &mut Vec<Action>,
) {
    let collapsed = state.collapsed.contains(&account.id);
    let mine: Vec<Folder> = state
        .folders
        .iter()
        .filter(|folder| folder.account_id == account.id)
        .cloned()
        .collect();
    let unread: u32 = mine
        .iter()
        .filter(|folder| folder.kind == FolderKind::Inbox)
        .map(|folder| folder.unread_count)
        .sum();

    ui.add_space(4.0);
    ui.horizontal(|ui| {
        let arrow = if collapsed {
            icons::COLLAPSED
        } else {
            icons::SORT_DESC
        };
        if ui.small_button(arrow).clicked() {
            actions.push(Action::ToggleAccount(account.id));
        }
        let selected = state.all_inboxes && state.active_account == Some(account.id);
        let mut label = RichText::new(account.display_name.clone()).strong();
        if !account.enabled {
            label = RichText::new(format!("{} (inaktiv)", account.display_name));
        }
        if ui.selectable_label(selected, label).clicked() {
            actions.push(Action::SelectAccount(Some(account.id)));
        }
        if unread > 0 {
            ui.colored_label(palette().unread, format!("{unread}"));
        }
        if let Some(connection) = state.connections.get(&account.id) {
            let failed = connection.contains("nicht") || connection.contains("Fehler");
            ui.colored_label(
                if failed {
                    palette().error
                } else {
                    palette().success
                },
                icons::DOT,
            )
            .on_hover_text(connection.clone());
        }
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if ui
                .small_button(icons::RELOAD)
                .on_hover_text("Ordner neu laden")
                .clicked()
            {
                actions.push(Action::ReloadFolders(account.id));
            }
            if ui
                .small_button(icons::SYNC)
                .on_hover_text("Dieses Konto jetzt abrufen")
                .clicked()
            {
                actions.push(Action::SyncAccount(account.id));
            }
        });
    });
    if collapsed {
        return;
    }

    for (depth, folder_id) in folder_rows(&state.folders, account.id) {
        let Some(folder) = mine.iter().find(|folder| folder.id == folder_id) else {
            continue;
        };
        folder_row(ui, state, folder, depth, actions);
    }
}

/// One folder row of the tree.
fn folder_row(
    ui: &mut egui::Ui,
    state: &UiState,
    folder: &Folder,
    depth: usize,
    actions: &mut Vec<Action>,
) {
    let selected = state.selected_folder == Some(folder.id);
    let indent = 8.0 + depth as f32 * 14.0;
    ui.horizontal(|ui| {
        ui.add_space(indent);
        let icon = folder_icon(folder);
        let text = format!("{icon}  {}", folder.name);
        let label = if folder.unread_count > 0 {
            RichText::new(text).color(palette().unread)
        } else {
            RichText::new(text)
        };
        let response = ui.selectable_label(selected, label);
        // A message dragged onto the folder moves there (`FR-DND-1`).
        if response.dnd_hover_payload::<DragMessage>().is_some() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
        }
        if let Some(payload) = response.dnd_release_payload::<DragMessage>() {
            actions.push(Action::MoveDragged {
                message_id: payload.0,
                target: folder.id,
            });
        } else if response.clicked() {
            actions.push(Action::SelectFolder(folder.id));
        }
        if folder.unread_count > 0 {
            ui.colored_label(palette().text_dim, format!("{}", folder.unread_count));
        }
    });
}

/// The glyph of a well-known folder.
fn folder_icon(folder: &Folder) -> &'static str {
    match folder.kind {
        FolderKind::Inbox => icons::FOLDER_INBOX,
        FolderKind::Sent => icons::FOLDER_SENT,
        FolderKind::Drafts => icons::FOLDER_DRAFTS,
        FolderKind::Archive => icons::FOLDER_ARCHIVE,
        FolderKind::Junk => icons::FOLDER_JUNK,
        FolderKind::Trash => icons::FOLDER_TRASH,
        FolderKind::Outbox => icons::FOLDER_OUTBOX,
        FolderKind::VirtualAllInboxes => icons::FOLDER_ALL,
        FolderKind::Custom => icons::FOLDER_CUSTOM,
    }
}

/// The message list above the reading pane.
fn list_panel(
    ui: &mut egui::Ui,
    state: &mut UiState,
    actions: &mut Vec<Action>,
    default_height: f32,
) {
    egui::TopBottomPanel::top("hylmail-list")
        .resizable(true)
        .default_height(default_height)
        .height_range(120.0..=2000.0)
        .show_inside(ui, |ui| {
            // Fill the panel's full height even when the list is short or empty.
            // `TopBottomPanel` remembers its size from the content rect, so a
            // shrinking content would otherwise snap the divider back and the
            // dragged split would never stick.
            ui.set_min_height(ui.max_rect().height());
            ui.add_space(4.0);
            // A slim status line; the sorting itself lives in the clickable
            // column header below, so this only reports count and order.
            ui.horizontal(|ui| {
                ui.colored_label(
                    palette().text_dim,
                    format!("{} von {} geladen", state.messages.len(), state.total),
                );
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.colored_label(
                        palette().text_dim,
                        format!(
                            "Sortiert nach {}{}",
                            state.sort.label(),
                            if state.ascending {
                                icons::SORT_ASC
                            } else {
                                icons::SORT_DESC
                            }
                        ),
                    );
                });
            });

            if state.threaded && !state.conversations.is_empty() {
                threaded_list(ui, state, actions);
                ui.add_space(2.0);
                ui.horizontal(|ui| {
                    if (state.loaded as u64) < state.total
                        && ui
                            .button(format!(
                                "Weitere laden ({})",
                                state.total - state.loaded as u64
                            ))
                            .clicked()
                    {
                        actions.push(Action::LoadMore);
                    }
                });
                return;
            }

            // The sort state is copied out so the header closure below can read
            // it while the body closure still borrows the message list.
            let sort = state.sort;
            let ascending = state.ascending;
            // The account column only appears in the aggregate view; inside a
            // single account it would repeat the same name on every row
            // (`FR-UNIFIED-2`).
            let show_account = state.active_account.is_none();
            let messages = &state.messages;
            let selected = state.selected;
            // The ticked checkboxes, and whether the header box is fully on.
            let selected_ids = &state.selected_ids;
            let all_selected = !messages.is_empty()
                && messages
                    .iter()
                    .all(|header| selected_ids.contains(&header.id));
            if messages.is_empty() {
                ui.add_space(24.0);
                ui.vertical_centered(|ui| {
                    ui.label(RichText::new("Keine Nachrichten").text_style(subtitle()));
                    ui.colored_label(palette().text_dim, "In dieser Ansicht ist nichts zu sehen.");
                });
                return;
            }

            let mut clicked: Option<MessageId> = None;
            // The cells have to sense clicks: `TableBuilder` defaults to
            // `Sense::hover()`, and then no click ever reaches a row.
            let ctx = ui.ctx().clone();
            let mut table = TableBuilder::new(ui)
                .id_salt("hylmail-message-table")
                .sense(egui::Sense::click_and_drag())
                .striped(true)
                .auto_shrink([false, false])
                .cell_layout(Layout::left_to_right(Align::Center))
                .column(Column::exact(24.0))
                .column(Column::exact(18.0))
                .column(Column::exact(18.0))
                .column(Column::exact(24.0));
            if show_account {
                table = table.column(Column::initial(120.0).at_least(70.0).resizable(true));
            }
            table
                .column(Column::initial(180.0).at_least(90.0).resizable(true))
                .column(Column::remainder().at_least(140.0))
                .column(Column::exact(18.0))
                .column(Column::exact(90.0))
                .column(Column::exact(104.0))
                .min_scrolled_height(80.0)
                .header(HEADER_HEIGHT, |mut header| {
                    header.col(|ui| {
                        let mut checked = all_selected;
                        if ui
                            .checkbox(&mut checked, "")
                            .on_hover_text("Alle geladenen Nachrichten auswählen")
                            .changed()
                        {
                            if checked {
                                actions.push(Action::SelectAll);
                            } else {
                                actions.push(Action::ClearSelection);
                            }
                        }
                    });
                    header.col(|ui| {
                        sort_header(ui, actions, sort, ascending, SortKey::Unread, icons::DOT)
                    });
                    header.col(|ui| {
                        sort_header(ui, actions, sort, ascending, SortKey::Flagged, icons::FLAG)
                    });
                    header.col(|ui| {
                        ui.colored_label(palette().text_dim, icons::DONE)
                            .on_hover_text("Beantwortet / Erledigt");
                    });
                    if show_account {
                        header.col(|ui| {
                            ui.colored_label(palette().text_dim, "Konto");
                        });
                    }
                    header
                        .col(|ui| sort_header(ui, actions, sort, ascending, SortKey::From, "Von"));
                    header.col(|ui| {
                        sort_header(ui, actions, sort, ascending, SortKey::Subject, "Betreff");
                    });
                    header.col(|ui| {
                        sort_header(
                            ui,
                            actions,
                            sort,
                            ascending,
                            SortKey::Attachment,
                            icons::PAPERCLIP,
                        );
                    });
                    header.col(|ui| {
                        ui.colored_label(palette().text_dim, "Größe");
                    });
                    header.col(|ui| {
                        sort_header(ui, actions, sort, ascending, SortKey::Date, "Datum")
                    });
                })
                .body(|body| {
                    body.rows(ROW_HEIGHT, messages.len(), |mut row| {
                        let index = row.index();
                        let header = &messages[index];
                        row.set_selected(selected == Some(header.id));

                        // The bulk-selection checkbox (`FR-LIST-6`).
                        row.col(|ui| {
                            let mut checked = selected_ids.contains(&header.id);
                            if ui
                                .checkbox(&mut checked, "")
                                .on_hover_text("Für Sammelaktionen auswählen")
                                .changed()
                            {
                                actions.push(Action::ToggleSelection(header.id));
                            }
                        });

                        row.col(|ui| {
                            if header.is_unread() {
                                ui.colored_label(palette().accent, icons::DOT);
                            } else {
                                ui.colored_label(palette().text_dim, " ");
                            }
                        });

                        row.col(|ui| {
                            if header.is_flagged() {
                                ui.colored_label(palette().flagged, icons::FLAG);
                            }
                        });

                        // Answered `↩` and done `✓` marks (`FR-LIST-2`).
                        row.col(|ui| {
                            if header.is_answered() {
                                ui.colored_label(palette().text_dim, icons::REPLY)
                                    .on_hover_text("Beantwortet");
                            } else if header.is_done() {
                                ui.colored_label(palette().success, icons::DONE)
                                    .on_hover_text("Erledigt");
                            }
                        });

                        if show_account {
                            row.col(|ui| {
                                let name = state.account(header.account_id).map_or_else(
                                    || "—".to_string(),
                                    |account| account.display_name.clone(),
                                );
                                ui.colored_label(palette().text_dim, truncate(&name, 18));
                            });
                        }

                        row.col(|ui| {
                            ui.label(
                                RichText::new(truncate(&addresses_label(&header.from), 28)).color(
                                    if header.is_unread() {
                                        palette().text
                                    } else {
                                        palette().text_dim
                                    },
                                ),
                            );
                        });

                        row.col(|ui| {
                            let subject = if header.subject.trim().is_empty() {
                                "(kein Betreff)"
                            } else {
                                &header.subject
                            };
                            ui.label(RichText::new(subject).color(if header.is_unread() {
                                palette().unread
                            } else {
                                palette().text
                            }));
                        });

                        row.col(|ui| {
                            if header.has_attachments {
                                ui.colored_label(palette().text_dim, icons::PAPERCLIP);
                            }
                            if header.is_calendar_invite {
                                ui.colored_label(palette().accent_dim, icons::CALENDAR);
                            }
                        });

                        // The size column (`FR-LIST-1`).
                        row.col(|ui| {
                            if header.size_bytes > 0 {
                                ui.colored_label(
                                    palette().text_dim,
                                    format_size(header.size_bytes),
                                );
                            }
                        });

                        row.col(|ui| {
                            ui.colored_label(
                                palette().text_dim,
                                format_datetime(header.date.or(header.received_at)),
                            );
                        });

                        // The response of a row is the union of its cells, so
                        // a click anywhere in the row opens that message, and a
                        // drag from anywhere carries the message along.
                        let response = row.response();
                        response.dnd_set_drag_payload(DragMessage(header.id));
                        if response.hovered() {
                            ctx.set_cursor_icon(egui::CursorIcon::PointingHand);
                        }
                        if response.clicked() {
                            clicked = Some(header.id);
                        }
                    });
                });

            if let Some(id) = clicked {
                actions.push(Action::SelectMessage(id));
            }

            ui.add_space(2.0);
            ui.horizontal(|ui| {
                if (state.loaded as u64) < state.total
                    && ui
                        .button(format!(
                            "Weitere laden ({})",
                            state.total - state.loaded as u64
                        ))
                        .clicked()
                {
                    actions.push(Action::LoadMore);
                }
            });
        });
}

/// The conversation view: one collapsible row per thread.
///
/// A thread never crosses an account boundary (`FR-THREAD-2`): the summaries
/// come from [`hylmail_core::db::conversations`], which groups by account.  The
/// members shown under an expanded thread are the loaded messages that carry
/// the same conversation key.
fn threaded_list(ui: &mut egui::Ui, state: &UiState, actions: &mut Vec<Action>) {
    ScrollArea::vertical()
        .id_salt("hylmail-thread-list")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            for conversation in &state.conversations {
                let expanded = state.expanded_threads.contains(&conversation.thread_id);
                let arrow = if expanded {
                    icons::SORT_DESC
                } else {
                    icons::COLLAPSED
                };
                ui.horizontal(|ui| {
                    if ui.small_button(arrow).clicked() {
                        actions.push(Action::ToggleThread {
                            thread_id: conversation.thread_id.clone(),
                        });
                    }
                    if conversation.is_unread() {
                        ui.colored_label(palette().accent, icons::DOT);
                    } else {
                        ui.colored_label(palette().text_dim, " ");
                    }
                    let subject = conversation
                        .subject
                        .clone()
                        .unwrap_or_else(|| "(kein Betreff)".to_string());
                    let label = if conversation.is_unread() {
                        RichText::new(truncate(&subject, 60)).color(palette().unread)
                    } else {
                        RichText::new(truncate(&subject, 60))
                    };
                    ui.label(label);
                    ui.colored_label(
                        palette().text_dim,
                        format!("({} Nachrichten)", conversation.message_count),
                    );
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.colored_label(
                            palette().text_dim,
                            format_datetime(conversation.last_date),
                        );
                    });
                });
                if !expanded {
                    continue;
                }
                let members: Vec<&hylmail_core::models::MessageHeader> = state
                    .messages
                    .iter()
                    .filter(|header| {
                        header.thread_id.as_deref() == Some(conversation.thread_id.as_str())
                    })
                    .collect();
                for header in members {
                    ui.horizontal(|ui| {
                        ui.add_space(24.0);
                        if header.is_unread() {
                            ui.colored_label(palette().accent, icons::DOT);
                        } else {
                            ui.colored_label(palette().text_dim, " ");
                        }
                        ui.label(truncate(&addresses_label(&header.from), 24));
                        let subject = if header.subject.trim().is_empty() {
                            "(kein Betreff)"
                        } else {
                            &header.subject
                        };
                        if ui
                            .selectable_label(
                                state.selected == Some(header.id),
                                truncate(subject, 50),
                            )
                            .clicked()
                        {
                            actions.push(Action::SelectMessage(header.id));
                        }
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            ui.colored_label(
                                palette().text_dim,
                                format_datetime(header.date.or(header.received_at)),
                            );
                        });
                    });
                }
            }
        });
}

/// The summed size of the attachments whose size the server reported.
///
/// Attachments without a declared size are skipped, so the total is a lower
/// bound rather than a wrong number (`FR-LIST-1`).
fn attachments_total_size(attachments: &[hylmail_core::models::Attachment]) -> u64 {
    attachments
        .iter()
        .filter_map(|attachment| attachment.size_bytes)
        .sum()
}

/// The reading pane.
fn reader_panel(
    ui: &mut egui::Ui,
    state: &mut UiState,
    actions: &mut Vec<Action>,
    links: &mut Vec<String>,
) {
    // The session image cache is uploaded once; the clone hands the handles to
    // the drawing code while the reader is borrowed.
    prepare_textures(ui.ctx(), state);
    let textures = state.textures.clone();
    // The answer buttons are only offered for an account that carries a
    // calendar and for a message the user was actually invited to (`FR-CAL-3`).
    let can_reply = state
        .reader
        .as_ref()
        .and_then(|reader| {
            let account = state.account(reader.header.account_id)?;
            let calendar = reader.calendar.as_ref()?;
            Some(account.kind.supports_calendar() && calendar.can_reply(&account.email))
        })
        .unwrap_or(false);
    let Some(reader) = state.reader.as_mut() else {
        ui.centered_and_justified(|ui| {
            ui.vertical_centered(|ui| {
                ui.add_space(24.0);
                ui.label(
                    RichText::new(icons::COMPOSE)
                        .size(44.0)
                        .color(palette().text_dim),
                );
                ui.add_space(8.0);
                ui.label(RichText::new("Keine Nachricht ausgewählt").text_style(subtitle()));
                ui.add_space(2.0);
                ui.colored_label(
                    palette().text_dim,
                    "Wählen Sie oben eine Nachricht, um sie hier zu lesen.",
                );
            });
        });
        return;
    };

    ui.add_space(6.0);
    let unread = reader.header.is_unread();

    // Action bar above the header card, right-aligned: the familiar place for
    // the message actions.
    ui.horizontal(|ui| {
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if ui
                .selectable_label(
                    reader.header.is_answered(),
                    format!("{} Beantwortet", icons::REPLY),
                )
                .on_hover_text("Als beantwortet markieren (Strg+Umschalt+W)")
                .clicked()
            {
                actions.push(Action::ToggleAnswered);
            }
            if ui
                .button(if unread {
                    format!("{} Gelesen", icons::READ)
                } else {
                    format!("{} Ungelesen", icons::UNREAD)
                })
                .clicked()
            {
                actions.push(if unread {
                    Action::MarkRead
                } else {
                    Action::MarkUnread
                });
            }
            if ui
                .selectable_label(reader.show_source, "Quelltext")
                .clicked()
            {
                actions.push(Action::ToggleSource);
            }
            let images_blocked = reader
                .body
                .as_ref()
                .is_some_and(|body| body.external_images_blocked)
                && !reader.show_images;
            if images_blocked
                && ui
                    .button(format!("{} Externe Bilder anzeigen", icons::IMAGES))
                    .on_hover_text("Bilder werden erst nach Ihrer Zustimmung geladen")
                    .clicked()
            {
                actions.push(Action::ShowImages);
            }
            if images_blocked
                && ui
                    .button("Immer von diesem Absender")
                    .on_hover_text("Bilder dieses Absenders künftig immer laden")
                    .clicked()
            {
                actions.push(Action::TrustSenderImages);
            }
        });
    });

    // The message header as one raised card, so subject and metadata read as a
    // block and stand apart from the body below.
    ui.add_space(6.0);
    egui::Frame::new()
        .fill(palette().panel_alt)
        .stroke(egui::Stroke::new(1.0_f32, palette().border))
        .corner_radius(egui::CornerRadius::same(8))
        .inner_margin(egui::Margin::same(12))
        .show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label(
                    RichText::new(if reader.header.subject.trim().is_empty() {
                        "(kein Betreff)"
                    } else {
                        &reader.header.subject
                    })
                    .text_style(TextStyle::Heading),
                );
                if reader.header.is_calendar_invite {
                    ui.colored_label(
                        palette().accent,
                        format!("{} Termineinladung", icons::CALENDAR),
                    );
                }
            });
            ui.add_space(6.0);
            egui::Grid::new("hylmail-reader-headers")
                .num_columns(2)
                .spacing([12.0, 2.0])
                .show(ui, |ui| {
                    ui.colored_label(palette().text_dim, "Von:");
                    ui.label(addresses_label(&reader.header.from));
                    ui.end_row();
                    if !reader.header.to.is_empty() {
                        ui.colored_label(palette().text_dim, "An:");
                        ui.label(truncate(&addresses_label(&reader.header.to), 120));
                        ui.end_row();
                    }
                    if !reader.header.cc.is_empty() {
                        ui.colored_label(palette().text_dim, "Cc:");
                        ui.label(truncate(&addresses_label(&reader.header.cc), 120));
                        ui.end_row();
                    }
                    ui.colored_label(palette().text_dim, "Datum:");
                    ui.label(format_datetime(
                        reader.header.date.or(reader.header.received_at),
                    ));
                    ui.end_row();
                    if reader.header.size_bytes > 0 {
                        ui.colored_label(palette().text_dim, "Größe:");
                        ui.label(format_size(reader.header.size_bytes));
                        ui.end_row();
                    }
                });
        });
    ui.add_space(6.0);

    if let Some(calendar) = &reader.calendar {
        event_box(ui, calendar, can_reply, actions);
    }

    if let Some(body) = &reader.body {
        if !body.attachments.is_empty() {
            ui.add_space(4.0);
            let total_size = attachments_total_size(&body.attachments);
            ui.horizontal(|ui| {
                ui.collapsing(
                    plural(body.attachments.len() as u64, "Anhang", "Anhänge"),
                    |ui| {
                        for attachment in &body.attachments {
                            ui.horizontal(|ui| {
                                if ui
                                    .button(icons::SAVE)
                                    .on_hover_text("Diesen Anhang speichern")
                                    .clicked()
                                {
                                    actions.push(Action::SaveAttachment {
                                        part_id: attachment.part_id.clone(),
                                    });
                                }
                                ui.label(attachment.display_name());
                                ui.colored_label(palette().text_dim, &attachment.mime_type);
                                if let Some(size) = attachment.size_bytes {
                                    ui.colored_label(palette().text_dim, format_size(size));
                                }
                            });
                        }
                    },
                );
                if total_size > 0 {
                    ui.colored_label(
                        palette().text_dim,
                        format!("Gesamt: {}", format_size(total_size)),
                    );
                }
                if ui
                    .button("Alle speichern")
                    .on_hover_text("Alle Anhänge in einen Ordner schreiben")
                    .clicked()
                {
                    actions.push(Action::SaveAllAttachments);
                }
            });
        }
    }

    ui.add_space(6.0);
    ui.separator();
    // The scroll state is keyed by the message: without that a newly opened
    // mail would inherit the offset of the one before it.
    ScrollArea::vertical()
        .id_salt(("hylmail-reader-body", reader.header.id.to_string()))
        .auto_shrink([false, false])
        .show(ui, |ui| {
            if reader.loading {
                ui.add_space(12.0);
                ui.spinner();
                ui.colored_label(palette().text_dim, "Der Inhalt wird geladen …");
                return;
            }
            if let Some(error) = &reader.load_error {
                ui.colored_label(
                    palette().error,
                    format!("Der Inhalt konnte nicht geladen werden: {error}"),
                );
                return;
            }
            if reader.show_source {
                if let Some(body) = &reader.body {
                    let source = body
                        .html_sanitized
                        .as_deref()
                        .or(body.html_raw.as_deref())
                        .unwrap_or_default();
                    let mut job = egui::text::LayoutJob::default();
                    job.append(
                        source,
                        0.0,
                        egui::TextFormat {
                            font_id: egui::FontId::monospace(12.0),
                            color: palette().text,
                            ..egui::TextFormat::default()
                        },
                    );
                    ui.add(egui::Label::new(job).wrap());
                }
                return;
            }
            if let Some(document) = &reader.document {
                if document.is_empty() {
                    ui.colored_label(
                        palette().text_dim,
                        "Diese Nachricht hat keinen lesbaren Inhalt.",
                    );
                    return;
                }
                let mut copy = false;
                ui.horizontal(|ui| {
                    if ui
                        .small_button(format!("{} Text kopieren", icons::COPY))
                        .on_hover_text("Den lesbaren Text in die Zwischenablage legen")
                        .clicked()
                    {
                        copy = true;
                    }
                });
                if copy {
                    ui.ctx().copy_text(document.plain_text());
                }
                ui.add(
                    egui::Label::new(document.layout(palette(), 13.0, reader.original_colours))
                        .wrap(),
                );
                if !document.links.is_empty() {
                    ui.add_space(8.0);
                    ui.separator();
                    ui.label(RichText::new("Links").text_style(subtitle()));
                    for link in &document.links {
                        if ui
                            .link(format!("{} – {}", link.label, link.href))
                            .on_hover_text("Im Browser öffnen")
                            .clicked()
                        {
                            links.push(link.href.clone());
                        }
                    }
                }
                if reader.show_images {
                    render_images(ui, &textures, &document.images);
                }
                return;
            }
            if let Some(body) = &reader.body {
                if let Some(plain) = &body.plain_text {
                    ui.add(egui::Label::new(plain.as_str()).wrap());
                    return;
                }
            }
            ui.colored_label(
                palette().text_dim,
                "Diese Nachricht hat keinen lesbaren Inhalt.",
            );
        });
}

/// Uploads every decoded image the session cache holds as a texture, once.
fn prepare_textures(ctx: &egui::Context, state: &mut UiState) {
    if state.image_cache.is_empty() {
        return;
    }
    let cache = std::mem::take(&mut state.image_cache);
    for (url, decoded) in &cache {
        if state.textures.contains_key(url) {
            continue;
        }
        let image = egui::ColorImage::from_rgba_unmultiplied(
            [decoded.width as usize, decoded.height as usize],
            &decoded.rgba,
        );
        let handle = ctx.load_texture(url.clone(), image, egui::TextureOptions::LINEAR);
        state.textures.insert(url.clone(), handle);
    }
    state.image_cache = cache;
}

/// Draws the remote images of a message that have already been loaded.
fn render_images(
    ui: &mut egui::Ui,
    textures: &std::collections::HashMap<String, egui::TextureHandle>,
    images: &[crate::html_view::ImageRef],
) {
    for image in images {
        if !crate::html_view::is_remote_source(&image.src) {
            continue;
        }
        ui.add_space(4.0);
        match textures.get(&image.src) {
            Some(texture) => {
                ui.add(egui::Image::new(texture).max_width(520.0));
            }
            None => {
                let label = if image.alt.trim().is_empty() {
                    "[Bild wird geladen …]".to_string()
                } else {
                    format!("[Bild: {} wird geladen …]", image.alt.trim())
                };
                ui.colored_label(palette().text_dim, label);
            }
        }
    }
}

/// A framed invitation summary, with the answer buttons (`FR-CAL-2`).
fn event_box(
    ui: &mut egui::Ui,
    calendar: &hylmail_core::calendar::CalendarSummary,
    can_reply: bool,
    actions: &mut Vec<Action>,
) {
    egui::Frame::new()
        .fill(palette().panel_alt)
        .stroke(egui::Stroke::new(1.0_f32, palette().accent_dim))
        .corner_radius(egui::CornerRadius::same(6))
        .inner_margin(egui::Margin::same(10))
        .show(ui, |ui| {
            ui.label(RichText::new(calendar.title()).text_style(subtitle()));
            if let Some(when) = calendar.when() {
                ui.colored_label(palette().text_dim, when);
            }
            if let Some(organiser) = calendar.organizer_line() {
                ui.colored_label(palette().text_dim, organiser);
            }
            ui.label(calendar.describe());
            if can_reply {
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    for (status, text) in [
                        (hylmail_core::calendar::PartStat::Accepted, "✔ Zusagen"),
                        (
                            hylmail_core::calendar::PartStat::Tentative,
                            "? Mit Vorbehalt",
                        ),
                        (hylmail_core::calendar::PartStat::Declined, "✖ Absagen"),
                    ] {
                        if ui.button(text).clicked() {
                            actions.push(Action::CalendarReply { status });
                        }
                    }
                });
            }
        });
    ui.add_space(6.0);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::Reader;
    use hylmail_core::models::{
        utc_now, AccountId, Attachment, FolderId, MailAddress, MessageBody, MessageHeader, Settings,
    };

    /// A fixed window, so that the row geometry is reproducible.
    fn input(events: Vec<egui::Event>) -> egui::RawInput {
        egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1280.0, 800.0),
            )),
            events,
            ..egui::RawInput::default()
        }
    }

    /// Draws one frame and returns the actions the widgets produced.
    fn frame(ctx: &egui::Context, state: &mut UiState, events: Vec<egui::Event>) -> Vec<Action> {
        let mut actions = Vec::new();
        let mut links = Vec::new();
        let _ = ctx.run(input(events), |ctx| {
            draw(ctx, state, &mut actions, &mut links);
        });
        actions
    }

    /// A pointer move plus a primary button press at `pos`.
    fn press(pos: egui::Pos2) -> Vec<egui::Event> {
        vec![
            egui::Event::PointerMoved(pos),
            egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: egui::Modifiers::default(),
            },
        ]
    }

    /// The matching release, which completes the click.
    fn release(pos: egui::Pos2) -> Vec<egui::Event> {
        vec![egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed: false,
            modifiers: egui::Modifiers::default(),
        }]
    }

    /// A state that shows exactly one message in the list.
    fn one_message() -> (UiState, MessageId) {
        let mut state = UiState::new(50);
        let mut header = MessageHeader::new(AccountId::new(), FolderId::new());
        header.subject = "Quartalsbericht".to_string();
        header.from = vec![MailAddress::with_name("anna@example.org", "Anna")];
        header.date = Some(utc_now());
        let id = header.id;
        state.messages = vec![header];
        state.total = 1;
        state.loaded = 1;
        (state, id)
    }

    /// Clicks every point of the list area until a row reacts, and returns
    /// whether the message was opened.
    ///
    /// The scan walks a fixed grid instead of a hand-computed row rectangle so
    /// that the test survives a changed theme, font or panel height.
    fn click_first_reaction(ctx: &egui::Context, state: &mut UiState, id: MessageId) -> bool {
        for y in (24..560).step_by(4) {
            for x in [260.0_f32, 520.0] {
                let pos = egui::pos2(x, y as f32);
                frame(ctx, state, press(pos));
                let actions = frame(ctx, state, release(pos));
                if actions
                    .iter()
                    .any(|action| matches!(action, Action::SelectMessage(found) if *found == id))
                {
                    return true;
                }
            }
        }
        false
    }

    #[test]
    fn a_click_on_a_message_row_opens_it() {
        let ctx = egui::Context::default();
        crate::theme::apply(&ctx, hylmail_core::models::Theme::Dark);
        let (mut state, id) = one_message();
        // One warm-up frame, so that the table knows its geometry.
        frame(&ctx, &mut state, Vec::new());
        assert!(
            click_first_reaction(&ctx, &mut state, id),
            "no click inside the message list opened the message"
        );
    }

    /// A message with HTML, an attachment and an invitation, drawn in every
    /// reader mode.  Nothing here must panic: a panic inside `update` closes
    /// the whole window.
    #[test]
    fn the_reader_draws_html_attachments_and_an_invitation() {
        const ICS: &str = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//Example//DE\r\n\
             METHOD:REQUEST\r\nBEGIN:VEVENT\r\nUID:1\r\nDTSTAMP:20261112T120000Z\r\n\
             DTSTART:20261120T140000Z\r\nDTEND:20261120T150000Z\r\n\
             SUMMARY:Quartalsbesprechung\r\nLOCATION:Raum 4.12\r\n\
             ORGANIZER;CN=Ada Lovelace:mailto:ada@example.org\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";

        let ctx = egui::Context::default();
        crate::theme::apply(&ctx, hylmail_core::models::Theme::Dark);
        let (mut state, id) = one_message();
        let mut header = state.messages[0].clone();
        header.has_attachments = true;
        header.is_calendar_invite = true;

        let mut body = MessageBody::empty(id);
        body.plain_text = Some("Der Bericht liegt bei.".to_string());
        body.html_raw = Some(
            "<p>Der <b>Bericht</b> liegt <a href=\"https://example.org/bericht\">hier</a>.</p>
             <img src=\"https://example.org/logo.png\">"
                .to_string(),
        );
        body.calendar_ics = Some(ICS.to_string());
        body.attachments = vec![Attachment {
            part_id: "1.2".to_string(),
            filename: Some("bericht.pdf".to_string()),
            mime_type: "application/pdf".to_string(),
            size_bytes: Some(4096),
            content_id: None,
            inline: false,
            is_calendar: false,
        }];

        let mut reader = Reader::new(header);
        reader.calendar = body
            .calendar_ics
            .as_deref()
            .and_then(|ics| hylmail_core::calendar::parse(ics).ok());
        reader.body = Some(body);
        reader.resanitise(&Settings::default());
        state.reader = Some(reader);

        for (show_source, show_images) in [(false, false), (true, false), (false, true)] {
            if let Some(reader) = state.reader.as_mut() {
                reader.show_source = show_source;
                reader.show_images = show_images;
                reader.resanitise(&Settings::default());
            }
            frame(&ctx, &mut state, Vec::new());
        }
    }

    /// A named attachment, so the total-size test reads at a glance.
    fn attachment(part_id: &str, size: Option<u64>) -> Attachment {
        Attachment {
            part_id: part_id.to_string(),
            filename: Some(format!("{part_id}.bin")),
            mime_type: "application/octet-stream".to_string(),
            size_bytes: size,
            content_id: None,
            inline: false,
            is_calendar: false,
        }
    }

    #[test]
    fn the_attachment_total_sums_the_known_sizes() {
        let attachments = vec![
            attachment("1", Some(100)),
            attachment("2", Some(250)),
            attachment("3", None),
        ];
        assert_eq!(
            attachments_total_size(&attachments),
            350,
            "a size the server did not report must not be guessed"
        );
        assert_eq!(attachments_total_size(&[]), 0);
    }
}
