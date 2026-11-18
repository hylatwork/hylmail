//! The `hylmail-gui` executable: HylMail as a desktop application.
//!
//! It accepts the same options as `hylmail-cli` - `--where`, `--check-config`,
//! `--sync`, a bare `.eml` file, all of them - but starts the native window
//! when the command line asked for nothing at all.  That is what the desktop
//! entry, the installer's shortcut and the `.eml` file association point at.

fn main() -> std::process::ExitCode {
    hylmail_cli::desktop()
}
