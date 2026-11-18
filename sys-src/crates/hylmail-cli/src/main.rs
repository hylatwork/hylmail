//! The `hylmail-cli` executable.
//!
//! Diagnostics, scripting and the `.eml` hand-off.  Called without an argument
//! it prints the usage text; `--ui` opens the native window.  The desktop
//! launcher with the same command line is `hylmail-gui`.
//!
//! Everything lives in [`hylmail_cli`], so both executables share one startup
//! path and one set of tests.

fn main() -> std::process::ExitCode {
    hylmail_cli::cli()
}
