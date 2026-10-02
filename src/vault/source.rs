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

/// Replace every `\r\n` with `\n`, and nothing else.
///
/// A lone `\r` is left alone, and that is the design rather than a shortcut.
/// `note::Lines` finds `\n` and nothing else, so a CR-only note is ONE line to
/// this crate; replacing every `\r` would make it as many lines as it has
/// carriage returns, inventing a line count the note never had. Leaving it alone
/// is also what keeps such a note round-tripping byte for byte.
///
/// `\r\r\n` becomes `\r\n`, because the pair at the end of it is still a pair.
/// Degrading a doubled carriage return to a real CRLF on the way to being
/// normalised is strictly better than passing it up.
///
/// See [`VaultSource::read_note`], which is where this runs.
pub fn normalise_line_endings(text: &str) -> String {
    // Notes are LF already unless something outside Obsidian said otherwise, so
    // this is the common path and the copy is a memcpy with nothing to scan.
    if !text.contains('\r') {
        return text.to_string();
    }
    text.replace("\r\n", "\n")
}

/// A place vault content comes from and goes to.
#[async_trait(?Send)]
pub trait VaultSource {
    /// A stable identifier for logging and error messages.
    fn kind(&self) -> SourceKind;

    /// Vault-relative POSIX paths of every note (`.md`) and base (`.base`).
    async fn list(&self) -> Result<Vec<String>>;

    /// A note's text, with CRLF normalised to LF. **Not the bytes on disk.**
    ///
    /// The name is the warning. This translates, and a function that translates
    /// is not doing what `read_text` claimed to do — so the translation is in the
    /// name now rather than in a paragraph nobody reads.
    ///
    /// Normalising HERE, at the single I/O boundary, rather than in [`Vault`] is
    /// what makes it stick: every read of a note in the process routes through
    /// this method, so there is no caller a future change can forget, and the
    /// question "which line ending does this note use" has no answer to get wrong
    /// above it. Obsidian writes `\n` on every platform, so CRLF is the FOREIGN
    /// shape — it arrives from `core.autocrlf=true`, a sync client, or an edit
    /// made outside Obsidian. `docs/divergences.md` records what that costs.
    ///
    /// A cache this feeds holds the NORMALISED text, so "what is cached is what
    /// is returned" holds rather than being a thing each backend has to remember.
    /// [`crate::vault::content_hash`] is taken over this text too, which is why a
    /// CRLF note and its LF twin hash alike and `base_hash` round-trips across
    /// the two spellings.
    async fn read_note(&self, path: &str) -> Result<String>;

    /// Read the note as it is RIGHT NOW, bypassing any cache.
    ///
    /// This is what the write path uses, and the distinction from `read_note` is
    /// the whole point: `read_note` may answer from a snapshot taken earlier in
    /// the process's life, which is right for queries and catastrophic for a
    /// write. Reconciling an edit against a stale copy and then writing the
    /// result over the top of whatever the user has since typed destroys their
    /// work while reporting success.
    ///
    /// Obsidian autosaves continuously, so "the user edited this note in the
    /// last thirty seconds" is the normal case, not an edge case.
    ///
    /// Normalised exactly as [`Self::read_note`] is, and separately rather than
    /// by calling it: a fresh read that went through the cache would not be one.
    async fn read_fresh(&self, path: &str) -> Result<String>;

    /// Is there something at `path` RIGHT NOW?
    ///
    /// The listing is the wrong answer to this question. It is a snapshot that
    /// only moves when this server writes, and Obsidian autosaves without ever
    /// going through this server — so a note a human created two minutes ago is
    /// absent from it, and the write path behind `add_note_to_base` would replace
    /// that note's prose while reporting `verified: true`. This is the only
    /// method that asks the storage rather than the cache, and it exists because
    /// a clobber is unrecoverable.
    ///
    /// A `true` is the SAFE answer on every backend: a directory at the path, or
    /// anything else that is not a note, still means the path is not free for a
    /// new note.
    ///
    /// An ERROR MUST NEVER BECOME `false`. `false` tells the caller the path is
    /// free, which is permission to overwrite, so a backend that answered "I could
    /// not find out" with `false` would defeat the entire method. Only a positive
    /// statement that nothing is there may produce `false`.
    async fn exists(&self, path: &str) -> Result<bool>;

    async fn write_text(&self, path: &str, data: &str) -> Result<()>;

    async fn stat(&self, path: &str) -> Result<FileStat>;

    /// Content hash, used for read-verify-write.
    ///
    /// Deliberately NOT the server ETag: a WebDAV server is free to invent one,
    /// and the server this targets does not emit `getetag` at all. Hashing what
    /// we ourselves read is both backend-agnostic and sufficient to detect a
    /// concurrent write. [`crate::vault::content_hash`] is the one definition,
    /// and it is the definition every backend calls.
    ///
    /// Taken over the text [`Self::read_note`] returns, which makes it the hash
    /// of what a note MEANS here rather than of how some producer once spelled
    /// it. A CRLF note and its LF twin are the same note, and hashing the
    /// difference would refuse a conditional write that had nothing stale about
    /// it — the agent read the note through this server and got back LF, so the
    /// bytes on disk are not what it agreed to.
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
