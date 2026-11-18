//! HylMail native desktop user interface.
//!
//! # Framework decision
//!
//! The specification leaves the choice between **Slint** and **egui/eframe** to
//! the implementer, judged by how cleanly the required widgets can be built:
//! a three pane layout, virtualised tables, drag & drop and rich text.
//!
//! **HylMail uses `egui`/`eframe`.**
//!
//! | Requirement                | Why `egui`/`eframe` wins                                                                 |
//! |----------------------------|------------------------------------------------------------------------------------------|
//! | Virtualised message list   | `egui_extras::TableBuilder` renders only the visible row range, which is exactly the 500+ paging model of the specification. |
//! | Three pane layout          | `SidePanel` / `CentralPanel` / `TopBottomPanel` compose without a declarative DSL or a code generation step. |
//! | Rich text reading pane     | The sanitised HTML is laid out into an `egui::text::LayoutJob`, which keeps links selectable. |
//! | Build simplicity           | Pure Rust, no `.slint` language, no separate compiler, which keeps `cargo test` green on every platform. |
//! | Dark theme                 | `Visuals::dark()` is the default; every colour is overridable through `Style`, which is what the specification's own dark design needs. |
//!
//! # Threading
//!
//! The window thread owns the [`Core`] and never blocks.  Everything that can
//! take a TCP round trip — fetching a body, pushing a flag, submitting a
//! message, running the synchronisation engine — happens in
//! `Worker`, which reports back through two channels that the window drains
//! once per frame:
//!
//! ```text
//!   window thread                 worker thread(s)
//!   -------------                 ----------------
//!   view::draw  ──Action──► state::handle ──Task──► worker::spawn_jobs
//!   state::apply ◄────────────── UiEvent ────────────────┘
//!   state::apply_engine ◄─────── EngineEvent ──── Synchronisation engine
//! ```
#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::assertions_on_constants
    )
)]

use std::path::PathBuf;
use std::time::Duration;

use hylmail_core::ipc::{self, Instance, InstanceServer, Request};
use hylmail_core::{Core, Error};

use crate::action::{from_shortcut, Action, Shortcut};
use crate::state::{Task, UiState};
use crate::worker::Worker;

mod action;
mod dialogs;
mod format;
mod html_view;
mod state;
mod theme;
mod view;
mod worker;

/// Name of the user interface framework, reported by the diagnostics.
pub const FRAMEWORK: &str = "egui/eframe";

/// Minimum window size of the main window, in logical pixels.
pub const MIN_WINDOW_SIZE: [f32; 2] = [1024.0, 640.0];

/// Default window size of the main window, in logical pixels.
pub const DEFAULT_WINDOW_SIZE: [f32; 2] = [1440.0, 900.0];

/// Bundle identifier used by the macOS `.dmg` and the Inno Setup installer.
pub const BUNDLE_ID: &str = "de.hylmail.HylMail";

/// How long the window sleeps when nothing at all happens.
///
/// The worker wakes the context explicitly for every event, so this only
/// controls the redraw of clock dependent text such as "Heute 14:05".
const IDLE_REDRAW: Duration = Duration::from_millis(1000);

/// The five panes the specification asks for.
///
/// Keeping the list in the core of the crate means the layout code, the
/// keyboard shortcut table and the UI tests all iterate over the same source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Pane {
    /// Folder tree on the left.
    Folders,
    /// Virtualised message list in the middle.
    MessageList,
    /// Reading pane on the right.
    Reading,
    /// Composer, opened in its own window.
    Composer,
    /// Status bar at the bottom.
    StatusBar,
}

impl Pane {
    /// Every pane, in layout order.
    pub const ALL: [Self; 5] = [
        Self::Folders,
        Self::MessageList,
        Self::Reading,
        Self::Composer,
        Self::StatusBar,
    ];

    /// Human readable name, used in the view menu and in tests.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Folders => "Ordner",
            Self::MessageList => "Nachrichtenliste",
            Self::Reading => "Leseansicht",
            Self::Composer => "Verfassen",
            Self::StatusBar => "Statusleiste",
        }
    }
}

/// Starts the native user interface and blocks until the user closes it.
///
/// # Errors
///
/// Fails when the window system cannot be initialised, when the renderer
/// cannot be created, or when the single-instance endpoint cannot be
/// established.
pub fn run(core: Core) -> anyhow::Result<()> {
    run_with(core, Vec::new())
}

/// Starts the interface with files the operating system asked us to open.
///
/// When another HylMail instance already owns the configuration directory, the
/// files are handed to it over the loopback endpoint and this process exits
/// without opening a window.
///
/// # Errors
///
/// As [`run`], plus any error the forwarding of `pending` reports.
pub fn run_with(core: Core, pending: Vec<PathBuf>) -> anyhow::Result<()> {
    let instance = ipc::acquire(core.paths().config_dir())?;
    let server = match instance {
        Instance::Primary(server) => server,
        Instance::Secondary(lock) => {
            for path in &pending {
                ipc::notify(&lock, &Request::OpenEml { path: path.clone() })?;
            }
            ipc::notify(&lock, &Request::RaiseWindow)?;
            return Ok(());
        }
    };

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("HylMail")
            .with_app_id(BUNDLE_ID)
            .with_inner_size(DEFAULT_WINDOW_SIZE)
            .with_min_inner_size(MIN_WINDOW_SIZE),
        renderer: eframe::Renderer::Glow,
        ..eframe::NativeOptions::default()
    };

    let creator = Box::new(move |cc: &eframe::CreationContext<'_>| {
        Ok(Box::new(App::new(&cc.egui_ctx, core, server, pending)) as Box<dyn eframe::App>)
    });
    eframe::run_native("HylMail", options, creator).map_err(|error| {
        Error::config(format!("the window system could not be started: {error}")).into()
    })
}

/// The window: state, worker and the single-instance endpoint.
struct App {
    core: Core,
    state: UiState,
    worker: Worker,
    instance: InstanceServer,
    /// Actions collected while drawing this frame.
    actions: Vec<Action>,
    /// Links the user asked to open in the browser.
    links: Vec<String>,
}

impl App {
    /// Builds the application from an already booted core.
    fn new(
        ctx: &egui::Context,
        core: Core,
        instance: InstanceServer,
        pending: Vec<PathBuf>,
    ) -> Self {
        theme::apply(ctx, core.settings().theme);
        let mut state = UiState::new(core.settings().page_size);
        if let Err(error) = state.load(&core) {
            state.error = Some(error.to_string());
        }
        state.pending_eml = pending;
        let worker = Worker::start(&core, ctx.clone());
        if !worker.is_running() {
            state.error = Some(
                "Der Hintergrunddienst konnte nicht gestartet werden; es wird nichts abgerufen."
                    .to_string(),
            );
        }
        Self {
            core,
            state,
            worker,
            instance,
            actions: Vec::new(),
            links: Vec::new(),
        }
    }

    /// Hands a task to the worker, or to the engine it fronts.
    fn dispatch(&mut self, task: Task) {
        match task {
            Task::Sync(Some(account_id)) => self.worker.sync_account(account_id),
            Task::Sync(None) => self.worker.sync_all(),
            Task::ReloadFolders(account_id) => self.worker.refresh_folders(account_id),
            Task::ReloadAccounts => self.worker.reload_accounts(),
            Task::ReloadSettings => self.worker.reload_settings(),
            other => self.worker.dispatch(other),
        }
    }

    /// Drains engine events, worker events and single-instance requests.
    fn pump(&mut self, ctx: &egui::Context) {
        // Bounded so that a busy engine cannot starve the window.
        for _ in 0..64 {
            let Some(event) = self.worker.engine_event() else {
                break;
            };
            for task in self.state.apply_engine(&event, &self.core) {
                self.dispatch(task);
            }
        }
        for _ in 0..64 {
            let Some(event) = self.worker.try_event() else {
                break;
            };
            self.state.apply(event, &self.core);
        }
        for request in self.instance.drain() {
            match request {
                Request::OpenEml { path } => self.state.pending_eml.push(path),
                Request::RaiseWindow => ctx.send_viewport_cmd(egui::ViewportCommand::Focus),
                Request::Ping => {}
            }
        }
        for path in std::mem::take(&mut self.state.pending_eml) {
            self.actions.push(Action::OpenEml(path));
        }
    }

    /// Runs the actions the widgets produced and hands the tasks on.
    fn execute(&mut self, ctx: &egui::Context) {
        let actions = std::mem::take(&mut self.actions);
        for action in actions {
            for task in self.state.handle(action, &mut self.core) {
                self.dispatch(task);
            }
        }
        for url in std::mem::take(&mut self.links) {
            ctx.open_url(egui::OpenUrl { url, new_tab: true });
        }
        if self.state.quit {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // `theme::apply` writes both theme slots, so a platform theme switch
        // cannot drop the named styles.  `sync` repairs the state if anything
        // ever does drop them, and re-applies the design when the user changes
        // the appearance in the settings: `egui` panics when a widget asks for
        // a `TextStyle::Name` that is not registered, and a panic inside
        // `update` closes the whole window.
        theme::sync(ctx, self.core.settings().theme);

        self.pump(ctx);

        // Freshly synchronised mail is now in SQLite; re-query the list once,
        // after the burst of engine events, so it appears without a restart.
        self.state.flush_sync(&self.core);

        // A debounced server-side search, once its delay has elapsed
        // (`FR-SEARCH-2`); `Enter` arms it with no delay (`FR-SEARCH-3`).
        if let Some(task) = self.state.due_server_search() {
            self.dispatch(task);
        }

        if let Some(shortcut) = shortcut(ctx) {
            let has_selection = self.state.selected.is_some();
            if let Some(action) = from_shortcut(shortcut, has_selection) {
                self.actions.push(action);
            }
        }

        view::draw(ctx, &mut self.state, &mut self.actions, &mut self.links);
        dialogs::draw(ctx, &mut self.state, &mut self.actions);
        self.execute(ctx);

        ctx.request_repaint_after(IDLE_REDRAW);
    }
}

/// Reads the keyboard gesture of this frame, if any.
///
/// While a text field has the focus only `Escape` is honoured, so that typing
/// a `j` in the search box does not move the message selection (and `Delete`
/// does not delete the message being written).
fn shortcut(ctx: &egui::Context) -> Option<Shortcut> {
    let typing = ctx.memory(|memory| memory.focused()).is_some();
    ctx.input(|input| {
        let ctrl = input.modifiers.ctrl || input.modifiers.command;
        let shift = input.modifiers.shift;
        let alt = input.modifiers.alt;
        for event in &input.events {
            let egui::Event::Key {
                key, pressed: true, ..
            } = event
            else {
                continue;
            };
            if typing && !(ctrl || alt) {
                // While a text field has the focus, a bare key is text input;
                // only `Escape` is honoured.  Modified chords still work.
                return (*key == egui::Key::Escape).then_some(Shortcut::Escape);
            }
            if ctrl && alt {
                // `Strg+Alt+R` opens the rule editor; every other `Ctrl+Alt`
                // combination is left to the window system.
                if *key == egui::Key::R {
                    return Some(Shortcut::Rules);
                }
                continue;
            }
            let shortcut = match (*key, ctrl, shift) {
                (egui::Key::N, true, false) => Shortcut::NewMessage,
                (egui::Key::R, true, false) => Shortcut::Reply,
                (egui::Key::R, true, true) => Shortcut::ReplyAll,
                (egui::Key::U | egui::Key::L, true, false) => Shortcut::Forward,
                (egui::Key::Q, true, false) => Shortcut::MarkRead,
                (egui::Key::Q, true, true) => Shortcut::MarkUnread,
                (egui::Key::Space, true, false) => Shortcut::ToggleFlag,
                (egui::Key::W, true, true) => Shortcut::Answered,
                (egui::Key::E, false, false) => Shortcut::Done,
                (egui::Key::F, true, false) => Shortcut::Search,
                (egui::Key::Comma, true, false) => Shortcut::Settings,
                (egui::Key::E, true, false) => Shortcut::SaveDraft,
                (egui::Key::A, true, true) => Shortcut::Accounts,
                (egui::Key::Y, true, false) | (egui::Key::F5, _, _) => Shortcut::Sync,
                (egui::Key::Delete, _, false) => Shortcut::Delete,
                (egui::Key::ArrowDown | egui::Key::J, false, false) => Shortcut::NextMessage,
                (egui::Key::ArrowUp | egui::Key::K, false, false) => Shortcut::PreviousMessage,
                (egui::Key::Escape, _, _) => Shortcut::Escape,
                _ => continue,
            };
            return Some(shortcut);
        }
        None
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn every_pane_has_a_label() {
        for pane in Pane::ALL {
            assert!(!pane.label().is_empty());
        }
    }

    #[test]
    fn panes_are_unique() {
        let unique: HashSet<Pane> = Pane::ALL.into_iter().collect();
        assert_eq!(unique.len(), Pane::ALL.len());
    }

    #[test]
    fn window_geometry_is_usable() {
        assert!(MIN_WINDOW_SIZE[0] <= DEFAULT_WINDOW_SIZE[0]);
        assert!(MIN_WINDOW_SIZE[1] <= DEFAULT_WINDOW_SIZE[1]);
        assert!(MIN_WINDOW_SIZE[0] >= 640.0);
        assert!(MIN_WINDOW_SIZE[1] >= 480.0);
    }

    #[test]
    fn bundle_id_matches_the_macos_requirement() {
        assert_eq!(BUNDLE_ID, "de.hylmail.HylMail");
    }

    #[test]
    fn the_ui_framework_is_eframe() {
        assert_eq!(FRAMEWORK, "egui/eframe");
    }

    #[test]
    fn the_idle_redraw_is_not_a_busy_loop() {
        assert!(IDLE_REDRAW >= Duration::from_millis(100));
    }

    #[test]
    fn a_second_instance_forwards_without_a_window() {
        let dir = tempfile::tempdir().unwrap();
        let env = hylmail_core::paths::MapEnv::new().with(
            hylmail_core::paths::PORTABLE_ENV,
            dir.path().to_string_lossy().to_string(),
        );
        let paths = hylmail_core::AppPaths::resolve_for(hylmail_core::Os::Linux, &env).unwrap();
        let config = paths.config_dir().to_path_buf();
        let core = Core::bootstrap(
            paths,
            std::sync::Arc::new(hylmail_core::secrets::InMemorySecretStore::new()),
        )
        .unwrap();

        let first = ipc::acquire(&config).unwrap();
        assert!(matches!(first, Instance::Primary(_)));
        run_with(core, Vec::new()).unwrap();
        drop(first);
    }
}
