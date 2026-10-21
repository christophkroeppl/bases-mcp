//! The oracle's manifest: every file in the Testing vault, with its SHA-256.
//!
//! A COUNT says how many files the Testing vault holds. It cannot say which
//! ones, or whether their bytes are the ones the oracle was built from — so a
//! file that was MODIFIED (the Obsidian desktop app rewriting a `.base` file,
//! a probe overwriting a note) leaves the count at nine and the gate green,
//! while the oracle has silently changed what `obsidian base:query` returns.
//!
//! The manifest is the record a count cannot be: a path and a SHA-256 per
//! file, checked in beside the vault, compared on every gate run. A failure
//! names the offender — path, expected digest, actual digest — so it says
//! WHICH file changed and HOW, rather than that the vault is different.
//!
//! ## The format
//!
//! `sha256sum`'s, so the stock tool can check it without this crate:
//!
//! ```text
//! cd test/vault && sha256sum -c ../vault.manifest
//! ```
//!
//! A line is a SHA-256, TWO SPACES, and the path relative to the vault root.
//! Lines starting with `#` are comments, which is what `sha256sum -c` ignores
//! too.
//!
//! ## Bytes, not text
//!
//! The digest is of the file's BYTES. `bases_mcp::vault::content_hash` hashes
//! NORMALISED text, which is right for a note's identity and wrong here: a
//! hash that folded CRLF would call a rewritten file the same file, and the
//! rewriting is exactly what this guard exists to catch.

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use super::vault_dir;

/// The checked-in manifest of the Testing vault.
///
/// Beside the vault rather than inside it: a file inside the vault would be a
/// tenth file, and the oracle is exactly the set of files the manifest
/// records.
pub fn manifest_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("test")
        .join("vault.manifest")
}

/// One file, as the manifest records it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestEntry {
    /// The path relative to the vault root, `/`-separated.
    pub path: String,
    /// The SHA-256 of the file's bytes, in lowercase hex.
    pub sha256: String,
}

/// The manifest, as it is checked in.
pub fn manifest_entries() -> Vec<ManifestEntry> {
    read_manifest(&manifest_path())
}

/// Read a manifest file.
///
/// Loud about every way it can be wrong, because a guard that fails OPEN is
/// worse than no guard at all: a manifest that cannot be read, or a line that
/// is not a digest and a path, must stop the run rather than leave the vault
/// unchecked.
pub fn read_manifest(path: &Path) -> Vec<ManifestEntry> {
    let text = std::fs::read_to_string(path).unwrap_or_else(|error| {
        panic!(
            "the oracle manifest {} is unreadable: {error}\n\
             Without it nothing can tell a changed vault from an unchanged one.",
            path.display()
        )
    });
    let mut entries = Vec::new();
    for (number, line) in text.lines().enumerate() {
        let line = line.trim_end();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (digest, file) = line.split_once("  ").unwrap_or_else(|| {
            panic!(
                "{}:{} is not a manifest line: {line:?}\n\
                 A line is a SHA-256, two spaces, and a path relative to the vault.",
                path.display(),
                number + 1
            )
        });
        assert!(
            digest.len() == 64 && digest.chars().all(|c| c.is_ascii_hexdigit()),
            "{}:{} does not begin with a SHA-256: {digest:?}",
            path.display(),
            number + 1
        );
        assert!(
            !file.is_empty(),
            "{}:{} names no file",
            path.display(),
            number + 1
        );
        entries.push(ManifestEntry {
            path: file.to_string(),
            sha256: digest.to_string(),
        });
    }
    entries
}

/// Every file in a vault, relative to it, `/`-separated, sorted.
///
/// The rule is the count's rule: a dot-prefixed entry is not part of the
/// oracle. That is what keeps Obsidian's own `.obsidian/` state out of the
/// comparison — it is the desktop app's scratch space, it is gitignored, and
/// it is not what `obsidian base:query` reads. Every OTHER file counts,
/// whatever its extension, so a stray `README.txt` is still a change.
pub fn vault_files(dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    collect_files(dir, dir, &mut out);
    out.sort();
    out
}

fn collect_files(root: &Path, dir: &Path, out: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        let path = entry.path();
        if path.is_dir() {
            collect_files(root, &path, out);
        } else if path.is_file() {
            let relative: Vec<String> = path
                .strip_prefix(root)
                .expect("a path under the root")
                .components()
                .map(|component| component.as_os_str().to_string_lossy().into_owned())
                .collect();
            out.push(relative.join("/"));
        }
    }
}

/// The SHA-256 of a file's BYTES, in the lowercase hex `sha256sum` prints.
pub fn file_digest(path: &Path) -> String {
    let bytes = std::fs::read(path)
        .unwrap_or_else(|error| panic!("{} is unreadable: {error}", path.display()));
    hex::encode(Sha256::digest(&bytes))
}

/// How a vault differs from its manifest: one line per offender.
///
/// Each line names the path, the digest the manifest expects and the digest on
/// disk, so a failure says which file changed and how. An empty result means
/// the vault IS the manifest.
pub fn manifest_drift(vault: &Path, manifest: &Path) -> Vec<String> {
    let expected = read_manifest(manifest);
    let mut drift = Vec::new();

    for entry in &expected {
        let path = vault.join(&entry.path);
        if !path.exists() {
            drift.push(format!(
                "{}: expected {}, found no file",
                entry.path, entry.sha256
            ));
            continue;
        }
        let found = file_digest(&path);
        if found != entry.sha256 {
            drift.push(format!(
                "{}: expected {}, found {}",
                entry.path, entry.sha256, found
            ));
        }
    }

    for path in vault_files(vault) {
        if !expected.iter().any(|entry| entry.path == path) {
            let found = file_digest(&vault.join(&path));
            drift.push(format!("{path}: not in the manifest, found {found}"));
        }
    }

    drift.sort();
    drift
}

/// The guard: assert the Testing vault is exactly what the manifest records.
///
/// It fails on a file that was MODIFIED, one that was ADDED and one that was
/// REMOVED, and it names which. This is the check a count cannot be: the
/// desktop app rewriting `Tickets.base` leaves nine files in the vault and
/// changes what the oracle answers.
pub fn assert_vault_matches_manifest() {
    let manifest = manifest_path();
    let drift = manifest_drift(&vault_dir(), &manifest);
    assert!(
        drift.is_empty(),
        "test/vault does not match {}:\n  {}\n\
         The Testing vault is the parity oracle: every file and its bytes are \
         recorded, so a changed file names itself.",
        manifest.display(),
        drift.join("\n  ")
    );
}

/// Write the manifest for a vault.
///
/// The only way the checked-in manifest should ever change: deliberately, with
/// the diff in front of whoever reviews it.
pub fn write_manifest(vault: &Path, manifest: &Path) {
    let mut text = String::from(
        "# The Testing vault's manifest: every file in it, with its SHA-256.\n\
         #\n\
         # The vault is the parity oracle, and this is what says it is still the\n\
         # oracle. A COUNT cannot see a file whose bytes changed, and a changed\n\
         # .base file changes what `obsidian base:query` returns.\n\
         #\n\
         # Regenerate it with:\n\
         #   cargo test --test oracle_manifest -- --ignored regenerate\n\
         #\n\
         # A line is a SHA-256, two spaces, and the path relative to test/vault.\n\
         # `cd test/vault && sha256sum -c ../vault.manifest` checks it.\n",
    );
    for path in vault_files(vault) {
        let digest = file_digest(&vault.join(&path));
        text.push_str(&format!("{digest}  {path}\n"));
    }
    if let Some(parent) = manifest.parent() {
        std::fs::create_dir_all(parent).expect("the manifest's directory is writable");
    }
    std::fs::write(manifest, text).expect("the manifest is writable");
}
