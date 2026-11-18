# HylMail — Software Requirements

> **Problem-space specification.** This document is authoritative for **what**
> HylMail must do and **why**. It is deliberately implementation- and
> technology-agnostic: it names external protocols and standards because they are
> part of the problem, but it does not prescribe libraries, storage engines,
> internal modules or UI toolkits. The matching **solution-space** contract is
> [`ARCHITECTURE.md`](ARCHITECTURE.md); the two documents are kept traceable to
> each other (see [§7](#7-traceability-to-the-solution-space)) and are updated in
> the same change.

---

## 0. How to read this document

### Requirement identifiers

| Prefix | Meaning |
|---|---|
| `G-` | product goal |
| `FR-` | functional requirement |
| `BR-` | business rule or domain constraint |
| `NFR-` | non-functional requirement |
| `AC-` | end-to-end acceptance criterion |

### Priority (MoSCoW)

| Code | Meaning |
|---|---|
| **M** | must — part of the agreed scope |
| **S** | should — important, may slip one increment |
| **C** | could — desirable |

### Verification method

| Code | Meaning |
|---|---|
| **T** | automated test (unit, integration, protocol mock) |
| **D** | demonstration / manual acceptance on a running build |
| **I** | inspection of artifacts (document, manifest, packaged output) |

A requirement is only "done" when its verification method has been applied on
the platforms named in the requirement.

---

## 1. Purpose, goals and scope

### 1.1 Product summary

HylMail is a cross-platform desktop email client for **macOS, Windows and
Linux**. It unites three classes of mailboxes behind a single native, dark
user interface:

1. standards-based mailboxes reached over **IMAP4rev1** and **SMTP**;
2. on-premises **Microsoft Exchange** mailboxes reached over **Exchange Web
   Services (EWS)**;
3. **Novell / MicroFocus GroupWise** mailboxes reached through a **GroupWise
   proxy** server.

The client targets corporate environments: internal certificate authorities,
TLS-inspection proxies, federated sign-in, app passwords, intermittent
connectivity, and mailboxes large enough that naive list rendering is not
viable.

### 1.2 Goals

| ID | Goal |
|---|---|
| `G-1` | One client for a heterogeneous corporate mail estate (IMAP, Exchange, GroupWise) instead of one client per backend. |
| `G-2` | Correct and lossless handling of mail, calendar invitations and contacts. |
| `G-3` | Privacy and credential safety are the default, not an option. |
| `G-4` | The interface stays responsive with very large mailboxes and slow or intermittent networks. |
| `G-5` | Installable, signed-ready artifacts exist for all three operating systems. |

### 1.3 Audience

* **End user** — a knowledge worker who reads, composes, files and searches mail.
  The entire user-facing interface is **German**.
* **Deployer / administrator** — operates internal CAs, proxies and Exchange or
  GroupWise infrastructure; needs no per-user administration to install the app.

### 1.4 Out of scope

See [§8](#8-out-of-scope). Notably absent: server-side administration, shared
calendars and group scheduling beyond invitations, and mobile clients.

---

## 2. Users and operating context

| Aspect | Constraint |
|---|---|
| Connectivity | Mail servers may be reachable only from the corporate network or VPN; links may be slow or drop. |
| TLS | The organisation may use internal CAs and TLS-inspection proxies; the OS trust store is the source of truth. |
| Authentication | Federated sign-in (browser-based), NTLM, or app passwords may be required. |
| Offline | Reading cached mail and queuing outgoing mail must work without a connection. |
| Privileges | The user may have no administrator rights; the app must run per-user. |
| Portability | The app must be able to keep **all** of its state inside a single directory (for example a USB stick). |
| Mail volume | Individual folders routinely hold tens of thousands of messages; the inbox is updated continuously. |

---

## 3. Functional requirements

### 3.1 Data locations, portability and migration

| ID | Pri | Requirement | Verified by |
|---|---|---|---|
| `FR-DATA-1` | M | On macOS and Linux, configuration and cache live under a single per-user directory `~/.HylMail`. | T, D |
| `FR-DATA-2` | M | On Windows, configuration (accounts, rules, drafts, outbox) lives under `%APPDATA%\HylMail` and the cache, database and logs live under `%LOCALAPPDATA%\HylMail`. | T, D |
| `FR-DATA-3` | M | **Portable mode:** when the environment variable `HYLMAIL_HOME` is set to a non-empty value, *all* state lives inside that one directory and nothing is written elsewhere. | T |
| `FR-DATA-4` | M | **Windows first-start migration:** if a legacy single-directory layout (`%USERPROFILE%\.HylMail`) exists, its contents are migrated into the new split layout **transactionally** — on any error the original layout is left untouched. | T |
| `FR-DATA-5` | S | The migration reports what happened (migrated / skipped, and why) so the first start is diagnosable. | T |

**Acceptance:** a fresh install creates the documented directories; setting
`HYLMAIL_HOME` relocates every file; a deliberately corrupted migration aborts
without data loss.

### 3.2 Single instance and OS integration

| ID | Pri | Requirement | Verified by |
|---|---|---|---|
| `FR-IPC-1` | M | At most one instance runs per user and configuration directory. | T, D |
| `FR-IPC-2` | M | A second launch while an instance is running hands control to the running instance and exits; it never opens a second window. | T, D |
| `FR-IPC-3` | M | Launching the application with a `.eml` file path opens that message in the running instance. | T, D |
| `FR-IPC-4` | S | The hand-off channel carries no credentials and no message content beyond the file path. | I |

### 3.3 Accounts and authentication

| ID | Pri | Requirement | Verified by |
|---|---|---|---|
| `FR-AUTH-1` | M | Several accounts can be configured and used simultaneously, each with its own backend type. | T, D |
| `FR-AUTH-2` | M | Domain presets (for example Gmail, Outlook.com, the university's own service) pre-fill server names, ports and TLS modes; the user can override every field. | T, D |
| `FR-AUTH-3` | M | Spaces inside app passwords are removed automatically before the password is used. | T |
| `FR-AUTH-4` | M | Where the server requires browser-based federated sign-in, the app performs the standard authorisation-code flow with **PKCE** and a loopback redirect, opening the system browser and never collecting the password itself. | T, D |
| `FR-AUTH-5` | M | Where the server requires Windows-integrated authentication, the app authenticates using the signed-in Windows identity. | D |
| `FR-AUTH-6` | M | Credentials and tokens are stored in the operating-system credential vault and are never written to any configuration file or log. | T, I |
| `FR-AUTH-7` | M | The GroupWise proxy sign-in lets the user enter one or more mailboxes shared with them (proxied mailboxes) explicitly. | T, D |
| `FR-AUTH-8` | M | The GroupWise proxy sign-in falls back to a server certificate whose CN matches within the same domain when the configured host name is unreachable. | T |
| `FR-AUTH-9` | M | When a session expires, the app re-authenticates automatically where it can do so silently, and otherwise prompts the user once. | T, D |
| `FR-AUTH-10` | M | When a proxy mailbox cannot be written to (insufficient rights), the user receives a comprehensible German explanation instead of a raw protocol error. | D |
| `FR-AUTH-11` | M | Once an account is configured and saved, the client contacts it and synchronises it without requiring the application to be restarted; its folders and messages then appear in the running window. | T, D |

### 3.4 Folder tree and special folders

| ID | Pri | Requirement | Verified by |
|---|---|---|---|
| `FR-FOLDER-1` | M | Each account shows a hierarchical folder tree with unread counters and vector icons. | D |
| `FR-FOLDER-2` | M | Special folders (Inbox, Sent, Drafts, Trash, Junk, Archive) are detected automatically from the server's `SPECIAL-USE` attributes and from common German and English folder names. | T |
| `FR-FOLDER-3` | M | The automatic special-folder mapping can be overridden manually per account. | T, D |
| `FR-FOLDER-4` | S | Folders that the backend does not expose for a given account are hidden rather than shown as empty. | D |

### 3.5 Unified inbox

| ID | Pri | Requirement | Verified by |
|---|---|---|---|
| `FR-UNIFIED-1` | M | A virtual folder **"Alle Posteingänge"** shows the inboxes of all accounts together. | D |
| `FR-UNIFIED-2` | M | In the unified inbox the message list carries an additional **account** column; the column is absent or redundant inside a single account view. | D |
| `FR-UNIFIED-3` | C | The unified inbox can be selected as the start view. | T, D |

### 3.6 Message list

| ID | Pri | Requirement | Verified by |
|---|---|---|---|
| `FR-LIST-1` | M | The message list shows, at minimum: read/flag/answered/done/invitation status, attachment indicator, sender, subject, date, size and (per `FR-UNIFIED-2`) account. | D |
| `FR-LIST-2` | M | Status is conveyed with the conventional marks: `●` unread, `★` flagged, `↩` answered, `☑` done, `📅` invitation; `📎` marks an attachment. | D |
| `FR-LIST-3` | M | Quick filters exist for **All, Unread, Flagged, With attachment**, plus done/open in the mode selected by `FR-FLAG-2`. | T, D |
| `FR-LIST-4` | M | The list stays responsive while a folder is being synchronised. | D |
| `FR-LIST-5` | M | The list is sorted by date, newest first; sorting by at least one additional column is possible. | D |
| `FR-LIST-6` | S | The list offers a checkbox per message and a header checkbox that ticks every loaded message; the status bar shows how many are selected, and delete, move, flag and the done action then apply to the whole selection. | T, D |

### 3.7 Search, filtering and paging

| ID | Pri | Requirement | Verified by |
|---|---|---|---|
| `FR-SEARCH-1` | M | Typing filters the locally cached messages immediately against a full-text index. | T, D |
| `FR-SEARCH-2` | M | After a 700 ms debounce and at least 3 characters, a **server-side search** runs asynchronously and returns at most 500 hits. | T |
| `FR-SEARCH-3` | M | Pressing **Enter** forces a server-side search immediately, without waiting for the debounce. | T, D |
| `FR-SEARCH-4` | M | For backends that cannot search the server (Exchange and the GroupWise proxy), the search filters only locally cached messages; for the proxy that is at most the most recent 1000 (see `BR-GW-3`). | T |
| `FR-PAGE-1` | M | A folder initially loads the newest 500 messages; scrolling to the end loads the next 500 automatically. | T, D |
| `FR-PAGE-2` | S | The page size is configurable. | T |

### 3.8 Reading pane and remote content

| ID | Pri | Requirement | Verified by |
|---|---|---|---|
| `FR-READ-1` | M | Selecting a message shows its body in a reading pane without a perceptible delay for cached messages. | D |
| `FR-READ-2` | M | Remote images are blocked by default and a banner offers **"Bilder laden"** for this message and **"Immer von diesem Absender"** for the sender. | T, D |
| `FR-READ-3` | M | Allowing images downloads them in parallel with a 30-second timeout and a 15 MB total cap per message. | T |
| `FR-READ-4` | M | Downloaded images are recognised by their magic bytes, not by their declared content type or file name. | T |
| `FR-READ-5` | M | Image data is cached in memory for the session, capped at 100 MB. | T |
| `FR-READ-6` | M | HTML in messages is treated as hostile input and is rendered against an allow-list; scripts and other active content never execute. | T |
| `FR-READ-7` | M | Newsletter-style messages that contain three or more images are rendered in their original colours; simple messages are colour-adapted for the dark theme. | T, D |
| `FR-READ-8` | S | Inline vector graphics are rasterised and shown as pictures. | T, D |
| `FR-READ-9` | M | Attachments are listed and can be saved through the native file dialog. | D |

### 3.9 Calendar invitations

| ID | Pri | Requirement | Verified by |
|---|---|---|---|
| `FR-CAL-1` | M | A message that carries a calendar invitation is shown as an interactive appointment card (subject, time, location, organiser). | D |
| `FR-CAL-2` | M | The card offers **Accept**, **Tentative** and **Decline**, and sends the reply to the organiser as a standard calendar reply. | T, D |
| `FR-CAL-3` | M | Calendar and contact features are hidden for GroupWise proxy accounts (see `BR-GW-4`). | D |

> **Implementation note.** The invitation card offers **Zusagen**, **Mit
> Vorbehalt** and **Absagen** for accounts that carry a calendar and only when
> the user was actually invited. The answer is a `METHOD:REPLY` calendar object
> with the chosen `PARTSTAT`, sent to the organiser as an ordinary submission.

### 3.10 Composer, signatures and drafts

| ID | Pri | Requirement | Verified by |
|---|---|---|---|
| `FR-COMPOSE-1` | M | Composing opens in its own window. | D |
| `FR-COMPOSE-2` | M | The user can compose rich text (with a generated plain-text alternative) or plain text. | T, D |
| `FR-COMPOSE-3` | M | Images can be embedded in the body and shown to the recipient as embedded parts. | T, D |
| `FR-COMPOSE-4` | M | **Bcc** recipients never appear in a header visible to other recipients. | T |
| `FR-COMPOSE-5` | M | Recipient fields autocomplete from the address book and from previously used addresses. | T, D |
| `FR-COMPOSE-6` | M | An account can have several signatures; for a reply the user chooses whether the signature goes above or below the quote. | T, D |
| `FR-COMPOSE-7` | M | Drafts are saved automatically, both locally and (where the backend supports it) as a server-side draft; the user can also save explicitly. | T, D |
| `FR-COMPOSE-8` | M | Replies and forwards quote the original with correct attribution and also carry attachment references where appropriate. | T, D |

### 3.11 Offline outgoing queue

| ID | Pri | Requirement | Verified by |
|---|---|---|---|
| `FR-OUTBOX-1` | M | When the connection is down, sending queues the message locally instead of failing. | T, D |
| `FR-OUTBOX-2` | M | A background worker retries queued messages automatically and delivers them as soon as a connection is available. | T |
| `FR-OUTBOX-3` | M | The outbox is visible to the user, who can inspect, cancel or resend a queued message. | D |
| `FR-OUTBOX-4` | M | Queued messages survive a restart of the application. | T |

> **Implementation note.** A background worker retries the queue every minute, so
> a message queued while offline is delivered as soon as the connection returns;
> the outbox is also visible in the UI, where an entry can be inspected, retried
> or removed, or the queue flushed on demand.

### 3.12 Rule engine

| ID | Pri | Requirement | Verified by |
|---|---|---|---|
| `FR-RULE-1` | M | The user can define rules that run automatically on newly arriving inbox messages, and can also run a rule manually over a folder. | T, D |
| `FR-RULE-2` | M | Conditions cover: sender, To/Cc, subject, any header combination, presence of an attachment, and size greater than a threshold. | T |
| `FR-RULE-3` | M | Operators are: contains / does not contain, equals, starts with, ends with, and regular expression. | T |
| `FR-RULE-4` | M | Multiple conditions combine with **AND** or **OR**. | T |
| `FR-RULE-5` | M | Actions are: move to folder, mark read, flag, delete, and stop processing further rules. | T |
| `FR-RULE-6` | M | Rules are available from a shortcut and can be previewed against existing messages before being enabled. | D |
| `FR-RULE-7` | M | Rules are disabled for GroupWise proxy accounts (`BR-GW-2`). | T, D |

> **Implementation note.** The rule engine is reachable from the running
> application: the settings have a rule editor (ctrl+alt+R), "Regeln jetzt
> anwenden" runs the evaluated plan over the listed messages, and the enabled
> arrival rules run automatically on every newly stored inbox message.

### 3.13 Address book and contact sync

| ID | Pri | Requirement | Verified by |
|---|---|---|---|
| `FR-ADDR-1` | M | The app maintains an address book that can be synchronised with a standard CardDAV server using ETag-based change detection. | T, D |
| `FR-ADDR-2` | M | Synchronisation is **lossless**: properties the app does not understand are preserved verbatim and written back unchanged. | T |
| `FR-ADDR-3` | M | Addresses collected from sent and received mail supplement the synchronised contacts. | T |
| `FR-ADDR-4` | M | The address book is browsable and editable in the UI. | D |
| `FR-ADDR-5` | M | Contact features are hidden for GroupWise proxy accounts (`BR-GW-4`). | D |

> **Implementation note.** The address book is browsable and editable in the UI,
> the composer autocompletes from it and from previously used addresses, sent
> recipients are collected automatically, and a CardDAV collection is
> synchronised with ETag change detection. The raw vCard is preserved verbatim
> for a lossless round trip.

### 3.14 Conversation threading

| ID | Pri | Requirement | Verified by |
|---|---|---|---|
| `FR-THREAD-1` | M | The message list can be switched between flat and conversation view. | D |
| `FR-THREAD-2` | M | Threads are formed from the message's `Message-ID`, `In-Reply-To` and `References` headers, and **never cross an account boundary**. | T |
| `FR-THREAD-3` | S | A conversation expands and collapses in place, with unread counts per thread. | D |

> **Implementation note.** The message list can be switched between the flat view
> and a conversation view; a thread never crosses an account boundary, and a
> thread expands and collapses in place with its unread and total counts.

### 3.15 Message flags and "done" semantics

| ID | Pri | Requirement | Verified by |
|---|---|---|---|
| `FR-FLAG-1` | M | Read/unread, flagged and answered state is changed from the list and the reading pane and is pushed to the server where the backend supports it. | T, D |
| `FR-FLAG-2` | M | A **"Done"** action (`E` key) is available and is configurable between two modes: **move to archive** (default) and **mark as done**. | T, D |
| `FR-FLAG-3` | M | In "mark as done" mode, the backend-appropriate representation is used: an IMAP keyword, the Exchange item status, or a local mark for GroupWise proxy (`BR-GW-1`). | T |
| `FR-FLAG-4` | M | A destructive action (delete) asks for confirmation or is undoable. | D |

### 3.16 Drag and drop, import and export

| ID | Pri | Requirement | Verified by |
|---|---|---|---|
| `FR-DND-1` | M | Messages can be dragged onto a folder to move them. | D |
| `FR-DND-2` | M | A message can be exported as an `.eml` file that any other mail client can reopen. | D |
| `FR-DND-3` | S | A `.eml` file can be opened in the app (see `FR-IPC-3`). | T, D |

> **Implementation note.** A message row is a drag source: dropping it on a
> folder moves it, and a drop that no folder accepts exports the message as an
> `.eml` file through the native file dialog. The GUI toolkit has no
> cross-application drag-out, so exporting through the file dialog is the
> delivered form of "move the message out of the application"; the file is
> written from the bytes the server hands out, so it is a complete message any
> client can reopen.

### 3.17 Crash reporting and diagnostics

| ID | Pri | Requirement | Verified by |
|---|---|---|---|
| `FR-DIAG-1` | M | A command-line entry point exists for diagnostics and for scripting, with a self-describing help output. | T, D |
| `FR-DIAG-2` | M | If the application crashes, a report is written to a file (`absturz.txt`) that is folded into the log on the next start. | T, D |
| `FR-DIAG-3` | M | The application can report its version and configuration locations without starting the UI. | D |

> **Implementation note.** `start_core` installs the panic hook, so a crash
> writes `absturz.txt` and the next start folds it into the log (`FR-DIAG-2`).

---

## 4. Backend capability matrix and domain constraints

All functional areas are not available on every backend. Where a backend cannot
express a feature, the feature is **disabled visibly** and the user is told why —
it is never silently ignored.

### 4.1 IMAP / SMTP (`BR-IMAP-*`)

| ID | Constraint |
|---|---|
| `BR-IMAP-1` | Folder names use modified UTF-7 on the wire; the user sees and edits the decoded name. |
| `BR-IMAP-2` | New mail is detected promptly via server push (`IDLE`) where the server offers it; otherwise polling is used. |
| `BR-IMAP-3` | Moving a message uses a server-side move when supported; otherwise it degrades to copy + delete + expunge, and the user-visible result is identical. |
| `BR-IMAP-4` | Synchronisation is incremental and resumable based on server UIDs; a reconnect must not re-download the whole folder. |
| `BR-IMAP-5` | The IMAP keyword used for "done" (`FR-FLAG-3`) is `$Done`. |

### 4.2 Exchange on-premises (EWS) (`BR-EWS-*`)

| ID | Constraint |
|---|---|
| `BR-EWS-1` | Folders, items, MIME bodies, marks, moves and deletes are exchanged over the EWS protocol. |
| `BR-EWS-2` | Windows-integrated authentication is used where configured (`FR-AUTH-5`). |
| `BR-EWS-3` | The "done" state maps to the Exchange item status. |
| `BR-EWS-4` | Search is served from the local index; the EWS client does not issue a server-side search. |

### 4.3 GroupWise proxy (`BR-GW-*`)

The GroupWise proxy backend exposes a deliberately reduced feature set. These are
**hard domain constraints**, not implementation gaps.

| ID | Constraint |
|---|---|
| `BR-GW-1` | The **"flagged"** and **"done"** states are stored **only locally**; they do not exist on the server and are not visible elsewhere. |
| `BR-GW-2` | The **rule engine is disabled** for proxy accounts. |
| `BR-GW-3` | Search is **local only** and filters at most the most recent 1000 messages. |
| `BR-GW-4` | **Calendar and contacts** are hidden for proxy accounts. |
| `BR-GW-5` | Creating archive folders on the server is **not supported**; archiving means moving within existing folders. |
| `BR-GW-6` | Mail is sent as another mailbox (`sendItem` on behalf of), not as the proxy identity. |
| `BR-GW-7` | The proxy may redirect the session to a different post office in the same domain; the client follows the redirect transparently. |

### 4.4 Fail-visible rule

| ID | Pri | Requirement | Verified by |
|---|---|---|---|
| `BR-FV-1` | M | A capability that a backend does not support is disabled in the UI with an explanation; it is never offered and then failed. | D |

---

## 5. Non-functional requirements

### 5.1 Security and privacy

| ID | Pri | Requirement | Verified by |
|---|---|---|---|
| `NFR-SEC-1` | M | Message HTML is sanitised against a strict allow-list; scripts, event handlers and active content never execute. | T |
| `NFR-SEC-2` | M | Remote content (images, style, fonts) is blocked until the user allows it for a message or sender. | T, D |
| `NFR-SEC-3` | M | Credentials and tokens live only in the OS credential vault and in process memory; no file on disk contains a secret. | T, I |
| `NFR-SEC-4` | M | The log layer redacts passwords, tokens and authentication payloads; unit tests prove the redaction. | T |
| `NFR-SEC-5` | M | All network transport is TLS by default; conclusions about trust come from the operating system trust store so that internal CAs and inspection proxies work. | I, D |
| `NFR-SEC-6` | M | The single-instance channel is bound to the local host only and never carries credentials or message content. | I |
| `NFR-SEC-7` | M | Attachments and embedded parts never overwrite a file outside the user-chosen destination directory; a crafted part name cannot escape it. | T |

### 5.2 Reliability and resilience

| ID | Pri | Requirement | Verified by |
|---|---|---|---|
| `NFR-REL-1` | M | Closing the application is never blocked by a hung network socket; shutdown cancels all background work promptly. | T, D |
| `NFR-REL-2` | M | Transient network failures surface as a retryable state, not as data loss or a crash. | T, D |
| `NFR-REL-3` | M | Local metadata (accounts, settings, rules, drafts, outbox) is written atomically so that a crash cannot leave a half-written document. | T |
| `NFR-REL-4` | M | The local database uses a crash-safe journal mode and a schema-migration mechanism that upgrades an older database in place. | T |
| `NFR-REL-5` | S | A corrupt or unreadable optional document degrades to defaults rather than preventing startup. | T |

### 5.3 Performance and scalability

| ID | Pri | Requirement | Verified by |
|---|---|---|---|
| `NFR-PERF-1` | M | Scrolling a folder of tens of thousands of messages stays smooth; rows are materialised on demand, not all at once. | D |
| `NFR-PERF-2` | M | The UI thread never blocks on network or disk I/O; all such work happens off the UI thread and communicates by message passing. | I, D |
| `NFR-PERF-3` | M | Local search returns first results within the same interaction (no full re-scan of the mailbox). | D |
| `NFR-PERF-4` | M | The content cache is bound by a configurable limit, default **500 MB**, and evicts least-recently-used entries when the limit is reached. | T |
| `NFR-PERF-5` | S | A cold start reaches an interactive window in a few seconds on mid-range hardware. | D |

### 5.4 Usability, localisation and accessibility

| ID | Pri | Requirement | Verified by |
|---|---|---|---|
| `NFR-UX-1` | M | **Every** user-facing string (labels, dialogs, error messages) is German. | I |
| `NFR-UX-2` | M | The application ships a dark theme as the default and primary look, and lets the user switch to a light theme in the settings; the choice persists across restarts. | D |
| `NFR-UX-3` | M | Core actions are reachable by keyboard; the commonly used ones have documented shortcuts (for example `E` for done, `Ctrl+E` to save a draft, `Ctrl+Alt+R` for rules). | D |
| `NFR-UX-4` | S | Errors are phrased in terms of what the user can do, not in terms of the underlying protocol. | D |
| `NFR-UX-5` | S | Primary controls expose accessible names and support the platform's high-contrast affordances. | D |

### 5.5 Portability and deployability

| ID | Pri | Requirement | Verified by |
|---|---|---|---|
| `NFR-PORT-1` | M | The application builds and runs on macOS, Windows and Linux. | I, D |
| `NFR-PORT-2` | M | Linux distributables are a `.tar.gz` archive and a `.deb` package that installs a desktop entry and icons. | I |
| `NFR-PORT-3` | M | The macOS distributable is a `.dmg` whose application bundle identifier is `de.hylmail.HylMail` and which is prepared for the hardened runtime. | I |
| `NFR-PORT-4` | M | The Windows distributables are an Inno Setup `.exe` installer (per user) and a WiX `.msi` installer (per machine); only one of the two may be installed at a time, and each registers the same `.eml` association. | I |
| `NFR-PORT-5` | M | Packaging helper scripts use the line endings their platform expects — LF for the POSIX shell scripts, CRLF for cmd/PowerShell scripts — and are stored as UTF-8; an automated check fails the build when a POSIX script carries Windows line endings. | T |
| `NFR-PORT-6` | M | Continuous integration runs formatting, static analysis and the test suite on all three operating systems. | I |
| `NFR-PORT-7` | M | The product version has a single source of truth and is embedded consistently in every artifact and in the running application. | T, I |

### 5.6 Maintainability and quality assurance

| ID | Pri | Requirement | Verified by |
|---|---|---|---|
| `NFR-MAINT-1` | M | Production code contains no unhandled panics; fallible operations return structured errors. | I, T |
| `NFR-MAINT-2` | M | The build is warning-free at the configured strictness, including static analysis, and CI fails on any warning. | I |
| `NFR-MAINT-3` | M | Behaviour is covered by an automated test suite of the order of 1000 tests, including simulated servers for each protocol, the HTML sanitiser, the rule engine and the migration rollback. | I |
| `NFR-MAINT-4` | M | The architecture document is checked against the source tree, and this requirement document for structural integrity and identifier uniqueness, by an automated guard test. | T |
| `NFR-MAINT-5` | S | New modules are documented in the architecture document, and the guard test fails until they are. | T |

---

## 6. Acceptance criteria

End-to-end scenarios that must pass on a released build. Each maps back to the
requirements above.

| ID | Scenario | Covers |
|---|---|---|
| `AC-1` | **First start (three platforms).** Install, launch, configure one account of each backend type, see the folder tree and the newest 500 messages of the inbox, restart, and find the same state. | `FR-DATA-*`, `FR-AUTH-1/2`, `FR-FOLDER-*`, `FR-PAGE-1` |
| `AC-2` | **Portable run.** Set `HYLMAIL_HOME` to an empty directory, configure an account, send a queued message offline, close and reopen; all state is inside that directory and nothing is written to the home directory. | `FR-DATA-3`, `FR-OUTBOX-4` |
| `AC-3` | **Single instance.** With the app running, double-click a `.eml` file; the message opens in the existing window and no second window appears. | `FR-IPC-1/2/3` |
| `AC-4` | **Hostile message.** Open a message with scripts, tracking pixels and remote images; nothing loads automatically, the banner appears, "Bilder laden" loads only that message's images, and the sanitiser removes the script. | `FR-READ-2/3/6`, `NFR-SEC-1/2` |
| `AC-5` | **Rules end to end.** Create a rule "subject contains X → move to folder Y" in the rule editor and receive a matching message; running "Regeln jetzt anwenden" files it automatically. | `FR-RULE-1/5`, `FR-RULE-6` |
| `AC-6` | **Offline outbox.** Disconnect, send a message, observe it in the outbox, reconnect, and observe delivery without manual action. | `FR-OUTBOX-1/2` |
| `AC-7` | **GroupWise proxy reduction.** Configure a proxy mailbox; calendar, contacts and rules are hidden; "flagged" and "done" work but remain local; search returns only local hits. | `BR-GW-1..7`, `BR-FV-1` |
| `AC-8` | **Lossless contacts.** Add an unknown property to a contact on the server, sync, edit another field, sync again; the unknown property is unchanged. | `FR-ADDR-2` |
| `AC-9` | **Shutdown.** Start synchronising a large folder, then close the window; the process exits promptly without leaving orphaned sockets. | `NFR-REL-1` |
| `AC-10` | **Packages.** Install from the `.deb`, the `.dmg`, the Inno Setup `.exe` and the WiX `.msi`; each registers its desktop icon/entry, its `.eml` association and the expected version. | `NFR-PORT-2/3/4/7` |
| `AC-11` | **Crash report.** Force a panic; on the next start the log contains the crash report. | `FR-DIAG-2` |

---

## 7. Traceability to the solution space

This table is the bridge between the problem space (this document) and the
solution space (`ARCHITECTURE.md`). It is intentionally coarse: it points a
requirement group at the architectural section that realises it.

| Requirement group | Architecture |
|---|---|
| `G-1`, `FR-AUTH-*`, protocols, `BR-IMAP-*`, `BR-EWS-*`, `BR-GW-*` | [§1 System Context](ARCHITECTURE.md#1-level-1--system-context), [§3.1 `hylmail-core`](ARCHITECTURE.md#31-hylmail-core--the-domain-protocol-and-process-components) |
| `FR-DATA-*`, `NFR-REL-3/4`, `NFR-PERF-4` | [§2.2 Where state lives](ARCHITECTURE.md#22-where-state-lives), [§3.1](ARCHITECTURE.md#31-hylmail-core--the-domain-protocol-and-process-components) |
| `FR-IPC-*`, `NFR-SEC-6` | [§2.3 The single-instance hand-off](ARCHITECTURE.md#23-the-single-instance-hand-off) |
| `FR-FOLDER-*`, `FR-LIST-*`, `FR-SEARCH-*`, `FR-UNIFIED-*`, `FR-THREAD-*`, `NFR-PERF-1/3`, `NFR-UX-*` | [§3.2 `hylmail-gui`](ARCHITECTURE.md#32-hylmail-gui--the-ui-components), [§3.4 Interactions](ARCHITECTURE.md#34-interactions-what-happens-on-the-wire) |
| `FR-READ-*`, `FR-CAL-*`, `NFR-SEC-1/2` | [§3.1](ARCHITECTURE.md#31-hylmail-core--the-domain-protocol-and-process-components) MIME/HTML components, [§3.4](ARCHITECTURE.md#34-interactions-what-happens-on-the-wire) |
| `FR-COMPOSE-*`, `FR-OUTBOX-*` | [§3.1](ARCHITECTURE.md#31-hylmail-core--the-domain-protocol-and-process-components) send/outbox components |
| `FR-RULE-*`, `FR-ADDR-*`, `FR-THREAD-2`, `FR-DIAG-2` | [§3.1](ARCHITECTURE.md#31-hylmail-core--the-domain-protocol-and-process-components) `rules`, `carddav`, `db::contacts`, `db::conversations`, `logging`; [§3.2 `hylmail-gui`](ARCHITECTURE.md#32-hylmail-gui--the-ui-components) |
| `NFR-PORT-*` | [§2.4 Deployment (packaging)](ARCHITECTURE.md#24-deployment-packaging) |
| `NFR-SEC-*` | [§1 Trust boundaries](ARCHITECTURE.md#1-level-1--system-context), [§4 Dependency laws](ARCHITECTURE.md#4-dependency-laws) |
| `NFR-MAINT-4/5` | [§8 Guard test](ARCHITECTURE.md#8-guard-test-and-how-to-update-this-document) |

**Known gaps.** None are open at this revision. Every requirement listed in this
document is realised by a component reachable from the running application;
three behaviours are deliberately bounded rather than unimplemented:

* server-side search (`FR-SEARCH-2/3`) is realised for **IMAP** accounts;
  Exchange and the GroupWise proxy keep the local-only behaviour of
  `FR-SEARCH-4`, where the proxy's window is bounded to the most recent 1000
  messages (`BR-GW-3`);
* the remote-image pipeline decodes PNG and JPEG — other raster formats are
  recognised by their magic bytes but not decoded, and inline SVG is rasterised;
* a message is **exported** as an `.eml` file rather than dragged out of the
  window, because the UI toolkit offers no cross-application drag-out
  (`FR-DND-2`).

`ARCHITECTURE.md` [§5](ARCHITECTURE.md#5-dormant-components) is empty.

> **Implementation notes.** Remote images are fetched on demand, in parallel,
> bounded by a 30-second timeout and a 15 MB cap, recognised by magic bytes and
> kept in a 100 MB session cache (`FR-READ-2/3/4/5`); a newsletter (at or above
> `settings.newsletter_image_threshold` images) keeps the colours it declared
> while a simple message is rendered in the palette, i.e. colour-adapted for the
> dark theme (`FR-READ-7`); the background outbox worker retries queued mail
> every minute (`FR-OUTBOX-2`); arrival rules run once per newly stored inbox
> message (`FR-RULE-1`); the "done" key `E` archives or marks done per
> `settings.done_mode`, pushing `$Done` on IMAP, the item status on Exchange and a
> local mark on GroupWise (`FR-FLAG-2/3`); delete asks for confirmation when
> `settings.confirm_delete` is set (`FR-FLAG-4`); the composer edits plain text or
> HTML and can embed images (`FR-COMPOSE-2/3`), saving a local draft on close and
> on `Ctrl+E` (`FR-COMPOSE-7`); the message list carries size and (in the
> aggregate view) account columns plus the answered and done marks
> (`FR-LIST-1/2`, `FR-UNIFIED-2`); special folders can be overridden per account
> in the settings (`FR-FOLDER-3`); and the start view follows
> `settings.show_all_inboxes_on_start` (`FR-UNIFIED-3`).

---

## 8. Out of scope

* Server-side administration of accounts, mailboxes or distribution lists.
* Group scheduling beyond invitation replies: no calendar view, no free/busy,
  no resource booking.
* Mobile clients (iOS, Android) and a web client.
* End-to-end encryption of message content between users.
* A plugin or scripting marketplace.
* Localisation into languages other than German (the interface is German only).

---

## 9. Glossary

| Term | Meaning |
|---|---|
| **EWS** | Exchange Web Services — the SOAP-based protocol of on-premises Microsoft Exchange. |
| **GroupWise proxy** | A mailbox on a Novell/MicroFocus GroupWise post office that is accessed on behalf of another user through a proxy server. |
| **SASL payload** | The encoded credential material exchanged during authentication; sensitive. |
| **Special folder** | A folder whose role (inbox, sent, drafts, trash, junk, archive) is advertised by the server or recognised by name. |
| **Unified inbox** | A virtual folder that aggregates the inboxes of all configured accounts. |
| **Portable mode** | Operation in which every file the app writes lives inside one directory selected by the user (`HYLMAIL_HOME`). |
| **Dormant component** | A component that exists and is tested but is not reachable from the running application (architecture term). |

---

## 10. Change history

| Date | Change |
|---|---|
| Initial | Authored from the original product brief (working name "AgMail"); renamed to HylMail throughout. Problem-space/solution-space boundary established with `ARCHITECTURE.md`. |
| 2026-11-18 | Corrected the Windows artifacts (Inno `.exe` + WiX `.msi`, no `.zip`), the packaging line-ending rule, the test-suite order of magnitude, and the search scope (Exchange added to `FR-SEARCH-4` with `BR-EWS-4`); `NFR-MAINT-4` now reflects the added requirements guard test. |
| 2026-11-18 | Aligned the wording with the delivered product and closed the gap analysis: `FR-DND-2` now states the `.eml` export rather than a cross-application drag, `FR-AUTH-7` the manual entry of a proxied mailbox, and `NFR-SEC-7` the attachment and embedded-part destinations; the `BR-GW-3` 1000-message search window is now enforced by the local query. |
