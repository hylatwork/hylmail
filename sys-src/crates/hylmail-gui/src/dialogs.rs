//! The composer, the settings window and the accounts window.
//!
//! Like [`crate::view`], nothing here performs I/O.  The dialogs edit a
//! working copy and push an [`Action`] when the user asks for something that
//! touches the disk or the network.

use egui::{Align, Layout, RichText, ScrollArea};

use hylmail_core::models::{
    Account, AccountId, AccountKind, AuthKind, Endpoint, ExternalImagePolicy, Folder, FolderKind,
    GroupWiseProxyConfig, Settings, SignaturePosition, Theme, TlsMode,
};

use crate::action::Action;
use crate::format::{address_label, format_size, plural, truncate};
use crate::state::{
    AccountsForm, ComposeMode, Composer, ContactsForm, MovePicker, RulesForm, SettingsForm, UiState,
};
use crate::theme::{icons, palette, subtitle};

/// Draws every open dialog.
pub(crate) fn draw(ctx: &egui::Context, state: &mut UiState, actions: &mut Vec<Action>) {
    composer_window(ctx, state, actions);
    settings_window(ctx, state, actions);
    accounts_window(ctx, state, actions);
    rules_window(ctx, state, actions);
    contacts_window(ctx, state, actions);
    move_window(ctx, state, actions);
    delete_window(ctx, state, actions);
    outbox_window(ctx, state, actions);
}

/// The German label of a queue state.
fn outbox_state_label(state: hylmail_core::models::OutboxState) -> &'static str {
    match state {
        hylmail_core::models::OutboxState::Queued => "wartet",
        hylmail_core::models::OutboxState::Sending => "sendet",
        hylmail_core::models::OutboxState::Sent => "gesendet",
        hylmail_core::models::OutboxState::Failed => "fehlgeschlagen",
        hylmail_core::models::OutboxState::Deferred => "zurückgestellt",
    }
}

/// The outgoing queue (`FR-OUTBOX-3`).
fn outbox_window(ctx: &egui::Context, state: &mut UiState, actions: &mut Vec<Action>) {
    if state.outbox_form.is_none() {
        return;
    }
    let mut open = true;
    egui::Window::new("Postausgang")
        .id(egui::Id::new("hylmail-outbox"))
        .open(&mut open)
        .resizable(true)
        .default_size([580.0, 420.0])
        .show(ctx, |ui| {
            let Some(form) = state.outbox_form.as_mut() else {
                return;
            };
            if form.document.items.is_empty() {
                ui.colored_label(palette().text_dim, "Der Postausgang ist leer.");
            } else {
                ScrollArea::vertical()
                    .id_salt("hylmail-outbox-list")
                    .max_height(300.0)
                    .show(ui, |ui| {
                        for item in &form.document.items {
                            let subject = if item.draft.subject.trim().is_empty() {
                                "(kein Betreff)"
                            } else {
                                item.draft.subject.as_str()
                            };
                            let recipients = item
                                .draft
                                .to
                                .iter()
                                .map(|address| address.email.as_str())
                                .collect::<Vec<_>>()
                                .join(", ");
                            ui.horizontal(|ui| {
                                ui.colored_label(
                                    palette().text_dim,
                                    outbox_state_label(item.state),
                                );
                                ui.label(truncate(subject, 40));
                                ui.colored_label(palette().text_dim, truncate(&recipients, 30));
                            });
                            if let Some(error) = &item.last_error {
                                ui.colored_label(palette().error, truncate(error, 70));
                            }
                            ui.horizontal(|ui| {
                                if ui.small_button("Erneut versuchen").clicked() {
                                    actions.push(Action::OutboxRetry { id: item.id });
                                }
                                if ui.small_button("Entfernen").clicked() {
                                    actions.push(Action::OutboxRemove { id: item.id });
                                }
                            });
                            ui.separator();
                        }
                    });
            }
            if let Some(status) = &form.status {
                ui.colored_label(palette().success, status);
            }
            ui.horizontal(|ui| {
                if ui.button("Jetzt senden").clicked() {
                    actions.push(Action::OutboxSendNow);
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.button("Schließen").clicked() {
                        actions.push(Action::CloseDialog);
                    }
                });
            });
        });
    if !open {
        actions.push(Action::CloseDialog);
    }
}

/// The wording of the delete confirmation, for one message or a whole batch.
fn delete_prompt(state: &UiState) -> String {
    let ids = &state.pending_delete;
    if let [id] = ids.as_slice() {
        let subject = state
            .messages
            .iter()
            .find(|header| header.id == *id)
            .map(|header| header.subject_or_placeholder().to_string())
            .unwrap_or_default();
        format!("„{subject}“ in den Papierkorb verschieben?")
    } else {
        format!("{} Nachrichten in den Papierkorb verschieben?", ids.len())
    }
}

/// The delete confirmation prompt (`FR-FLAG-4`).
fn delete_window(ctx: &egui::Context, state: &mut UiState, actions: &mut Vec<Action>) {
    if state.pending_delete.is_empty() {
        return;
    }
    let prompt = delete_prompt(state);
    egui::Window::new("Löschen bestätigen")
        .id(egui::Id::new("hylmail-confirm-delete"))
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .show(ctx, |ui| {
            ui.label(prompt.as_str());
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if ui.button("Löschen").clicked() {
                    actions.push(Action::ConfirmDelete);
                }
                if ui.button("Abbrechen").clicked() {
                    actions.push(Action::CancelDelete);
                }
            });
        });
}

/// The folder picker behind "Verschieben".
///
/// The list is derived from the folder tree every frame, so a synchronisation
/// that adds a mailbox is visible at once.  Folders that cannot take the
/// message are not offered at all.
fn move_window(ctx: &egui::Context, state: &mut UiState, actions: &mut Vec<Action>) {
    let Some(picker) = state.picker.as_ref() else {
        return;
    };
    let mut open = true;
    let folder_id = picker.folder_id;
    let filter = picker.filter.clone();
    let candidates: Vec<Folder> = state
        .move_targets(folder_id)
        .filter(|folder| MovePicker::passes(&filter, folder))
        .cloned()
        .collect();

    egui::Window::new("Nachricht verschieben")
        .id(egui::Id::new("hylmail-move"))
        .open(&mut open)
        .collapsible(false)
        .resizable(true)
        .default_width(360.0)
        .default_height(380.0)
        .show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label("Filter:");
                if let Some(picker) = state.picker.as_mut() {
                    ui.add(
                        egui::TextEdit::singleline(&mut picker.filter)
                            .desired_width(200.0)
                            .hint_text("Ordner suchen"),
                    );
                }
            });
            ui.add_space(4.0);
            ScrollArea::vertical().show(ui, |ui| {
                if candidates.is_empty() {
                    ui.colored_label(palette().text_dim, "Kein Ordner passt zu diesem Filter.");
                    return;
                }
                for folder in &candidates {
                    let label = if folder.unread_count > 0 {
                        format!("{}  ({})", folder.name, folder.unread_count)
                    } else {
                        folder.name.clone()
                    };
                    if ui.button(label).clicked() {
                        actions.push(Action::MoveTo { target: folder.id });
                    }
                }
            });
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                if ui.button("Abbrechen").clicked() {
                    actions.push(Action::CloseDialog);
                }
                let count = u64::try_from(candidates.len()).unwrap_or(u64::MAX);
                ui.colored_label(
                    palette().text_dim,
                    plural(count, "Zielordner", "Zielordner"),
                );
            });
        });
    if !open {
        actions.push(Action::CloseDialog);
    }
}

/// The message being written.
fn composer_window(ctx: &egui::Context, state: &mut UiState, actions: &mut Vec<Action>) {
    if state.composer.is_none() {
        return;
    }
    let mut open = true;
    let title = state
        .composer
        .as_ref()
        .map_or_else(|| "Nachricht".to_string(), composer_title);
    // Moved out for the duration of the frame so the composer and the address
    // book can be borrowed at the same time; restored right after.
    let contacts = std::mem::take(&mut state.contacts);
    egui::Window::new(title)
        .id(egui::Id::new("hylmail-composer"))
        .open(&mut open)
        .collapsible(true)
        .resizable(true)
        .default_size([720.0, 560.0])
        .show(ctx, |ui| {
            let Some(composer) = state.composer.as_mut() else {
                return;
            };
            composer_form(ui, composer, &contacts, actions);
        });
    state.contacts = contacts;
    if !open {
        actions.push(Action::CloseDialog);
    }
}

/// The heading of the composer window.
fn composer_title(composer: &Composer) -> String {
    match composer.mode {
        ComposeMode::New => "Neue Nachricht".to_string(),
        ComposeMode::Reply { all: false } => "Antworten".to_string(),
        ComposeMode::Reply { all: true } => "Allen antworten".to_string(),
        ComposeMode::Forward => "Weiterleiten".to_string(),
    }
}

/// The fields of the composer.
fn composer_form(
    ui: &mut egui::Ui,
    composer: &mut Composer,
    contacts: &[hylmail_core::models::Contact],
    actions: &mut Vec<Action>,
) {
    egui::Grid::new("hylmail-composer-fields")
        .num_columns(3)
        .spacing([8.0, 6.0])
        .show(ui, |ui| {
            ui.colored_label(palette().text_dim, "Von:");
            ui.label(address_label(&composer.from));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if !composer.show_cc && ui.small_button("Cc").clicked() {
                    composer.show_cc = true;
                }
                if !composer.show_bcc && ui.small_button("Bcc").clicked() {
                    composer.show_bcc = true;
                }
            });
            ui.end_row();

            if composer.mode == ComposeMode::New {
                ui.colored_label(palette().text_dim, "An:");
                ui.add(
                    egui::TextEdit::singleline(&mut composer.to)
                        .desired_width(420.0)
                        .hint_text("name@beispiel.de, weitere@beispiel.de"),
                );
                ui.label("");
                ui.end_row();
            } else {
                ui.colored_label(palette().text_dim, "An:");
                ui.add(egui::TextEdit::singleline(&mut composer.to).desired_width(420.0));
                ui.label("");
                ui.end_row();
            }

            if composer.show_cc {
                ui.colored_label(palette().text_dim, "Cc:");
                ui.add(egui::TextEdit::singleline(&mut composer.cc).desired_width(420.0));
                if ui
                    .small_button(icons::CLOSE)
                    .on_hover_text("Cc ausblenden")
                    .clicked()
                {
                    composer.show_cc = false;
                    composer.cc.clear();
                }
                ui.end_row();
            }
            if composer.show_bcc {
                ui.colored_label(palette().text_dim, "Bcc:");
                ui.add(egui::TextEdit::singleline(&mut composer.bcc).desired_width(420.0));
                if ui
                    .small_button(icons::CLOSE)
                    .on_hover_text("Bcc ausblenden")
                    .clicked()
                {
                    composer.show_bcc = false;
                    composer.bcc.clear();
                }
                ui.end_row();
            }

            ui.colored_label(palette().text_dim, "Betreff:");
            ui.add(egui::TextEdit::singleline(&mut composer.subject).desired_width(420.0));
            ui.label("");
            ui.end_row();
        });
    recipient_suggestions(ui, composer, contacts);

    if let Some(signature) = composer.signature.clone() {
        let mut position = signature.position;
        let mut drop_signature = false;
        ui.horizontal(|ui| {
            ui.colored_label(palette().text_dim, format!("Signatur: {}", signature.name));
            // For a reply or forward the user chooses whether the signature goes
            // above or below the quote (`FR-COMPOSE-6`).
            if composer.mode != ComposeMode::New {
                for (value, label) in [
                    (SignaturePosition::AboveQuote, "Über dem Zitat"),
                    (SignaturePosition::BelowQuote, "Unter dem Zitat"),
                ] {
                    if ui.selectable_label(position == value, label).clicked() {
                        position = value;
                    }
                }
            }
            if ui.small_button("Entfernen").clicked() {
                drop_signature = true;
            }
        });
        if drop_signature {
            composer.signature = None;
        } else if position != signature.position {
            if let Some(current) = composer.signature.as_mut() {
                current.position = position;
            }
        }
    }

    if !composer.attachments.is_empty() {
        ui.add_space(4.0);
        let rows: Vec<usize> = (0..composer.attachments.len()).collect();
        let mut remove: Option<usize> = None;
        ui.collapsing(
            plural(composer.attachments.len() as u64, "Anhang", "Anhänge"),
            |ui| {
                for index in rows {
                    let Some(attachment) = composer.attachments.get(index) else {
                        continue;
                    };
                    ui.horizontal(|ui| {
                        ui.label(&attachment.filename);
                        ui.colored_label(palette().text_dim, format_size(attachment.size_bytes));
                        if ui
                            .small_button(icons::CLOSE)
                            .on_hover_text("Entfernen")
                            .clicked()
                        {
                            remove = Some(index);
                        }
                    });
                }
            },
        );
        if let Some(index) = remove {
            composer.attachments.remove(index);
        }
    }

    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.label("Format:");
        if ui.selectable_label(!composer.html_mode, "Text").clicked() {
            composer.html_mode = false;
        }
        if ui
            .selectable_label(composer.html_mode, "HTML")
            .on_hover_text("Rich-Text als HTML bearbeiten")
            .clicked()
        {
            composer.html_mode = true;
        }
    });

    ui.add_space(4.0);
    ScrollArea::vertical()
        .id_salt("hylmail-composer-body")
        .max_height(280.0)
        .auto_shrink([false, false])
        .show(ui, |ui| {
            if composer.html_mode {
                ui.add(
                    egui::TextEdit::multiline(&mut composer.body_html)
                        .desired_width(f32::INFINITY)
                        .desired_rows(14)
                        .code_editor()
                        .hint_text("<p>Ihre Nachricht …</p>"),
                );
            } else {
                ui.add(
                    egui::TextEdit::multiline(&mut composer.body)
                        .desired_width(f32::INFINITY)
                        .desired_rows(14)
                        .hint_text("Ihre Nachricht …"),
                );
            }
        });

    if let Some(error) = &composer.error {
        ui.colored_label(palette().error, format!("⚠ {error}"));
    } else if let Some(status) = &composer.status {
        ui.colored_label(palette().success, status);
    }

    ui.add_space(6.0);
    ui.horizontal(|ui| {
        let send = egui::Button::new(format!("{}  Senden", icons::COMPOSE)).fill(palette().accent);
        if ui.add_enabled(!composer.busy, send).clicked() {
            actions.push(Action::Send);
        }
        if ui
            .button(format!("{}  Entwurf", icons::SAVE))
            .on_hover_text("Als Entwurf speichern (Strg+E)")
            .clicked()
        {
            actions.push(Action::SaveDraft);
        }
        if ui.button(format!("{}  Anhang", icons::PAPERCLIP)).clicked() {
            actions.push(Action::Attach);
        }
        if ui
            .button(format!("{}  Bild einbetten", icons::IMAGES))
            .on_hover_text("Ein Bild in den Text einfügen")
            .clicked()
        {
            actions.push(Action::EmbedImage);
        }
        if composer.signature.is_none() && ui.button("Signatur").clicked() {
            actions.push(Action::OpenSettings);
        }
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if ui.button("Abbrechen").clicked() {
                actions.push(Action::CloseDialog);
            }
        });
        if composer.busy {
            ui.spinner();
            ui.colored_label(palette().text_dim, "Wird gesendet …");
        }
    });
}

/// Address suggestions for the recipient field (`FR-COMPOSE-5`).
///
/// The last comma separated token of the `An` field is matched against the
/// address book; picking a suggestion replaces that token.
fn recipient_suggestions(
    ui: &mut egui::Ui,
    composer: &mut Composer,
    contacts: &[hylmail_core::models::Contact],
) {
    let token = composer
        .to
        .rsplit([',', ';'])
        .next()
        .unwrap_or("")
        .trim()
        .to_string();
    if token.chars().count() < 2 {
        return;
    }
    let needle = token.to_lowercase();
    let matches: Vec<&hylmail_core::models::Contact> = contacts
        .iter()
        .filter(|contact| {
            contact.label().to_lowercase().contains(&needle)
                || contact
                    .primary_email()
                    .is_some_and(|email| email.to_lowercase().contains(&needle))
        })
        .take(5)
        .collect();
    if matches.is_empty() {
        return;
    }
    ui.horizontal_wrapped(|ui| {
        ui.colored_label(palette().text_dim, "Vorschläge:");
        for contact in matches {
            let Some(email) = contact.primary_email() else {
                continue;
            };
            let label = if contact.label() == email {
                email.to_string()
            } else {
                format!("{} <{email}>", contact.label())
            };
            if ui.small_button(&label).clicked() {
                let head = match composer.to.rfind([',', ';']) {
                    Some(index) => composer.to[..=index].to_string(),
                    None => String::new(),
                };
                composer.to = format!("{head}{label}, ");
            }
        }
    });
}

/// The settings window, including the signature editor.
fn settings_window(ctx: &egui::Context, state: &mut UiState, actions: &mut Vec<Action>) {
    if state.settings_form.is_none() {
        return;
    }
    // Cloned so the form and the folder list can be borrowed at the same time.
    let folders = state.folders.clone();
    let accounts = state.accounts.clone();
    let mut open = true;
    egui::Window::new("Einstellungen")
        .id(egui::Id::new("hylmail-settings"))
        .open(&mut open)
        .resizable(true)
        .default_size([620.0, 560.0])
        .show(ctx, |ui| {
            let Some(form) = state.settings_form.as_mut() else {
                return;
            };
            ScrollArea::vertical()
                .id_salt("hylmail-settings-body")
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    appearance_section(ui, &mut form.settings);
                    behaviour_section(ui, &mut form.settings);
                    security_section(ui, form);
                    addressbook_section(ui, &mut form.settings);
                    folder_override_section(ui, &mut form.settings, &folders, &accounts);
                    signature_section(ui, form, &accounts);
                    if let Some(error) = &form.error {
                        ui.colored_label(palette().error, format!("⚠ {error}"));
                    } else if let Some(status) = &form.status {
                        ui.colored_label(palette().success, status);
                    }
                });

            ui.add_space(4.0);
            ui.horizontal(|ui| {
                if ui.button("Speichern").clicked() {
                    actions.push(Action::SaveSettings);
                }
                if ui.button("Verwerfen").clicked() {
                    actions.push(Action::CloseDialog);
                }
            });
        });
    if !open {
        actions.push(Action::CloseDialog);
    }
}

/// The appearance: the theme and the size limits.
fn appearance_section(ui: &mut egui::Ui, settings: &mut Settings) {
    ui.label(RichText::new("Darstellung").text_style(subtitle()));
    egui::Grid::new("hylmail-settings-appearance")
        .num_columns(2)
        .spacing([10.0, 6.0])
        .show(ui, |ui| {
            ui.label("Erscheinungsbild:");
            ui.horizontal(|ui| {
                for (value, label) in [(Theme::Dark, "Dunkel"), (Theme::Light, "Hell")] {
                    if ui
                        .selectable_label(settings.theme == value, label)
                        .clicked()
                    {
                        settings.theme = value;
                    }
                }
            });
            ui.end_row();

            ui.label("Nachrichten pro Seite:");
            ui.add(egui::DragValue::new(&mut settings.page_size).range(50..=5000));
            ui.end_row();

            ui.label("Cache-Grenze (MB):");
            ui.add(egui::DragValue::new(&mut settings.cache_limit_mb).range(16..=65536));
            ui.end_row();

            ui.label("Synchronisierung (Minuten):");
            ui.add(egui::DragValue::new(&mut settings.sync_interval_minutes).range(0..=240));
            ui.end_row();
        });
    ui.colored_label(
        palette().text_dim,
        "Die Darstellung wird beim Speichern angewendet.",
    );
    ui.add_space(8.0);
}

/// Delete confirmation, trash handling and the done key.
fn behaviour_section(ui: &mut egui::Ui, settings: &mut Settings) {
    ui.label(RichText::new("Verhalten").text_style(subtitle()));
    ui.checkbox(
        &mut settings.confirm_delete,
        "Vor dem endgültigen Löschen nachfragen",
    );
    ui.checkbox(
        &mut settings.empty_trash_on_exit,
        "Papierkorb beim Beenden leeren",
    );
    ui.checkbox(
        &mut settings.show_all_inboxes_on_start,
        "Beim Start „Alle Posteingänge“ anzeigen",
    );
    ui.checkbox(
        &mut settings.dark_mode_transform,
        "Einfache Nachrichten abdunkeln",
    );
    ui.checkbox(&mut settings.sanitize_html, "HTML bereinigen (empfohlen)");
    ui.add_space(8.0);
}

/// External images and tracking protection.
fn security_section(ui: &mut egui::Ui, form: &mut SettingsForm) {
    ui.label(RichText::new("Sicherheit").text_style(subtitle()));
    ui.horizontal(|ui| {
        ui.label("Externe Bilder:");
        for (value, label) in [
            (ExternalImagePolicy::Block, "Blockieren"),
            (ExternalImagePolicy::Ask, "Nachfragen"),
            (ExternalImagePolicy::Allow, "Erlauben"),
        ] {
            if ui
                .selectable_label(form.settings.external_images == value, label)
                .clicked()
            {
                form.settings.external_images = value;
            }
        }
    });
    ui.horizontal(|ui| {
        ui.label("Tracking-Domains:");
        let changed = ui
            .add(
                egui::TextEdit::singleline(&mut form.tracking_domains)
                    .desired_width(360.0)
                    .hint_text("tracker.example, ads.example"),
            )
            .changed();
        if changed {
            form.settings.blocked_tracking_domains = parse_tracking_domains(&form.tracking_domains);
        }
    });
    if ui
        .button("Vertrauenswürdige Absender zurücksetzen")
        .clicked()
    {
        form.settings.trusted_image_senders.clear();
    }
    ui.add_space(8.0);
}

/// Splits the comma separated tracking-domain field into the stored list.
fn parse_tracking_domains(text: &str) -> Vec<String> {
    text.split([',', ';', '\n', ' '])
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// The CardDAV server used for the address book.
fn addressbook_section(ui: &mut egui::Ui, settings: &mut Settings) {
    ui.label(RichText::new("Adressbuch (CardDAV)").text_style(subtitle()));
    egui::Grid::new("hylmail-settings-carddav")
        .num_columns(2)
        .spacing([10.0, 6.0])
        .show(ui, |ui| {
            ui.label("Server-Adresse:");
            ui.add(
                egui::TextEdit::singleline(&mut settings.carddav_url)
                    .desired_width(340.0)
                    .hint_text("https://dav.example.org/addressbooks/user/"),
            );
            ui.end_row();
            ui.label("Benutzername:");
            ui.add(
                egui::TextEdit::singleline(&mut settings.carddav_username)
                    .desired_width(340.0)
                    .hint_text("leer = Name des Kontos"),
            );
            ui.end_row();
        });
    ui.colored_label(
        palette().text_dim,
        "Das Kennwort ist das des zugehörigen Kontos.",
    );
    ui.add_space(8.0);
}

/// Manual overrides of the special-folder mapping (`FR-FOLDER-3`).
fn folder_override_section(
    ui: &mut egui::Ui,
    settings: &mut Settings,
    folders: &[Folder],
    accounts: &[Account],
) {
    ui.label(RichText::new("Spezialordner").text_style(subtitle()));
    if accounts.is_empty() {
        ui.colored_label(palette().text_dim, "Es ist noch kein Konto eingerichtet.");
        ui.add_space(8.0);
        return;
    }
    let roles = [
        (FolderKind::Inbox, "Posteingang"),
        (FolderKind::Sent, "Gesendet"),
        (FolderKind::Drafts, "Entwürfe"),
        (FolderKind::Trash, "Papierkorb"),
        (FolderKind::Junk, "Spam"),
        (FolderKind::Archive, "Archiv"),
    ];
    for account in accounts {
        ui.colored_label(palette().text_dim, &account.display_name);
        let mine: Vec<&Folder> = folders
            .iter()
            .filter(|folder| folder.account_id == account.id)
            .collect();
        egui::Grid::new(("hylmail-folder-overrides", account.id))
            .num_columns(2)
            .spacing([10.0, 4.0])
            .show(ui, |ui| {
                for (kind, label) in roles {
                    ui.label(label);
                    let current = settings
                        .folder_overrides
                        .iter()
                        .find(|entry| entry.account_id == account.id && entry.kind == kind)
                        .map(|entry| entry.folder_id);
                    let selected_name = current
                        .and_then(|id| mine.iter().find(|folder| folder.id == id))
                        .map_or_else(|| "Automatisch".to_string(), |folder| folder.name.clone());
                    let mut chosen = current;
                    egui::ComboBox::from_id_salt(("hylmail-override", account.id, kind))
                        .selected_text(selected_name)
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut chosen, None, "Automatisch");
                            for folder in &mine {
                                ui.selectable_value(&mut chosen, Some(folder.id), &folder.name);
                            }
                        });
                    if chosen != current {
                        settings.folder_overrides.retain(|entry| {
                            !(entry.account_id == account.id && entry.kind == kind)
                        });
                        if let Some(folder_id) = chosen {
                            settings
                                .folder_overrides
                                .push(hylmail_core::models::FolderOverride {
                                    account_id: account.id,
                                    kind,
                                    folder_id,
                                });
                        }
                    }
                    ui.end_row();
                }
            });
        ui.add_space(4.0);
    }
    ui.colored_label(
        palette().text_dim,
        "„Automatisch“ lässt HylMail die Rolle aus dem Server erkennen.",
    );
    ui.add_space(8.0);
}

/// The rule editor (`FR-RULE-6`).
fn rules_window(ctx: &egui::Context, state: &mut UiState, actions: &mut Vec<Action>) {
    if state.rules_form.is_none() {
        return;
    }
    let mut open = true;
    egui::Window::new("Regeln")
        .id(egui::Id::new("hylmail-rules"))
        .open(&mut open)
        .resizable(true)
        .default_size([680.0, 520.0])
        .show(ctx, |ui| {
            let Some(form) = state.rules_form.as_mut() else {
                return;
            };
            ui.horizontal_top(|ui| {
                rule_list(ui, form, actions);
                ui.separator();
                ui.vertical(|ui| {
                    ui.set_min_width(320.0);
                    rule_detail(ui, form);
                });
            });
            ui.add_space(4.0);
            if let Some(error) = &form.error {
                ui.colored_label(palette().error, format!("⚠ {error}"));
            } else if let Some(status) = &form.status {
                ui.colored_label(palette().success, status);
            }
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                if ui.button("Speichern").clicked() {
                    actions.push(Action::SaveRules);
                }
                if ui
                    .button("Regeln jetzt anwenden")
                    .on_hover_text("Die Regeln auf die angezeigten Nachrichten anwenden")
                    .clicked()
                {
                    actions.push(Action::RunRules);
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.button("Schließen").clicked() {
                        actions.push(Action::CloseDialog);
                    }
                });
            });
        });
    if !open {
        actions.push(Action::CloseDialog);
    }
}

/// The left pane of the rule editor: the rule list plus add/remove.
fn rule_list(ui: &mut egui::Ui, form: &mut RulesForm, actions: &mut Vec<Action>) {
    ui.vertical(|ui| {
        ui.set_min_width(200.0);
        ui.label(RichText::new("Regeln").text_style(subtitle()));
        let entries: Vec<(hylmail_core::models::RuleId, String, bool)> = form
            .rules
            .rules
            .iter()
            .map(|rule| (rule.id, rule.name.clone(), rule.enabled))
            .collect();
        ScrollArea::vertical()
            .id_salt("hylmail-rule-list")
            .max_height(340.0)
            .show(ui, |ui| {
                if entries.is_empty() {
                    ui.colored_label(palette().text_dim, "Es ist noch keine Regel angelegt.");
                }
                for (id, name, enabled) in entries {
                    let label = if enabled {
                        name
                    } else {
                        format!("{name} (aus)")
                    };
                    if ui
                        .selectable_label(form.selected == Some(id), label)
                        .clicked()
                    {
                        form.selected = Some(id);
                    }
                }
            });
        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut form.new_rule_name)
                    .desired_width(120.0)
                    .hint_text("Name"),
            );
            if ui.button("Anlegen").clicked() {
                actions.push(Action::AddRule);
            }
        });
        if let Some(id) = form.selected {
            if ui.button("Regel löschen").clicked() {
                actions.push(Action::RemoveRule { rule_id: id });
            }
        }
    });
}

/// The right pane of the rule editor: the selected rule's fields.
fn rule_detail(ui: &mut egui::Ui, form: &mut RulesForm) {
    let Some(id) = form.selected else {
        ui.colored_label(palette().text_dim, "Keine Regel ausgewählt.");
        return;
    };
    let Some(rule) = form.rules.get_mut(id) else {
        return;
    };
    ui.horizontal(|ui| {
        ui.label("Name:");
        ui.add(egui::TextEdit::singleline(&mut rule.name).desired_width(200.0));
    });
    ui.horizontal(|ui| {
        ui.checkbox(&mut rule.enabled, "Aktiv");
        ui.checkbox(&mut rule.apply_on_arrival, "Automatisch anwenden");
        ui.label("Priorität:");
        ui.add(egui::DragValue::new(&mut rule.priority).range(-9999..=9999));
    });
    ui.horizontal(|ui| {
        ui.label("Verknüpfung:");
        for conjunction in [
            (hylmail_core::models::rule::Conjunction::And, "UND"),
            (hylmail_core::models::rule::Conjunction::Or, "ODER"),
        ] {
            if ui
                .selectable_label(rule.conjunction == conjunction.0, conjunction.1)
                .clicked()
            {
                rule.conjunction = conjunction.0;
            }
        }
    });

    ui.add_space(4.0);
    ui.label(RichText::new("Bedingungen").text_style(subtitle()));
    let mut remove_condition: Option<usize> = None;
    for (index, condition) in rule.conditions.iter_mut().enumerate() {
        ui.horizontal(|ui| {
            egui::ComboBox::from_id_salt(("hylmail-rule-field", index))
                .selected_text(condition.field.label())
                .show_ui(ui, |ui| {
                    for field in hylmail_core::models::rule::ConditionField::all() {
                        if ui
                            .selectable_value(&mut condition.field, *field, field.label())
                            .clicked()
                        {
                            // Keep the operator valid for the new field.
                            let allowed = if field.is_textual() {
                                hylmail_core::models::rule::ConditionOp::textual()
                            } else {
                                hylmail_core::models::rule::ConditionOp::numeric()
                            };
                            if !allowed.contains(&condition.op) {
                                condition.op = allowed[0];
                            }
                        }
                    }
                });
            if condition.field != hylmail_core::models::rule::ConditionField::HasAttachment {
                egui::ComboBox::from_id_salt(("hylmail-rule-op", index))
                    .selected_text(condition.op.label())
                    .show_ui(ui, |ui| {
                        let ops = if condition.field.is_textual() {
                            hylmail_core::models::rule::ConditionOp::textual()
                        } else {
                            hylmail_core::models::rule::ConditionOp::numeric()
                        };
                        for op in ops {
                            ui.selectable_value(&mut condition.op, *op, op.label());
                        }
                    });
                ui.add(
                    egui::TextEdit::singleline(&mut condition.value)
                        .desired_width(140.0)
                        .hint_text("Wert"),
                );
            }
            if ui
                .small_button(icons::CLOSE)
                .on_hover_text("Bedingung entfernen")
                .clicked()
            {
                remove_condition = Some(index);
            }
        });
    }
    if let Some(index) = remove_condition {
        rule.conditions.remove(index);
    }
    if ui.button("Bedingung hinzufügen").clicked() {
        rule.conditions
            .push(hylmail_core::models::rule::Condition::contains(
                hylmail_core::models::rule::ConditionField::Subject,
                "",
            ));
    }

    ui.add_space(4.0);
    ui.label(RichText::new("Aktionen").text_style(subtitle()));
    let mut remove_action: Option<usize> = None;
    for (index, action) in rule.actions.iter().enumerate() {
        ui.horizontal(|ui| {
            ui.label(action.label());
            if ui
                .small_button(icons::CLOSE)
                .on_hover_text("Aktion entfernen")
                .clicked()
            {
                remove_action = Some(index);
            }
        });
    }
    if let Some(index) = remove_action {
        rule.actions.remove(index);
    }
    ui.horizontal(|ui| {
        if ui.button("Gelesen").clicked() {
            rule.actions.push(hylmail_core::models::Action::MarkRead);
        }
        if ui.button("Markieren").clicked() {
            rule.actions.push(hylmail_core::models::Action::Flag);
        }
        if ui.button("Löschen").clicked() {
            rule.actions.push(hylmail_core::models::Action::Delete);
        }
        if ui.button("Stoppen").clicked() {
            rule.actions
                .push(hylmail_core::models::Action::StopProcessing);
        }
    });
    ui.horizontal(|ui| {
        ui.label("Verschieben nach:");
        let mut target = rule
            .move_target()
            .map(ToString::to_string)
            .unwrap_or_default();
        if ui
            .add(
                egui::TextEdit::singleline(&mut target)
                    .desired_width(160.0)
                    .hint_text("Ordnername"),
            )
            .changed()
        {
            rule.actions
                .retain(|action| !matches!(action, hylmail_core::models::Action::MoveTo { .. }));
            if !target.trim().is_empty() {
                rule.actions.push(hylmail_core::models::Action::MoveTo {
                    folder_path: target.trim().to_string(),
                });
            }
        }
    });
}

/// The address book (`FR-ADDR-4`).
fn contacts_window(ctx: &egui::Context, state: &mut UiState, actions: &mut Vec<Action>) {
    if state.contacts_form.is_none() {
        return;
    }
    let mut open = true;
    egui::Window::new("Adressbuch")
        .id(egui::Id::new("hylmail-contacts"))
        .open(&mut open)
        .resizable(true)
        .default_size([680.0, 520.0])
        .show(ctx, |ui| {
            let Some(form) = state.contacts_form.as_mut() else {
                return;
            };
            ui.horizontal(|ui| {
                ui.label("Filter:");
                ui.add(
                    egui::TextEdit::singleline(&mut form.filter)
                        .desired_width(220.0)
                        .hint_text("Name oder Adresse"),
                );
                if ui
                    .button("CardDAV abgleichen")
                    .on_hover_text("Das Adressbuch mit dem Server synchronisieren")
                    .clicked()
                {
                    actions.push(Action::SyncContacts);
                }
            });
            ui.add_space(4.0);
            ui.horizontal_top(|ui| {
                contacts_list(ui, form);
                ui.separator();
                contact_detail(ui, form);
            });
            ui.add_space(4.0);
            if let Some(error) = &form.error {
                ui.colored_label(palette().error, format!("⚠ {error}"));
            } else if let Some(status) = &form.status {
                ui.colored_label(palette().success, status);
            }
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                if ui.button("Speichern").clicked() {
                    actions.push(Action::SaveContacts);
                }
                if ui.button("Kontakt löschen").clicked() {
                    if let Some(contact) = form.contacts.get(form.selected) {
                        actions.push(Action::DeleteContact {
                            contact_id: contact.id,
                        });
                    }
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.button("Schließen").clicked() {
                        actions.push(Action::CloseDialog);
                    }
                });
            });
        });
    if !open {
        actions.push(Action::CloseDialog);
    }
}

/// The list pane of the address book.
fn contacts_list(ui: &mut egui::Ui, form: &mut ContactsForm) {
    ui.vertical(|ui| {
        ui.set_min_width(240.0);
        ScrollArea::vertical()
            .id_salt("hylmail-contact-list")
            .max_height(320.0)
            .show(ui, |ui| {
                let visible = form.visible();
                if form.contacts.is_empty() {
                    ui.colored_label(
                        palette().text_dim,
                        "Noch keine Kontakte. Adressen aus gesendeten Nachrichten werden automatisch gesammelt.",
                    );
                }
                for index in visible {
                    let label = form
                        .contacts
                        .get(index)
                        .map_or_else(String::new, |contact| contact.label());
                    if ui.selectable_label(form.selected == index, label).clicked() {
                        form.selected = index;
                    }
                }
            });
        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut form.new_name)
                    .desired_width(90.0)
                    .hint_text("Name"),
            );
            ui.add(
                egui::TextEdit::singleline(&mut form.new_email)
                    .desired_width(120.0)
                    .hint_text("adresse@…"),
            );
            if ui.button("Neu").clicked() {
                let email = form.new_email.trim().to_string();
                if !email.is_empty() {
                    let address = hylmail_core::models::MailAddress::with_name(
                        email,
                        form.new_name.trim(),
                    );
                    let mut contact = hylmail_core::models::Contact::collected(&address);
                    contact.source = hylmail_core::models::ContactSource::Manual;
                    form.contacts.push(contact);
                    form.selected = form.contacts.len() - 1;
                    form.new_name.clear();
                    form.new_email.clear();
                }
            }
        });
    });
}

/// The detail pane of one contact.
fn contact_detail(ui: &mut egui::Ui, form: &mut ContactsForm) {
    ui.vertical(|ui| {
        ui.set_min_width(300.0);
        let Some(contact) = form.contacts.get_mut(form.selected) else {
            ui.colored_label(palette().text_dim, "Kein Kontakt ausgewählt.");
            return;
        };
        egui::Grid::new("hylmail-contact-detail")
            .num_columns(2)
            .spacing([10.0, 6.0])
            .show(ui, |ui| {
                ui.label("Anzeigename:");
                let mut name = contact.display_name.clone().unwrap_or_default();
                if ui
                    .add(egui::TextEdit::singleline(&mut name).desired_width(240.0))
                    .changed()
                {
                    contact.display_name = non_empty_string(&name);
                }
                ui.end_row();

                ui.label("Vorname:");
                let mut given = contact.given_name.clone().unwrap_or_default();
                if ui
                    .add(egui::TextEdit::singleline(&mut given).desired_width(240.0))
                    .changed()
                {
                    contact.given_name = non_empty_string(&given);
                }
                ui.end_row();

                ui.label("Nachname:");
                let mut family = contact.family_name.clone().unwrap_or_default();
                if ui
                    .add(egui::TextEdit::singleline(&mut family).desired_width(240.0))
                    .changed()
                {
                    contact.family_name = non_empty_string(&family);
                }
                ui.end_row();

                ui.label("Organisation:");
                let mut organization = contact.organization.clone().unwrap_or_default();
                if ui
                    .add(egui::TextEdit::singleline(&mut organization).desired_width(240.0))
                    .changed()
                {
                    contact.organization = non_empty_string(&organization);
                }
                ui.end_row();

                ui.label("E-Mail:");
                let mut email = contact
                    .emails
                    .first()
                    .map(|email| email.address.clone())
                    .unwrap_or_default();
                if ui
                    .add(egui::TextEdit::singleline(&mut email).desired_width(240.0))
                    .changed()
                {
                    match contact.emails.first_mut() {
                        Some(existing) => existing.address = email.trim().to_string(),
                        None if !email.trim().is_empty() => {
                            contact.emails.push(hylmail_core::models::ContactEmail {
                                address: email.trim().to_string(),
                                label: None,
                                preferred: true,
                            });
                        }
                        None => {}
                    }
                }
                ui.end_row();
            });
        if contact.is_remote() {
            ui.colored_label(palette().text_dim, "Mit CardDAV synchronisiert");
        } else {
            ui.colored_label(
                palette().text_dim,
                format!("Herkunft: {:?}", contact.source),
            );
        }
    });
}

/// `Some(trimmed)` unless the value is empty.
fn non_empty_string(value: &str) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// The signature editor.
fn signature_section(ui: &mut egui::Ui, form: &mut SettingsForm, accounts: &[Account]) {
    ui.label(RichText::new("Signaturen").text_style(subtitle()));
    let count = form.settings.signatures.len();
    if count == 0 {
        ui.colored_label(palette().text_dim, "Es ist noch keine Signatur angelegt.");
    }
    let mut selected = form.signature_index;
    for index in 0..count {
        let name = form
            .settings
            .signatures
            .get(index)
            .map_or_else(String::new, |signature| signature.name.clone());
        if ui.selectable_label(selected == Some(index), name).clicked() {
            selected = Some(index);
        }
    }
    form.signature_index = selected;

    ui.horizontal(|ui| {
        ui.add(
            egui::TextEdit::singleline(&mut form.new_signature_name)
                .desired_width(200.0)
                .hint_text("Name der neuen Signatur"),
        );
        // The account the new signature belongs to is a real choice, not the
        // account of whichever signature happens to be first.
        let chosen = form
            .new_signature_account
            .and_then(|id| accounts.iter().find(|account| account.id == id))
            .map_or_else(
                || "Konto wählen".to_string(),
                |account| account.display_name.clone(),
            );
        egui::ComboBox::from_id_salt("hylmail-signature-account")
            .selected_text(chosen)
            .show_ui(ui, |ui| {
                for account in accounts {
                    ui.selectable_value(
                        &mut form.new_signature_account,
                        Some(account.id),
                        &account.display_name,
                    );
                }
            });
        if ui.button("Anlegen").clicked() {
            let account = form
                .new_signature_account
                .or_else(|| accounts.first().map(|account| account.id));
            if let Some(account_id) = account {
                let name = if form.new_signature_name.trim().is_empty() {
                    "Neue Signatur".to_string()
                } else {
                    form.new_signature_name.trim().to_string()
                };
                form.settings
                    .signatures
                    .push(hylmail_core::models::Signature::new(account_id, name));
                form.signature_index = Some(form.settings.signatures.len() - 1);
                form.new_signature_name.clear();
            } else {
                form.error =
                    Some("Eine Signatur braucht ein Konto; legen Sie zuerst eines an.".to_string());
            }
        }
    });

    if let Some(index) = form.signature_index {
        let positions = [
            (SignaturePosition::AboveQuote, "Über dem Zitat"),
            (SignaturePosition::BelowQuote, "Unter dem Zitat"),
        ];
        let mut remove = false;
        if let Some(signature) = form.settings.signatures.get_mut(index) {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.label("Name:");
                ui.add(egui::TextEdit::singleline(&mut signature.name).desired_width(220.0));
                ui.checkbox(&mut signature.is_default, "Standard");
            });
            ui.horizontal(|ui| {
                ui.label("Position:");
                for (value, label) in positions {
                    if ui
                        .selectable_label(signature.position == value, label)
                        .clicked()
                    {
                        signature.position = value;
                    }
                }
            });
            ui.label("HTML:");
            ui.add(
                egui::TextEdit::multiline(&mut signature.html)
                    .desired_width(f32::INFINITY)
                    .desired_rows(3),
            );
            ui.label("Text:");
            ui.add(
                egui::TextEdit::multiline(&mut signature.plain)
                    .desired_width(f32::INFINITY)
                    .desired_rows(2),
            );
            if ui.button("Signatur löschen").clicked() {
                remove = true;
            }
        }
        if remove {
            form.settings.signatures.remove(index);
            form.signature_index = None;
        }
    }
}

/// A blank account, used when the user adds one in the dialog.
///
/// Every field is validated by [`Account::validate`] before the document is
/// written, so an account that is still missing its server refuses to save.
#[must_use]
fn blank_account(name: &str) -> Account {
    Account {
        id: AccountId::new(),
        display_name: name.to_string(),
        email: String::new(),
        kind: AccountKind::Imap,
        auth: AuthKind::Password,
        username: String::new(),
        incoming: Endpoint::new(String::new(), 993, TlsMode::Implicit),
        outgoing: Endpoint::new(String::new(), 465, TlsMode::Implicit),
        proxy: None,
        oauth: None,
        enabled: true,
        created_at: hylmail_core::models::ids::utc_now(),
    }
}

/// Width of the server text field of an endpoint row.
const HOST_WIDTH: f32 = 210.0;

/// Width of the encryption combo box of an endpoint row.
const TLS_WIDTH: f32 = 150.0;

/// The encryption modes the form offers, most secure first.
const TLS_MODES: [TlsMode; 3] = [TlsMode::Implicit, TlsMode::StartTls, TlsMode::None];

/// Which side of the connection an endpoint belongs to.
///
/// The only thing the port follow-up needs to know: it maps an encryption mode
/// onto the port that mode conventionally uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Side {
    /// The mailbox (IMAP, EWS or the GroupWise proxy).
    Incoming,
    /// Submission to the outbound relay.
    Outgoing,
    /// The GroupWise SOAP proxy.
    Proxy,
}

impl Side {
    /// The port this endpoint conventionally uses with the given mode.
    #[must_use]
    const fn conventional_port(self, mode: TlsMode) -> u16 {
        match (self, mode) {
            (Self::Incoming, TlsMode::Implicit) => 993,
            (Self::Incoming, TlsMode::StartTls | TlsMode::None) => 143,
            (Self::Outgoing, TlsMode::Implicit) => 465,
            (Self::Outgoing, TlsMode::StartTls) => 587,
            (Self::Outgoing, TlsMode::None) => 25,
            // The proxy always speaks TLS on one port, so the mode does not
            // change it.
            (Self::Proxy, _) => 7191,
        }
    }
}

/// German label of an encryption mode.
#[must_use]
const fn tls_label(mode: TlsMode) -> &'static str {
    match mode {
        TlsMode::Implicit => "SSL/TLS",
        TlsMode::StartTls => "STARTTLS",
        TlsMode::None => "Keine",
    }
}

/// Applies a new encryption mode to one endpoint.
///
/// The port follows the mode as long as it still is the conventional one for
/// the mode selected before, so switching an IMAPS account (993) to STARTTLS
/// moves it to 143 instead of silently keeping the implicit-TLS port.  A port
/// the user typed themselves is never overwritten.
fn apply_tls_mode(endpoint: &mut Endpoint, side: Side, mode: TlsMode) {
    if endpoint.port == side.conventional_port(endpoint.tls) {
        endpoint.port = side.conventional_port(mode);
    }
    endpoint.tls = mode;
}

/// Applies a protocol change and keeps the account valid.
///
/// [`Account::validate`] rejects a proxy block on IMAP and Exchange accounts,
/// so switching away from GroupWise has to drop it; switching *to* GroupWise
/// has to create one, otherwise there is nothing to type the proxy into.
fn apply_kind(account: &mut Account, kind: AccountKind) {
    if kind == AccountKind::GroupWiseProxy {
        if account.proxy.is_none() {
            account.proxy = Some(GroupWiseProxyConfig::new(String::new()));
        }
    } else {
        account.proxy = None;
    }
    if kind != AccountKind::Exchange && account.auth == AuthKind::Ntlm {
        // NTLM is the handshake of EWS; the other protocols would only refuse
        // the account, so the switch falls back to the password form.
        account.auth = AuthKind::Password;
    }
    account.kind = kind;
}

/// The sign-in mechanisms the account form offers for one protocol.
///
/// `NTLM` (Windows authentication) only appears for Exchange: IMAP and the
/// GroupWise proxy sign in with a password or an OAuth2 token, so offering the
/// handshake there would promise something the protocol client does not do.
fn auth_choices(kind: AccountKind) -> Vec<(AuthKind, &'static str)> {
    let mut choices = vec![
        (AuthKind::Password, "Kennwort"),
        (AuthKind::OAuth2, "OAuth2"),
    ];
    if kind == AccountKind::Exchange {
        choices.push((AuthKind::Ntlm, "Windows (NTLM)"));
    }
    choices
}

/// `true` when any endpoint of the account is unencrypted.
#[must_use]
fn transmits_plaintext(account: &Account) -> bool {
    std::iter::once(&account.incoming)
        .chain(std::iter::once(&account.outgoing))
        .chain(account.proxy.as_ref().map(|proxy| &proxy.proxy))
        .any(|endpoint| !endpoint.tls.is_encrypted())
}

/// The combo box that selects the encryption of one endpoint.
fn tls_combo(ui: &mut egui::Ui, salt: &str, endpoint: &mut Endpoint, side: Side) {
    let mut chosen = endpoint.tls;
    egui::ComboBox::from_id_salt(salt)
        .selected_text(tls_label(chosen))
        .width(TLS_WIDTH)
        .show_ui(ui, |ui| {
            for mode in TLS_MODES {
                ui.selectable_value(&mut chosen, mode, tls_label(mode));
            }
        });
    if chosen != endpoint.tls {
        apply_tls_mode(endpoint, side, chosen);
    }
    if endpoint.tls == TlsMode::None {
        ui.colored_label(palette().flagged, "⚠ unverschlüsselt");
    }
}

/// One server/port/encryption line of the account grid.
fn endpoint_row(ui: &mut egui::Ui, label: &str, salt: &str, endpoint: &mut Endpoint, side: Side) {
    ui.label(label);
    ui.horizontal(|ui| {
        ui.add(
            egui::TextEdit::singleline(&mut endpoint.host)
                .desired_width(HOST_WIDTH)
                .hint_text("Server"),
        );
        ui.label("Port:");
        ui.add(
            egui::DragValue::new(&mut endpoint.port)
                .range(1..=65535)
                .speed(1.0),
        );
        tls_combo(ui, salt, endpoint, side);
    });
    ui.end_row();
}

/// One line of the account grid for an optional string; the proxy post office
/// and user id are optional in `accounts.json`.
fn optional_field(ui: &mut egui::Ui, label: &str, value: &mut Option<String>) {
    ui.label(label);
    let mut text = value.clone().unwrap_or_default();
    let changed = ui
        .add(
            egui::TextEdit::singleline(&mut text)
                .desired_width(HOST_WIDTH)
                .hint_text("optional"),
        )
        .changed();
    if changed {
        let trimmed = text.trim();
        *value = if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        };
    }
    ui.end_row();
}

/// The accounts window.
fn accounts_window(ctx: &egui::Context, state: &mut UiState, actions: &mut Vec<Action>) {
    if state.accounts_form.is_none() {
        return;
    }
    let mut open = true;
    egui::Window::new("Konten")
        .id(egui::Id::new("hylmail-accounts"))
        .open(&mut open)
        .resizable(true)
        .default_size([720.0, 450.0])
        .max_height(620.0)
        .show(ctx, |ui| {
            let Some(form) = state.accounts_form.as_mut() else {
                return;
            };
            ui.horizontal_top(|ui| {
                account_list(ui, form);
                ui.separator();
                account_detail(ui, form, actions);
            });
            ui.add_space(4.0);
            if let Some(error) = &form.error {
                ui.colored_label(palette().error, format!("⚠ {error}"));
            } else if let Some(status) = &form.status {
                ui.colored_label(palette().success, status);
            }
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                if ui.button("Speichern").clicked() {
                    actions.push(Action::SaveAccounts);
                }
                if ui.button("Kennwort speichern").clicked() {
                    actions.push(Action::SavePassword);
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.button("Schließen").clicked() {
                        actions.push(Action::CloseDialog);
                    }
                });
            });
        });
    if !open {
        actions.push(Action::CloseDialog);
    }
}

/// The list of configured accounts.
fn account_list(ui: &mut egui::Ui, form: &mut AccountsForm) {
    ui.vertical(|ui| {
        ui.set_min_width(200.0);
        ui.label(RichText::new("Konten").text_style(subtitle()));
        let names: Vec<String> = form
            .document
            .accounts
            .iter()
            .map(|account| account.display_name.clone())
            .collect();
        for (index, name) in names.iter().enumerate() {
            if ui.selectable_label(form.selected == index, name).clicked() {
                form.selected = index;
                form.password.clear();
            }
        }
        if ui.button("Konto hinzufügen").clicked() {
            let name = unique_account_name(&names);
            form.document.accounts.push(blank_account(&name));
            form.selected = form.document.accounts.len() - 1;
        }
        if form.document.accounts.len() > 1 && ui.button("Konto entfernen").clicked() {
            form.document.accounts.remove(form.selected);
            form.selected = 0;
        }
    });
}

/// A display name for a freshly added account that no existing account uses.
fn unique_account_name(existing: &[String]) -> String {
    const BASE: &str = "Neues Konto";
    if !existing.iter().any(|name| name == BASE) {
        return BASE.to_string();
    }
    (2_u32..)
        .map(|n| format!("{BASE} {n}"))
        .find(|candidate| !existing.iter().any(|name| name == candidate))
        .unwrap_or_else(|| BASE.to_string())
}

/// The detail pane of one account.
fn account_detail(ui: &mut egui::Ui, form: &mut AccountsForm, actions: &mut Vec<Action>) {
    ui.vertical(|ui| {
        let Some(account) = form.document.accounts.get_mut(form.selected) else {
            ui.colored_label(palette().text_dim, "Kein Konto ausgewählt.");
            return;
        };
        ScrollArea::vertical()
            .id_salt("hylmail-account-detail")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                egui::Grid::new("hylmail-account-fields")
                    .num_columns(2)
                    .spacing([10.0, 6.0])
                    .show(ui, |ui| {
                        ui.label("Anzeigename:");
                        ui.add(
                            egui::TextEdit::singleline(&mut account.display_name)
                                .desired_width(300.0),
                        );
                        ui.end_row();

                        ui.label("Benutzername:");
                        ui.add(
                            egui::TextEdit::singleline(&mut account.username).desired_width(300.0),
                        );
                        ui.end_row();

                        ui.label("E-Mail-Adresse:");
                        ui.add(egui::TextEdit::singleline(&mut account.email).desired_width(300.0));
                        ui.end_row();

                        ui.label("Protokoll:");
                        ui.horizontal(|ui| {
                            for (value, label) in [
                                (AccountKind::Imap, "IMAP"),
                                (AccountKind::Exchange, "Exchange"),
                                (AccountKind::GroupWiseProxy, "GroupWise"),
                            ] {
                                if ui.selectable_label(account.kind == value, label).clicked() {
                                    apply_kind(account, value);
                                }
                            }
                        });
                        ui.end_row();

                        ui.label("Anmeldung:");
                        ui.horizontal(|ui| {
                            for (value, label) in auth_choices(account.kind) {
                                if ui.selectable_label(account.auth == value, label).clicked() {
                                    account.auth = value;
                                    // The wizard has to show *some* endpoints
                                    // once OAuth2 is picked; an empty block
                                    // keeps the form self-explanatory.
                                    if value == AuthKind::OAuth2 && account.oauth.is_none() {
                                        account.oauth =
                                            Some(hylmail_core::models::OAuthConfig::default());
                                    }
                                }
                            }
                        });
                        ui.end_row();

                        endpoint_row(
                            ui,
                            "Eingang:",
                            "hylmail-account-tls-in",
                            &mut account.incoming,
                            Side::Incoming,
                        );
                        endpoint_row(
                            ui,
                            "Ausgang:",
                            "hylmail-account-tls-out",
                            &mut account.outgoing,
                            Side::Outgoing,
                        );

                        if account.kind == AccountKind::GroupWiseProxy {
                            if let Some(proxy) = account.proxy.as_mut() {
                                endpoint_row(
                                    ui,
                                    "Proxy:",
                                    "hylmail-account-tls-proxy",
                                    &mut proxy.proxy,
                                    Side::Proxy,
                                );
                                optional_field(ui, "Postamt:", &mut proxy.post_office);
                                optional_field(ui, "Benutzer-ID:", &mut proxy.user_id);
                            }
                        }

                        ui.label("Aktiv:");
                        ui.checkbox(&mut account.enabled, "Konto synchronisieren");
                        ui.end_row();
                    });

                if transmits_plaintext(account) {
                    ui.add_space(6.0);
                    ui.colored_label(
                        palette().flagged,
                        "⚠ Ohne Verschlüsselung werden Kennwort und Nachrichten im Klartext übertragen.",
                    );
                }

                ui.add_space(6.0);
                preset_row(ui, &mut form.preset, account);
                ui.add_space(6.0);
                if account.auth == AuthKind::OAuth2 {
                    oauth_fields(ui, account, actions);
                }
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.label("Kennwort:");
                    ui.add(
                        egui::TextEdit::singleline(&mut form.password)
                            .desired_width(240.0)
                            .password(true)
                            .hint_text("wird im Schlüsselbund abgelegt"),
                    );
                    if ui.button("Ablegen").clicked() {
                        actions.push(Action::SavePassword);
                    }
                });
                ui.colored_label(
                    palette().text_dim,
                    format!(
                        "Der Tresor liest und schreibt unter dem Dienst „{}“.",
                        hylmail_core::secrets::DEFAULT_SERVICE
                    ),
                );
            });
    });
}

/// The provider preset combo box, plus the button that applies it.
fn preset_row(ui: &mut egui::Ui, preset_index: &mut usize, account: &mut Account) {
    let presets = hylmail_core::models::presets::all();
    ui.horizontal(|ui| {
        ui.label("Vorlage:");
        let selected = presets
            .get(*preset_index)
            .map(|preset| preset.label)
            .unwrap_or("");
        egui::ComboBox::from_id_salt("hylmail-account-preset")
            .selected_text(selected)
            .show_ui(ui, |ui| {
                for (index, preset) in presets.iter().enumerate() {
                    ui.selectable_value(preset_index, index, preset.label);
                }
            });
        if ui.button("Übernehmen").clicked() {
            if let Some(preset) = presets.get(*preset_index) {
                preset.apply_to(account);
            }
        }
        if let Some(preset) = presets.get(*preset_index) {
            ui.colored_label(palette().text_dim, preset.hint);
        }
    });
}

/// The OAuth2 client settings plus the browser sign-in button.
fn oauth_fields(ui: &mut egui::Ui, account: &mut Account, actions: &mut Vec<Action>) {
    let Some(oauth) = account.oauth.as_mut() else {
        ui.colored_label(
            palette().flagged,
            "OAuth2 ist gewählt, aber es sind keine Endpunkte hinterlegt.",
        );
        return;
    };
    egui::Grid::new("hylmail-account-oauth")
        .num_columns(2)
        .spacing([10.0, 6.0])
        .show(ui, |ui| {
            ui.label("Client-ID:");
            ui.add(
                egui::TextEdit::singleline(&mut oauth.client_id)
                    .desired_width(300.0)
                    .hint_text("von der Administration registriert"),
            );
            ui.end_row();

            ui.label("Autorisierung:");
            ui.add(
                egui::TextEdit::singleline(&mut oauth.authorization_endpoint).desired_width(300.0),
            );
            ui.end_row();

            ui.label("Token:");
            ui.add(egui::TextEdit::singleline(&mut oauth.token_endpoint).desired_width(300.0));
            ui.end_row();

            ui.label("Berechtigungen:");
            let mut scopes = oauth.scopes.join(" ");
            if ui
                .add(egui::TextEdit::singleline(&mut scopes).desired_width(300.0))
                .changed()
            {
                oauth.scopes = scopes.split_whitespace().map(str::to_string).collect();
            }
            ui.end_row();

            ui.label("Redirect-Port:");
            let mut port = u32::from(oauth.redirect_port);
            if ui
                .add(egui::DragValue::new(&mut port).range(0..=65535))
                .changed()
            {
                oauth.redirect_port = u16::try_from(port).unwrap_or(0);
            }
            ui.end_row();
        });

    let configured = oauth.is_configured();
    ui.horizontal(|ui| {
        if ui
            .add_enabled(configured, egui::Button::new("Im Browser anmelden"))
            .on_hover_text("Öffnet die Anmeldeseite; das Kennwort wird nie gesehen")
            .clicked()
        {
            actions.push(Action::OAuthSignIn);
        }
        ui.colored_label(
            palette().text_dim,
            format!(
                "Redirect-URI beim Anbieter hinterlegen: {}",
                oauth.redirect_uri(oauth.redirect_port)
            ),
        );
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An account that only needs an endpoint host to pass validation.
    fn valid_account() -> Account {
        let mut account = blank_account("Testkonto");
        account.email = "ada@example.org".to_string();
        account.username = "ada".to_string();
        account.incoming = Endpoint::imaps("imap.example.org");
        account.outgoing = Endpoint::smtps("smtp.example.org");
        account
    }

    #[test]
    fn a_blank_account_starts_encrypted_on_the_standard_ports() {
        let account = blank_account("Neu");
        assert_eq!(account.incoming.port, 993);
        assert_eq!(account.incoming.tls, TlsMode::Implicit);
        assert_eq!(account.outgoing.port, 465);
        assert_eq!(account.outgoing.tls, TlsMode::Implicit);
    }

    #[test]
    fn the_combo_offers_every_mode_exactly_once() {
        let mut seen = Vec::new();
        for mode in TLS_MODES {
            assert!(!seen.contains(&mode), "{mode:?} is listed twice");
            seen.push(mode);
        }
        assert_eq!(seen.len(), 3);
        // Most secure first, off last.
        assert_eq!(TLS_MODES[0], TlsMode::Implicit);
        assert_eq!(TLS_MODES[2], TlsMode::None);
    }

    #[test]
    fn every_mode_has_a_german_label() {
        assert_eq!(tls_label(TlsMode::Implicit), "SSL/TLS");
        assert_eq!(tls_label(TlsMode::StartTls), "STARTTLS");
        assert_eq!(tls_label(TlsMode::None), "Keine");
    }

    #[test]
    fn the_conventional_ports_match_the_registered_ones() {
        for (side, implicit, starttls, none) in [
            (Side::Incoming, 993, 143, 143),
            (Side::Outgoing, 465, 587, 25),
        ] {
            assert_eq!(side.conventional_port(TlsMode::Implicit), implicit);
            assert_eq!(side.conventional_port(TlsMode::StartTls), starttls);
            assert_eq!(side.conventional_port(TlsMode::None), none);
        }
        // The proxy has one port for every mode.
        for mode in TLS_MODES {
            assert_eq!(Side::Proxy.conventional_port(mode), 7191);
        }
    }

    #[test]
    fn changing_the_mode_moves_a_standard_port() {
        let mut endpoint = Endpoint::imaps("imap.example.org");
        apply_tls_mode(&mut endpoint, Side::Incoming, TlsMode::StartTls);
        assert_eq!(endpoint.port, 143);
        assert_eq!(endpoint.tls, TlsMode::StartTls);

        let mut endpoint = Endpoint::smtps("smtp.example.org");
        apply_tls_mode(&mut endpoint, Side::Outgoing, TlsMode::None);
        assert_eq!(endpoint.port, 25);
        assert_eq!(endpoint.tls, TlsMode::None);

        // And back again: 587 is the STARTTLS port of the outgoing side.
        apply_tls_mode(&mut endpoint, Side::Outgoing, TlsMode::StartTls);
        assert_eq!(endpoint.port, 587);
    }

    #[test]
    fn changing_the_mode_keeps_a_port_the_user_typed() {
        let mut endpoint = Endpoint::new("imap.example.org", 1993, TlsMode::Implicit);
        apply_tls_mode(&mut endpoint, Side::Incoming, TlsMode::StartTls);
        assert_eq!(endpoint.port, 1993, "a custom port must survive");
        assert_eq!(endpoint.tls, TlsMode::StartTls);
    }

    #[test]
    fn the_proxy_port_never_follows_the_mode() {
        let mut endpoint = Endpoint::groupwise_proxy("gw.example.org");
        apply_tls_mode(&mut endpoint, Side::Proxy, TlsMode::StartTls);
        assert_eq!(endpoint.port, 7191);
        assert_eq!(endpoint.tls, TlsMode::StartTls);
    }

    #[test]
    fn only_exchange_offers_the_windows_sign_in() {
        let exchange: Vec<AuthKind> = auth_choices(AccountKind::Exchange)
            .into_iter()
            .map(|(kind, _)| kind)
            .collect();
        assert_eq!(
            exchange,
            [AuthKind::Password, AuthKind::OAuth2, AuthKind::Ntlm]
        );
        for kind in [AccountKind::Imap, AccountKind::GroupWiseProxy] {
            let choices: Vec<AuthKind> = auth_choices(kind)
                .into_iter()
                .map(|(choice, _)| choice)
                .collect();
            assert_eq!(choices, [AuthKind::Password, AuthKind::OAuth2], "{kind:?}");
        }
    }

    #[test]
    fn switching_away_from_exchange_returns_to_the_password_form() {
        let mut account = valid_account();
        account.kind = AccountKind::Exchange;
        account.auth = AuthKind::Ntlm;
        apply_kind(&mut account, AccountKind::Exchange);
        assert_eq!(account.auth, AuthKind::Ntlm, "Exchange keeps NTLM");

        apply_kind(&mut account, AccountKind::Imap);
        assert_eq!(account.auth, AuthKind::Password);

        // A token is a valid choice for every protocol, so it survives the switch.
        account.kind = AccountKind::Exchange;
        account.auth = AuthKind::OAuth2;
        apply_kind(&mut account, AccountKind::Exchange);
        assert_eq!(account.auth, AuthKind::OAuth2);
    }

    #[test]
    fn switching_to_groupwise_creates_the_proxy_block_it_needs() {
        let mut account = valid_account();
        apply_kind(&mut account, AccountKind::GroupWiseProxy);
        let proxy = account.proxy.as_ref().expect("proxy block");
        assert_eq!(proxy.proxy.port, 7191);
        assert_eq!(proxy.proxy.tls, TlsMode::Implicit);
        // The host is still empty, and `Account::validate` says so.
        assert!(account.validate().is_err());

        account.proxy.as_mut().expect("proxy block").proxy.host = "gw.example.org".to_string();
        assert!(account.validate().is_ok());
    }

    #[test]
    fn switching_to_groupwise_twice_keeps_what_was_typed() {
        let mut account = valid_account();
        apply_kind(&mut account, AccountKind::GroupWiseProxy);
        account.proxy.as_mut().expect("proxy block").proxy.host = "gw.example.org".to_string();
        apply_kind(&mut account, AccountKind::GroupWiseProxy);
        assert_eq!(
            account.proxy.as_ref().expect("proxy block").proxy.host,
            "gw.example.org"
        );
    }

    #[test]
    fn switching_away_from_groupwise_drops_the_proxy_block() {
        let mut account = valid_account();
        apply_kind(&mut account, AccountKind::GroupWiseProxy);
        apply_kind(&mut account, AccountKind::Imap);
        assert!(account.proxy.is_none());
        assert!(account.validate().is_ok(), "{:?}", account.validate());
    }

    #[test]
    fn an_unencrypted_endpoint_is_reported() {
        let account = valid_account();
        assert!(!transmits_plaintext(&account));

        let mut insecure = account.clone();
        insecure.incoming.tls = TlsMode::None;
        assert!(transmits_plaintext(&insecure));

        let mut insecure = account.clone();
        insecure.outgoing.tls = TlsMode::None;
        assert!(transmits_plaintext(&insecure));

        let mut proxy = valid_account();
        apply_kind(&mut proxy, AccountKind::GroupWiseProxy);
        proxy.proxy.as_mut().expect("proxy block").proxy.host = "gw.example.org".to_string();
        assert!(!transmits_plaintext(&proxy));
        proxy.proxy.as_mut().expect("proxy block").proxy.tls = TlsMode::None;
        assert!(transmits_plaintext(&proxy));
    }

    #[test]
    fn the_tracking_domain_field_splits_and_normalises() {
        let parsed = parse_tracking_domains("Tracker.Example, ads.example;  mail.example  ");
        assert_eq!(
            parsed,
            vec!["tracker.example", "ads.example", "mail.example"]
        );
        assert!(parse_tracking_domains("   ").is_empty());
        assert!(parse_tracking_domains("").is_empty());
    }

    #[test]
    fn a_new_account_never_shadows_an_existing_name() {
        assert_eq!(unique_account_name(&[]), "Neues Konto");
        assert_eq!(unique_account_name(&["Arbeit".to_string()]), "Neues Konto");
        assert_eq!(
            unique_account_name(&["Neues Konto".to_string()]),
            "Neues Konto 2"
        );
        assert_eq!(
            unique_account_name(&["Neues Konto".to_string(), "Neues Konto 2".to_string()]),
            "Neues Konto 3"
        );
    }
}
