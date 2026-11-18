//! Saving attachments to disk, driven through the public API.
//!
//! The unit tests around `mime::extract_attachments` and
//! `body::save_attachments` cover the individual steps; this suite does what
//! the reading pane does: list the parts of a message, decide file names, and
//! write the bytes.  The interesting case is the hostile one, where the
//! message chooses the file name and the client must not believe it.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};

use hylmail_core::body;
use hylmail_core::html::HtmlPolicy;
use hylmail_core::mime;
use hylmail_core::models::{unique_file_names, MessageId, Settings};
use tempfile::TempDir;

/// Reads a fixture from `tests/fixtures/eml`.
fn fixture(name: &str) -> Vec<u8> {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.push("tests");
    path.push("fixtures");
    path.push("eml");
    path.push(name);
    std::fs::read(&path).unwrap_or_else(|error| panic!("reading {}: {error}", path.display()))
}

fn policy() -> HtmlPolicy {
    HtmlPolicy::from_settings(&Settings::sanitized_default(), None)
}

/// `(part_id, destination)` pairs exactly as `state.rs` builds them.
fn targets(raw: &[u8], directory: &Path) -> Vec<(String, PathBuf)> {
    let attachments = mime::extract_attachments(raw)
        .unwrap()
        .into_iter()
        .map(|payload| payload.attachment)
        .collect::<Vec<_>>();
    attachments
        .iter()
        .zip(unique_file_names(&attachments))
        .map(|(attachment, name)| (attachment.part_id.clone(), directory.join(name)))
        .collect()
}

/// The file names present in a directory, sorted.
fn names_in(directory: &Path) -> Vec<String> {
    let mut names = std::fs::read_dir(directory)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    names.sort();
    names
}

#[test]
fn every_attachment_is_written_with_its_decoded_bytes() {
    let raw = fixture("attachment.eml");
    let payloads = mime::extract_attachments(&raw).unwrap();
    assert_eq!(payloads.len(), 1);
    assert_eq!(payloads[0].suggested_name(), "bericht.txt");

    let directory = TempDir::new().unwrap();
    let written = body::save_attachments(&raw, &targets(&raw, directory.path())).unwrap();

    assert_eq!(written, 1);
    assert_eq!(names_in(directory.path()), ["bericht.txt"]);
    let content = std::fs::read(directory.path().join("bericht.txt")).unwrap();
    assert_eq!(content, payloads[0].bytes);
    assert_eq!(
        String::from_utf8(content).unwrap(),
        "Bericht in einer Ziele\n"
    );
}

#[test]
fn the_listed_part_ids_are_the_ones_the_save_path_finds() {
    // The reading pane lists `MessageBody::attachments`; saving names one of
    // those `part_id`s.  If the two ever diverge, the click fails.
    for name in ["attachment.eml", "traversal.eml", "invite.eml"] {
        let raw = fixture(name);
        let body = mime::parse(&raw, MessageId::new(), &policy()).unwrap();
        let extracted = mime::extract_attachments(&raw).unwrap();
        let saved: Vec<&str> = extracted
            .iter()
            .map(|payload| payload.attachment.part_id.as_str())
            .collect();
        for attachment in &body.body.attachments {
            assert!(
                saved.contains(&attachment.part_id.as_str()),
                "{name}: `{}` is listed but cannot be saved",
                attachment.part_id
            );
        }
        assert_eq!(saved.len(), body.body.attachments.len(), "{name}");
    }
}

#[test]
fn saving_one_part_of_several_leaves_the_others_alone() {
    let raw = fixture("traversal.eml");
    let directory = TempDir::new().unwrap();
    let all = targets(&raw, directory.path());
    assert_eq!(all.len(), 3);

    let written = body::save_attachments(&raw, &all[2..3]).unwrap();

    assert_eq!(written, 1);
    assert_eq!(names_in(directory.path()), ["bericht.txt"]);
}

#[test]
fn a_hostile_file_name_cannot_leave_the_target_directory() {
    let raw = fixture("traversal.eml");
    let outside = TempDir::new().unwrap();
    let directory = outside.path().join("anhaenge");
    std::fs::create_dir(&directory).unwrap();

    let written = body::save_attachments(&raw, &targets(&raw, &directory)).unwrap();

    // Both parts asked to write `../../evil.txt`; they land side by side in the
    // directory the user picked, and the second does not overwrite the first.
    assert_eq!(written, 3);
    assert_eq!(
        names_in(&directory),
        ["bericht.txt", "evil (2).txt", "evil.txt"]
    );
    // Nothing was created next to the directory, and nothing replaced it.
    assert_eq!(names_in(outside.path()), ["anhaenge"]);
    assert_eq!(
        std::fs::read(directory.join("evil.txt")).unwrap(),
        b"EVIL\n"
    );
    assert_eq!(
        std::fs::read(directory.join("evil (2).txt")).unwrap(),
        b"ZEITE\n"
    );
    assert_eq!(
        std::fs::read(directory.join("bericht.txt")).unwrap(),
        b"BASIS\n"
    );
}

#[test]
fn a_part_the_message_does_not_have_is_an_error_not_a_silent_no_op() {
    let raw = fixture("attachment.eml");
    let directory = TempDir::new().unwrap();
    let missing = directory.path().join("nichts.txt");

    let error =
        body::save_attachments(&raw, &[("9.9.9".to_string(), missing.clone())]).unwrap_err();

    assert!(matches!(error, hylmail_core::Error::NotFound { .. }));
    assert!(!missing.exists(), "a failed save must not create a file");
}

#[test]
fn a_message_without_attachments_writes_nothing() {
    let raw = fixture("plain.eml");
    let directory = TempDir::new().unwrap();
    let targets = targets(&raw, directory.path());

    assert!(targets.is_empty());
    assert_eq!(body::save_attachments(&raw, &targets).unwrap(), 0);
    assert!(names_in(directory.path()).is_empty());
}

#[test]
fn an_empty_target_list_never_parses_the_message() {
    // `state.rs` returns early when the reader shows no attachment, so the
    // core must not reject an empty list - even for bytes that are garbage.
    assert_eq!(
        body::save_attachments(b"not a message at all", &[]).unwrap(),
        0
    );
}

#[test]
fn garbage_bytes_are_a_malformed_response() {
    let directory = TempDir::new().unwrap();
    let error = body::save_attachments(
        b"not a message at all",
        &[("1".to_string(), directory.path().join("x"))],
    )
    .unwrap_err();

    assert!(matches!(
        error,
        hylmail_core::Error::MalformedResponse { .. }
    ));
    assert!(!error.to_string().contains("not a message"));
    assert!(names_in(directory.path()).is_empty());
}

#[test]
fn a_file_that_cannot_be_written_keeps_its_path_in_the_error() {
    let raw = fixture("attachment.eml");
    let part_id = mime::extract_attachments(&raw).unwrap()[0]
        .attachment
        .part_id
        .clone();
    let directory = TempDir::new().unwrap();
    // A directory cannot be opened as a file, on every platform.
    let blocked = directory.path().join("kein-ordner");
    std::fs::create_dir(&blocked).unwrap();

    let error = body::save_attachments(&raw, &[(part_id, blocked.clone())]).unwrap_err();

    assert!(
        matches!(error, hylmail_core::Error::Io { .. }),
        "got {error:?}"
    );
    assert!(
        error.to_string().contains("kein-ordner"),
        "the path must survive into the message: {error}"
    );
}
