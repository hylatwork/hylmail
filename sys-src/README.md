# HylMail

A cross-platform desktop mail client written in Rust that unifies **IMAP/SMTP**,
**Exchange on-premises (EWS, SOAP/XML)** and **Novell/MicroFocus GroupWise
proxy** mailboxes behind one native, dark user interface.

HylMail is a private mail client: credentials never leave the operating system's
credential vault, no telemetry is sent, and every protocol byte that reaches the
log file is scrubbed of passwords and tokens first.

> **All five phases are complete.** The workspace, the domain model, the file
> system layout, the SQLite schema with its migration system, the credential
> vault abstraction, the redacting logger, the settings persistence layer, the
> IMAP/SMTP engine, the Exchange (EWS) and GroupWise proxy clients, the MIME
> pipeline with the HTML sanitiser, the rule engine, the composer and the
> Outbox, and the native egui/eframe window with its single-instance hand-off
> are implemented, documented and covered by 1012 tests.

---

## Table of contents

1. [Repository layout](#repository-layout)
2. [Requirements](#requirements)
3. [The two launchers](#the-two-launchers)
4. [Building and running](#building-and-running)
5. [Command line reference](#command-line-reference)
6. [Where data lives](#where-data-lives)
7. [Architecture](#architecture)
8. [Protocol engine](#protocol-engine-phase-2)
9. [Exchange and GroupWise](#exchange-and-groupwise-phase-3)
10. [MIME pipeline, rules and the composer](#mime-pipeline-rules-and-the-composer-phase-4)
11. [The native user interface](#the-native-user-interface-phase-5)
12. [Security model](#security-model)
13. [Testing](#testing)
14. [Roadmap](#roadmap)
15. [Packaging](#packaging)

---

## Repository layout

```
hylmail/
├── Cargo.toml                 # workspace: [workspace.package] holds the version
├── rustfmt.toml
├── .github/workflows/ci.yml   # fmt + clippy + test on Linux, Windows and macOS
├── crates/
│   ├── hylmail-core/           # business logic - NO UI dependency at all
│   │   ├── src/
│   │   │   ├── bootstrap.rs   # `Core`: the façade that wires everything together
│   │   │   ├── error.rs       # structured `Error` / `Result`, no panics
│   │   │   ├── paths.rs       # platform paths, portable mode, Windows migration
│   │   │   ├── secrets.rs     # credential vault abstraction
│   │   │   ├── ipc.rs         # single-instance lock and the loopback `.eml` hand-off
│   │   │   ├── body.rs        # on-demand body download for the reading pane
│   │   │   ├── flags.rs       # pushing read/flagged changes back to the server
│   │   │   ├── models/        # accounts, folders, messages, rules, settings, ...
│   │   │   ├── net/           # sockets, TLS policy, PEM, deadlines, NTLM (`MS-NLMP`)
│   │   │   ├── soap/          # SOAP DOM, envelopes, shared HTTP transport
│   │   │   ├── imap/          # IMAP client, IDLE, synchronisation
│   │   │   ├── smtp/          # submission through lettre
│   │   │   ├── send.rs        # the submission dispatch per account kind
│   │   │   ├── ews/           # Exchange Web Services: folders, sync, MIME, flags
│   │   │   ├── groupwise/     # GroupWise SOAP proxy: session, folders, items
│   │   │   ├── mime/          # MIME parsing: `mail-parser` into the domain model
│   │   │   ├── html/          # `ammonia` + `scraper` sanitiser, plain-text rendering
│   │   │   ├── calendar.rs    # `icalendar` invitations, German summaries
│   │   │   ├── image.rs       # inline images and `resvg` rasterising
│   │   │   ├── compose/       # message builder, replies, signatures
│   │   │   ├── outbox/        # the delivery queue state machine
│   │   │   ├── rules/         # the rule engine and its action plan
│   │   │   ├── sync/          # background engine, commands/events, backoff
│   │   │   ├── store/         # atomic JSON documents
│   │   │   ├── db/            # SQLite: schema, migrations, FTS5, cache, repositories
│   │   │   └── logging/       # redacting `tracing` subscriber, crash reports
│   │   └── tests/             # integration tests (migration, persistence, redaction, IMAP, EWS, GroupWise, MIME, Outbox)
│   ├── hylmail-gui/            # native UI (egui/eframe): window boot, panels, dialogs
│   │   └── src/               # lib, theme, action, state, worker, view, dialogs, html_view, format
│   └── hylmail-cli/            # the two launchers: `hylmail-gui` (window) and `hylmail-cli` (terminal)
│       ├── src/lib.rs         # shared startup: paths, logging, crash reports, command dispatch
│       ├── src/gui_main.rs    # `hylmail-gui`: the window when called without an argument
│       └── src/main.rs        # `hylmail-cli`: the usage text when called without an argument
└── packaging/                 # installers, icons and platform metadata (`.iss`, `.wxs`, `Info.plist`, `.desktop`)
```

The dependency direction is strictly one-way:

```
hylmail-cli ──► hylmail-gui ──► hylmail-core
     └──────────────────────────┘
```

`hylmail-core` never depends on a UI crate, which is what allows the whole core
to be tested headlessly and to be reused from a future server-side component.

## Requirements

* **Rust 1.88 or newer** (the workspace declares `rust-version = "1.88"`; the
  IMAP client is an edition 2024 crate).
* **Windows:** the **MSVC** toolchain
  (`rustup default stable-x86_64-pc-windows-msvc`). The `*-pc-windows-gnu`
  toolchain needs a MinGW C compiler for the bundled SQLite amalgamation; on a
  machine without one, `rusqlite` cannot build. MSVC comes with Visual Studio
  Build Tools and needs no extra C compiler.
* **Linux (desktop build):** the Secret Service backend needs `libdbus-1-dev`
  and `pkg-config`, and the window needs an OpenGL 1.4 driver plus
  `libxkbcommon` and either X11 or Wayland:
  `sudo apt install libdbus-1-dev pkg-config libxkbcommon0 libgl1`.
* **macOS:** nothing beyond the Xcode command line tools.

There is deliberately **no `rust-toolchain.toml`**: pinning a host specific
toolchain name (`x86_64-pc-windows-msvc`, `aarch64-apple-darwin`, ...) in the
repository would break every other platform. Select the toolchain in CI and
document it here instead.

## The two launchers

One library, two executables - the whole difference is what an empty command
line means:

| Executable | Without an argument | What it is for |
|---|---|---|
| `hylmail-gui` | opens the window | the desktop entry, the installer shortcut, the `.eml` file association, the macOS bundle |
| `hylmail-cli` | prints the usage text | scripts, diagnostics, `--sync`, the terminal |

Every option exists in both, so `hylmail-gui --where` and `hylmail-cli --where`
are the same program, and `hylmail-cli --ui` opens the same window as
`hylmail-gui`. `cargo run -p hylmail-cli` runs the desktop one because the
package sets `default-run = "hylmail-gui"`; add `--bin hylmail-cli` for the
terminal.

Both share the single-instance lock, which lives in the configuration
directory: starting `hylmail-cli --ui` while `hylmail-gui` is already running
raises that window instead of opening a second one.

## Building and running

```bash
# Debug build of both launchers
cargo build -p hylmail-cli

# Release build of everything
cargo build --workspace --release

# Quality gates - all four must be silent
cargo fmt --all --check
cargo clippy --workspace --all-targets
cargo test --workspace
cargo doc --workspace --no-deps

# Run the application (opens the window)
cargo run -p hylmail-cli

# Hand a message file to a running instance (or start one with it)
cargo run -p hylmail-cli -- nachricht.eml
```

Phase 4 is exercised from the command line without opening the UI
(`--bin hylmail-cli` selects the terminal launcher):

```bash
# Parse a message file and print what the MIME pipeline made of it
cargo run -p hylmail-cli --bin hylmail-cli -- --parse message.eml

# ... and write every attachment into a directory (base64 already decoded)
cargo run -p hylmail-cli --bin hylmail-cli -- --parse message.eml --save-attachments ./anhaenge

# Inspect and drive the outbox
cargo run -p hylmail-cli --bin hylmail-cli -- --outbox
cargo run -p hylmail-cli --bin hylmail-cli -- --retry
cargo run -p hylmail-cli --bin hylmail-cli -- --send

# Build a stored draft JSON into an .eml file
cargo run -p hylmail-cli --bin hylmail-cli -- --compose draft.json --out message.eml
```

On Windows, prefix the commands with the toolchain:

```powershell
cargo +stable-x86_64-pc-windows-msvc clippy --workspace --all-targets
```

## Command line reference

```
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
        --compose FILE  Build a draft JSON document into an .eml file
        --out FILE      With --compose: write the .eml here instead of printing
        --sync [WHO]    Synchronise the matching account once and print a report
        --full          With --sync: also drop messages deleted on the server
        --max-new N     With --sync: stop after N new messages per folder
        --oauth-login WHO
                        Sign in to the OAuth2 account WHO in the browser
        --ui            Start the native user interface

Environment:
    HYLMAIL_HOME         Portable mode: keep every file inside this directory
    RUST_LOG            Overrides the configured log level
```

`--where` is the fastest way to see which files an installation is using:

```
$ hylmail-cli --where
HylMail data locations
  mode        : Portable
  platform    : Windows
  config dir  : C:/Users/ada/Stick/HylMail
  data dir    : C:/Users/ada/Stick/HylMail
  settings    : C:/Users/ada/Stick/HylMail\settings.json
  ...
```

## Where data lives

| Platform | Configuration | Cache, logs and database |
|---|---|---|
| Windows | `%APPDATA%\HylMail` | `%LOCALAPPDATA%\HylMail` |
| macOS | `~/.HylMail` | `~/.HylMail` |
| Linux | `~/.HylMail` | `~/.HylMail` |
| **Portable** (`HYLMAIL_HOME` set) | `$HYLMAIL_HOME` | `$HYLMAIL_HOME` |

Setting `HYLMAIL_HOME` switches every path into a single directory, which makes
HylMail usable from a USB stick and is what the test suite uses to avoid touching
the developer's real profile.

**Windows migration.** On the first start of a Windows build that finds a legacy
`%USERPROFILE%\.HylMail` directory and an empty target, the legacy directory is
migrated transactionally: the tree is copied into a staging directory, verified
byte-for-byte, renamed into place and only then is the legacy directory removed.
If anything fails, the source is left untouched and the application starts with
a fresh profile instead of losing data. The database and `logs/` move to the
local profile, the JSON documents to the roaming profile.

### The configuration documents

Configuration is stored as separate, human readable JSON documents rather than
one monolithic file, so an unreadable file can never take the whole application
down:

| File | Contents |
|---|---|
| `settings.json` | theme, cache limit, page size, search behaviour, image policy, signatures |
| `accounts.json` | account definitions - **without** passwords |
| `rules.json` | filter rules with conditions and actions |
| `drafts.json` | drafts that are not yet being sent |
| `outbox.json` | queued mail with its delivery state |

Every write goes through `write_json_atomic`: a temporary file in the same
directory, `flush`, `persist` (an atomic rename) and a directory `fsync`. A
corrupt document is quarantined as `<name>.json.corrupt-<timestamp>` and
replaced by defaults instead of aborting the start.

Notable defaults: 500 MiB content cache, page size 500, server-side search after
700 ms and at least 3 characters, at most 500 hits, German UI language.

## Architecture

The C4 model - context, containers, components, the trust boundaries and the
four load-bearing call flows - lives in [`sys-doc/ARCHITECTURE.md`](../sys-doc/ARCHITECTURE.md).
It is a document under test: `crates/hylmail-cli/tests/architecture.rs` fails
when a module, a crate boundary, a diagram or the dormant list stops matching
the source, and checks that `sys-doc/REQUIREMENTS.md` keeps its headings, its
unique identifiers and its traceability links. This section is the short version
for day to day work.

### `hylmail-core`

| Module | Responsibility |
|---|---|
| `models` | domain types - `Account`, `Folder`, `MessageHeader`, `MessageBody`, `Rule`, `Settings`, server presets |
| `paths` | data locations per platform, portable mode, legacy migration |
| `store` | atomic JSON documents, corruption quarantine |
| `db` | SQLite schema, migrations, FTS5 full text search, LRU content cache, repositories |
| `net` | sockets, TLS policy, certificate pinning, PEM handling, deadlines, `MS-NLMP` messages for NTLM |
| `soap` | SOAP DOM, envelope builder, escaping, the `reqwest`-based HTTP transport both SOAP protocols share, and the single-connection client the NTLM handshake needs |
| `imap` | IMAP4rev1 client: modified UTF-7, capabilities, mailbox tree, headers, flags, fetch, `IDLE`, synchronisation |
| `smtp` | submission through `lettre` with STARTTLS/implicit TLS and the same trust policy |
| `send` | the submission dispatch: SMTP for IMAP and GroupWise, EWS `CreateItem`/`SendAndSaveCopy` for Exchange |
| `ews` | Exchange Web Services: folder tree, incremental sync with `SyncState`, MIME download, read state, flags, move, delete |
| `groupwise` | GroupWise SOAP proxy: login sessions, folder list, paged item listing, MIME download, marks and moves |
| `mime` | MIME parsing with `mail-parser` into the domain model, headers and decoded attachment payloads included |
| `html` | `ammonia` + `scraper` sanitiser, image policy, plain-text rendering |
| `calendar` | `icalendar` invitations: method, title, organiser, attendee, German formatting |
| `image` | inline `cid:` images and SVG rasterising through `resvg` |
| `compose` | the message builder, replies and forwards, signature insertion |
| `outbox` | the delivery queue state machine and its `Submitter` seam |
| `rules` | the rule engine, condition evaluation and the merged action plan |
| `sync` | the background engine: one task per account, commands/events, retry backoff, `IDLE` watching, item reconciliation |
| `body` | on-demand body download and attachment saving in one entry point over IMAP, EWS and GroupWise |
| `flags` | pushing read and flagged changes back to the server |
| `delete` | removing a message: move into the trash where the backend has one, otherwise delete it for good |
| `relocate` | moving a message into another folder, including the item id Exchange hands out for the moved copy |
| `ipc` | single-instance lock and the loopback hand-off for `.eml` files |
| `oauth` | OAuth2 authorisation code flow with PKCE: loopback redirect, token exchange, refresh, vault storage |
| `secrets` | `SecretStore` trait: OS vault, in-memory vault for tests |
| `logging` | redacting `tracing` subscriber, log trimming, crash reports |
| `bootstrap` | `Core`, the façade that owns paths + config store + database + vault |

`Core::bootstrap_from_env()` resolves `HYLMAIL_HOME` (or the platform paths),
runs the legacy migration, creates the directories, loads/repairs
`settings.json`, opens and migrates the database and hands back a ready to use
handle. `Core::bootstrap(paths, vault)` is the injectable form, used by the
tests to run against an in-memory credential vault.

### Database

SQLite through `rusqlite` with the bundled amalgamation, opened with
`journal_mode = WAL`, `synchronous = NORMAL`, `foreign_keys = ON`, a busy
timeout and **`STRICT` tables** so a wrong type is a hard error instead of a
silently coerced value.

* **Schema version** lives in `PRAGMA user_version`; `db::MIGRATIONS` is an
  append-only array of `(version, name, sql)` entries that is applied in a
  transaction per version, so a failed migration rolls back completely and the
  next start resumes at the same point. A database written by a **newer** build
  is refused with `Error::SchemaTooNew` rather than being damaged.
* **Tables:** `folders`, `messages`, `message_content`, `contacts`,
  `conversations`, `kv_state`, plus the FTS5 index `messages_fts`.
* **Full text search** uses FTS5 with `unicode61 remove_diacritics 2`, so
  `Uebung` finds `Übung`; user input is escaped into a safe `MATCH` query.
* **Content cache** keeps MIME bodies in a separate table and evicts the least
  recently used rows (`enforce_limit`) to stay under `cache_limit_mb`.
* Timestamps are Unix milliseconds in UTC; binary blobs stay out of the
  database.

The connection is owned by a `parking_lot::Mutex` and reached through
`Database::read(..)` / `Database::write(..)`, both returning `Result`, which is
what keeps `unwrap` out of the persistence layer.

### GUI crate

`hylmail-gui` is the native window, built on **egui/eframe 0.33** with the
`glow` (OpenGL) renderer. It is the only crate that may depend on a UI
framework; `hylmail-core` stays headless so the whole core can be tested without
a display.

| Module | Responsibility |
|---|---|
| `lib` | window boot (`eframe::run_native`), the single-instance hand-off, keyboard shortcuts, the frame loop |
| `theme` | the dark palette and the `egui::Style` built from it |
| `action` | `Action` (what the user asked for), `Shortcut` (what was pressed), `SortKey` |
| `state` | `UiState`: selection, paging, composer, settings and account forms; turns an `Action` into `Task`s |
| `worker` | the background half: network jobs against `hylmail-core` and the two event channels back |
| `view` | the panels: toolbar, folder tree, virtualised message table, reading pane, status bar |
| `dialogs` | the composer, settings, accounts and move windows |
| `html_view` | a small HTML renderer: sanitised markup into an `egui` `LayoutJob` |
| `format` | German date, size and address formatting |

Nothing in `draw` performs I/O: a panel records `Action`s, `UiState::handle`
turns them into `Task`s and the worker executes those off the window thread.
The two exceptions are documented in place - the file picker (`rfd`) and
opening a `.eml` the operating system handed over.

## Protocol engine (phase 2)

### `net`

One place decides how a connection is secured, and both protocols use it.
`TlsPolicy` distinguishes `Strict`, `TrustExtraRoots(Vec<Certificate>)` and the
`Insecure` mode that the account dialog has to warn about. Implicit TLS and
STARTTLS both end in a `tokio-rustls` stream whose verifier is
`rustls-platform-verifier`, so a server with an internal CA is trusted exactly
as far as the operating system trusts it. `Timeouts` separates connect,
greeting and command deadlines so a dead host cannot block the UI. PEM bundles
are parsed and validated here (a malformed certificate is an error, never a
panic), because `lettre`'s `Certificate::from_pem` aborts the process on bad
input.

### `imap`

The client is built on `async-imap` with an explicit `imap-proto` dependency for
the response types. The interesting decisions:

* **Modified UTF-7** is implemented in `utf7`, so `Uebung` and `Übung` name the
  same mailbox and a broken encoding cannot take down a folder listing.
* **Capabilities are re-read after `STARTTLS` and after `LOGIN`.** Servers
  routinely advertise different sets on the encrypted connection, and `LOGIN`
  may be forbidden while `AUTHENTICATE PLAIN`/`XOAUTH2` are not.
* **`BODY.PEEK` everywhere.** A synchronisation must never mark a message as
  read as a side effect of downloading its headers.
* **`EXAMINE` instead of `SELECT`** for every folder that is not a target of a
  move, so `\Recent` and `\Seen` cannot change because HylMail looked.
* **`IDLE` owns the connection.** `IdleGuard` takes the session by value, which
  makes it impossible to interleave a command with an `IDLE`; the wait is bounded
  by a 25 minute re-issue deadline even if the server never says anything.
* **Incremental by default.** A pass fetches `UID FETCH <highest+1>:*`, writes
  headers first and bodies later, reconciles `\Seen`/`\Flagged` through `UID
  SEARCH`, and only a full pass removes messages the server no longer has. A
  changed `UIDVALIDITY` rebuilds the folder instead of mixing two id spaces.
* **Folders are stored parents first** (`db::folders::upsert_tree`). A `LIST`
  reply is ordered for display, so a special-use child such as
  `Projekte/Gesendet` arrives before its plain parent; the `parent_id` foreign
  key is checked per statement, so the write order has to be depth order.

### `smtp`

Submission uses `lettre` with the same `TlsPolicy`. Accounts are resolved into
an envelope (from/to/cc/bcc), the message is built as a full MIME document by
the composer in phase 4, and `verify()` performs the connectivity check of the
account dialog. The `pool` feature is deliberately *not* enabled: it spawns a
background task when a transport is constructed, which would make the transport
unusable outside a running tokio runtime.

### `sync`

The engine runs one task per enabled account behind a command/event pair of
channels, so the UI never talks to a socket:

```
EngineCommand::SyncNow/RefreshFolders/StopAccount/NetworkAvailable/Shutdown
EngineEvent::Connected/FoldersDiscovered/FolderSynced/NewMail/Idling/Disconnected/Stopped/Finished
```

`AccountState` is a small state machine (`Idle → Syncing → Idling → Waiting →
Stopped`) with the transitions unit tested; the retry delay comes from
`Backoff`, which grows exponentially, is jittered and is capped. Servers
without `IDLE` are polled with `NOOP` at the configured interval instead.

`reconcile` holds the comparison the two SOAP protocols share: which stored
messages the server still reports (matched by its opaque item id) and which are
gone. IMAP answers that with UIDs, Exchange with a `SyncState` cursor and
GroupWise by listing the folder again, but the "what disappeared?" question is
the same everywhere.

### Command line diagnostics

`hylmail-cli --sync` runs exactly one pass per account and prints what happened, so a
support case can be diagnosed without the UI:

```
$ hylmail-cli --sync ada@example.org
Ada <ada@example.org> [IMAP]
  server      : imap.example.org [TLSv1.3]
  capabilities: IMAP4rev1 IDLE MOVE UIDPLUS AUTH=PLAIN
  folders     : 2
  ... INBOX
      12 new message(s), highest UID 4711
      INBOX                       12 new      0 flags      0 removed
  stored here : 12 new, 0 flag update(s), 0 removed, 0 rebuilt
```

The command exits with a non-zero status when an account could not be
synchronised, which makes it usable from a monitoring script. All three account
kinds are covered; the `capabilities` line is IMAP specific, an Exchange or
GroupWise account shows `protocol` instead:

```
$ hylmail-cli --sync
Ada <ada@example.org> [Exchange (EWS)]
  server      : mail.example.org [encrypted]
  capabilities:
  protocol    : Exchange 15.01.2507.037 at mail.example.org (encrypted, 42 ms)
  folders     : 3
      Posteingang                    2 new      1 flags      0 removed
  stored here : 2 new, 1 flag update(s), 0 removed, 0 rebuilt
```

## Exchange and GroupWise (phase 3)

Both protocols speak SOAP, so they share one HTTP transport and one DOM.

### `soap`

* `dom::Node` is a small read-only DOM built on `quick-xml`. Element names are
  reduced to their **local name**, because Exchange and GroupWise use different
  prefixes (`t:`, `m:`, `soap:`, `gw:`, `log:`) for the same schema. Entity
  references and `CDATA` sections are resolved while parsing, and malformed XML
  becomes `Error::MalformedResponse` instead of a panic.
* `envelope::Envelope` renders the request document and the `SOAPAction` URI;
  `fault_message` extracts a human readable reason from SOAP 1.1 and 1.2 faults
  and appends the Exchange `ResponseCode` from the fault detail.
* `http::HttpClient` builds one `reqwest` client from the very same
  `TlsPolicy` the IMAP and SMTP clients use, so "trust this certificate", the
  extra roots and `require_tls13` are one per-account setting for all four
  protocols. Redirects are deliberately **not** followed automatically: the
  GroupWise proxy answers with a redirect to the post office and that decision
  has to consult the account's certificate policy. `http::HttpClient::ntlm`
  builds the second, deliberately unshared client an NTLM account needs: one
  connection, HTTP/1.1, and the handshake (`negotiate` → `401` with a challenge
  → `authenticate` on the same connection) repeated for every request.
* `net::ntlm` computes the three `NTLMSSP` messages from MD4 and HMAC-MD5; it
  knows nothing about HTTP and is verified against the published `MS-NLMP`
  vectors, which is why the transport and the credential code can be tested
  separately.

### `ews`

A custom client for on-premises Exchange - no MAPI, no autodiscover:

| Operation | Use |
|---|---|
| `GetFolder` | probes the connection, reads `ServerVersionInfo` |
| `FindFolder` | shallow, walks the tree breadth first (depth and folder caps) |
| `SyncFolderItems` | incremental sync; `SyncState` is stored in `Folder::sync_marker` |
| `GetItem` | downloads the raw MIME content (`IncludeMimeContent`) |
| `CreateItem` | submits a composed message as MIME (`SendAndSaveCopy`), so Exchange delivers it and files the copy in `Sent Items` |
| `UpdateItem` | read state and follow-up flag |
| `MoveItem` / `DeleteItem` | archiving and deleting |

* **Item shape.** `BaseShape="IdOnly"` plus an explicit list of
  `AdditionalProperties` transfers exactly the fields HylMail stores - no body
  previews for a mailbox with a hundred thousand messages.
* **Deletion detection.** A pass *without* a stored cursor enumerates the whole
  folder, so the local rows the server did not report are dropped; a pass *with*
  a cursor applies the `<t:Delete>` elements Exchange sends. `--full` (and
  `SyncDepth::Full`) simply starts without the cursor.
* **Truncated answers.** `IncludesLastItemInRange=false` makes the client call
  again with the returned cursor until the server has sent everything or
  `max_new` is reached; the next pass continues from the stored cursor.
* **Read state is not an item field.** Exchange reports it as a
  `<t:ReadFlagChange>` next to the items, so it is applied to the already stored
  header.
* **Folder ids.** Exchange folders are addressed by opaque ids, so
  `Folder::path` holds that id (exactly like the IMAP mailbox name) and the
  parent link comes from `ParentFolderId`; the display path only exists while
  the tree is discovered and is used for the kind heuristics.
* **Authentication:** `Basic` (`domain\user` is accepted), `Bearer` for OAuth2
  accounts and `NTLM`/`Negotiate` for installations that ask for the Windows
  handshake. NTLM authenticates the *connection*, not the request: the answer to
  a challenge is only valid on the connection the challenge arrived on, so an
  NTLM account gets a transport that keeps exactly one connection and speaks
  HTTP/1.1, and every request repeats the handshake (`net::ntlm`,
  `HttpClient::ntlm`). The client answers a `Negotiate` offer as well, because
  an on-premises Exchange frequently advertises only that scheme while still
  accepting an `NTLMSSP` message. A rejected password is `Error::Auth`, whether
  the server sends HTTP 401 or a `200` with `ErrorInvalidCredentials`.

### `groupwise`

The supported way into GroupWise is the SOAP proxy of the GroupWise Internet
Agent. The protocol is session based, which the client models explicitly:

1. `loginRequest` exchanges user name and password for a session id,
2. every other operation sends that session in a SOAP header, and
3. `logoutRequest` ends it.

A session can expire at any time; the client recognises the session status
codes, logs in again and **replays the request once**, so the caller only sees a
failure when the second attempt fails as well.

* **No delta protocol.** A pass pages through the folder with
  `getItemsRequest` (250 items per page, following `<items><total>`), stores what
  it does not know yet and drops the local rows the server no longer lists.
* **GroupWise names.** `Mailbox` is the inbox, `Sent Items`, `Work In
  Progress`, `Trash`, `Junk Mail` and `Cabinet` (the archive) are recognised by
  name, with the usual heuristics as a fallback.
* **Local-only flags.** The proxy exposes no categories, so
  `AccountKind::uses_local_only_flags` keeps "flagged" and "done" in SQLite for
  these accounts.
* **Local search window.** There is no search on the server, so a search filters
  the local cache and reaches back at most the most recent 1000 messages
  (`BR-GW-3`).
* **Redirects.** A redirect to the post office is never followed silently: it
  becomes an error that names the target and points at the
  `allow_certificate_cn_fallback` setting, because following it means trusting a
  certificate that does not match the configured host.
* **Authentication:** password only. GroupWise has no OAuth2 and no bearer
  tokens, so an account configured that way is refused with `Error::Unsupported`
  before a request is sent.

### What phase 3 deliberately leaves out

* **Sending** through the GroupWise proxy: a composed message is always
  submitted over SMTP. Exchange accounts send through EWS (`CreateItem` with
  `SendAndSaveCopy`, see above); the proxy's own send operation is not
  implemented, because its wire format could not be verified against a real
  server, and an SMTP submission is the path that demonstrably works.
* **Calendar and contacts** folders: they are listed and stored so the tree
  keeps its shape, but they are marked non-selectable because there is no view
  for them yet.

### Testing without a server

Both protocols are tested against **hand written HTTP/SOAP servers** that speak
just enough HTTP/1.1 to script an answer per `SOAPAction`. The tests assert what
the client *sent* (the `RequestServerVersion` header, the paging window, the
`SyncState` cursor, the session header), which is where protocol bugs live, and
they run offline and deterministically.

## MIME pipeline, rules and the composer (phase 4)

Phase 4 turns raw bytes into something a person can read and something a person
can send. It is entirely inside `hylmail-core`, so the CLI can drive every part
of it without a window.

### `mime`

`mail-parser` does the framing (headers, nested `multipart/*`, transfer
encodings, RFC 2047 words); the module maps the result onto the domain model
the database already understands.

* **Headers** become a `MessageHeader` complete enough to be listed, threaded
  and searched: decoded `Subject`, address lists, normalised `Message-ID`,
  `In-Reply-To`, the `References` chain and a UTC `Date`.
* **The body** produces a `MessageBody`: the `text/plain` part, the raw HTML and
  its sanitised form, the attachment list, an image count, the snippet, and -
  when the message carries one - the calendar part.
* **Attachments** keep the `mail-parser` part id as a string, so an attachment
  can be fetched again from the exact part it came from. `extract_attachments`
  decodes those parts into `AttachmentPayload { attachment, bytes }` on demand,
  which is what saving an attachment uses; the cached body never carries bytes.
* **A malformed message is an `Error::MalformedResponse` with protocol
  `"MIME"`.** The error carries a *reason*, never the message text: a parse
  failure must not copy private mail into a log file.

### `html`

The sanitiser is `ammonia` (an allow-list of elements and attributes) plus a
`scraper` walk that enforces the image policy:

* **`<script>`, `on*` handlers, `javascript:` URLs and CSS `expression()` are
  removed.** Links gain `rel="noopener noreferrer nofollow"`.
* **External images** are blocked, allowed or asked about, per the account's
  `ExternalImagePolicy`. A **configured tracking domain** is dropped even when
  external images are allowed, and the sanitiser counts how many were removed,
  blocked or kept inline for the banner the reader shows.
* **Inline `cid:` images** are counted and kept, so a message with a logo still
  renders offline.
* A plain-text alternative is generated from the HTML when the sender did not
  provide one, using a German inventory line ("N Bilder, M Verweise") rather
  than dumping markup.

### `calendar`

`icalendar` parses the `text/calendar` part into a `CalendarSummary`:
method (`REQUEST` / `CANCEL` / `REPLY`), title, organiser, attendees and the
start/end instants, rendered with a German date format. A floating time with a
`TZID` the client cannot resolve is **never silently treated as UTC**: the
instant stays absent instead of being guessed, and a `TZID` start is never
merged with a UTC end.

### `rules`

The rule engine evaluates a `RuleSet` against a `MessageHeader` (and the body
when a condition needs it) and merges the matches into a single `RulePlan`:
where to move, whether to mark read, flag or delete. Conditions cover the sender,
the recipient, the subject, an attachment, the size and a combined text search;
operators include `contains`, `equals`, `regex` and numeric comparisons, joined
with `and` or `or`. Lower priorities run first and a `MoveTo` may not be undone
by a later rule. `plan_on_arrival` filters to the rules the user marked for
automatic use; the "apply now" path uses `plan` for everything.

### `compose` and `outbox`

The composer builds the bytes, the Outbox delivers them.

* **Every message is `multipart/alternative`** with both a `text/plain` and a
  `text/html` part; when the user wrote only HTML the plain text is generated
  from it. Attachments make the whole thing a `multipart/mixed` around that
  alternative block - a one-form body is never wrapped in a pointless
  alternative.
* **`Bcc` builds the envelope and never the headers.** The SMTP recipients are
  `To` + `Cc` + `Bcc` deduplicated case-insensitively; the emitted header block
  carries only `To` and `Cc`, so a blind copy stays blind.
* **The signature** is inserted above or below the quote as configured, into
  both body forms, and a `cid:` attachment that cannot be found is a hard error
  rather than a silently dropped image.
* **Replies and forwards** carry the `In-Reply-To` parent and the full
  `References` chain (set in a single call, because `lettre` replaces the header
  each time), the German `Re:` / `WG:` prefix without stacking, and a `> `
  quote of the original.
* **The Outbox is a caller-driven queue.** `drain` walks the due entries, asks
the `Submitter` for a verdict and advances the state machine: `Accepted`
  records the delivery before the next attempt, `Transient` defers with a
  retry time, `Permanent` parks the entry until the user retries it. The CLI,
  the engine and (in phase 5) the GUI all call the same `drain`.

### What phase 4 deliberately leaves out

* **A local spell checker and a rich text editor** - the GUI of phase 5 owns
  the editing surface; the core only ever sees finished HTML and plain text.
* **An address book picker.** Contacts are parsed and stored, but the composer
  takes literal addresses; the picker is a UI concern.
* **A queue thread.** The engine owns the clock; `drain` is a function, not a
  task, so the same code works from a test, the CLI and the event loop.

### Testing without a network

`tests/mime_pipeline.rs` drives `.eml` fixtures through the parser, the
sanitiser and the rule engine; `tests/attachment_save.rs` does the same for
saving attachment bytes, including the message that picks its own file name;
`tests/outbox_drain.rs` exercises the queue
state machine with a scripted `Submitter`; `tests/compose_submit.rs` composes a
message and puts it on the wire through the same SMTP client the engine uses,
asserting that the `Bcc` list reaches the envelope but not the headers.

## The native user interface (phase 5)

The window is a three pane layout inside a single `eframe::App`:

```text
┌───────────────────────── toolbar: Neu  Antworten  ...  Einstellungen  Konten ─┐
│ search field, filters                                                         │
├────────────┬─────────────────────────────────────────────────────────────────┤
│ accounts   │ message list: ● ★ | Von | Betreff | 📎 | Datum     (TableBuilder) │
│ folder     ├─────────────────────────────────────────────────────────────────┤
│ tree       │ reading pane: headers, invitation box, attachments, body         │
├────────────┴─────────────────────────────────────────────────────────────────┤
│ status bar: last message, counts, per-account connection state                │
└───────────────────────────────────────────────────────────────────────────────┘
```

### Virtualised list

The list is an `egui_extras::TableBuilder` body rendered with
`body.rows(ROW_HEIGHT, count, ..)`, so a folder holding 20 000 cached headers
still only lays out the ~20 rows on screen; the specification's 500 row page
size is what `MessageQuery` retrieves, and "Weitere laden" appends the next
page. Sortable columns are `Date`, `From`, `Subject`, `Unread`, `Flagged` and
`Attachment`; unread and flagged are drawn as fixed width glyph columns so the
row never reflows when a flag changes.

### Reading pane

The body is fetched on demand (`hylmail_core::body::download`) and parsed into
the domain model before the window ever sees it, so `MessageBody` is always the
sanitised form. `html_view::render` then walks the sanitised markup and builds
one `egui::text::LayoutJob`; links are collected into a clickable list and
external images are only shown after the user allows them, which mirrors the
core's `ExternalImagePolicy`. `Quelltext` shows the sanitised source verbatim
and `📋 Text kopieren` copies the rendered text without formatting.

### Saving an attachment

The cached `MessageBody` holds the metadata of every part (name, MIME type,
size, `Content-ID`) but never the bytes, because the sync pass only fetches
headers. Saving therefore re-fetches the raw message on demand:

1. `view.rs` puts a `💾` button on every attachment row and an `Alle speichern`
   button above the list; both push an `Action`.
2. `state.rs` turns it into a single `Task::SaveAttachments` carrying
   `(part_id, destination)` pairs - one pair for the row, one per attachment
   for *save all*, so either way there is exactly one network round trip.
3. The worker fetches the raw message with `hylmail_core::body::fetch_raw` and
   hands it to `hylmail_core::body::save_attachments`, which decodes every part
   with `mime::extract_attachments` and writes the requested ones.

The file name is never taken straight from the message: `safe_file_name` keeps
only the last path component, replaces what Windows forbids and falls back to
`anhang`, and `unique_file_names` numbers collisions apart (`bericht (2).txt`)
instead of letting the second attachment overwrite the first. Both live in
`hylmail-core`, so the window and `--parse … --save-attachments` behave
identically, and a malicious `filename="../../.ssh/authorized_keys"` lands
inside the chosen directory. A body the user is reading is not re-downloaded
for the *save all* case, because both paths share the same raw fetch.
`tests/attachment_save.rs` drives exactly this from the outside: a message
whose two attachments both ask for `../../evil.txt` is written as `evil.txt`
and `evil (2).txt` in the picked directory, with nothing created next to it.

### Composer, settings and accounts

All three are `egui::Window`s over the same `UiState`. The composer edits a
working copy of a `Draft`; sending hands the draft to the worker, which passes
it to `hylmail_core::compose::build` and the Outbox, so a failed submission is
retried by the same state machine the CLI uses. The signature is inserted by
`hylmail_core::compose::signature`, so plain and HTML bodies get the same
treatment as the CLI's `--compose`. The settings window edits a copy of
`Settings` and calls `Core::save_settings`, which validates and clamps the
document before writing it atomically; the accounts window writes
`accounts.json` and stores a typed password in the OS vault under every
credential slot the account kind can use.

Every endpoint row of the accounts window carries its own **host, port and
encryption mode**: a `DragValue` for the port (`1..=65535`) and a combo box for
`SSL/TLS`, `STARTTLS` or `Keine`. The choice is never cosmetic - it is the
`Endpoint` the protocol clients build their TLS policy from. Changing the mode
also moves the port, but only as long as the port is still the conventional one
for the mode selected before, so an IMAPS account (993) switched to STARTTLS
ends up on 143 while a port the user typed themselves (1993) is left alone. The
GroupWise proxy is edited in the same grid and keeps its 7191 whatever the
mode; selecting the GroupWise protocol creates the proxy block that
`Account::validate` demands, and switching away drops it again, so the form can
never write a document the core would reject. An unencrypted endpoint is called
out in the form (`⚠ Ohne Verschlüsselung ...`) because the specification's
security model assumes TLS is on.

### Single instance and the `.eml` hand-off

The lock is a file in the configuration directory holding `{pid, port}`; the
endpoint is a TCP listener on `127.0.0.1:0`, so there is no platform specific
IPC crate and no `unsafe`. On start `ipc::acquire` pings the recorded port: if
it answers, this process is a **secondary**, forwards one `OpenEml { path }`
request per argument plus a `RaiseWindow`, and exits without creating a window.
A stale lock file (a crashed process) is detected, removed and replaced. The
primary drains its queue once per frame, so a second double click on a `.eml`
adds a tab to the *existing* window instead of starting a second one.

### Deleting a message

`Delete` (or the toolbar button) removes the message **on the server**: the
window only drops the row once the request succeeded, so a failed delete leaves
the message where it is and reports why in the status bar. The button says where
the message is going - "In den Papierkorb verschieben" while it is somewhere
else, "Endgültig löschen" once it already sits in the trash.

| Backend | What happens |
|---|---|
| IMAP | `UID MOVE` into the account's trash folder; without `MOVE` a `UID COPY` followed by `UID STORE \Deleted` and an expunge (`UID EXPUNGE` with `UIDPLUS`, otherwise `EXPUNGE`). The `STORE` sends the message's full flag set, so `\Seen` survives the delete. |
| Exchange | `DeleteItem` with `SoftDelete`, which is what puts the item into `Deleted Items`; a delete out of the trash asks for `HardDelete`. |
| GroupWise | `moveItemRequest` into the trash folder, or `deleteItemRequest` for a final removal. |

A permanent delete never touches the trash: it clears the message from the
folder it is in. The trash folder is found in the local folder tree, so the
first synchronisation of an account has to have run - without it the client
falls back to a permanent delete instead of inventing a mailbox name.

### Moving a message

The toolbar has **Verschieben** (a folder picker with a filter box),
**Archivieren** and **Spam**; all three move the message *on the server* and
drop the row locally only once it is gone from its old mailbox.

| Backend | What happens |
|---|---|
| IMAP | `UID MOVE` into the target mailbox; without the `MOVE` capability a `UID COPY` followed by `UID STORE \Deleted` and an expunge. The message gets a fresh server uid in the target mailbox, so the cached uid is cleared. |
| Exchange | `MoveItem` with a `FolderId`; EWS renumbers the item, so the new `ItemId` from the response replaces the stored one. |
| GroupWise | `moveItemRequest` through the SOAP proxy; the item id stays as it was. |

The destination is checked before a single request leaves the machine: it has
to belong to the same account, has to be able to receive mail, and may not be
the folder the message is already in - compared by id *and* by server path, so
a re-created folder row for an unchanged mailbox is not offered.

Archivieren and Spam need a folder with that role: when the synchronisation has
not found one, the status bar says so instead of guessing a mailbox name.

### Signing in to an OAuth2 account

An account whose authentication is `OAuth2` never sees a password. The accounts
window offers **Im Browser anmelden** (it stays disabled until a client id and
both endpoints are filled in), and the press starts the authorisation code flow
with PKCE from [`hylmail_core::oauth`](crates/hylmail-core/src/oauth.rs):

1. a loopback listener binds `127.0.0.1` on the configured redirect port
   (`0` lets the operating system pick one),
2. the browser opens the authorisation URL with `S256` `code_challenge`, the
   `state` and the redirect URI,
3. the provider redirects to `/oauth2/callback`, where the listener answers a
   German confirmation page and hands the code back to the worker,
4. the code plus the `code_verifier` are exchanged for a token set, which is
   written to the credential vault under the account id.

The waiting happens on the worker thread, so the window keeps drawing; the
status bar reports success or the reason the flow failed. On every later sync
`sync::account_secret` hands out a cached access token, refreshes it when it is
about to expire (`REFRESH_SKEW_SECONDS`) and only then falls back to asking the
user to sign in again. A refresh answer that omits a new refresh token keeps the
stored one, which is what Google does.

Some deliberate details:

* The verifier is stored *before* the browser opens, so a crash leaves a
  resumable flow instead of an orphaned one.
* A request for any path other than `/oauth2/callback` (browsers probe
  `/favicon.ico`) is answered `404` and the listener keeps waiting.
* A `state` that does not match the one that was sent is refused with `400` and
  the flow does not continue.
* A non-`2xx` token response is still parsed, so the user reads the provider's
  own wording instead of a status code.
* Provider presets for Gmail and Outlook.com ship the endpoints and scopes but
  deliberately no client id - every deployment registers its own.
* `--oauth-login WHO` in the CLI runs the same flow for headless setups and
  prints only how many tokens were stored, never a token.

### Keyboard

Shortcuts are suppressed while a text field has focus, except `Escape`, so
typing `j` or `Delete` in the composer can never move or delete a message.

| Key | Action |
|---|---|
| `Ctrl+N` / `Ctrl+R` / `Ctrl+Shift+R` / `Ctrl+Shift+F` | new message, reply, reply all, forward |
| `Ctrl+U` | toggle read |
| `Ctrl+L` / `Ctrl+Shift+A` | flag, mark all read |
| `Ctrl+Q` / `Ctrl+Shift+Q` | open settings, quit |
| `Ctrl+F` / `Ctrl+Comma` | focus search, settings |
| `F5` | synchronise the selected account |
| `Delete` | move to the trash; pressed on a message in the trash, delete it for good |
| `↓` / `↑` / `j` / `k` / `Space` | move in the list |
| `Escape` | clear the selection or close the dialog |

### What phase 5 deliberately leaves out

* **A rich text editing surface.** The composer edits plain text; the HTML body
  is produced by the same `compose::builder` the CLI uses.
* **Drag & drop onto folders**, multi-select and a conversation view.
* **Code signing and notarisation.** The installers are unsigned; see
  [`packaging/README.md`](packaging/README.md).

### Testing without a display

The GUI suite runs headless: `lib.rs` boots a real `Core` against a temporary
`HYLMAIL_HOME`, takes the lock and asserts that a second instance returns `Ok`
without ever calling `run_native`; `theme` asserts that the palette's text
colour contrasts with its panels; `html_view` drives the renderer over inline
markup without a window; `state` exercises the paging, sorting and composer
logic through `Action`s and asserts the resulting `Task`s.

## Security model

* **No secrets on disk.** `SecretString` (from `secrecy`) wraps credentials in
  memory and its `Debug` implementation prints `SecretBox<str>([REDACTED])`.
  Persisted accounts only carry a `CredentialKey { account_id, kind }`; the
  value lives in the OS vault (DPAPI / Keychain / Secret Service) under the
  service name `HylMail`.
* **No secrets in the log.** The `tracing` subscriber renders every event into a
  buffer, runs it through `Redactor` and only then writes it out. The standard
  rule set covers `Authorization`/`Proxy-Authorization` headers, standalone
  bearer tokens, SASL payloads (`AUTH PLAIN ...`), IMAP `LOGIN` commands, SOAP
  fields such as `<Password>` and `<SessionId>`, credentials in URL query
  strings, `Cookie`/`Set-Cookie` headers, generic `key=value` pairs for
  credential-ish keys and long base64 blobs. If the rule set fails to compile,
  the redactor **fails closed** and drops the event instead of leaking it.
* **No `unwrap`/`expect` in production code.** The workspace denies
  `clippy::unwrap_used`, `clippy::expect_used` and `clippy::dbg_macro`; the
  allow list is restricted to `#[cfg(test)]` code and to the integration test
  crates that exist to fail. `unsafe_code` is forbidden in every crate.
* **Crash reports** are written to `absturz.txt` next to the logs, pick up the
  backtrace, are folded into the log on the next start and are then archived as
  `absturz.txt.reported-<timestamp>`.

Integration tests assert these promises against real files: `logging_redaction`
writes secrets through the real logging stack and then fails the build if any of
them appears in the log file.

## Testing

```bash
cargo test --workspace          # 1008 tests (2 ignored)
cargo test -p hylmail-core       # 711 unit (1 ignored) + 122 integration (1 ignored) + 3 doc tests
cargo test -p hylmail-gui        # 138 unit tests
cargo test -p hylmail-cli        # 22 unit tests (both launchers) + 7 packaging + 5 architecture guard tests
```

| Suite | What it covers |
|---|---|
| `hylmail-core` unit tests | models, validation (including that NTLM is only valid for Exchange), JSON documents, schema/migrations, repositories, FTS query building, cache eviction, redaction rules, path resolution, the SOAP DOM, the NTLM messages (`MS-NLMP` against the published vectors, the `WWW-Authenticate` token, the single-connection client), EWS and GroupWise parsing, item reconciliation, the single-instance lock and its loopback hand-off, on-demand body download, flag push, the delete dispatch that picks a trash folder or a final removal, the
move dispatch with its destination checks, and the OAuth2 PKCE flow with its
loopback listener, token parsing and vault storage |
| `tests/legacy_migration.rs` | the Windows `.HylMail` migration: happy path, no-op, existing installation, overlap rejection, rollback with the source intact, no staging leftovers |
| `tests/persistence_roundtrip.rs` | write one of everything, shut down, boot a second instance from the same files, verify it all; cache trimming; account deletion cascade |
| `tests/logging_redaction.rs` | five kinds of credentials never reach a real log file, while ordinary prose stays readable |
| `tests/crash_report.rs` | the panic hook writes, reads and archives reports |
| `tests/imap_sync.rs` | the real client against a scripted IMAP server on a loopback socket: login, `LIST`, `EXAMINE`, `UID FETCH`, `UID SEARCH`, `IDLE` with `+ idling`/`DONE`, incremental and full passes, a changed `UIDVALIDITY`, a refused login, deleting a message with `UID MOVE` (and with `UID COPY` + `UID STORE` + `UID EXPUNGE` when the server has no `MOVE`, and with a plain `EXPUNGE` when it has no `UIDPLUS`) without ever clearing `\Seen`, moving a message with `UID MOVE` (and with `UID COPY` + `UID EXPUNGE` when the server has no `MOVE`) without touching its replacement in the target mailbox, refusing a move into the folder the message is already in, and the background engine end to end |
| `tests/smtp_submit.rs` | submission against a scripted SMTP server: envelope recipients (including `Bcc:`), `DATA` framing, a `550` recipient, a `535` credential, and the dialog's connection check |
| `tests/ews_sync.rs` | the EWS client against a scripted SOAP server: login and `ServerVersionInfo`, the folder tree with linked parents, a first pass and an incremental one with a read change and a deletion, a full pass that drops stale rows, MIME download, flag and move requests, a soft delete through the core and a hard one for a message that already is in the trash, a move that keeps the item id the server answered with and a refusal to move an item into its own folder, a submission through `send` that puts the composed bytes on the wire as base64 MIME and keeps the id of the Sent copy, a denied send that stays a permanent failure, `401` and SOAP fault handling, the NTLM handshake (a negotiate and an authenticate leg over one connection, with the domain and user of the account in the answer), and no credential in `Debug` |
| `tests/ews_engine.rs` | the background engine driving an Exchange account: `Connected` → `FoldersDiscovered` → `FolderSynced`, the rows in the database, and a clean shutdown |
| `tests/groupwise_sync.rs` | the proxy against a scripted SOAP server: session handling including a second login after `0x4202`, the folder tree (`Mailbox` → inbox), paged `getItemsRequest`, a pass that removes what the server stopped listing, MIME download, marks, moves and a delete through the core, a destination check for a move, and the redirect policy |
| `tests/attachment_save.rs` | saving attachments the way the reading pane does it: the decoded bytes land on disk, the listed `part_id`s are the ones the save path finds, a single part can be saved out of several, a hostile `filename="../../evil.txt"` (twice) stays inside the chosen directory and does not overwrite itself, an unknown part and garbage bytes are errors rather than silent no-ops, and a failed write keeps the path in the message |
| `tests/mime_pipeline.rs` | `.eml` fixtures through the whole pipeline: RFC 2047 subjects, a sanitised newsletter with its tracker dropped, the image policy matrix, an attachment, a calendar invitation, a message without a `Content-Type`, the rule engine moving a newsletter, a reply's thread headers and the `Bcc` envelope rule |
| `tests/oauth_flow.rs` | the PKCE flow against two scripted servers: a full sign-in where the sent `code_verifier` hashes to the `code_challenge` of the opened URL and the token set lands in the vault, a refresh with the `refresh_token` grant, a provider error body reported as `Error::Auth`, a non-JSON answer reported as malformed, an expired token refreshed from the vault, a still valid token that never opens a socket, and a missing configuration |
| `tests/outbox_drain.rs` | the outbox state machine with a scripted `Submitter`: an accepted delivery becomes `Sent`, a transient failure defers with a retry time, a permanent failure parks the entry, a parked entry is skipped until retried, and the per-run limit is honoured |
| `tests/compose_submit.rs` | the composer feeding the real SMTP client over a loopback socket: the composed bytes reach the wire, both body forms arrive, the signature survives and the `Bcc` recipient is in the envelope but never in the headers |
| `tests/icon_assets.rs` | the committed icon set: every PNG parses and is the expected size, the `.ico` and the big endian `.icns` carry the same rasters, and the accent colour covers the centre of the large tile; `regenerate_the_icon_set` rewrites the set from `packaging/icons/hylmail.svg` |
| `tests/packaging.rs` | the installer templates: the two launchers are named everywhere, the version only comes from the workspace, the Inno `AppId` and the MSI `UpgradeCode` are different valid GUIDs, the bundle id matches `hylmail_gui::BUNDLE_ID`, every icon, licence and deb asset exists, and the release workflow builds all five packages |
| `tests/architecture.rs` | [`sys-doc/ARCHITECTURE.md`](../sys-doc/ARCHITECTURE.md): every top level module of the three crates is named in it, `hylmail-core` has no UI dependency in its manifest or its sources, every Mermaid fence is closed and well formed - each flowchart node carries a C4 label, every edge connects two of them, each sequence diagram only uses the participants it declares - and the components section 5 calls dormant really are unreachable; plus [`sys-doc/REQUIREMENTS.md`](../sys-doc/REQUIREMENTS.md): its section headings survive, every requirement identifier is defined exactly once, every prefix (`G-`, `FR-`, `BR-`, `NFR-`, `AC-`) is used, and the traceability table still links into the architecture document |
| `hylmail-cli` unit tests | argument parsing, usage text, version, `--where` output and the launcher default |
| `hylmail-gui` unit tests | the pane model and window geometry, the dark palette's contrast and spacing, the HTML renderer, address splitting, the worker's credential slot choice, safe attachment file names, the attachment save tasks, the delete task and its permanent variant, the move picker with its destination filter and the archive/junk shortcuts, the OAuth2 sign-in hand-off and its guards, the account form's port/encryption follow-up, its sign-in choices (Windows authentication is offered for Exchange only, and switching away falls back to the password form) and its GroupWise proxy block, the settings and composer logic, opening a message into the reading pane (a real click on a list row and a cached body that is rendered at once), the pane drawing HTML, attachments and an invitation without panicking, and a second instance that forwards and returns without opening a window |

Two tests are `#[ignore]`d on purpose: `keyring_round_trip` talks to the
operating system credential vault, which would leave a real entry (and pop up a
keyring prompt) on a developer machine, and `regenerate_the_icon_set` overwrites
the committed icons. Run them explicitly with
`cargo test -p hylmail-core -- --ignored keyring_round_trip` and
`cargo test -p hylmail-core -- --ignored regenerate_the_icon_set`.

## Roadmap

| Phase | Deliverable | State |
|---|---|---|
| 1 | Workspace, domain model, paths + migration, SQLite schema, vault, redacting log, settings | **done** |
| 2 | IMAP/SMTP engine: modified UTF-7, IDLE, MOVE/UIDPLUS, sync scheduler, flag sync | **done** |
| 3 | EWS client (SOAP/XML, on-premises) and GroupWise SOAP proxy | **done** (NTLM implemented, see the note above) |
| 4 | MIME pipeline: `mail-parser`, `icalendar`, HTML sanitising (`ammonia` + `scraper`), inline images (`resvg`), rule engine, composer, Outbox | **done** (a spell checker, an address picker and the queue thread are phase 5 concerns, see above) |
| 5 | egui/eframe UI: three pane dark layout, virtualised list, reader, composer, signature editor, attachment saving, server-side deletion, move/archive/junk, settings, OAuth2 sign-in in the browser, single-instance IPC, installer | **done** (a rich text editor and drag & drop are deferred, see above) |

## Packaging

See [`packaging/README.md`](packaging/README.md) for the identifiers, the build
commands and what was verified where. In short:

* bundle identifier `de.hylmail.HylMail` (also the macOS bundle id and the
  X11 `WM_CLASS`),
* Windows: an Inno Setup 6 script for a per-user installation
  (`dist/HylMail-<version>-setup.exe`) and a WiX 3 source for a per-machine MSI
  (`dist/HylMail-<version>.msi`) - install only one of the two,
* macOS: `HylMail.app` built by `packaging/macos/make-app.sh` from universal
  binaries and wrapped into a `.dmg`,
* Linux: a `.deb` built with `cargo deb` and a tarball with the `.desktop`
  entry and the icon set,
* a `.eml` association on all three platforms that launches
  `hylmail-gui <file>`, which forwards the path to the running window,
* the icon set is generated from `packaging/icons/hylmail.svg` by an ignored
  test and checked by `cargo test --workspace`,
* `cargo test -p hylmail-cli --test packaging` keeps the installer templates
  and the version placeholders in sync on every platform.

Code signing (and macOS notarisation) is the remaining packaging work; the
release workflow in `.github/workflows/release.yml` builds all five packages
on a `v*` tag.

## License

The Rust workspace in this directory is released under the MIT License; see
[`LICENSE`](LICENSE).  The project documentation under `../sys-doc/` is licensed
separately under Creative Commons Attribution 4.0; see
[`../LICENSE.sys-doc.md`](../LICENSE.sys-doc.md).
