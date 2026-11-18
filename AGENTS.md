
# AGENTS.md

Guidance for AI coding assistants (running in **PiDeck/PiCove** or Claude Code) working in the **HylMail** repository.

## Project Overview

**HylMail** is a cross-platform desktop email client (macOS, Windows, Linux) written in Rust (Edition 2021+) that unites IMAP, Exchange (on-premises via EWS), and Novell/MicroFocus GroupWise proxy mailboxes into a native UI with a custom dark theme.

Course / Context: Advanced Software Engineering / Desktop UI Engineering  
Requirements Specification: `sys-doc/REQUIREMENTS.md` (authoritative problem-space specification)  
Architecture Contract: `sys-doc/ARCHITECTURE.md` (authoritative C4 solution-space contract)

---

## Repository Structure


```

sys-src/                        # Rust workspace: crates, packaging, CI
├── Cargo.toml                  # Workspace root (single source of truth for version)
├── rustfmt.toml                # newline_style = "Auto" (CRLF tree + LF CI checkout)
├── README.md                   # Build, usage, module tour, test counts, roadmap
├── .github/workflows/          # ci.yml (3-OS fmt/clippy/test), release.yml (packaging)
├── crates/
│   ├── hylmail-core/            # Domain logic, protocols, SQLite, crypto, sanitizer, rules (NO UI deps)
│   ├── hylmail-gui/             # Native desktop UI library (eframe/egui); no binary of its own
│   └── hylmail-cli/             # Both launchers (`hylmail-gui`, `hylmail-cli`) + guard tests
└── packaging/                  # Inno Setup, WiX MSI, macOS bundle, Debian & tarball packaging scripts
sys-doc/                        # Problem- and solution-space documentation (English)
├── REQUIREMENTS.md             # Problem Space: functional & non-functional requirements
└── ARCHITECTURE.md             # Solution Space: C4 architecture model & dependency laws
graphify-out/                   # Local navigable knowledge graph (git-ignored, generated from sys-src/)
.graphifyignore                 # graphify exclusion rules (repo-relative; see "Knowledge Graph")

```

---

## Tech Stack & Architecture

### Core Library (`sys-src/crates/hylmail-core/`)

| Domain | Crate / Technology |
|---|---|
| Async Runtime | `tokio` (Multi-threaded) with typesafe channels (`tokio::sync::mpsc`) |
| IMAP & SMTP | `async-imap` (with UTF-7, `IDLE`, `MOVE`/`UIDPLUS`), `lettre` |
| Exchange (EWS) | Custom SOAP/XML client over `reqwest` + `quick-xml` (hand-rolled NTLM / `MS-NLMP` in `net::ntlm`; `sspi` is deliberately avoided) |
| GroupWise Proxy | Custom SOAP/XML client (port 7191, HTTPS, proxy redirect fallback) |
| CardDAV | HTTP client with `ETag` sync and lossless vCard roundtrip |
| TLS & Trust | `rustls` with `rustls-platform-verifier` (OS truststore) |
| Security & Vault | `keyring` crate + `secrecy::SecretString`; OAuth2 RFC 8252 (PKCE `S256`) |
| Persistence | SQLite (`rusqlite` in WAL mode + FTS5 full-text search) + Atomic JSON (`serde_json`) |
| Parsing & Security | `mail-parser`, `icalendar` (RFC 5545), `ammonia` + `html5ever` XSS sanitizer |
| Logging & Crashes | `tracing` + custom redacting layer (masks passwords, tokens, SASL strings) |

### GUI (`sys-src/crates/hylmail-gui/`)

| | |
|---|---|
| Framework | `eframe` / `egui` (0.33+) with OpenGL backend (`glow`) |
| State & Workers | Dedicated Tokio worker runtime communicating via non-blocking channels |
| Dialogs & Picker | `rfd` (native OS file dialogs) |

---

## Conventions & Standards

### Language

- **Code, identifiers, comments:** English
- **Commit messages & PR titles:** Conventional Commits in English
- **User-facing strings (UI labels, error messages):** **German**
- **Documentation in `sys-doc/`:** English (consistent across `REQUIREMENTS.md` and `ARCHITECTURE.md`)

### Commit Messages — Conventional Commits

Format: `<type>(<scope>): <subject>`

- Subject: imperative mood, lowercase, no trailing period, ≤ 72 chars
- Allowed types: `feat`, `fix`, `refactor`, `test`, `docs`, `style`, `chore`, `ci`, `perf`
- Common scopes: `core`, `gui`, `cli`, `imap`, `ews`, `groupwise`, `carddav`, `sync`, `db`, `ui`, `auth`, `docs`, `ci`

---

## Code Quality & Rust Rules

1. **Zero `unwrap()` or `expect()` in production code.** Use structured error handling with `thiserror` in `hylmail-core` and `anyhow` in `hylmail-gui`/`hylmail-cli`.
2. **Strict warnings & Clippy:** Code must compile cleanly with `cargo clippy --workspace --all-targets -- -D warnings` (CI sets `RUSTFLAGS=-D warnings`).
3. **No Secrets in Logs:** Ensure the redacting logging layer masks passwords, Bearer tokens, and base64 SASL payloads.
4. **No Transaction Across `.await`:** SQLite database connections are shared via `Arc<Database>`; never hold a database lock guard across an `.await` boundary.
5. **Graceful Shutdown:** All background tasks (`IDLE`, sync timers, fetch workers) must listen to a `tokio_util::sync::CancellationToken`.
6. **Formatting & line endings:** Rust sources are kept **CRLF** in the working tree (course convention), and `sys-src/rustfmt.toml` uses `newline_style = "Auto"`, which preserves the file's existing newline style - so `cargo fmt --all --check` stays green both on the CRLF working tree here and on the LF checkout in the Linux CI job. Do not switch it to `"Unix"` or `"Windows"`: each breaks one of the two.

---

## Documentation Maintenance & Boundaries

After every significant feature realization or structural change, both `sys-doc/REQUIREMENTS.md` and `sys-doc/ARCHITECTURE.md` must be kept up to date in the same commit/PR.

### Strict Separation of Problem Space and Solution Space

Maintain a strict separation between what the system needs to do and how it is built:

- **`sys-doc/REQUIREMENTS.md` (Problem Space):**
  - Describes **what** the system must achieve and **why**.
  - Contains functional and non-functional requirements, business rules, domain constraints (e.g., protocol limitations like GroupWise local-only flags), and acceptance criteria.
  - **Must remain implementation- and technology-agnostic.** Do not reference Rust crates, SQLite schemas, UI widgets, or internal modules here.
- **`sys-doc/ARCHITECTURE.md` (Solution Space):**
  - Describes **how** the system achieves those requirements.
  - Contains C4 architectural diagrams (Context, Container, Component), architectural decisions, concrete technologies, integrations, data models, dependency laws, and dormant component tracking.
- **Rule:** Never mix the two. Do not prescribe technical solutions in `REQUIREMENTS.md`, and do not introduce business requirements implicitly through `ARCHITECTURE.md`. Keep both documents consistent and traceable to one another.

### Architecture Guard Tests

HylMail enforces architectural integrity via automated tests. Run the guard tests from `sys-src/` before every push:

```bash
cd sys-src && cargo test -p hylmail-cli --test architecture && cargo test -p hylmail-cli --test packaging

```

The `architecture` test verifies that crate boundaries (`hylmail-core` has zero UI dependencies), module declarations, container definitions and dormant component states stay synchronized with `sys-doc/ARCHITECTURE.md`, and that `sys-doc/REQUIREMENTS.md` keeps its headings, its unique identifiers and its traceability links. The `packaging` test guards the installer templates, the single-source version and the release workflow the artifacts are built from.

---

## Knowledge Graph (`graphify`)

A local knowledge graph of the Rust codebase is generated into `graphify-out/` (`graph.json`, `GRAPH_REPORT.md`, `graph.html`) as a fast navigation aid. `graphify-out/` is git-ignored and must never be committed.

* **Strict Scoping:** Run `graphify` scoped **specifically to `sys-src/**`**. The committed `.graphifyignore` (at the repository root) excludes `sys-src/target/` and `sys-src/packaging/`, so the graph stays compact and fast. `GRAPHIFY_OUT` must be an **absolute** path to the git-ignored `graphify-out/` at the repository root - a relative value would make graphify write to `sys-src/graphify-out/` instead:
```bash
# bash/zsh, from the repository root
GRAPHIFY_OUT="$PWD/graphify-out" graphify update sys-src

# Windows cmd
set "GRAPHIFY_OUT=%CD%\graphify-out" && graphify update sys-src
```

(Or via skill: `/graphify sys-src --update`; the exclusions come from `.graphifyignore`, not from `--exclude` flags - the installed graphify version rejects unknown `--exclude` options.)
* **When to Re-Index:** Re-index **only after structural refactors** (adding, removing, renaming, or re-wiring modules, crates, files, public structs/traits, or cross-module call boundaries). Do **not** re-run `graphify` for routine bug fixes, test additions, or purely internal function/behaviour changes.


* **Querying:** When `graphify-out/graph.json` exists, prefer `graphify query "<question>"`, `graphify path "<A>" "<B>"`, or `graphify explain "<concept>"` for targeted architectural exploration before broad raw file grepping.

* **Curated labels:** the community names are labelled by hand and live in `graphify-out/.graphify_labels.json` (with its `.sig` and `.pending` sidecars) - the one part of the graph that is not regenerable. `_cpn_makeClean.cmd` therefore never wipes `graphify-out/`; `_cpn_makeClean.cmd graphify` drops only the regenerable renders, reports, exports and cache while keeping `graph.json` and the labels.

---

## Testing — Test-Driven Development (TDD)

* **Mandatory TDD:** Write a failing test first (Red → Green → Refactor).


* **Test Suite:** Includes unit tests and TCP mock servers (`tokio::net::TcpListener`) for IMAP, GroupWise SOAP, CardDAV, and OAuth2.


* **Run before push (from `sys-src/`):**
```bash
cargo fmt --all --check
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo test -p hylmail-cli --test architecture
cargo test -p hylmail-cli --test packaging

```

* **Where the numbers live:** the workspace test count in `sys-src/README.md` is the reference. CI (`sys-src/.github/workflows/ci.yml`) runs `rustfmt` on Linux only, and Clippy + the full test suite on Linux, Windows and macOS.

---

## Workflow Quick Reference (PiDeck / PiCove Integration)

* **Workspace Root:** PiDeck/PiCove manages workspace state and isolated agent processes. All `cargo` commands must be executed inside `sys-src/` where the workspace `Cargo.toml` resides.


* **Local Build & Dev:**
```bash
cd sys-src
cargo build
cargo run -p hylmail-cli          # `default-run` is the `hylmail-gui` launcher

```


* **CLI Diagnostics & Scripting:**
```bash
cd sys-src
cargo run -p hylmail-cli -- --help

```

---

## Notes for AI Assistants in PiDeck / PiCove

* **Respect Architectural Laws:**
1. `hylmail-core` must **never** depend on `hylmail-gui`, `egui`, `eframe`, or `rfd`.


2. The UI thread never blocks on I/O; all network/disk operations run via asynchronous tasks communicating through `Task` / `UiEvent` channels.


3. Single writer patterns: `store` writes JSON, `db` writes SQLite, `secrets` manages the OS keyring.




* **Keep `sys-doc/` synchronized:** Whenever a significant feature is realized, update `sys-doc/REQUIREMENTS.md` (if business rules, user capabilities, or constraints changed) and `sys-doc/ARCHITECTURE.md` (if modules, boundaries, or flows changed) while strictly respecting the problem-space vs. solution-space boundary.
* **Before every git push:** Run `cargo fmt --all --check`, `cargo test --workspace`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test -p hylmail-cli --test architecture` and `cargo test -p hylmail-cli --test packaging` inside `sys-src/`.
