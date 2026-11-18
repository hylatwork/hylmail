//! Command line parsing.
//!
//! HylMail is a GUI application first; the command line exists for diagnostics,
//! for the single-instance hand-off and for use from scripts.  The parser is
//! hand written on purpose: adding a dependency for six flags would slow down
//! every CI run without buying anything.

use std::path::PathBuf;

use anyhow::{bail, Result};

use crate::sync::Request;

/// What the process was asked to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Print the version and exit.
    Version,
    /// Print the usage text and exit.
    Help,
    /// Print the resolved data locations and exit.
    Where,
    /// Create every directory, document and database.
    Init,
    /// Print database statistics.
    Stats,
    /// Validate the JSON configuration documents.
    CheckConfig,
    /// Run one synchronisation pass and print a report.
    Sync(Request),
    /// Parse a `.eml` file and print what the MIME pipeline made of it.
    Parse(PathBuf),
    /// Print the outbox.
    Outbox,
    /// Put every parked outbox message back into the queue.
    Retry,
    /// Submit the due outbox messages once.
    Send,
    /// Build a draft JSON into an `.eml` file.
    Compose {
        /// The draft document to build.
        draft: PathBuf,
        /// Where the `.eml` file is written; `None` prints a summary.
        out: Option<PathBuf>,
    },
    /// Start the native user interface.
    Ui,
    /// Hand a `.eml` file to the running instance.
    OpenEml(PathBuf),
    /// Run the OAuth2 sign-in for the account with this address.
    OAuthLogin(String),
}

/// Parsed command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Args {
    /// Requested action.
    pub command: Command,
    /// `true` when the command line asked for something specific.
    ///
    /// The two launchers differ exactly here: `hylmail-cli` prints the usage
    /// text when the command line is empty, `hylmail-gui` opens the window.
    /// `--help` therefore sets the flag, a lone `--verbose` does not.
    pub explicit: bool,
    /// Raise the log level to `debug`.
    pub verbose: bool,
    /// With `--parse`: write every attachment of the file into this directory.
    pub save_attachments: Option<PathBuf>,
}

impl Default for Args {
    fn default() -> Self {
        Self {
            command: Command::Help,
            explicit: false,
            verbose: false,
            save_attachments: None,
        }
    }
}

/// Usage text.
pub const USAGE: &str = "\
HylMail - cross-platform mail client for IMAP, Exchange (EWS) and GroupWise proxy

Usage:
    hylmail-cli [OPTIONS] [FILE.eml]
    hylmail-gui [OPTIONS] [FILE.eml]

Both executables accept the same options.  `hylmail-cli` prints this text when
it is called without an argument, `hylmail-gui` opens the window instead.

Options:
    -h, --help          Print this help text
    -V, --version       Print the version and exit
    -v, --verbose       Log at debug level
        --where         Print the resolved configuration and data directories
        --init          Create directories, configuration files and the database
        --stats         Print database and cache statistics
        --check-config  Validate accounts.json, rules.json and settings.json
        --parse FILE    Parse a .eml file and print the MIME pipeline result
        --save-attachments DIR
                        With --parse: write every attachment into DIR
        --outbox        Print the outbox queue
        --retry         Put every parked outbox message back into the queue
        --send          Submit the due outbox messages once
        --compose FILE  Build a draft JSON into an .eml file
        --out FILE      With --compose: write the .eml here instead of printing
        --sync [WHO]    Synchronise the matching account once and print a report
        --full          With --sync: also drop messages deleted on the server
        --max-new N     With --sync: stop after N new messages per folder
        --oauth-login WHO
                        Sign in to the OAuth2 account WHO in the browser
        --ui            Start the native user interface (egui/eframe)

Environment:
    HYLMAIL_HOME         Portable mode: keep every file inside this directory
    RUST_LOG            Overrides the configured log level
";

/// Parses the arguments, excluding the program name.
///
/// The input is collected first so that an option can consume the argument that
/// follows it (`--sync ada@example.org`, `--max-new 500`).
///
/// # Errors
///
/// Returns an error for an unknown option, a missing option argument or a `.eml`
/// path that is combined with an action.
pub fn parse<I, S>(arguments: I) -> Result<Args>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let arguments: Vec<String> = arguments.into_iter().map(Into::into).collect();
    let mut args = Args::default();
    let mut explicit = false;
    let mut pending_eml: Option<PathBuf> = None;
    let mut sync_request = Request::default();
    let mut compose_draft: Option<PathBuf> = None;
    let mut compose_out: Option<PathBuf> = None;

    let mut index = 0usize;
    while index < arguments.len() {
        let raw = arguments[index].clone();
        let raw = raw.as_str();
        index += 1;

        match raw {
            "-h" | "--help" => {
                args.command = Command::Help;
                explicit = true;
            }
            "-V" | "--version" => {
                args.command = Command::Version;
                explicit = true;
            }
            "-v" | "--verbose" => args.verbose = true,
            "--where" => {
                args.command = Command::Where;
                explicit = true;
            }
            "--init" => {
                args.command = Command::Init;
                explicit = true;
            }
            "--stats" => {
                args.command = Command::Stats;
                explicit = true;
            }
            "--check-config" => {
                args.command = Command::CheckConfig;
                explicit = true;
            }
            "--parse" => {
                let value = value_of(&arguments, &mut index, "--parse")?;
                args.command = Command::Parse(PathBuf::from(value));
                explicit = true;
            }
            "--outbox" => {
                args.command = Command::Outbox;
                explicit = true;
            }
            "--save-attachments" => {
                let value = value_of(&arguments, &mut index, "--save-attachments")?;
                args.save_attachments = Some(PathBuf::from(value));
            }
            "--retry" => {
                args.command = Command::Retry;
                explicit = true;
            }
            "--send" => {
                args.command = Command::Send;
                explicit = true;
            }
            "--compose" => {
                compose_draft = Some(PathBuf::from(value_of(
                    &arguments,
                    &mut index,
                    "--compose",
                )?));
            }
            "--out" => {
                compose_out = Some(PathBuf::from(value_of(&arguments, &mut index, "--out")?));
            }
            "--sync" => {
                // An optional account filter may follow.
                if let Some(value) = arguments.get(index) {
                    if !value.starts_with('-') {
                        sync_request.account = Some(value.clone());
                        index += 1;
                    }
                }
                args.command = Command::Sync(sync_request.clone());
                explicit = true;
            }
            "--full" => {
                sync_request.full = true;
                if !explicit || matches!(args.command, Command::Sync(_)) {
                    args.command = Command::Sync(sync_request.clone());
                }
            }
            "--max-new" => {
                let value = value_of(&arguments, &mut index, "--max-new")?;
                sync_request.max_new = value
                    .parse()
                    .map_err(|_| anyhow::anyhow!("`{value}` is not a number"))?;
                if matches!(args.command, Command::Sync(_)) {
                    args.command = Command::Sync(sync_request.clone());
                }
            }
            "--ui" => {
                args.command = Command::Ui;
                explicit = true;
            }
            "--oauth-login" => {
                let value = value_of(&arguments, &mut index, "--oauth-login")?;
                args.command = Command::OAuthLogin(value);
                explicit = true;
            }
            other if other.starts_with("--parse=") => {
                args.command = Command::Parse(PathBuf::from(other.trim_start_matches("--parse=")));
                explicit = true;
            }
            other if other.starts_with("--save-attachments=") => {
                args.save_attachments = Some(PathBuf::from(
                    other.trim_start_matches("--save-attachments="),
                ));
            }
            other if other.starts_with("--compose=") => {
                compose_draft = Some(PathBuf::from(other.trim_start_matches("--compose=")));
            }
            other if other.starts_with("--out=") => {
                compose_out = Some(PathBuf::from(other.trim_start_matches("--out=")));
            }
            other if other.starts_with("--sync=") => {
                sync_request.account = Some(other.trim_start_matches("--sync=").to_string());
                args.command = Command::Sync(sync_request.clone());
                explicit = true;
            }
            other if other.starts_with("--max-new=") => {
                let value = other.trim_start_matches("--max-new=");
                let count: usize = value
                    .parse()
                    .map_err(|_| anyhow::anyhow!("`{value}` is not a number"))?;
                sync_request.max_new = count;
                if matches!(args.command, Command::Sync(_)) {
                    args.command = Command::Sync(sync_request.clone());
                }
            }
            other if other.starts_with("--") => {
                bail!("unknown option `{other}`; run `hylmail-cli --help`");
            }
            other if other.starts_with('-') && other.len() > 1 => {
                bail!("unknown option `{other}`; run `hylmail-cli --help`");
            }
            path => {
                let path = PathBuf::from(path);
                if is_eml(&path) {
                    if pending_eml.is_some() {
                        bail!("only one .eml file may be passed at a time");
                    }
                    pending_eml = Some(path);
                } else {
                    bail!(
                        "`{}` is neither an option nor a .eml file; run `hylmail-cli --help`",
                        path.display()
                    );
                }
            }
        }
    }

    if let Some(draft) = compose_draft {
        if explicit {
            bail!("--compose cannot be combined with another action");
        }
        args.command = Command::Compose {
            draft,
            out: compose_out,
        };
        explicit = true;
    } else if compose_out.is_some() {
        bail!("--out needs --compose");
    }

    if let Some(path) = pending_eml {
        if explicit {
            bail!("a .eml file cannot be combined with another action");
        }
        args.command = Command::OpenEml(path);
        explicit = true;
    }

    if args.save_attachments.is_some() && !matches!(args.command, Command::Parse(_)) {
        bail!("--save-attachments only makes sense together with --parse FILE");
    }

    args.explicit = explicit;
    Ok(args)
}

/// Takes the value of an option that was spelled as a separate argument, e.g.
/// `--max-new 500`.
fn value_of(arguments: &[String], index: &mut usize, option: &str) -> Result<String> {
    let value = arguments.get(*index).cloned();
    match value {
        Some(value) => {
            *index += 1;
            Ok(value)
        }
        None => bail!("`{option}` needs a value; run `hylmail-cli --help`"),
    }
}

fn is_eml(path: &std::path::Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("eml"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_str(input: &str) -> Result<Args> {
        parse(input.split_whitespace().map(ToString::to_string))
    }

    #[test]
    fn no_arguments_shows_help() {
        let args = parse_str("").unwrap();
        assert_eq!(args.command, Command::Help);
        assert!(!args.verbose);
        // Nothing was asked for, so `hylmail-gui` may substitute the window.
        assert!(!args.explicit);
    }

    #[test]
    fn only_an_action_makes_the_command_line_explicit() {
        for arguments in ["", "--verbose", "-v"] {
            assert!(!parse_str(arguments).unwrap().explicit, "{arguments}");
        }
        for arguments in [
            "--help",
            "--version",
            "--where",
            "--stats",
            "--ui",
            "--outbox",
            "--parse in.eml",
            "nachricht.eml",
        ] {
            assert!(parse_str(arguments).unwrap().explicit, "{arguments}");
        }
    }

    #[test]
    fn version_flags_are_recognised() {
        assert_eq!(parse_str("-V").unwrap().command, Command::Version);
        assert_eq!(parse_str("--version").unwrap().command, Command::Version);
        assert_eq!(parse_str("-h").unwrap().command, Command::Help);
        assert_eq!(parse_str("--help").unwrap().command, Command::Help);
    }

    #[test]
    fn diagnostic_commands_are_recognised() {
        assert_eq!(parse_str("--where").unwrap().command, Command::Where);
        assert_eq!(parse_str("--init").unwrap().command, Command::Init);
        assert_eq!(parse_str("--stats").unwrap().command, Command::Stats);
        assert_eq!(
            parse_str("--check-config").unwrap().command,
            Command::CheckConfig
        );
        assert_eq!(parse_str("--ui").unwrap().command, Command::Ui);
    }

    #[test]
    fn verbose_can_be_combined_with_an_action() {
        let args = parse_str("--verbose --stats").unwrap();
        assert!(args.verbose);
        assert_eq!(args.command, Command::Stats);
    }

    #[test]
    fn sync_takes_an_optional_account_filter() {
        assert_eq!(
            parse_str("--sync").unwrap().command,
            Command::Sync(Request::default())
        );
        let filter = parse_str("--sync ada@example.org").unwrap().command;
        assert_eq!(
            filter,
            Command::Sync(Request {
                account: Some("ada@example.org".to_string()),
                ..Request::default()
            })
        );
        // The two-step flags work in either order.
        let full = parse_str("--full --sync").unwrap().command;
        assert_eq!(
            full,
            Command::Sync(Request {
                full: true,
                ..Request::default()
            })
        );
        let full = parse_str("--sync --full").unwrap().command;
        assert_eq!(
            full,
            Command::Sync(Request {
                full: true,
                ..Request::default()
            })
        );
    }

    #[test]
    fn the_message_limit_accepts_both_spellings() {
        for input in ["--sync --max-new 500", "--sync --max-new=500"] {
            let command = parse_str(input).unwrap().command;
            assert_eq!(
                command,
                Command::Sync(Request {
                    max_new: 500,
                    ..Request::default()
                }),
                "{input}"
            );
        }
        assert!(parse_str("--max-new=abc").is_err());
        assert!(parse_str("--sync --max-new").is_err());
    }

    #[test]
    fn eml_files_are_recognised_case_insensitively() {
        let args = parse_str("C:\\mail\\Note.EML").unwrap();
        assert_eq!(
            args.command,
            Command::OpenEml(PathBuf::from("C:\\mail\\Note.EML"))
        );
        let args = parse_str("./message.eml").unwrap();
        assert_eq!(
            args.command,
            Command::OpenEml(PathBuf::from("./message.eml"))
        );
    }

    #[test]
    fn several_eml_files_are_rejected() {
        assert!(parse_str("a.eml b.eml").is_err());
    }

    #[test]
    fn eml_cannot_be_combined_with_an_action() {
        assert!(parse_str("--stats a.eml").is_err());
    }

    #[test]
    fn save_attachments_needs_parse() {
        let args = parse_str("--parse in.eml --save-attachments out").unwrap();
        assert_eq!(args.command, Command::Parse(PathBuf::from("in.eml")));
        assert_eq!(args.save_attachments, Some(PathBuf::from("out")));

        let args = parse_str("--parse=in.eml --save-attachments=out").unwrap();
        assert_eq!(args.command, Command::Parse(PathBuf::from("in.eml")));
        assert_eq!(args.save_attachments, Some(PathBuf::from("out")));

        assert!(parse_str("--stats --save-attachments out").is_err());
        assert!(parse_str("--save-attachments out").is_err());
        assert!(parse_str("--parse in.eml --save-attachments").is_err());
    }

    #[test]
    fn a_command_without_save_attachments_leaves_it_out() {
        let args = parse_str("--parse in.eml").unwrap();
        assert!(args.save_attachments.is_none());
    }

    #[test]
    fn non_eml_arguments_are_rejected() {
        let error = parse_str("notes.txt").unwrap_err();
        assert!(error
            .to_string()
            .contains("neither an option nor a .eml file"));
    }

    #[test]
    fn unknown_options_are_rejected() {
        assert!(parse_str("--nope").is_err());
        assert!(parse_str("-x").is_err());
        let error = parse_str("--nope").unwrap_err();
        assert!(error.to_string().contains("unknown option"));
    }

    #[test]
    fn usage_mentions_every_option() {
        for option in [
            "--help",
            "--version",
            "--verbose",
            "--where",
            "--init",
            "--stats",
            "--check-config",
            "--ui",
            "HYLMAIL_HOME",
        ] {
            assert!(USAGE.contains(option), "usage text is missing `{option}`");
        }
        // The usage text names both executables, because both print it.
        assert!(USAGE.contains("hylmail-cli"));
        assert!(USAGE.contains("hylmail-gui"));
    }
}
