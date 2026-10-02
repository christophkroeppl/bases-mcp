//! Local filesystem vault source.
//!
//! Reads are served from a snapshot so that a single query sees a consistent
//! view: `file.backlinks` and `file.links` would otherwise observe the vault
//! mid-write. [`FsVaultSource::refresh`] is explicit rather than watching,
//! because the filesystem backend is a development and test path and
//! deterministic behaviour matters more than freshness here.
//!
//! The blocking calls are `tokio::fs`, which is `spawn_blocking` under a
//! hood — the right amount of ceremony for a stdio server whose vault lives on
//! a local disk and whose latency budget is milliseconds.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use chrono::{DateTime, Local};
use sha2::{Digest, Sha256};

use crate::error::{BasesError, Result};
use crate::vault::source::{is_indexable, FileStat, SourceKind, VaultSource};

/// The content hash every backend must compute identically.
///
/// Truncated to 32 hex characters (128 bits): long enough that a collision is
/// not a thing a vault will ever meet, and short enough to read in a log line.
/// Deliberately NOT a server ETag, which a WebDAV server is free to invent.
///
/// It lives here rather than beside the trait because it is a property of the
/// CONTENT, not of a backend: read-verify-write only ever compares two hashes
/// from one backend, so nothing upstream would notice a second backend hashing
/// differently — until two clients edited the same note through different
/// backends and the conflict check silently stopped firing.
/// `tests/vault_equivalence.rs` compares hashes across backends, and that
/// comparison is only meaningful while there is one definition of the function.
pub fn content_hash(text: &str) -> String {
    let digest = Sha256::digest(text.as_bytes());
    hex::encode(digest)[..32].to_string()
}

pub struct FsVaultSource {
    root: PathBuf,
    files: RefCell<Option<Vec<String>>>,
    text_cache: RefCell<HashMap<String, String>>,
    hash_cache: RefCell<HashMap<String, String>>,
}

impl std::fmt::Debug for FsVaultSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FsVaultSource")
            .field("root", &self.root)
            .finish()
    }
}

impl FsVaultSource {
    /// `root` is resolved to an absolute path, so every later comparison against
    /// it is a comparison between two absolute paths.
    ///
    /// A root of `/` is refused. The escape guard below compares path COMPONENTS,
    /// and every absolute path starts with the root component, so against `/` the
    /// guard would be inert and `read_text("/etc/passwd")` would succeed. No
    /// vault is the filesystem root, so the configuration is a mistake and saying
    /// so is cheaper than a guard that quietly stops working.
    pub fn new(root: impl AsRef<Path>) -> std::result::Result<Self, BasesError> {
        let root = absolute(root.as_ref());
        if root.parent().is_none() {
            return Err(BasesError::new(format!(
                "Refusing to open the filesystem root as a vault: {}. Every absolute path is \
                 inside it, so a vault can contain nothing and no path can be refused.",
                root.display()
            )));
        }
        Ok(Self {
            root,
            files: RefCell::new(None),
            text_cache: RefCell::new(HashMap::new()),
            hash_cache: RefCell::new(HashMap::new()),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Drop the snapshot so the next read observes the current vault.
    ///
    /// Synchronous because there is nothing to await: the TypeScript is `async`
    /// only because every method in that interface was.
    pub fn refresh(&self) {
        *self.files.borrow_mut() = None;
        self.text_cache.borrow_mut().clear();
        self.hash_cache.borrow_mut().clear();
    }

    /// Depth-first over the tree, collecting indexable paths.
    ///
    /// Boxed at the recursive call because an `async fn` that calls itself has an
    /// infinitely sized future. The box is a `Pin`, not an allocation per level
    /// of intent: a vault is a handful of directories deep and this is once per
    /// `list()`.
    ///
    /// A directory that cannot be read is SKIPPED and the walk continues, which
    /// is how this backend hands back a partial vault. That is a deliberate
    /// disagreement with [`crate::vault::webdav`], which refuses a listing it
    /// could not get: over HTTP there is no directory to stat first, and the
    /// result of swallowing is a vault that answers every query with zero rows
    /// and no reason. `src/main.rs` compensates here by stat-ing the directory
    /// before opening it, and `tests/vault_equivalence.rs` pins both halves.
    async fn walk(&self, rel: &str, out: &mut Vec<String>) {
        let abs = if rel.is_empty() {
            self.root.clone()
        } else {
            self.root.join(rel)
        };
        let mut entries = match tokio::fs::read_dir(&abs).await {
            Ok(entries) => entries,
            Err(_) => return,
        };
        loop {
            let entry = match entries.next_entry().await {
                Ok(Some(entry)) => entry,
                Ok(None) | Err(_) => return,
            };
            let name = entry.file_name().to_string_lossy().into_owned();
            let child_rel = if rel.is_empty() {
                name.clone()
            } else {
                format!("{rel}/{name}")
            };
            let file_type = match entry.file_type().await {
                Ok(file_type) => file_type,
                // Node's `Dirent.isDirectory()` reads a type cached at `readdir`
                // time and answers `false` rather than throwing; an entry whose
                // type cannot be read is therefore not a directory and not a
                // file, and is skipped.
                Err(_) => continue,
            };
            if file_type.is_dir() {
                if name.starts_with('.') {
                    continue;
                }
                Box::pin(self.walk(&child_rel, out)).await;
            } else if file_type.is_file() && is_indexable(&child_rel) {
                out.push(child_rel);
            }
        }
    }

    /// Guard against a path escaping the vault root.
    ///
    /// Both refusals run before any I/O, and both are about the path's TEXT rather
    /// than about the tree: a path this backend cannot interpret the same way on
    /// every platform it might run on is refused rather than resolved on one of
    /// them and discovered on another.
    fn abs(&self, rel: &str) -> Result<PathBuf> {
        // A leading `/` names an absolute path, and the resolution below would
        // otherwise fold it onto the root as though it were relative — which is
        // how `/etc/passwd` becomes a note in this vault. It is refused here,
        // before any I/O.
        if rel.starts_with('/') {
            return Err(escapes_root(rel));
        }
        // `\` is the same class of mistake and the more dangerous of the two,
        // because the check at the bottom is COMPONENT-based and `PathBuf::push`
        // is not. The loop below splits on `/` only, so `..\..\outside\evil.md` is
        // one segment and the root does start with the result — but
        // `Path::components` splits on either separator on Windows, so the
        // operating system sees four segments there and the write lands outside
        // the root.
        //
        // Unconditional, deliberately, and NOT `#[cfg(windows)]`: a rule whose
        // safety depends on which binary it was compiled into is the defect itself
        // rather than its repair, and it is also the one part of the fix this test
        // suite could not otherwise reach from Linux. The cost is real and is
        // stated here rather than discovered: a Linux vault holding a file whose
        // name literally contains a backslash is indexed by `walk`, and reading it
        // now fails. That name is not portable to Windows in the first place, and
        // `VaultSource::list` documents its paths as vault-relative POSIX, in which
        // a backslash is not a legal character.
        if rel.contains('\\') {
            return Err(backslash_in_path(rel));
        }
        let mut out = self.root.clone();
        for segment in rel.split('/') {
            match segment {
                "" | "." => {}
                // Popping past the root is what `path.resolve` does, and it is
                // what makes `../outside.md` a refusal rather than a path: the
                // resolved result simply no longer starts with the root.
                ".." => {
                    out.pop();
                }
                other => out.push(other),
            }
        }
        if !out.starts_with(&self.root) {
            return Err(escapes_root(rel));
        }
        Ok(out)
    }
}

fn escapes_root(rel: &str) -> BasesError {
    BasesError::new(format!("Path escapes the vault root: {rel}"))
}

fn backslash_in_path(rel: &str) -> BasesError {
    BasesError::new(format!(
        "Path contains a backslash, which this backend will not resolve: {rel}. A vault path is \
         POSIX-relative and uses `/`; a backslash is a path separator on Windows, where the same \
         argument would name a different file or walk out of the root."
    ))
}

/// `path.resolve(root)`: absolute, with `.` and `..` folded out.
///
/// Folding happens lexically rather than through the filesystem, exactly as
/// `path.resolve` does. A symlink that leaves the vault would pass this guard;
/// the same is true of the original, and neither backend is the place to
/// resolve symlinks behind the caller's back.
fn absolute(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn io(context: &str, path: &Path, error: std::io::Error) -> BasesError {
    BasesError::new(format!("{context} {}: {error}", path.display()))
}

#[async_trait(?Send)]
impl VaultSource for FsVaultSource {
    fn kind(&self) -> SourceKind {
        SourceKind::Fs
    }

    async fn list(&self) -> Result<Vec<String>> {
        if let Some(files) = self.files.borrow().clone() {
            return Ok(files);
        }
        let mut out: Vec<String> = Vec::new();
        self.walk("", &mut out).await;
        out.sort();
        *self.files.borrow_mut() = Some(out.clone());
        Ok(out)
    }

    async fn read_text(&self, rel: &str) -> Result<String> {
        if let Some(text) = self.text_cache.borrow().get(rel) {
            return Ok(text.clone());
        }
        let abs = self.abs(rel)?;
        // Lossy, because Node's `readFile(path, "utf8")` is: invalid bytes
        // become U+FFFD there rather than raising, and the WebDAV backend's
        // `Buffer.toString("utf8")` behaves the same way. A note that is not
        // valid UTF-8 has to read the same on both backends or the equivalence
        // suite is comparing a decoding policy rather than a backend.
        let bytes = tokio::fs::read(&abs)
            .await
            .map_err(|e| io("read", &abs, e))?;
        let text = String::from_utf8_lossy(&bytes).into_owned();
        self.text_cache
            .borrow_mut()
            .insert(rel.to_string(), text.clone());
        Ok(text)
    }

    async fn read_fresh(&self, rel: &str) -> Result<String> {
        // The cache is deliberately bypassed rather than invalidated first: the
        // write path calls this to see the note as it is now, and dropping the
        // whole cache to do it would make every write a full re-listing.
        let abs = self.abs(rel)?;
        let bytes = tokio::fs::read(&abs)
            .await
            .map_err(|e| io("read", &abs, e))?;
        let text = String::from_utf8_lossy(&bytes).into_owned();
        // Keep the cache consistent with disk, so a later `read_text` cannot
        // hand back the pre-write copy we just superseded.
        self.text_cache
            .borrow_mut()
            .insert(rel.to_string(), text.clone());
        self.hash_cache.borrow_mut().remove(rel);
        Ok(text)
    }

    /// Ask the filesystem, never the listing snapshot.
    ///
    /// The snapshot is dropped by `refresh` and by every write, and the caller
    /// here is a commit that is about to overwrite: a `refresh` landing between
    /// the check and the write would not prevent the clobber, it would just make
    /// it happen over fresher bytes. `metadata` rather than `read_text` because
    /// the question is whether the path is taken, not what it says.
    ///
    /// Only `NotFound` is absence. The TypeScript tree answers `false` for EVERY
    /// throw here, because `fs.stat` does not hand back a code the `catch` can
    /// read -- so a permissions error there reports the note as absent, and the
    /// commit that asked goes ahead and overwrites. This backend can tell the
    /// codes apart and does, because `false` here is permission to destroy
    /// something rather than an absence of information.
    async fn exists(&self, rel: &str) -> Result<bool> {
        let abs = self.abs(rel)?;
        match tokio::fs::metadata(&abs).await {
            Ok(_) => Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(io("stat", &abs, error)),
        }
    }

    async fn write_text(&self, rel: &str, data: &str) -> Result<()> {
        let abs = self.abs(rel)?;
        if let Some(parent) = abs.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| io("create directory", parent, e))?;
        }
        tokio::fs::write(&abs, data.as_bytes())
            .await
            .map_err(|e| io("write", &abs, e))?;
        self.text_cache
            .borrow_mut()
            .insert(rel.to_string(), data.to_string());
        self.hash_cache.borrow_mut().remove(rel);
        *self.files.borrow_mut() = None;
        Ok(())
    }

    async fn stat(&self, rel: &str) -> Result<FileStat> {
        let abs = self.abs(rel)?;
        let meta = tokio::fs::metadata(&abs)
            .await
            .map_err(|e| io("stat", &abs, e))?;
        let mtime = meta
            .modified()
            .map(DateTime::<Local>::from)
            .map_err(|e| io("read mtime of", &abs, e))?;
        Ok(FileStat {
            size: meta.len(),
            mtime: mtime.fixed_offset(),
        })
    }

    async fn hash(&self, rel: &str) -> Result<String> {
        if let Some(hash) = self.hash_cache.borrow().get(rel) {
            return Ok(hash.clone());
        }
        let hash = content_hash(&self.read_text(rel).await?);
        self.hash_cache
            .borrow_mut()
            .insert(rel.to_string(), hash.clone());
        Ok(hash)
    }

    async fn ensure_dir(&self, rel: &str) -> Result<()> {
        let abs = self.abs(rel)?;
        tokio::fs::create_dir_all(&abs)
            .await
            .map_err(|e| io("create directory", &abs, e))
    }

    /// Remove a file, reporting success for one that was never there.
    ///
    /// This is `rm --force`, and it is a deliberate divergence from WebDAV: a
    /// `DELETE` of a missing resource answers `404` there, and a delete that did
    /// not happen must not be reported as having happened. It is preserved
    /// because the filesystem backend is the reference every other backend is
    /// pinned to, and `VaultSource::delete` has no caller in `src/` yet for the
    /// difference to bite.
    async fn delete(&self, rel: &str) -> Result<()> {
        let abs = self.abs(rel)?;
        match tokio::fs::remove_file(&abs).await {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(io("delete", &abs, error)),
        }
        self.text_cache.borrow_mut().remove(rel);
        self.hash_cache.borrow_mut().remove(rel);
        *self.files.borrow_mut() = None;
        Ok(())
    }
}
