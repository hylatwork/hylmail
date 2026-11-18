//! Guards `ARCHITECTURE.md` against drifting away from the source tree.
//!
//! The document claims a shape for this repository: which modules exist, which
//! crate may depend on which, how many diagrams the C4 model carries, and which
//! components are deliberately unreachable. A claim nobody re-checks is a claim
//! that quietly becomes false, so this test is the tripwire: it parses the
//! document and the source and fails when the two disagree, pointing at the
//! section that has to change with the code. The companion `REQUIREMENTS.md` is
//! guarded structurally — it has no source symbols to grep for, but its
//! headings, identifiers and traceability links are checked too.
//!
//! It is a tripwire, not a proof. It checks what a machine can check cheaply:
//! top-level module names, the absence of a UI dependency in `hylmail-core`, the
//! well-formedness of the Mermaid fences, the dormant list in section 5 and the
//! container claims in section 2. It cannot tell whether a diagram still means
//! what it draws - that stays a review duty.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::fs;
use std::path::{Path, PathBuf};

/// The workspace root (`sys-src/`): where the crates live.  Project-level
/// documents such as `ARCHITECTURE.md` sit one level above, in `sys-doc/`.
fn repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn read(relative: &str) -> String {
    let path = repo().join(relative);
    fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()))
}

fn architecture() -> String {
    let path = repo().join("../sys-doc/ARCHITECTURE.md");
    fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()))
}

/// A path relative to `crates/`, with forward slashes on every platform.
fn relative_to_crates(path: &Path) -> String {
    path.strip_prefix(repo().join("crates"))
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn rust_files(directory: &Path, out: &mut Vec<PathBuf>) {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            out.push(path);
        }
    }
}

/// Every `src` file of every crate, in a stable order.  The `tests`
/// directories are left out on purpose: a test may exercise a dormant
/// component, that is exactly what makes it dormant instead of dead.
fn source_files() -> Vec<PathBuf> {
    let mut files = Vec::new();
    let crates = repo().join("crates");
    let entries = match fs::read_dir(&crates) {
        Ok(entries) => entries,
        Err(_) => return files,
    };
    for entry in entries.flatten() {
        let src = entry.path().join("src");
        if src.is_dir() {
            rust_files(&src, &mut files);
        }
    }
    files.sort();
    files
}

/// A character that can be part of a Mermaid node id or an identifier.
fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// `true` when the document names `module` *structurally*: as a backticked
/// identifier (a table cell or prose) or as the start of a Mermaid node label.
///
/// A bare word is not enough.  `send` is also a command line flag and a verb in
/// the prose; without this rule the module could be missing from every table and
/// the document would still pass.
fn names_component(document: &str, module: &str) -> bool {
    let backticked = format!("`{module}`");
    let label = format!("\"{module}<br/>");
    let short_label = format!("\"{module}\"]");
    document.contains(&backticked) || document.contains(&label) || document.contains(&short_label)
}

/// The module names a `lib.rs` declares, in source order.
///
/// Only file backed declarations count (`pub mod foo;`): an inline `mod tests {
/// … }` is a test module, not a component of the architecture, and it would
/// otherwise have to appear in the document.
fn declared_modules(relative: &str) -> Vec<String> {
    let text = read(relative);
    let mut names = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with("//") {
            continue;
        }
        let rest = line
            .strip_prefix("pub(crate) ")
            .or_else(|| line.strip_prefix("pub "))
            .unwrap_or(line);
        let Some(rest) = rest.strip_prefix("mod ") else {
            continue;
        };
        let Some(declaration) = rest.strip_suffix(';') else {
            continue;
        };
        let name = declaration.trim();
        if !name.is_empty() && !name.contains(char::is_whitespace) {
            names.push(name.to_owned());
        }
    }
    names
}

/// Every module of every crate has a home in section 3.
#[test]
fn every_module_of_every_crate_is_named_in_the_document() {
    let document = architecture();
    let crates = [
        ("crates/hylmail-core/src/lib.rs", "hylmail-core", 20),
        ("crates/hylmail-gui/src/lib.rs", "hylmail-gui", 8),
        ("crates/hylmail-cli/src/lib.rs", "hylmail-cli", 3),
    ];
    for (lib, name, least) in crates {
        let modules = declared_modules(lib);
        assert!(
            modules.len() >= least,
            "{name} should declare at least {least} modules, found {modules:?} - the parser or the crate layout changed"
        );
        let missing: Vec<&String> = modules
            .iter()
            .filter(|module| !names_component(&document, module))
            .collect();
        assert!(
            missing.is_empty(),
            "ARCHITECTURE.md never names {name} modules {missing:?}; add them to the matching table in section 3, or to a diagram as `name[\"name<br/>…\"]`"
        );
    }
}

/// Law 1 of section 4: the core library knows nothing about any user interface.
#[test]
fn the_core_crate_stays_free_of_any_ui_dependency() {
    let manifest = read("crates/hylmail-core/Cargo.toml");
    for needle in ["egui", "eframe", "egui_extras", "rfd", "hylmail-gui"] {
        assert!(
            !manifest.contains(needle),
            "crates/hylmail-core/Cargo.toml mentions `{needle}`; the core must stay UI free"
        );
    }
    for file in source_files() {
        let relative = relative_to_crates(&file);
        if !relative.starts_with("hylmail-core/") {
            continue;
        }
        let Ok(text) = fs::read_to_string(&file) else {
            continue;
        };
        for needle in [
            "use egui",
            "use eframe",
            "use rfd",
            "egui_extras::",
            "hylmail_gui::",
        ] {
            assert!(
                !text.contains(needle),
                "{relative} uses `{needle}`; the core must stay UI free"
            );
        }
    }
}

/// The diagram kinds the document is allowed to use.
const DIAGRAM_KEYWORDS: [&str; 3] = ["flowchart", "graph", "sequenceDiagram"];

/// The line without its quoted labels, so that a `[Container]` inside a C4 label
/// does not look like a node declaration and a quoted word does not look like
/// an edge label.
fn without_labels(line: &str) -> String {
    let mut out = String::new();
    let mut inside = false;
    for character in line.chars() {
        if character == '"' {
            inside = !inside;
            continue;
        }
        if !inside {
            out.push(character);
        }
    }
    out
}

/// The node ids a flowchart declares, whether on their own line
/// (`worker["..."]`) or inline in an edge (`src --> guiexe["..."]`).
fn declared_node_ids(block: &[String]) -> Vec<String> {
    let mut ids = Vec::new();
    for line in block {
        let bare = without_labels(line);
        for (index, character) in bare.char_indices() {
            if character != '[' && character != '(' {
                continue;
            }
            let head = bare[..index].trim_end();
            let id = head.rsplit(|c: char| !is_word_char(c)).next().unwrap_or("");
            if !id.is_empty() {
                ids.push(id.to_owned());
            }
        }
    }
    ids
}

/// The endpoints of every arrow in a flowchart, as `(from, to)`.
fn flowchart_edges(block: &[String]) -> Vec<(String, String)> {
    let mut edges = Vec::new();
    for line in block {
        let line = line.trim();
        for arrow in ["-.->", "==>", "-->"] {
            let Some((left, right)) = line.split_once(arrow) else {
                continue;
            };
            let from = left
                .trim_end()
                .rsplit(|c: char| !is_word_char(c))
                .next()
                .unwrap_or("");
            // The right side may carry an edge label: `|"Task"| worker`.
            let right = match right.strip_prefix('|') {
                Some(rest) => rest.split_once('|').map_or(rest, |(_, tail)| tail),
                None => right,
            };
            let to = right
                .trim_start()
                .split(|c: char| !is_word_char(c))
                .next()
                .unwrap_or("");
            if !from.is_empty() && !to.is_empty() {
                edges.push((from.to_owned(), to.to_owned()));
            }
            break;
        }
    }
    edges
}

/// Every node of a flowchart must carry a C4 label, and every edge must connect
/// two of those nodes - a renamed label with a stale edge would otherwise make
/// Mermaid invent a second, unlabelled node and the model would silently rot.
fn assert_flowchart(block: &[String], index: usize) {
    let declared = declared_node_ids(block);
    assert!(
        declared.len() >= 3,
        "flowchart {index} declares fewer than three labelled nodes"
    );
    let edges = flowchart_edges(block);
    assert!(
        !edges.is_empty(),
        "flowchart {index} has no edge at all - the parser sees fewer connections than the diagram draws"
    );
    for (from, to) in edges {
        for endpoint in [&from, &to] {
            assert!(
                declared.contains(endpoint),
                "flowchart {index}: `{endpoint}` appears in an edge but carries no `[label]`"
            );
        }
    }
}

/// A sequence diagram may only use the participants it declares.
fn assert_sequence(block: &[String], index: usize) {
    let mut participants: Vec<String> = Vec::new();
    let mut messages = 0usize;
    for line in block {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("participant ") {
            let id = rest.split_whitespace().next().unwrap_or("");
            if !id.is_empty() {
                participants.push(id.to_owned());
            }
        }
        for arrow in ["-->>", "->>"] {
            let Some((left, right)) = line.split_once(arrow) else {
                continue;
            };
            let sender = left
                .trim_end()
                .rsplit(|c: char| !is_word_char(c))
                .next()
                .unwrap_or("");
            let receiver = right
                .trim_start()
                .split(|c: char| !is_word_char(c))
                .next()
                .unwrap_or("");
            for endpoint in [sender, receiver] {
                assert!(
                    participants.iter().any(|p| p == endpoint),
                    "sequence diagram {index}: `{endpoint}` is not declared as a participant"
                );
            }
            messages += 1;
            break;
        }
    }
    assert!(
        participants.len() >= 3,
        "sequence diagram {index} has fewer than three participants"
    );
    assert!(
        messages >= 3,
        "sequence diagram {index} carries fewer than three messages"
    );
}

fn assert_diagram(block: &[String], index: usize) {
    let head = block
        .iter()
        .find(|line| !line.trim().is_empty())
        .expect("a diagram has content")
        .trim();
    assert!(
        DIAGRAM_KEYWORDS
            .iter()
            .any(|keyword| head == *keyword || head.starts_with(&format!("{keyword} "))),
        "diagram {index} starts with `{head}`, which is not a Mermaid diagram keyword"
    );
    assert!(
        block.len() >= 3,
        "diagram {index} is too small to say anything"
    );
    if head.starts_with("sequenceDiagram") {
        assert_sequence(block, index);
        return;
    }
    let subgraphs = block
        .iter()
        .filter(|line| line.trim().starts_with("subgraph "))
        .count();
    let ends = block.iter().filter(|line| line.trim() == "end").count();
    assert_eq!(
        subgraphs, ends,
        "diagram {index} opens {subgraphs} subgraphs but closes {ends}"
    );
    assert_flowchart(block, index);
}

/// Every fence is closed, starts with a known keyword and keeps its invariants.
#[test]
fn the_mermaid_diagrams_are_well_formed() {
    let document = architecture();
    let mut blocks: Vec<Vec<String>> = Vec::new();
    let mut current: Vec<String> = Vec::new();
    let mut inside = false;
    for line in document.lines() {
        if line.trim_end() == "```mermaid" {
            assert!(!inside, "a Mermaid fence is opened twice in a row");
            inside = true;
            current.clear();
            continue;
        }
        if inside && line.trim_end() == "```" {
            inside = false;
            blocks.push(std::mem::take(&mut current));
            continue;
        }
        if inside {
            current.push(line.to_owned());
        }
    }
    assert!(!inside, "a Mermaid fence is never closed");
    assert!(
        blocks.len() >= 11,
        "the document should still carry the whole C4 model, found {} diagrams",
        blocks.len()
    );
    for (index, block) in blocks.iter().enumerate() {
        assert_diagram(block, index + 1);
    }
}

/// The container claims of section 2, checked against the manifests.
#[test]
fn the_container_claims_match_the_manifests() {
    let document = architecture();
    let manifest = read("crates/hylmail-cli/Cargo.toml");
    let mut binaries: Vec<String> = Vec::new();
    let mut in_bin = false;
    for line in manifest.lines() {
        let trimmed = line.trim();
        if trimmed == "[[bin]]" {
            in_bin = true;
            continue;
        }
        if trimmed.starts_with('[') {
            in_bin = false;
            continue;
        }
        if !in_bin {
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("name") {
            if let Some((_, value)) = rest.split_once('=') {
                let value = value.trim().trim_matches('"');
                if !value.is_empty() {
                    binaries.push(value.to_owned());
                }
            }
        }
    }
    assert!(
        binaries.len() >= 2,
        "the launcher crate should declare two binaries, found {binaries:?}"
    );
    for binary in &binaries {
        assert!(
            document.contains(binary.as_str()),
            "ARCHITECTURE.md does not name the binary `{binary}`"
        );
    }
    for needle in [
        "hylmail-core",
        "hylmail-gui",
        "hylmail-cli",
        "hylmail.sqlite",
        "hylmail.lock",
        "ARCHITECTURE.md",
    ] {
        assert!(
            document.contains(needle),
            "ARCHITECTURE.md does not mention `{needle}`"
        );
    }
    for heading in [
        "## 1. Level 1",
        "## 2. Level 2",
        "## 3. Level 3",
        "## 5. Dormant",
        "## 8. Guard test",
    ] {
        assert!(
            document.contains(heading),
            "ARCHITECTURE.md lost the heading `{heading}`"
        );
    }
}

/// The problem-space document, one level above `sys-src/` next to this one.
fn requirements() -> String {
    let path = repo().join("../sys-doc/REQUIREMENTS.md");
    fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()))
}

/// `true` for a requirement identifier such as `FR-DATA-1` or `BR-GW-3`.
///
/// The prefix has to be one of the five defined kinds and the identifier has to
/// end in a digit, which keeps look-alikes such as `MS-NLMP` or `UTF-7` out.
fn is_requirement_id(token: &str) -> bool {
    let Some((prefix, rest)) = token.split_once('-') else {
        return false;
    };
    matches!(prefix, "G" | "FR" | "BR" | "NFR" | "AC")
        && !rest.is_empty()
        && rest
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '-')
        && rest.chars().last().is_some_and(|c| c.is_ascii_digit())
}

/// The identifiers a definition table introduces: the first cell of a table row
/// when that cell is exactly one backticked identifier.
///
/// Reading only the first cell is what separates a definition from the grouped
/// mentions in the traceability table, so every identifier is expected once.
fn requirement_ids(document: &str) -> Vec<String> {
    let mut ids = Vec::new();
    for line in document.lines() {
        let cells: Vec<&str> = line.split('|').collect();
        if cells.len() < 3 {
            continue;
        }
        let cell = cells[1].trim().trim_matches('`').trim();
        if is_requirement_id(cell) {
            ids.push(cell.to_owned());
        }
    }
    ids
}

/// `NFR-MAINT-4`: the requirement document is present, structured and internally
/// consistent, and still links into this architecture document.  The cross-check
/// is structural on purpose — a requirement has no source symbol to grep for.
#[test]
fn the_requirements_document_is_structured_and_consistent() {
    let document = requirements();
    for section in [
        "## 3. Functional requirements",
        "## 4. Backend capability matrix",
        "## 5. Non-functional requirements",
        "## 6. Acceptance criteria",
        "## 7. Traceability to the solution space",
        "## 8. Out of scope",
    ] {
        assert!(
            document.contains(section),
            "REQUIREMENTS.md lost the heading `{section}`"
        );
    }

    let ids = requirement_ids(&document);
    assert!(
        ids.len() >= 80,
        "REQUIREMENTS.md defines only {} requirements; the parser or the document layout changed",
        ids.len()
    );

    let mut seen = std::collections::BTreeSet::new();
    for id in &ids {
        assert!(
            seen.insert(id.clone()),
            "REQUIREMENTS.md defines `{id}` more than once"
        );
    }

    for prefix in ["G", "FR", "BR", "NFR", "AC"] {
        assert!(
            ids.iter()
                .any(|id| id.split_once('-').is_some_and(|(head, _)| head == prefix)),
            "REQUIREMENTS.md defines no `{prefix}-` requirement"
        );
    }

    assert!(
        document.contains("ARCHITECTURE.md#"),
        "REQUIREMENTS.md no longer links into ARCHITECTURE.md from its traceability table"
    );
}
