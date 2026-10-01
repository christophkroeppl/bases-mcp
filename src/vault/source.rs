//! The single I/O boundary for vault content.
//!
//! Everything above this interface is pure, so the filesystem and WebDAV
//! backends are interchangeable by construction. That equivalence is not
//! assumed: `tests/vault_equivalence.rs` asserts the same vault resolves
//! identically through both.
//!
//! The trait is `async` because one of its two implementations is: WebDAV is
//! HTTP, and an interface that cannot express a round trip would force the
//! filesystem backend to pretend it has one. The futures are `?Send` on purpose.
//! [`crate::value::FileValue`] is built out of `Rc`s and is therefore `!Send`
//! already, and the server is a stdio process that answers one call at a time —
//! requiring `Send` here would buy nothing and cost the borrow checker every
//! method that has to touch its own caches across an await. rmcp has a `local`
//! feature for exactly this shape of server.

use async_trait::async_trait;
use chrono::{DateTime, FixedOffset};

use crate::error::Result;

/// Paths Obsidian considers notes. Bases are queried alongside them.
pub const NOTE_EXT: &str = ".md";
pub const BASE_EXT: &str = ".base";

/// Which backend is behind a [`VaultSource`]. A stable identifier for logging
/// and error messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SourceKind {
    Fs,
    Webdav,
}

impl SourceKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            SourceKind::Fs => "fs",
            SourceKind::Webdav => "webdav",
        }
    }
}

impl std::fmt::Display for SourceKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

pub fn is_note_path(path: &str) -> bool {
    path.ends_with(NOTE_EXT)
}

pub fn is_base_path(path: &str) -> bool {
    path.ends_with(BASE_EXT)
}

/// Ignore `.obsidian/`, dotfiles and templates' scratch space.
///
/// The per-segment dot test subsumes a leading-dot path — its first segment is
/// the path — so there is one rule here rather than two that could disagree.
pub fn is_indexable(path: &str) -> bool {
    if path.split('/').any(|segment| segment.starts_with('.')) {
        return false;
    }
    is_note_path(path) || is_base_path(path)
}

/// What a backend knows about one file without reading it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileStat {
    /// Bytes on the wire, not characters.
    pub size: u64,
    /// The instant the backend's clock reported.
    ///
    /// A `FixedOffset` rather than UTC because the WebDAV backend hands over
    /// the server's own offset verbatim — normalising it would make the two
    /// backends agree on a reading the server never claimed.
    pub mtime: DateTime<FixedOffset>,
}

/// A place vault content comes from and goes to.
#[async_trait(?Send)]
pub trait VaultSource {
    /// A stable identifier for logging and error messages.
    fn kind(&self) -> SourceKind;

    /// Vault-relative POSIX paths of every note (`.md`) and base (`.base`).
    async fn list(&self) -> Result<Vec<String>>;

    async fn read_text(&self, path: &str) -> Result<String>;

    /// Read the note as it is RIGHT NOW, bypassing any cache.
    ///
    /// This is what the write path uses, and the distinction from `read_text` is
    /// the whole point: `read_text` may answer from a snapshot taken earlier in
    /// the process's life, which is right for queries and catastrophic for a
    /// write. Reconciling an edit against a stale copy and then writing the
    /// result over the top of whatever the user has since typed destroys their
    /// work while reporting success.
    ///
    /// Obsidian autosaves continuously, so "the user edited this note in the
    /// last thirty seconds" is the normal case, not an edge case.
    async fn read_fresh(&self, path: &str) -> Result<String>;

    async fn write_text(&self, path: &str, data: &str) -> Result<()>;

    async fn stat(&self, path: &str) -> Result<FileStat>;

    /// Content hash, used for read-verify-write.
    ///
    /// Deliberately NOT the server ETag: a WebDAV server is free to invent one,
    /// and the server this targets does not emit `getetag` at all. Hashing what
    /// we ourselves read is both backend-agnostic and sufficient to detect a
    /// concurrent write. [`crate::vault::fs::content_hash`] is the one
    /// definition, and it is the definition every backend calls.
    async fn hash(&self, path: &str) -> Result<String>;

    /// Create intermediate collections. A no-op on backends that need none.
    async fn ensure_dir(&self, path: &str) -> Result<()>;

    /// Remove a file.
    ///
    /// Backends disagree on what happens for a file that was never there, and
    /// the disagreement is recorded rather than papered over: see the notes on
    /// each implementation.
    async fn delete(&self, path: &str) -> Result<()>;
}
