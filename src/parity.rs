//! The parity snapshot corpus.
//!
//! `tests/parity.rs` compares against the **live** Obsidian CLI, which is the
//! only way to prove parity and the reason the port exists. It has one
//! dependency that cannot be shipped: a desktop app with the testing vault
//! registered. CI has no Obsidian, so there the live suite compares nothing.
//!
//! This module records what the CLI answered once, stores the answers under
//! `test/parity/data/`, and compares our output against the store afterwards.
//! A corpus turns "parity, if you have Obsidian" into "parity, if someone
//! recorded the oracle for this vault". It does not replace the live suite: a
//! corpus pins the answers of the version that recorded it, and only a live run
//! can say what the installed version answers today.
//!
//! # The corpus is keyed by its results, not by a version
//!
//! The directory name is a digest over every recorded answer, with JSON
//! canonicalised so key order does not matter. Two Obsidian versions that
//! answer identically therefore share one directory, and a version that answers
//! differently gets its own. The CLI version is written into `manifest.json`
//! for a human and is deliberately not part of the key.
//!
//! # Recording is the only step that needs the CLI
//!
//! [`check`] and [`compare`] read the store and never run the CLI. That matters
//! because the CLI is a client to the desktop app: it answers for the vault the
//! app has open, whichever checkout the command was issued from. [`record`]
//! therefore resolves the registered path for `vault=<name>` from Obsidian's
//! own config and refuses unless it is the directory it is enumerating.
//! Otherwise a corpus would pair one vault's bases with another vault's answers
//! and read as parity.
//!
//! Recording can also make the app rewrite the vault: Obsidian normalises
//! `.base` YAML on load. The testing vault is the parity oracle and must stay
//! at nine untouched files, so [`record`] hashes the vault before and after the
//! run and fails, writing nothing, if a byte moved. It cannot prevent the
//! rewrite; it refuses to launder one into a corpus.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::base::QueryOptions;
use crate::error::{BasesError, Result};
use crate::service::{json_rows, Resolver};

/// The on-disk format this binary reads and writes.
///
/// A corpus with a newer schema is refused rather than half-understood: a
/// validator that ignores the fields it does not know reports a corrupt store
/// as valid.
pub const SNAPSHOT_SCHEMA: u32 = 1;

/// Mixed into every digest, so a digest from another tool cannot collide with
/// one of ours by accident.
const DIGEST_TAG: &str = "bases-mcp/parity-snapshot/v1";

// ---------------------------------------------------------------------------
// Talking to the CLI
// ---------------------------------------------------------------------------

/// The Obsidian CLI, addressed by binary and registered vault name.
///
/// The contract this type encodes is hard-won and documented on the live suite:
/// an exit code is not a signal (Obsidian exits 0 with a dead bridge), and empty
/// stdout is not an empty result (it does not flush stdout before exiting). So
/// [`Cli::query_json`] refuses both.
#[derive(Debug, Clone)]
pub struct Cli {
    pub binary: String,
    pub vault: String,
}

impl Cli {
    pub fn new(binary: impl Into<String>, vault: impl Into<String>) -> Self {
        Self {
            binary: binary.into(),
            vault: vault.into(),
        }
    }

    pub fn run(&self, args: &[String]) -> Result<String> {
        let output = Command::new(&self.binary)
            .args(args)
            .output()
            .map_err(|e| BasesError::new(format!("could not run {}: {e}", self.binary)))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stderr = stderr.trim();
            let detail = if stderr.is_empty() {
                String::new()
            } else {
                format!(": {stderr}")
            };
            return Err(BasesError::new(format!(
                "{} exited {}{detail}",
                self.binary, output.status
            )));
        }
        String::from_utf8(output.stdout)
            .map_err(|e| BasesError::new(format!("{} printed invalid UTF-8: {e}", self.binary)))
    }

    fn query_args(&self, base: &str, view: Option<&str>, format: &str) -> Vec<String> {
        let mut args = vec![
            format!("vault={}", self.vault),
            "base:query".into(),
            format!("path={base}"),
            format!("format={format}"),
        ];
        if let Some(view) = view {
            args.push(format!("view={view}"));
        }
        args
    }

    /// Query a base as JSON rows.
    ///
    /// Empty stdout is an error, not an empty row set: Obsidian can return it
    /// while the bridge is still handing the vault over, or without flushing a
    /// heavy query. A recorder that stored that as `[]` would manufacture a
    /// corpus that says "Obsidian returns nothing", which is the exact bug this
    /// project was built to catch.
    pub fn query_json(&self, base: &str, view: Option<&str>) -> Result<Vec<Value>> {
        let out = self.run(&self.query_args(base, view, "json"))?;
        if out.trim().is_empty() {
            return Err(BasesError::new(format!(
                "empty stdout where rows were expected for {base}{}. Obsidian does not flush \
stdout before exiting, so this is a failed query rather than an empty result set — re-run \
with the app settled.",
                view.map(|v| format!(" [{v}]")).unwrap_or_default()
            )));
        }
        let value: Value = serde_json::from_str(&out).map_err(|e| {
            BasesError::new(format!(
                "unparseable JSON from {base}{}: {e}",
                view.map(|v| format!(" [{v}]")).unwrap_or_default()
            ))
        })?;
        match value {
            Value::Array(rows) => Ok(rows),
            other => Err(BasesError::new(format!(
                "expected a JSON array of rows from {base}{}, got {other}",
                view.map(|v| format!(" [{v}]")).unwrap_or_default()
            ))),
        }
    }

    pub fn query_markdown(&self, base: &str, view: Option<&str>) -> Result<String> {
        self.run(&self.query_args(base, view, "md"))
    }

    /// The CLI's own version, when it will say. Informational only: the digest
    /// keys the corpus by results, so a corpus is still valid if this fails.
    pub fn version(&self) -> Option<String> {
        self.run(&["version".to_string()])
            .ok()
            .map(|out| {
                out.lines()
                    .map(str::trim)
                    .find(|line| !line.is_empty())
                    .unwrap_or_default()
                    .to_string()
            })
            .filter(|line| !line.is_empty())
    }
}

// ---------------------------------------------------------------------------
// The recorded shape
// ---------------------------------------------------------------------------

/// One base view, as the CLI answered it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Recorded {
    pub base: String,
    pub view: String,
    pub rows: Vec<Value>,
    pub markdown: String,
}

/// One entry file: the rows, with the base and view they belong to.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EntryFile {
    pub base: String,
    pub view: String,
    pub rows: Vec<Value>,
}

/// What produced a corpus. Informational: the digest is the key.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecordedWith {
    pub cli: String,
    pub version: String,
    pub vault: String,
}

/// One entry, as the manifest lists it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EntryManifest {
    pub base: String,
    pub view: String,
    pub json_file: String,
    pub markdown_file: String,
    pub rows: usize,
    pub json_sha256: String,
    pub markdown_sha256: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    pub schema: u32,
    pub digest: String,
    pub recorded_with: RecordedWith,
    pub entries: Vec<EntryManifest>,
}

/// A corpus that has been read back and validated.
#[derive(Debug, Clone)]
pub struct LoadedCorpus {
    pub dir: PathBuf,
    pub manifest: Manifest,
    pub entries: Vec<Recorded>,
}

/// What [`record`] wrote, or found already written.
#[derive(Debug, Clone)]
pub struct RecordOutcome {
    pub dir: PathBuf,
    pub entries: usize,
    pub already_present: bool,
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

/// Serialise JSON with object keys in sorted order.
///
/// `serde_json` is compiled with `preserve_order`, so its own serialiser keeps
/// whatever order a producer used. Key order is a spelling difference between
/// two implementations that agree on every value, and the digest is about
/// agreement on values, so the corpus key must be insensitive to it.
pub fn canonical_json(value: &Value) -> String {
    fn write(value: &Value, out: &mut String) {
        match value {
            Value::Object(map) => {
                out.push('{');
                let mut keys: Vec<&String> = map.keys().collect();
                keys.sort();
                for (i, key) in keys.into_iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    out.push_str(&Value::String(key.clone()).to_string());
                    out.push(':');
                    write(&map[key], out);
                }
                out.push('}');
            }
            Value::Array(items) => {
                out.push('[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    write(item, out);
                }
                out.push(']');
            }
            other => out.push_str(&other.to_string()),
        }
    }
    let mut out = String::new();
    write(value, &mut out);
    out
}

/// The digest a corpus is keyed by.
///
/// Entries are sorted before they are fed in, so the order the vault enumerated
/// its bases cannot change the key. Each field is length-prefixed, so a base
/// name and a view name cannot run together into the same bytes as a different
/// pair.
pub fn digest_of(entries: &[Recorded]) -> String {
    fn feed(hasher: &mut Sha256, bytes: &[u8]) {
        hasher.update((bytes.len() as u64).to_be_bytes());
        hasher.update(bytes);
    }

    let mut sorted: Vec<&Recorded> = entries.iter().collect();
    sorted.sort_by(|a, b| (&a.base, &a.view).cmp(&(&b.base, &b.view)));

    let mut hasher = Sha256::new();
    hasher.update(DIGEST_TAG.as_bytes());
    for entry in sorted {
        feed(&mut hasher, entry.base.as_bytes());
        feed(&mut hasher, entry.view.as_bytes());
        let rows = Value::Array(entry.rows.clone());
        feed(&mut hasher, canonical_json(&rows).as_bytes());
        feed(&mut hasher, entry.markdown.as_bytes());
    }
    hex::encode(hasher.finalize())
}

// ---------------------------------------------------------------------------
// Recording
// ---------------------------------------------------------------------------

pub struct RecordRequest {
    pub cli: Cli,
    pub vault_dir: PathBuf,
    pub data_dir: PathBuf,
}

/// Record every base view the CLI will answer, and store the answers.
///
/// Every base in the vault is recorded. A base the CLI cannot bind `this` in
/// records an empty answer, which is the recorded form of divergence D1 — and
/// a comparison REQUIRES that answer to stay empty, so the corpus is also the
/// guard against Obsidian learning the construct silently.
///
/// The vault is not written to. It is hashed before and after the run, and a
/// difference aborts the recording: Obsidian normalises `.base` YAML when it
/// loads a vault, and the testing vault is an oracle whose bytes are asserted.
pub async fn record(request: &RecordRequest) -> Result<RecordOutcome> {
    let vault = ensure_cli_vault(&request.cli.vault, &request.vault_dir)?;
    ensure_data_outside_vault(&vault, &request.data_dir)?;

    let before = scan_vault(&vault)?;
    let resolver = Resolver::open_dir(&vault).await?;

    let selected = resolver.list_bases().await?;
    if selected.is_empty() {
        return Err(BasesError::new(
            "the vault has no bases, so there is nothing for the CLI to answer",
        ));
    }
    for base in &selected {
        if base.views.is_empty() {
            return Err(BasesError::new(format!(
                "{} declares no views, so there is nothing for the CLI to answer. A corpus \
that silently skips it would report parity over a base it never recorded.",
                base.path
            )));
        }
    }

    let mut recorded = Vec::new();
    for base in &selected {
        for view in &base.views {
            let rows = request.cli.query_json(&base.path, Some(&view.name))?;
            let markdown = request.cli.query_markdown(&base.path, Some(&view.name))?;
            if !rows.is_empty() && markdown.trim().is_empty() {
                return Err(BasesError::new(format!(
                    "{} [{}] answered {} rows as JSON but no markdown. Obsidian does not flush \
stdout before exiting, so the markdown answer was truncated; re-run rather than recording \
an empty rendering as the oracle.",
                    base.path,
                    view.name,
                    rows.len()
                )));
            }
            recorded.push(Recorded {
                base: base.path.clone(),
                view: view.name.clone(),
                rows,
                markdown,
            });
        }
    }

    let version = request.cli.version().unwrap_or_else(|| "unknown".into());
    let after = scan_vault(&vault)?;
    let changed = before.changed_since(&after);
    if !changed.is_empty() {
        return Err(BasesError::new(format!(
            "the Obsidian CLI CHANGED the vault while recording, so no corpus was written. \
This is what Obsidian does when it normalises `.base` YAML, and the testing vault is an \
oracle whose bytes are asserted. Restore it (git checkout -- {}) and record against a vault \
you are willing to have rewritten. Changed: {}",
            vault.display(),
            changed.join(", ")
        )));
    }

    let with = RecordedWith {
        cli: binary_label(&request.cli.binary),
        version,
        vault: request.cli.vault.clone(),
    };
    let (dir, already_present) = write_corpus(&request.data_dir, &recorded, &with)?;
    Ok(RecordOutcome {
        dir,
        entries: recorded.len(),
        already_present,
    })
}

fn binary_label(binary: &str) -> String {
    Path::new(binary)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(binary)
        .to_string()
}

fn ensure_data_outside_vault(vault: &Path, data: &Path) -> Result<()> {
    // Resolve the directory that WILL be created, before creating it: a guard
    // that mkdirs inside the vault and then refuses has already written there.
    let resolved = canonical_candidate(data)?;
    if resolved.starts_with(vault) {
        return Err(BasesError::new(format!(
            "the corpus directory {} is inside the vault {}. Recording there would write into \
the tree it reads; point --data-dir somewhere outside.",
            resolved.display(),
            vault.display()
        )));
    }
    std::fs::create_dir_all(data).map_err(|e| {
        BasesError::new(format!(
            "cannot create data directory {}: {e}",
            data.display()
        ))
    })?;
    Ok(())
}

/// Canonicalise a path that may not exist yet, by resolving its nearest
/// existing ancestor and appending the missing components.
fn canonical_candidate(path: &Path) -> Result<PathBuf> {
    let mut missing: Vec<std::ffi::OsString> = Vec::new();
    let mut current = path;
    loop {
        match std::fs::canonicalize(current) {
            Ok(mut resolved) => {
                for part in missing.iter().rev() {
                    resolved.push(part);
                }
                return Ok(resolved);
            }
            Err(error) => {
                let Some(name) = current.file_name() else {
                    return Err(BasesError::new(format!(
                        "cannot resolve {}: {error}",
                        path.display()
                    )));
                };
                missing.push(name.to_os_string());
                current = current.parent().ok_or_else(|| {
                    BasesError::new(format!("cannot resolve {}: {error}", path.display()))
                })?;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Obsidian's vault registry, and the tripwire around it
// ---------------------------------------------------------------------------

/// The directory holding `obsidian.json`.
///
/// `OBSIDIAN_CONFIG_DIR` exists because the app's config location is a
/// user-level fact this repo cannot assume: on Linux it follows
/// `XDG_CONFIG_HOME`, and a test can point it at a temp directory.
pub fn obsidian_config_path() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("OBSIDIAN_CONFIG_DIR") {
        return Ok(PathBuf::from(dir).join("obsidian.json"));
    }
    let config_home = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")));
    let linux = config_home.map(|dir| dir.join("obsidian").join("obsidian.json"));
    if let Some(path) = &linux {
        if path.exists() {
            return Ok(path.clone());
        }
    }
    #[cfg(target_os = "macos")]
    if let Some(home) = std::env::var_os("HOME") {
        let mac = PathBuf::from(home)
            .join("Library")
            .join("Application Support")
            .join("obsidian")
            .join("obsidian.json");
        if mac.exists() {
            return Ok(mac);
        }
    }
    match linux {
        Some(path) => Ok(path),
        None => Err(BasesError::new(
            "cannot locate Obsidian's config: neither XDG_CONFIG_HOME nor HOME is set. Set \
OBSIDIAN_CONFIG_DIR to the directory holding obsidian.json.",
        )),
    }
}

/// The path Obsidian has registered under `vault=<name>`.
///
/// The CLI addresses a vault by name, and the name is the directory's basename.
/// A name that matches no registered vault, or matches two, is refused: either
/// case would make "which directory did these answers come from" unanswerable.
pub fn registered_vault_path(name: &str) -> Result<PathBuf> {
    let config = obsidian_config_path()?;
    let bytes = std::fs::read(&config).map_err(|e| {
        BasesError::new(format!(
            "cannot read Obsidian's config at {}: {e}. Set OBSIDIAN_CONFIG_DIR to the \
directory holding obsidian.json.",
            config.display()
        ))
    })?;
    let parsed: Value = serde_json::from_slice(&bytes)
        .map_err(|e| BasesError::new(format!("{} is not valid JSON: {e}", config.display())))?;
    let vaults = parsed
        .get("vaults")
        .and_then(Value::as_object)
        .ok_or_else(|| {
            BasesError::new(format!(
                "{} has no `vaults` object, so no vault is registered",
                config.display()
            ))
        })?;

    let mut matches = Vec::new();
    let mut names = Vec::new();
    for value in vaults.values() {
        let Some(path) = value.get("path").and_then(Value::as_str) else {
            continue;
        };
        let path = PathBuf::from(path);
        if let Some(basename) = path.file_name().and_then(|part| part.to_str()) {
            names.push(basename.to_string());
            if basename == name {
                matches.push(path);
            }
        }
    }
    names.sort();
    names.dedup();

    match matches.len() {
        1 => Ok(matches.remove(0)),
        0 => Err(BasesError::new(format!(
            "no vault named `{name}` is registered in {}. Obsidian's CLI selects a vault by \
name from that file, so there is no directory to record. Registered: {}",
            config.display(),
            if names.is_empty() {
                "none".to_string()
            } else {
                names.join(", ")
            }
        ))),
        _ => {
            let paths: Vec<String> = matches
                .iter()
                .map(|path| path.display().to_string())
                .collect();
            Err(BasesError::new(format!(
                "more than one registered vault is named `{name}`: {}. The CLI's vault= cannot \
say which one it means, so recording refuses.",
                paths.join(", ")
            )))
        }
    }
}

/// Prove that `vault=<name>` addresses `vault_dir`, and return the canonical
/// directory.
///
/// Without this check a recorder run from a worktree whose vault is not the one
/// the app has open would enumerate one directory's bases and store another
/// directory's answers — a corpus that looks like parity and is not.
pub fn ensure_cli_vault(name: &str, vault_dir: &Path) -> Result<PathBuf> {
    let registered = registered_vault_path(name)?;
    let registered = std::fs::canonicalize(&registered).map_err(|e| {
        BasesError::new(format!(
            "vault `{name}` is registered as {}, which cannot be resolved: {e}",
            registered.display()
        ))
    })?;
    let requested = std::fs::canonicalize(vault_dir).map_err(|e| {
        BasesError::new(format!(
            "cannot resolve --vault-dir {}: {e}",
            vault_dir.display()
        ))
    })?;
    if registered != requested {
        return Err(BasesError::new(format!(
            "vault `{name}` is registered as {}, but --vault-dir is {}. The CLI answers for \
the registered vault, so recording here would pair one vault's bases with another vault's \
answers. Run this from the checkout that is open in Obsidian, or open {} in Obsidian.",
            registered.display(),
            requested.display(),
            requested.display()
        )));
    }
    Ok(registered)
}

/// A hash of every file in a vault, keyed by vault-relative path.
///
/// Dot-directories are skipped: `.obsidian/` is the app's own state and changes
/// on every command, so including it would turn the tripwire into noise.
#[derive(Debug, Clone, PartialEq)]
pub struct VaultGuard {
    files: BTreeMap<String, String>,
}

pub fn scan_vault(dir: &Path) -> Result<VaultGuard> {
    fn walk(root: &Path, dir: &Path, files: &mut BTreeMap<String, String>) -> Result<()> {
        let entries = std::fs::read_dir(dir)
            .map_err(|e| BasesError::new(format!("cannot read {}: {e}", dir.display())))?;
        for entry in entries {
            let entry = entry
                .map_err(|e| BasesError::new(format!("cannot read {}: {e}", dir.display())))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            let path = entry.path();
            let kind = entry
                .file_type()
                .map_err(|e| BasesError::new(format!("cannot stat {}: {e}", path.display())))?;
            if kind.is_dir() {
                walk(root, &path, files)?;
            } else if kind.is_file() {
                let bytes = std::fs::read(&path)
                    .map_err(|e| BasesError::new(format!("cannot read {}: {e}", path.display())))?;
                let relative = path
                    .strip_prefix(root)
                    .expect("a walked path is under its root")
                    .to_string_lossy()
                    .replace('\\', "/");
                files.insert(relative, sha256_hex(&bytes));
            }
        }
        Ok(())
    }

    let mut files = BTreeMap::new();
    walk(dir, dir, &mut files)?;
    Ok(VaultGuard { files })
}

impl VaultGuard {
    /// Every path that was added, removed or changed, in a stable order.
    pub fn changed_since(&self, after: &VaultGuard) -> Vec<String> {
        let mut changed = Vec::new();
        for (path, hash) in &self.files {
            match after.files.get(path) {
                Some(other) if other == hash => {}
                Some(_) => changed.push(format!("changed `{path}`")),
                None => changed.push(format!("removed `{path}`")),
            }
        }
        for path in after.files.keys() {
            if !self.files.contains_key(path) {
                changed.push(format!("added `{path}`"));
            }
        }
        changed.sort();
        changed
    }
}

// ---------------------------------------------------------------------------
// The store: writing, loading and checking a corpus
// ---------------------------------------------------------------------------

/// Write a corpus, keyed by the digest of its answers.
///
/// A corpus with this digest that already validates is left alone, so
/// re-recording the same answers is idempotent. One that exists but does not
/// validate is refused, not overwritten: a directory whose name is its content
/// digest and whose content disagrees with that name is evidence of something
/// wrong, and silently repairing it would erase the evidence.
pub fn write_corpus(
    root: &Path,
    entries: &[Recorded],
    with: &RecordedWith,
) -> Result<(PathBuf, bool)> {
    let digest = digest_of(entries);
    let target = root.join(&digest);
    if target.exists() {
        load_corpus(&target).map_err(|error| {
            BasesError::new(format!(
                "a corpus already exists at {} but does not validate ({error}). Inspect it, or \
remove it and record again — it was not overwritten.",
                target.display()
            ))
        })?;
        return Ok((target, true));
    }

    std::fs::create_dir_all(root).map_err(|e| {
        BasesError::new(format!("cannot create corpus root {}: {e}", root.display()))
    })?;
    let temp = root.join(format!(".{digest}.tmp"));
    if temp.exists() {
        std::fs::remove_dir_all(&temp).map_err(|e| {
            BasesError::new(format!(
                "cannot clear a stale temporary corpus {}: {e}",
                temp.display()
            ))
        })?;
    }
    std::fs::create_dir(&temp)
        .map_err(|e| BasesError::new(format!("cannot create {}: {e}", temp.display())))?;

    let written = write_entries(&temp, entries, with, &digest);
    if let Err(error) = written {
        let _ = std::fs::remove_dir_all(&temp);
        return Err(error);
    }
    std::fs::rename(&temp, &target).map_err(|e| {
        BasesError::new(format!(
            "cannot move the recorded corpus from {} to {}: {e}",
            temp.display(),
            target.display()
        ))
    })?;
    Ok((target, false))
}

fn write_entries(
    dir: &Path,
    entries: &[Recorded],
    with: &RecordedWith,
    digest: &str,
) -> Result<()> {
    let mut sorted: Vec<&Recorded> = entries.iter().collect();
    sorted.sort_by(|a, b| (&a.base, &a.view).cmp(&(&b.base, &b.view)));

    let mut manifest_entries = Vec::new();
    let mut used = HashSet::new();
    for entry in &sorted {
        let (json_file, markdown_file) = entry_paths(&entry.base, &entry.view)?;
        for file in [&json_file, &markdown_file] {
            if !used.insert(file.clone()) {
                return Err(BasesError::new(format!(
                    "{} [{}] and another view would both be written to {file}; view names that \
differ only in characters the corpus cannot store are refused rather than overwriting each \
other",
                    entry.base, entry.view
                )));
            }
        }

        let file = EntryFile {
            base: entry.base.clone(),
            view: entry.view.clone(),
            rows: entry.rows.clone(),
        };
        let json = pretty(&file)?;
        write_at(dir, &json_file, json.as_bytes())?;
        write_at(dir, &markdown_file, entry.markdown.as_bytes())?;

        manifest_entries.push(EntryManifest {
            base: entry.base.clone(),
            view: entry.view.clone(),
            json_file: json_file.clone(),
            markdown_file: markdown_file.clone(),
            rows: entry.rows.len(),
            json_sha256: sha256_hex(json.as_bytes()),
            markdown_sha256: sha256_hex(entry.markdown.as_bytes()),
        });
    }

    let manifest = Manifest {
        schema: SNAPSHOT_SCHEMA,
        digest: digest.to_string(),
        recorded_with: with.clone(),
        entries: manifest_entries,
    };
    write_at(dir, "manifest.json", pretty(&manifest)?.as_bytes())
}

fn pretty<T: Serialize>(value: &T) -> Result<String> {
    let mut text = serde_json::to_string_pretty(value)
        .map_err(|e| BasesError::new(format!("cannot serialise the corpus: {e}")))?;
    text.push('\n');
    Ok(text)
}

fn write_at(root: &Path, relative: &str, bytes: &[u8]) -> Result<()> {
    let path = safe_join(root, relative).map_err(BasesError::new)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| BasesError::new(format!("cannot create {}: {e}", parent.display())))?;
    }
    std::fs::write(&path, bytes)
        .map_err(|e| BasesError::new(format!("cannot write {}: {e}", path.display())))
}

/// Where one entry lives inside a corpus.
///
/// The base keeps its directory shape (`Notes/Deep.base` becomes `Notes/Deep/`)
/// so a corpus is navigable by the same path the vault uses. The view name has
/// to become a filename, and the manifest carries the real name, so a view that
/// needs sanitising still round-trips.
fn entry_paths(base: &str, view: &str) -> Result<(String, String)> {
    let stem = base
        .strip_suffix(".base")
        .unwrap_or(base)
        .replace('\\', "/");
    if stem.is_empty()
        || Path::new(&stem)
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err(BasesError::new(format!(
            "base path {base:?} cannot be stored under a corpus: it is empty or escapes the root"
        )));
    }
    let view = sanitise(view);
    Ok((format!("{stem}/{view}.json"), format!("{stem}/{view}.md")))
}

fn sanitise(view: &str) -> String {
    let mut out = String::with_capacity(view.len());
    for ch in view.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_' | ' ') {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() || out == "." || out == ".." || out.starts_with('.') {
        out.insert(0, '_');
    }
    out
}

/// Join a manifest-named file under a corpus root, refusing anything that could
/// leave it. A manifest is data on disk; a validator that follows `../` out of
/// the directory it is validating is not validating that directory.
fn safe_join(root: &Path, relative: &str) -> std::result::Result<PathBuf, String> {
    let path = Path::new(relative);
    if path.is_absolute() {
        return Err(format!("corpus path {relative:?} is absolute"));
    }
    if path
        .components()
        .any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err(format!(
            "corpus path {relative:?} is not a plain relative path"
        ));
    }
    Ok(root.join(path))
}

/// Read and fully validate one corpus directory.
pub fn load_corpus(dir: &Path) -> Result<LoadedCorpus> {
    validate_corpus(dir).map_err(|errors| BasesError::new(errors.join("; ")))
}

/// Validate one corpus directory, collecting every problem rather than stopping
/// at the first: a store with three corrupt files should say so in one run.
pub fn validate_corpus(dir: &Path) -> std::result::Result<LoadedCorpus, Vec<String>> {
    let mut errors = Vec::new();
    let manifest_path = dir.join("manifest.json");
    let manifest: Manifest = match std::fs::read(&manifest_path) {
        Ok(bytes) => match serde_json::from_slice(&bytes) {
            Ok(manifest) => manifest,
            Err(error) => {
                errors.push(format!(
                    "{}: not a valid manifest: {error}",
                    manifest_path.display()
                ));
                return Err(errors);
            }
        },
        Err(error) => {
            errors.push(format!(
                "{}: cannot read the manifest: {error}",
                manifest_path.display()
            ));
            return Err(errors);
        }
    };

    if manifest.schema != SNAPSHOT_SCHEMA {
        errors.push(format!(
            "{}: schema {} is not understood by this binary, which records schema {}",
            manifest_path.display(),
            manifest.schema,
            SNAPSHOT_SCHEMA
        ));
    }
    let name = dir
        .file_name()
        .and_then(|part| part.to_str())
        .unwrap_or_default();
    if manifest.digest != name {
        errors.push(format!(
            "{}: the manifest digest {} does not match the directory name {name}",
            manifest_path.display(),
            manifest.digest
        ));
    }
    if manifest.entries.is_empty() {
        errors.push(format!("{}: records no entries", manifest_path.display()));
    }

    let mut entries = Vec::new();
    let mut allowed = BTreeSet::new();
    allowed.insert("manifest.json".to_string());
    let mut seen_files = HashSet::new();
    let mut seen_views = HashSet::new();

    for listed in &manifest.entries {
        let label = format!("{} [{}]", listed.base, listed.view);
        if !seen_views.insert((listed.base.clone(), listed.view.clone())) {
            errors.push(format!("{label}: listed twice in the manifest"));
        }
        if listed.json_file.is_empty() || listed.markdown_file.is_empty() {
            errors.push(format!("{label}: the manifest names an empty file path"));
            continue;
        }

        let json_path = match safe_join(dir, &listed.json_file) {
            Ok(path) => path,
            Err(error) => {
                errors.push(format!("{label}: {error}"));
                continue;
            }
        };
        let markdown_path = match safe_join(dir, &listed.markdown_file) {
            Ok(path) => path,
            Err(error) => {
                errors.push(format!("{label}: {error}"));
                continue;
            }
        };
        for path in [&listed.json_file, &listed.markdown_file] {
            if !seen_files.insert(path.clone()) {
                errors.push(format!("{label}: {path} is named by more than one entry"));
            }
            allowed.insert(path.clone());
        }

        let json_bytes = match std::fs::read(&json_path) {
            Ok(bytes) => bytes,
            Err(error) => {
                errors.push(format!(
                    "{label}: cannot read {}: {error}",
                    json_path.display()
                ));
                continue;
            }
        };
        if sha256_hex(&json_bytes) != listed.json_sha256 {
            errors.push(format!(
                "{label}: {} does not match the digest the manifest records",
                json_path.display()
            ));
        }
        let entry_file: EntryFile = match serde_json::from_slice(&json_bytes) {
            Ok(entry) => entry,
            Err(error) => {
                errors.push(format!(
                    "{}: not a valid entry: {error}",
                    json_path.display()
                ));
                continue;
            }
        };
        if entry_file.base != listed.base || entry_file.view != listed.view {
            errors.push(format!(
                "{label}: the entry file says {} [{}]",
                entry_file.base, entry_file.view
            ));
        }
        if entry_file.rows.len() != listed.rows {
            errors.push(format!(
                "{label}: the manifest records {} rows, the entry holds {}",
                listed.rows,
                entry_file.rows.len()
            ));
        }

        let markdown_bytes = match std::fs::read(&markdown_path) {
            Ok(bytes) => bytes,
            Err(error) => {
                errors.push(format!(
                    "{label}: cannot read {}: {error}",
                    markdown_path.display()
                ));
                continue;
            }
        };
        if sha256_hex(&markdown_bytes) != listed.markdown_sha256 {
            errors.push(format!(
                "{label}: {} does not match the digest the manifest records",
                markdown_path.display()
            ));
        }
        let markdown = match String::from_utf8(markdown_bytes) {
            Ok(markdown) => markdown,
            Err(error) => {
                errors.push(format!(
                    "{label}: {} is not valid UTF-8: {error}",
                    markdown_path.display()
                ));
                continue;
            }
        };

        entries.push(Recorded {
            base: entry_file.base,
            view: entry_file.view,
            rows: entry_file.rows,
            markdown,
        });
    }

    match walk_files(dir) {
        Ok(files) => {
            for stray in files {
                if !allowed.contains(&stray) {
                    errors.push(format!(
                        "{}: {stray} is not named by the manifest",
                        dir.display()
                    ));
                }
            }
        }
        Err(error) => errors.push(error),
    }

    if errors.is_empty() {
        let recomputed = digest_of(&entries);
        if recomputed != manifest.digest {
            errors.push(format!(
                "{}: the recorded answers digest to {recomputed}, but the corpus is named {}",
                dir.display(),
                manifest.digest
            ));
        }
    }

    if errors.is_empty() {
        Ok(LoadedCorpus {
            dir: dir.to_path_buf(),
            manifest,
            entries,
        })
    } else {
        Err(errors)
    }
}

fn walk_files(root: &Path) -> std::result::Result<Vec<String>, String> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<String>) -> std::result::Result<(), String> {
        let entries = std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        for entry in entries {
            let entry = entry.map_err(|e| format!("{}: {e}", dir.display()))?;
            let path = entry.path();
            let kind = entry
                .file_type()
                .map_err(|e| format!("{}: {e}", path.display()))?;
            if kind.is_dir() {
                walk(root, &path, out)?;
            } else if kind.is_file() {
                out.push(
                    path.strip_prefix(root)
                        .expect("a walked path is under its root")
                        .to_string_lossy()
                        .replace('\\', "/"),
                );
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    walk(root, root, &mut out)?;
    out.sort();
    Ok(out)
}

/// The honest refusal when nothing has been recorded.
fn no_corpus_message(root: &Path) -> String {
    format!(
        "there is no snapshot corpus under {}. A corpus is recorded from a LIVE Obsidian CLI \
(`just record`) with the vault registered in the desktop app; it cannot be recorded in CI or \
synthesized, and a checker that passed without one would be checking nothing.",
        root.display()
    )
}

/// Every corpus under a root, validated.
pub fn check_root(root: &Path) -> std::result::Result<Vec<LoadedCorpus>, Vec<String>> {
    let mut corpora = Vec::new();
    let mut errors = Vec::new();
    let dirs = match corpus_dirs(root) {
        Ok(dirs) => dirs,
        Err(error) => return Err(vec![error]),
    };
    for dir in dirs {
        match validate_corpus(&dir) {
            Ok(corpus) => corpora.push(corpus),
            Err(mut problems) => errors.append(&mut problems),
        }
    }
    if errors.is_empty() {
        Ok(corpora)
    } else {
        Err(errors)
    }
}

fn corpus_dirs(root: &Path) -> std::result::Result<Vec<PathBuf>, String> {
    if !root.exists() {
        return Err(no_corpus_message(root));
    }
    let entries = std::fs::read_dir(root).map_err(|e| format!("{}: {e}", root.display()))?;
    let mut dirs = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| format!("{}: {e}", root.display()))?;
        let path = entry.path();
        let kind = entry
            .file_type()
            .map_err(|e| format!("{}: {e}", path.display()))?;
        if kind.is_dir() {
            dirs.push(path);
        } else {
            return Err(format!(
                "{}: {} is a file at the corpus root; a corpus lives in a directory named by \
its digest, and a stray file here would never be validated",
                path.display(),
                path.display()
            ));
        }
    }
    dirs.sort();
    if dirs.is_empty() {
        return Err(no_corpus_message(root));
    }
    Ok(dirs)
}

/// Pick the one corpus a comparison should read.
///
/// Several corpora mean several recorded answer sets (different Obsidian
/// versions that disagree). Choosing one silently would make "parity" depend on
/// directory sort order, so the caller has to say which.
pub fn sole_corpus(root: &Path) -> Result<PathBuf> {
    let mut dirs = corpus_dirs(root).map_err(BasesError::new)?;
    match dirs.len() {
        1 => Ok(dirs.remove(0)),
        _ => {
            let names: Vec<String> = dirs
                .iter()
                .map(|dir| {
                    dir.file_name()
                        .and_then(|name| name.to_str())
                        .unwrap_or_default()
                        .to_string()
                })
                .collect();
            Err(BasesError::new(format!(
                "{} corpora are recorded under {}: {}. They disagree, so which one is the oracle \
is a decision — pass --corpus <path>.",
                names.len(),
                root.display(),
                names.join(", ")
            )))
        }
    }
}

// ---------------------------------------------------------------------------
// Comparing
// ---------------------------------------------------------------------------

/// What a comparison found.
#[derive(Debug, Default)]
pub struct ComparisonReport {
    pub compared: usize,
    pub skipped: Vec<String>,
    pub failures: Vec<String>,
}

impl ComparisonReport {
    pub fn ok(&self) -> bool {
        self.failures.is_empty()
    }
}

/// Compare our resolver's answers with a recorded corpus.
///
/// This is the same comparison the live suite makes, with the CLI replaced by
/// the store. A `this`-scoped base is skipped and its recorded answer is
/// REQUIRED to be empty: that is divergence D1, and a corpus that suddenly has
/// rows for one is the signal that Obsidian learned to bind `this`.
///
/// Coverage is checked in both directions. A corpus that covers half the vault
/// would otherwise compare half the vault and report green.
pub async fn compare(resolver: &Resolver, corpus: &LoadedCorpus) -> Result<ComparisonReport> {
    let mut report = ComparisonReport::default();

    for entry in &corpus.entries {
        let label = format!("{} [{}]", entry.base, entry.view);
        let options = QueryOptions {
            view: Some(entry.view.clone()),
            context: None,
        };
        match resolver.query(&entry.base, &options).await {
            Ok(result) => {
                let base = resolver.load_base(&entry.base).await?;
                let ours = json_rows(&base, &result, &[]);
                if let Err(error) = compare_rows(&ours, &entry.rows, &label) {
                    report.failures.push(error);
                }
                match resolver.render(&entry.base, &options).await {
                    Ok(theirs) if theirs.trim() == entry.markdown.trim() => {}
                    Ok(theirs) => report.failures.push(first_line_difference(
                        &label,
                        theirs.trim(),
                        entry.markdown.trim(),
                    )),
                    Err(error) => report
                        .failures
                        .push(format!("{label}: rendering our markdown failed: {error}")),
                }
                report.compared += 1;
            }
            Err(error) if error.construct() == Some("this") => {
                if entry.rows.is_empty() {
                    report.skipped.push(format!(
                        "{label}: this-scoped; the CLI returned no rows (documented divergence)"
                    ));
                } else {
                    report.failures.push(format!(
                        "{label}: our resolver needs a Host note for `this`, but the recorded \
answer has {} rows. If Obsidian can bind `this` now, the divergence registry and this \
comparison both need updating.",
                        entry.rows.len()
                    ));
                }
            }
            Err(error) => report
                .failures
                .push(format!("{label}: querying our resolver failed: {error}")),
        }
    }

    let recorded: BTreeSet<(&str, &str)> = corpus
        .entries
        .iter()
        .map(|entry| (entry.base.as_str(), entry.view.as_str()))
        .collect();

    for summary in resolver.list_bases().await? {
        for view in &summary.views {
            if recorded.contains(&(summary.path.as_str(), view.name.as_str())) {
                continue;
            }
            let options = QueryOptions {
                view: Some(view.name.clone()),
                context: None,
            };
            match resolver.query(&summary.path, &options).await {
                Ok(_) => report.failures.push(format!(
                    "{} [{}]: the vault has this view but the corpus does not record it, so the \
corpus is stale — record again with `just record`",
                    summary.path, view.name
                )),
                Err(error) if error.construct() == Some("this") => report.skipped.push(format!(
                    "{} [{}]: this-scoped and not recorded",
                    summary.path, view.name
                )),
                Err(error) => report.failures.push(format!(
                    "{} [{}]: not recorded, and querying it failed: {error}",
                    summary.path, view.name
                )),
            }
        }
    }

    for entry in &corpus.entries {
        if resolver.resolve_base_path(&entry.base).is_err() {
            report.failures.push(format!(
                "{}: the corpus records this base, but the vault no longer has it",
                entry.base
            ));
        }
    }

    Ok(report)
}

/// Compare two JSON row sets, naming the row and column that disagreed.
///
/// A wholesale array comparison would report "these two big things differ",
/// which is not something an agent can act on at 3am. This walks to the exact
/// cell. Shared with the live suite so both comparisons fail the same way.
pub fn compare_rows(
    ours: &[Value],
    theirs: &[Value],
    label: &str,
) -> std::result::Result<(), String> {
    if ours.len() != theirs.len() {
        return Err(format!(
            "{label}: row COUNT differs — ours {}, theirs {}",
            ours.len(),
            theirs.len()
        ));
    }
    for (i, (our, their)) in ours.iter().zip(theirs.iter()).enumerate() {
        let our_map = our
            .as_object()
            .ok_or_else(|| format!("{label} row {i}: ours is not an object: {our}"))?;
        let their_map = their
            .as_object()
            .ok_or_else(|| format!("{label} row {i}: theirs is not an object: {their}"))?;
        if our_map.len() != their_map.len() {
            return Err(format!(
                "{label} row {i}: column COUNT differs — ours {}, theirs {} (ours {:?}, theirs \
{:?})",
                our_map.len(),
                their_map.len(),
                our_map.keys().collect::<Vec<_>>(),
                their_map.keys().collect::<Vec<_>>()
            ));
        }
        for (key, value) in our_map {
            let expected = their_map.get(key).ok_or_else(|| {
                format!(
                    "{label} row {i}: theirs has no column {key:?}; it has {:?}",
                    their_map.keys().collect::<Vec<_>>()
                )
            })?;
            if value != expected {
                return Err(format!(
                    "{label} row {i}, column {key:?}: ours {value}, theirs {expected}"
                ));
            }
        }
    }
    Ok(())
}

/// The first line of two markdown renderings that differs, with its number.
///
/// Printing two tables side by side buries the one differing cell; naming the
/// line is what an agent can act on.
pub fn first_line_difference(label: &str, ours: &str, theirs: &str) -> String {
    let ours: Vec<&str> = ours.lines().collect();
    let theirs: Vec<&str> = theirs.lines().collect();
    for (i, (our, their)) in ours.iter().zip(theirs.iter()).enumerate() {
        if our != their {
            return format!(
                "{label}: markdown differs at line {} — ours {our:?}, theirs {their:?}",
                i + 1
            );
        }
    }
    format!(
        "{label}: markdown differs in length — ours {} lines, theirs {} lines",
        ours.len(),
        theirs.len()
    )
}
