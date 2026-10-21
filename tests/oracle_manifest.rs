//! The oracle's manifest: the guard that a COUNT cannot be.
//!
//! `tests/common/mod.rs` holds the guard itself, and the four suites that
//! depend on the oracle call it. This file is the guard's own evidence: it
//! proves that the guard CATCHES the three ways the Testing vault can drift —
//! a file MODIFIED, a file ADDED, a file REMOVED — and that the count it
//! replaces could not.
//!
//! ## Why a count was not enough
//!
//! The count said the vault holds nine files. A file whose BYTES changed still
//! counts as one file, so the Obsidian desktop app rewriting `Tickets.base` —
//! which is what running any `obsidian` command does — left the count at nine
//! and the gate green, while the oracle had silently changed what
//! `obsidian base:query` returns. The manifest is a path and a digest per
//! file, so the rewrite is a DIFFERENT digest and the gate says which file
//! changed and how.
//!
//! Nothing here writes to `test/vault`. Every probe builds its vault in a
//! temporary directory, so a test that dies mid-write leaves a file the OS
//! reclaims.

mod common;

use std::path::{Path, PathBuf};

use common::manifest::{
    assert_vault_matches_manifest, file_digest, manifest_drift, manifest_entries, manifest_path,
    read_manifest, vault_files, write_manifest, ManifestEntry,
};
use common::{count_files, vault_dir, CORPUS_SIZE};
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// The oracle itself
// ---------------------------------------------------------------------------

/// The real Testing vault is what the manifest says it is.
///
/// This is the assertion every gate run makes, and the one that fires when the
/// desktop app has rewritten a `.base` file.
#[test]
fn the_testing_vault_matches_its_manifest() {
    assert_vault_matches_manifest();
}

/// The manifest records EVERY file of the oracle, not just the notes.
///
/// A manifest that listed only the `.md` files would leave the `.base` files
/// unchecked, and the `.base` files are the ones Obsidian rewrites.
#[test]
fn the_manifest_records_every_file_of_the_oracle() {
    let entries = manifest_entries();
    assert_eq!(
        entries.len(),
        CORPUS_SIZE,
        "the manifest records every file of the oracle"
    );
    let mut paths: Vec<&str> = entries.iter().map(|entry| entry.path.as_str()).collect();
    paths.sort();
    assert_eq!(
        paths,
        [
            "AllNotes.base",
            "Projects/OtherProject.md",
            "Projects/SomeProject.md",
            "Root Project.md",
            "Root Ticket.md",
            "Tickets.base",
            "Tickets/Add offline mode.md",
            "Tickets/Fix login redirect.md",
            "Tickets/Invoice export.md",
        ]
    );
}

/// The manifest and the oracle's own file list agree, path for path.
///
/// The manifest is only as good as the rule that decides which files are part
/// of the oracle. If the two rules disagreed, a file could be in the vault and
/// not in the manifest, and the drift check would never see it.
#[test]
fn the_manifest_and_the_oracle_agree_on_which_files_count() {
    let mut manifest_paths: Vec<String> = manifest_entries()
        .into_iter()
        .map(|entry| entry.path)
        .collect();
    manifest_paths.sort();
    assert_eq!(manifest_paths, vault_files(&vault_dir()));
}

// ---------------------------------------------------------------------------
// The format
// ---------------------------------------------------------------------------

/// The digest is a SHA-256 of the file's BYTES.
///
/// A known-answer test, so the guard cannot drift into hashing something else
/// — normalised text, say, which would call a CRLF rewrite the same file.
#[test]
fn the_digest_is_a_sha256_of_the_bytes() {
    let dir = TempDir::new().expect("a temp dir");
    let file = dir.path().join("hello");
    std::fs::write(&file, b"hello").expect("a file is writable");
    assert_eq!(
        file_digest(&file),
        "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
    );
}

/// A line is a digest, TWO SPACES, and a path — `sha256sum`'s own format.
///
/// The format is the stock tool's so that `cd test/vault && sha256sum -c
/// ../vault.manifest` checks the oracle without this crate.
#[test]
fn the_manifest_is_in_the_sha256sum_format() {
    let text = std::fs::read_to_string(manifest_path()).expect("the manifest is readable");
    let mut recorded = 0;
    for (number, line) in text.lines().enumerate() {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        recorded += 1;
        let (digest, file) = line
            .split_once("  ")
            .unwrap_or_else(|| panic!("line {} is not a manifest line: {line:?}", number + 1));
        assert_eq!(
            digest.len(),
            64,
            "line {} does not carry a SHA-256",
            number + 1
        );
        assert!(
            digest.chars().all(|c| c.is_ascii_hexdigit()),
            "line {} carries something that is not hex: {digest:?}",
            number + 1
        );
        assert!(!file.is_empty(), "line {} names no file", number + 1);
    }
    assert_eq!(recorded, CORPUS_SIZE, "every file is recorded");
}

/// The manifest is read back as the entries it was written as.
///
/// A round trip: what is written is what is read, so the manifest on disk and
/// the manifest the guard compares against cannot be two different things.
#[test]
fn the_manifest_round_trips() {
    let entries = manifest_entries();
    let dir = TempDir::new().expect("a temp dir");
    let path = dir.path().join("vault.manifest");
    let mut text = String::new();
    for entry in &entries {
        text.push_str(&format!("{}  {}\n", entry.sha256, entry.path));
    }
    std::fs::write(&path, text).expect("a manifest is writable");
    assert_eq!(read_manifest(&path), entries);
}

// ---------------------------------------------------------------------------
// The three ways the vault drifts
// ---------------------------------------------------------------------------

/// A MODIFIED file is caught, and the COUNT it replaces would not have been.
///
/// This is the whole reason the manifest exists: the desktop app rewriting a
/// `.base` file leaves nine files in the vault, so the old guard saw nothing.
#[test]
fn a_modified_file_is_caught_where_a_count_would_not_be() {
    let (dir, vault, manifest) = seeded_vault();

    let target = vault.join("Tickets.base");
    let before = file_digest(&target);
    std::fs::write(&target, b"views: []\n").expect("a sandbox file is writable");
    let after = file_digest(&target);
    assert_ne!(before, after, "the file really did change");

    // The count has not moved: still nine files, exactly as the old guard saw.
    assert_eq!(
        count_files(&vault),
        CORPUS_SIZE,
        "the count cannot see a change in a file's bytes"
    );

    // The manifest can, and it names the offender.
    let drift = manifest_drift(&vault, &manifest);
    assert_eq!(drift.len(), 1, "one file changed: {drift:?}");
    let line = &drift[0];
    assert!(line.starts_with("Tickets.base: "), "{line}");
    assert!(
        line.contains(&before),
        "the expected digest is named: {line}"
    );
    assert!(line.contains(&after), "the digest on disk is named: {line}");
    drop(dir);
}

/// An ADDED file is caught, and named as the addition it is.
#[test]
fn an_added_file_is_caught() {
    let (dir, vault, manifest) = seeded_vault();
    let added = vault.join("Tickets/Stray.md");
    std::fs::write(&added, b"# stray\n").expect("a sandbox file is writable");

    let drift = manifest_drift(&vault, &manifest);
    assert_eq!(drift.len(), 1, "one file was added: {drift:?}");
    assert!(
        drift[0].starts_with("Tickets/Stray.md: not in the manifest"),
        "{}",
        drift[0]
    );
    assert!(
        drift[0].contains(&file_digest(&added)),
        "the digest of the added file is named: {}",
        drift[0]
    );
    drop(dir);
}

/// A REMOVED file is caught, and named as the absence it is.
#[test]
fn a_removed_file_is_caught() {
    let (dir, vault, manifest) = seeded_vault();
    let removed = vault.join("Root Ticket.md");
    let digest = file_digest(&removed);
    std::fs::remove_file(&removed).expect("a sandbox file is removable");

    let drift = manifest_drift(&vault, &manifest);
    assert_eq!(drift.len(), 1, "one file was removed: {drift:?}");
    assert!(
        drift[0].starts_with("Root Ticket.md: expected "),
        "{}",
        drift[0]
    );
    assert!(drift[0].contains(&digest), "the expected digest is named");
    drop(dir);
}

/// An UNCHANGED vault has no drift at all.
///
/// The guard must be silent when there is nothing to say, or a run that always
/// fails is a run nobody reads.
#[test]
fn an_unchanged_vault_has_no_drift() {
    let (dir, vault, manifest) = seeded_vault();
    assert!(manifest_drift(&vault, &manifest).is_empty());
    drop(dir);
}

/// EVERY changed file is named, not just the first.
///
/// A guard that reports one offender per run would take nine runs to say what
/// one run could, and the runs in between would look green.
#[test]
fn every_changed_file_is_named() {
    let (dir, vault, manifest) = seeded_vault();
    std::fs::write(vault.join("Tickets.base"), b"views: []\n").expect("writable");
    std::fs::write(vault.join("Root Ticket.md"), b"# changed\n").expect("writable");
    std::fs::write(vault.join("Tickets/Stray.md"), b"# stray\n").expect("writable");

    let drift = manifest_drift(&vault, &manifest);
    assert_eq!(drift.len(), 3, "three files changed: {drift:?}");
    assert!(drift[0].starts_with("Root Ticket.md: "), "{}", drift[0]);
    assert!(drift[1].starts_with("Tickets.base: "), "{}", drift[1]);
    assert!(
        drift[2].starts_with("Tickets/Stray.md: not in the manifest"),
        "{}",
        drift[2]
    );
    drop(dir);
}

/// Obsidian's own `.obsidian/` state is not part of the oracle.
///
/// It is the desktop app's scratch space, it is gitignored, and it is not what
/// `obsidian base:query` reads — so a manifest that included it would go red
/// every time the app ran.
#[test]
fn a_dot_directory_is_not_part_of_the_oracle() {
    let (dir, vault, manifest) = seeded_vault();
    let obsidian = vault.join(".obsidian");
    std::fs::create_dir_all(&obsidian).expect("a sandbox directory is creatable");
    std::fs::write(obsidian.join("app.json"), b"{}").expect("writable");

    assert!(manifest_drift(&vault, &manifest).is_empty());
    assert_eq!(count_files(&vault), CORPUS_SIZE);
    drop(dir);
}

// ---------------------------------------------------------------------------
// The harness
// ---------------------------------------------------------------------------

/// A copy of the real oracle, with a manifest of its own.
///
/// Built in a temporary directory because the probe MODIFIES files, and
/// `test/vault` is the parity oracle in every worktree: writing to it would
/// change what the oracle answers on every branch that shares it.
fn seeded_vault() -> (TempDir, PathBuf, PathBuf) {
    let dir = TempDir::new().expect("a temp dir");
    let vault = dir.path().join("vault");
    std::fs::create_dir_all(&vault).expect("a sandbox directory is creatable");
    for entry in manifest_entries() {
        let from = vault_dir().join(&entry.path);
        let to = vault.join(&entry.path);
        if let Some(parent) = to.parent() {
            std::fs::create_dir_all(parent).expect("a sandbox directory is creatable");
        }
        std::fs::copy(&from, &to).expect("the oracle is readable");
    }
    let manifest = dir.path().join("vault.manifest");
    write_manifest(&vault, &manifest);
    (dir, vault, manifest)
}

/// The entries a manifest holds, for the assertions above.
#[allow(dead_code)]
fn entries_of(path: &Path) -> Vec<ManifestEntry> {
    read_manifest(path)
}

/// Rewrite the checked-in manifest.
///
/// Deliberately `#[ignore]`d: the manifest changes only on purpose, with the
/// diff in front of whoever reviews it. An ordinary test run never touches it.
#[test]
#[ignore = "rewrites the checked-in manifest; run it deliberately"]
fn regenerate() {
    write_manifest(&vault_dir(), &manifest_path());
}
