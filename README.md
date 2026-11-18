# HylMail

**One window for IMAP, Exchange and GroupWise mail.**

HylMail is a native desktop email client for **Windows, macOS and Linux** that
treats three kinds of corporate mailbox as first-class citizens: standard
**IMAP/SMTP** accounts, on-premises **Microsoft Exchange** (through Exchange Web
Services, EWS), and **Novell / Micro Focus GroupWise** mailboxes reached through
a GroupWise proxy.

![Platform](https://img.shields.io/badge/platform-Windows%20%7C%20macOS%20%7C%20Linux-blue)
![Version](https://img.shields.io/badge/version-0.1.0-informational)
![License](https://img.shields.io/badge/license-MIT-green)

## Why HylMail

Most desktop clients handle IMAP well, Exchange grudgingly, and GroupWise not at
all — so a single workplace often ends up with a separate application per
backend. HylMail was built for the opposite situation: one account of each type,
side by side, in a single dark interface that stays responsive on slow corporate
networks and mailboxes with tens of thousands of messages.

It is aimed at the realities of a corporate IT estate: internal certificate
authorities, TLS-inspection proxies, browser-based single sign-on, app
passwords, intermittent connections, and users who have no administrator rights.

## What it does

- **All your mailboxes, one window.** Configure several IMAP, Exchange and
  GroupWise accounts at once; a virtual **"Alle Posteingänge"** inbox shows every
  inbox together, with a folder tree, unread counters and per-account columns.
- **Find anything, fast.** Instant full-text search over the locally cached
  mailbox, a server-side search on IMAP after a short pause, quick filters
  (all / unread / flagged / with attachment / done), and automatic paging.
- **Read safely.** Remote images are blocked until you allow them — for one
  message or for the whole sender ("Bilder laden" / "Immer von diesem
  Absender"). Message HTML is sanitised, and attachments are opened through the
  native file dialog.
- **Nothing gets lost.** When the network is down, mail you send is queued in an
  outbox that retries by itself and survives a restart. Drafts are saved
  automatically, locally and — where the server supports it — server-side.
- **Write the way you want.** Rich text with a generated plain-text alternative,
  or plain text; embedded images; a Bcc that stays invisible to other
  recipients; signatures above or below the quote; recipient autocomplete from
  the address book and from people you have written to before.
- **Stay organised.** A rule engine (match sender, To/Cc, subject, headers,
  attachments or size; act by moving, reading, flagging, deleting or stopping)
  with a preview before you enable it, and rules that also run on arrival.
  Flat or thread view, and a configurable **"Done"** action on the `E` key.
- **Calendar invitations and contacts.** Answer meeting invites (accept /
  tentative / decline) straight from the message, and keep a browsable address
  book that syncs losslessly with a CardDAV server.
- **Runs your way.** A dark theme by default with a light alternative, a German
  interface, portable operation from a USB stick, and a single instance that
  hands a double-clicked `.eml` file to the window you already have open.

## Install

Ready-built installers are produced by the release workflow. **The installers are
currently unsigned**, so Windows SmartScreen and macOS Gatekeeper will warn on
first launch.

### Windows

- **Per user:** `HylMail-0.1.0-setup.exe` (Inno Setup) — installs under
  `%LOCALAPPDATA%\Programs\HylMail`, adds Start-menu and desktop shortcuts, and
  registers the `.eml` file association.
- **Per machine:** `HylMail-0.1.0.msi` (WiX) — installs under
  `%ProgramFiles%\HylMail` with the same shortcuts and association.

Install **one** of the two, not both.

### macOS

Open `HylMail-0.1.0.dmg` and drag **HylMail.app** into *Applications*.

### Linux

- `hylmail_0.1.0-1_amd64.deb` — installs the launchers plus a desktop entry and
  icons; or
- `HylMail-0.1.0-x86_64-unknown-linux-gnu.tar.gz` — unpack and run
  `HylMail-0.1.0/bin/hylmail-gui`.

### Build from source

You need a current Rust toolchain (the workspace requires **Rust 1.88** or
newer) and a C toolchain for the bundled SQLite. On Linux also install the
desktop libraries: `libdbus-1-dev pkg-config libxkbcommon-dev libwayland-dev
libx11-dev libgl1-mesa-dev`.

```bash
cd sys-src
cargo build --workspace --release
./target/release/hylmail-gui        # opens the window
```

To confirm the build works, ask the command-line launcher where it would keep
its files:

```bash
./target/release/hylmail-cli --where
```

## First run

Launch HylMail (an empty command line opens the window) and add your accounts in
the accounts dialog. Domain presets for common providers pre-fill the server
names and ports, and every field can be overridden. The **user interface is
German**.

Once an account is saved it connects and synchronises immediately — no restart
required. Double-clicking a `.eml` file, or passing its path on the command
line, opens it in the running window.

## Keyboard shortcuts

| Key | Action |
|---|---|
| `Ctrl+N` / `Ctrl+R` / `Ctrl+Shift+R` / `Ctrl+U` | new message, reply, reply all, forward |
| `E` | "Done" — archive or mark done, per your setting |
| `Ctrl+E` | save the composer as a draft |
| `Ctrl+F` | focus the search box |
| `Ctrl+Shift+A` / `Ctrl+Comma` | accounts, settings |
| `Ctrl+Alt+R` | the rule editor |
| `F5` / `Ctrl+Y` | synchronise |
| `Delete` | move to trash; on a message already in the trash, delete for good |
| `↑` `↓` `j` `k` | move through the list |
| `Escape` | clear the selection or close the dialog |

Shortcuts are suppressed while a text field has focus, so typing never moves or
deletes a message by accident.

## Where your data lives

| Platform | Configuration | Cache, database, logs |
|---|---|---|
| Windows | `%APPDATA%\HylMail` | `%LOCALAPPDATA%\HylMail` |
| macOS / Linux | `~/.HylMail` | `~/.HylMail` |
| **Portable** (`HYLMAIL_HOME` set) | `$HYLMAIL_HOME` | `$HYLMAIL_HOME` |

Set the `HYLMAIL_HOME` environment variable to keep **every** file HylMail
writes inside a single directory — for example on a USB stick. Nothing is then
written to your home directory.

## Privacy and security

- Passwords and OAuth tokens live in the **operating-system credential vault**
  (Windows Credential Manager, macOS Keychain, or the freedesktop Secret
  Service) — never in a configuration file, and never in a log.
- All connections are **TLS by default**, and trust comes from the operating
  system's trust store, so internal certificate authorities and TLS-inspection
  proxies work without extra configuration.
- Message HTML is treated as hostile input and rendered against an allow-list;
  scripts and other active content never run.
- The log file is written through a redacting formatter that masks passwords,
  tokens and authentication payloads.

## What HylMail does not do

This is deliberate, so you can decide before you invest time:

- **The interface is German only.** No other languages are planned.
- **No calendar view.** You can answer meeting invitations, but there is no
  calendar, no free/busy lookup and no resource booking.
- **No mobile or web client** — desktop only.
- **No end-to-end encryption** of message content between users.
- **No server-side administration** of accounts, mailboxes or distribution
  lists, and no plugin or scripting marketplace.
- **The GroupWise proxy is a reduced experience:** calendar and contacts are
  hidden, rules are switched off, "flagged" and "done" are stored only inside
  HylMail, search covers only the most recent 1000 cached messages, archiving
  moves within existing folders, and mail is sent as the proxied mailbox over
  SMTP rather than through GroupWise itself.
- **Server-side search works on IMAP accounts only.** Exchange and GroupWise
  search the local cache.
- **Remote images are decoded for PNG and JPEG**; other formats are recognised
  but not shown. Inline SVG is rasterised.
- **Messages are exported to `.eml` through a save dialog**, because the user
  interface toolkit offers no cross-application drag-out.
- **The installers are unsigned** (no code signing or notarisation yet).

## When not to use HylMail

- You need a web or mobile client.
- You need a full calendar or group scheduling, not just invitation replies.
- You need an interface in a language other than German.
- You need signed, notarised installers for a managed deployment.
- You need central, server-side administration of many users' mailboxes.

## Project status

HylMail is at **version 0.1.0** — an early first release, and a student project
for the *Advanced Software Engineering / Desktop UI Engineering* course.
Behaviour and configuration formats may still change between minor versions. The
current build is covered by a suite of **1012 automated tests** and is built and
tested on Linux, Windows and macOS in continuous integration.

## Documentation

- **Requirements** (what HylMail must do): [`sys-doc/REQUIREMENTS.md`](sys-doc/REQUIREMENTS.md)
- **Architecture** (how it is built): [`sys-doc/ARCHITECTURE.md`](sys-doc/ARCHITECTURE.md)
- **Developer guide** (build, modules, command line, testing): [`sys-src/README.md`](sys-src/README.md)
- **Packaging** (installers and release mechanics): [`sys-src/packaging/README.md`](sys-src/packaging/README.md)

## License

MIT — see [`LICENSE`](LICENSE). Copyright (c) 2026 hylatwork.
