//! An in-memory [`VaultSource`]: a fake WebDAV server with no network in it.
//!
//! This is the harness half of the WebDAV work. The invariant the project claims
//! -- "resolving a note containing a base yields identical output over fs and
//! WebDAV" -- was prose until this existed. A real WebDAV test needs a server, a
//! network and a clock, so it gets skipped, so it does not run on every commit,
//! so it rots. This source is that same contract with the I/O removed: any
//! `VaultSource` implementation can be pointed at the same corpus and held
//! against the filesystem one, in the ordinary unit suite, anywhere.
//!
//! Three rules make it a useful oracle rather than a second opinion:
//!
//!   1. It reuses [`content_hash`] rather than hashing for itself. Two hash
//!      implementations are two chances to disagree, and a disagreement there
//!      would be reported as a backend bug rather than a test bug.
//!   2. Its mtime is a FIXED instant, never the wall clock. The filesystem's
//!      mtime is the real clock, so any comparison touching mtime is a
//!      comparison against time, and a flaky one. See [`MEMORY_MTIME_MS`].
//!   3. Its path normalisation reproduces `path.resolve`, so a path that is
//!      legal for one backend is legal for the other. `Vault` hands out
//!      vault-relative POSIX paths, and a backend that resolved them differently
//!      would differ in `list()` output alone, which is noise masquerading as a
//!      finding.
//!
//! It is a FAKE, so it is wrong in the ways a fake must be: see `delete`, which
//! mirrors the filesystem's forgiving behaviour.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use async_trait::async_trait;
use bases_mcp::error::BasesError;
use bases_mcp::vault::{content_hash, is_indexable, FileStat, SourceKind, VaultSource};
use chrono::{DateTime, FixedOffset};

use super::VaultFile;

/// The instant every file in a memory vault reports as its mtime, in
/// milliseconds since the epoch.
///
/// Fixed rather than sampled, because the filesystem's mtime is the real clock
/// and any assertion comparing the two would compare two different moments. It
/// is in the past so that `(now() - file.mtime)` stays positive, which is the
/// idiom real vaults use and therefore the one worth keeping meaningful.
pub const MEMORY_MTIME_MS: i64 = 1_704_067_200_000;

/// The fixed instant, in the offset a filesystem would report it in.
///
/// UTC rather than local on purpose: a fake whose mtime moved with the machine's
/// timezone would make the filesystem-vs-memory comparison depend on where the
/// suite runs, and a fixture that answers a different question on a laptop than
/// on CI is a fixture nobody trusts.
pub fn memory_mtime() -> DateTime<FixedOffset> {
    DateTime::from_timestamp_millis(MEMORY_MTIME_MS)
        .expect("the fixed instant is representable")
        .fixed_offset()
}

/// The operations a fault can be attached to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryOp {
    List,
    Read,
    Stat,
    Hash,
    Exists,
    Write,
    Delete,
}

/// A fault on every path.
pub const ANY_PATH: &str = "**";

/// An injected failure.
///
/// The two cases that matter are the ones a correct-but-unlucky client hits:
/// a 5xx the server invented, and a write that reports success while storing
/// something else. The second is why `stored` exists: read-verify-write exists
/// precisely to catch it, and a fake that can only fail loudly cannot exercise
/// the verify step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryFault {
    /// Vault-relative POSIX path, or [`ANY_PATH`] for any path.
    pub path: String,
    /// The operation that fails. `None` fails every operation on the path.
    pub op: Option<MemoryOp>,
    /// The status a WebDAV server would answer with, e.g. 404 or 500.
    ///
    /// `None` only when `stored` is set: the write then SUCCEEDS and a status
    /// would be a lie.
    pub status: Option<u16>,
    /// Store this text instead of the requested one, simulating a server that
    /// accepted the PUT and wrote something else. Only meaningful on a write.
    pub stored: Option<String>,
    /// Fire at most this many times. `None` fails every time.
    pub times: Option<u32>,
}

impl MemoryFault {
    /// A fault that refuses every operation on `path` with `status`.
    pub fn refusing(path: &str, status: u16) -> Self {
        Self {
            path: path.to_string(),
            op: None,
            status: Some(status),
            stored: None,
            times: None,
        }
    }

    /// A fault on one operation of one path.
    pub fn on(path: &str, op: MemoryOp, status: u16) -> Self {
        Self {
            op: Some(op),
            ..Self::refusing(path, status)
        }
    }

    /// A write that succeeds and stores something else.
    pub fn storing(path: &str, stored: &str) -> Self {
        Self {
            path: path.to_string(),
            op: Some(MemoryOp::Write),
            status: None,
            stored: Some(stored.to_string()),
            times: None,
        }
    }

    /// Fire at most `times` times, then stop.
    pub fn times(mut self, times: u32) -> Self {
        self.times = Some(times);
        self
    }
}

/// A refusal from the fake server.
///
/// `status` is carried rather than left in the message because the caller that
/// has to react -- a backend deciding whether to retry -- needs it as a field. A
/// fake that only produced prose would let a real backend pass its tests by
/// matching on strings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryVaultError {
    pub status: u16,
    pub path: String,
    pub message: String,
}

impl std::fmt::Display for MemoryVaultError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl From<MemoryVaultError> for BasesError {
    fn from(error: MemoryVaultError) -> Self {
        BasesError::new(error.message)
    }
}

/// A fault that has been armed, and how many times it may still fire.
#[derive(Debug)]
struct ArmedFault {
    fault: MemoryFault,
    remaining: Option<u32>,
}

pub struct MemoryVaultSource {
    files: RefCell<HashMap<String, String>>,
    armed: RefCell<Vec<ArmedFault>>,
    /// Collections `ensure_dir` was asked to create.
    ///
    /// A `HashMap` of files has no directories, so `write_text` has no parents
    /// to create and `list()` cannot accidentally see a directory. The set
    /// exists so a test can still assert that `ensure_dir` was CALLED with the
    /// parent, which is the part of the contract a real backend has to honour
    /// and a map alone would hide.
    pub dirs: RefCell<std::collections::BTreeSet<String>>,
    /// Every refusal this source has issued, newest last.
    ///
    /// The trait's error type carries a message but not a status, so a test that
    /// wanted the status structurally had nowhere to find it. The log is that
    /// somewhere: it is the fake's account of what it refused, which is the
    /// thing a test about fault injection is actually asserting on.
    refusals: RefCell<Vec<MemoryVaultError>>,
}

impl MemoryVaultSource {
    pub fn new(seed: impl IntoIterator<Item = VaultFile>) -> Self {
        let mut files = HashMap::new();
        for file in seed {
            let path = normalise(&file.path).expect("a seeded path is vault-relative");
            files.insert(path, file.content);
        }
        Self {
            files: RefCell::new(files),
            armed: RefCell::new(Vec::new()),
            dirs: RefCell::new(Default::default()),
            refusals: RefCell::new(Vec::new()),
        }
    }

    pub fn from_paths(files: &[(&str, &str)]) -> Self {
        Self::new(
            files
                .iter()
                .map(|(path, content)| VaultFile {
                    path: (*path).to_string(),
                    content: (*content).to_string(),
                })
                .collect::<Vec<_>>(),
        )
    }

    /// Arm a fault. Faults are consulted in the order armed, so a later fault is
    /// only reached when the earlier ones decline to fire.
    ///
    /// `ensure_dir` consults the write table, because creating a collection is a
    /// write to the server; a fault on a write therefore also fires on
    /// `ensure_dir` for the same path.
    ///
    /// A fault naming neither outcome is refused HERE rather than in the tests,
    /// because the fake is the boundary between a test and the behaviour it is
    /// asking for, and a fault that means nothing would silently never fire.
    pub fn inject(&self, fault: MemoryFault) -> std::result::Result<(), BasesError> {
        if fault.status.is_none() && fault.stored.is_none() {
            return Err(BasesError::new(
                "A memory fault must say what happens: give a `status` to refuse with, or \
                 `stored` bytes to accept the write and keep.",
            ));
        }
        self.armed.borrow_mut().push(ArmedFault {
            remaining: fault.times,
            fault,
        });
        Ok(())
    }

    /// Disarm everything. Lets one test reuse a source without inheriting faults.
    pub fn clear_faults(&self) {
        self.armed.borrow_mut().clear();
    }

    /// The refusals this source has issued, newest last.
    pub fn refusals(&self) -> Vec<MemoryVaultError> {
        self.refusals.borrow().clone()
    }

    /// The status of the most recent refusal, if there was one.
    pub fn last_status(&self) -> Option<u16> {
        self.refusals.borrow().last().map(|refusal| refusal.status)
    }

    /// Normalise a caller-supplied path, logging the refusal if it escapes.
    fn normalise_refusing(&self, rel: &str) -> std::result::Result<String, BasesError> {
        normalise(rel).map_err(|error| self.refuse(error))
    }

    /// The stored text, or the refusal a server would have given.
    fn read(&self, path: &str, verb: &str) -> std::result::Result<String, BasesError> {
        self.files
            .borrow()
            .get(path)
            .cloned()
            .ok_or_else(|| self.refuse(not_found(verb, path, 404)))
    }

    /// Run the fault table for one operation.
    ///
    /// A fault carrying `stored` does not fail. It returns the replacement text,
    /// so the write SUCCEEDS and the server keeps something else: the failure
    /// mode read-verify-write exists to catch and the only one a client cannot
    /// detect from the response alone. Handing the replacement back rather than
    /// applying it here is what keeps the ordering honest -- applying it before
    /// the write would be overwritten by the write and the fault would be a
    /// no-op that looked armed.
    fn fire(&self, at: MemoryOp, path: &str) -> std::result::Result<Option<String>, BasesError> {
        let mut armed = self.armed.borrow_mut();
        let mut replacement: Option<String> = None;
        for entry in armed.iter_mut() {
            if entry.remaining == Some(0) {
                continue;
            }
            if entry.fault.op.is_some_and(|op| op != at) {
                continue;
            }
            let wanted = if entry.fault.path == ANY_PATH {
                None
            } else {
                // A fault on a path that is not vault-relative is a fault that can
                // never fire, so it is normalised once here and a normalisation
                // failure skips it rather than panicking the run.
                normalise(&entry.fault.path).ok()
            };
            if wanted.is_some() && wanted.as_deref() != Some(path) {
                continue;
            }

            entry.remaining = entry.remaining.map(|remaining| remaining - 1);
            if let Some(stored) = &entry.fault.stored {
                if at == MemoryOp::Write {
                    replacement = Some(stored.clone());
                }
                continue;
            }
            let status = entry
                .fault
                .status
                .expect("`inject` refuses a fault naming no outcome");
            let verb = entry.fault.op.unwrap_or(at).as_str().to_uppercase();
            return Err(self.refuse(MemoryVaultError {
                status,
                path: path.to_string(),
                message: format!("{verb} {path}: {status} injected"),
            }));
        }
        Ok(replacement)
    }

    /// Record a refusal and turn it into the trait's error type.
    ///
    /// The log is written on the way out rather than by the caller, so a refusal
    /// raised deep inside a fault table is as visible as one raised by a missing
    /// file. A test that has to remember to log its own failures logs the easy
    /// ones.
    fn refuse(&self, error: MemoryVaultError) -> BasesError {
        self.refusals.borrow_mut().push(error.clone());
        error.into()
    }
}

impl MemoryOp {
    fn as_str(self) -> &'static str {
        match self {
            MemoryOp::List => "list",
            MemoryOp::Read => "read",
            MemoryOp::Stat => "stat",
            MemoryOp::Hash => "hash",
            MemoryOp::Exists => "exists",
            MemoryOp::Write => "write",
            MemoryOp::Delete => "delete",
        }
    }
}

fn not_found(verb: &str, path: &str, status: u16) -> MemoryVaultError {
    MemoryVaultError {
        status,
        path: path.to_string(),
        message: format!("{verb} {path}: {status} Not Found"),
    }
}

#[async_trait(?Send)]
impl VaultSource for MemoryVaultSource {
    fn kind(&self) -> SourceKind {
        SourceKind::Webdav
    }

    /// Vault-relative POSIX paths of every indexable file, sorted.
    ///
    /// Sorted with the default comparator because that is what the filesystem
    /// backend's `list` does, and `list()` order feeds `Vault`'s insertion order,
    /// which feeds ambiguous-link resolution and every unsorted view. A different
    /// order here would show up as a phantom backend bug.
    ///
    /// No snapshot cache, unlike the filesystem backend. The cache is invisible
    /// to every assertion in the suite, and a fake that cached would let a
    /// backend get away with forgetting to re-list after a write.
    async fn list(&self) -> std::result::Result<Vec<String>, BasesError> {
        self.fire(MemoryOp::List, ANY_PATH)?;
        let mut paths: Vec<String> = self
            .files
            .borrow()
            .keys()
            .filter(|p| is_indexable(p))
            .cloned()
            .collect();
        paths.sort();
        Ok(paths)
    }

    async fn read_text(&self, rel: &str) -> std::result::Result<String, BasesError> {
        let path = self.normalise_refusing(rel)?;
        self.fire(MemoryOp::Read, &path)?;
        self.read(&path, "GET")
    }

    /// There is no cache here, so a fresh read is the same read.
    async fn read_fresh(&self, rel: &str) -> std::result::Result<String, BasesError> {
        self.read_text(rel).await
    }

    /// A membership test, which is all this fake has to answer.
    ///
    /// It consults the fault table like every other operation, so a `500` injected
    /// here is an error rather than a `false`. That is the property the trait
    /// demands and the property a real WebDAV source has to earn: reporting a note
    /// as absent because the server could not be reached would authorise the
    /// overwrite this method exists to prevent.
    async fn exists(&self, rel: &str) -> std::result::Result<bool, BasesError> {
        let path = self.normalise_refusing(rel)?;
        self.fire(MemoryOp::Exists, &path)?;
        Ok(self.files.borrow().contains_key(&path))
    }

    async fn stat(&self, rel: &str) -> std::result::Result<FileStat, BasesError> {
        let path = self.normalise_refusing(rel)?;
        self.fire(MemoryOp::Stat, &path)?;
        let text = self.read(&path, "PROPFIND")?;
        Ok(FileStat {
            size: text.len() as u64,
            mtime: memory_mtime(),
        })
    }

    /// The backend's own hash, taken over the text it read.
    ///
    /// [`content_hash`] is shared with the filesystem backend, so this cannot
    /// drift from it: a change to the hash function changes both, and the
    /// equivalence suite still passes.
    async fn hash(&self, rel: &str) -> std::result::Result<String, BasesError> {
        let path = self.normalise_refusing(rel)?;
        self.fire(MemoryOp::Hash, &path)?;
        Ok(content_hash(&self.read(&path, "GET")?))
    }

    /// Store text at a vault-relative path, creating whatever collections that
    /// implies and overwriting silently.
    ///
    /// Overwriting silently is what a WebDAV `PUT` does, and it is what the
    /// filesystem backend does, so a client that relies on either behaves the
    /// same over both.
    async fn write_text(&self, rel: &str, data: &str) -> std::result::Result<(), BasesError> {
        let path = self.normalise_refusing(rel)?;
        let swapped = self.fire(MemoryOp::Write, &path)?;
        self.files
            .borrow_mut()
            .insert(path, swapped.unwrap_or_else(|| data.to_string()));
        Ok(())
    }

    /// Record a collection.
    ///
    /// Creating a directory that already exists is not an error on either
    /// backend, and `ensure_dir` is not asked to report whether it did anything.
    async fn ensure_dir(&self, rel: &str) -> std::result::Result<(), BasesError> {
        let path = self.normalise_refusing(rel)?;
        self.fire(MemoryOp::Write, &path)?;
        self.dirs.borrow_mut().insert(path);
        Ok(())
    }

    /// Remove a file.
    ///
    /// Deleting something absent is a NO-OP, which is the filesystem backend's
    /// behaviour. A real WebDAV `DELETE` of a missing resource answers `404` and
    /// this backend's source TOLERATES that `404`, so all three backends say
    /// "gone" -- but the fake never learns the resource was absent, exactly as
    /// the filesystem's `rm --force` does not. A real WebDAV backend that copied
    /// this shape would silently skip a delete that never happened.
    async fn delete(&self, rel: &str) -> std::result::Result<(), BasesError> {
        let path = self.normalise_refusing(rel)?;
        self.fire(MemoryOp::Delete, &path)?;
        self.files.borrow_mut().remove(&path);
        Ok(())
    }
}

/// [`MemoryVaultSource`] behind a trait object, for the code path that holds a
/// `Box<dyn VaultSource>` and has to drive it.
///
/// A newtype rather than a blanket impl because the fixture is shared with the
/// filesystem backend in the same test: both have to be reachable as trait
/// objects, and only one of them is a fake.
pub struct SharedSource(pub Rc<MemoryVaultSource>);

#[async_trait(?Send)]
impl VaultSource for SharedSource {
    fn kind(&self) -> SourceKind {
        self.0.kind()
    }
    async fn list(&self) -> std::result::Result<Vec<String>, BasesError> {
        self.0.list().await
    }
    async fn read_text(&self, path: &str) -> std::result::Result<String, BasesError> {
        self.0.read_text(path).await
    }
    async fn read_fresh(&self, path: &str) -> std::result::Result<String, BasesError> {
        self.0.read_fresh(path).await
    }
    async fn exists(&self, path: &str) -> std::result::Result<bool, BasesError> {
        self.0.exists(path).await
    }
    async fn write_text(&self, path: &str, data: &str) -> std::result::Result<(), BasesError> {
        self.0.write_text(path, data).await
    }
    async fn stat(&self, path: &str) -> std::result::Result<FileStat, BasesError> {
        self.0.stat(path).await
    }
    async fn hash(&self, path: &str) -> std::result::Result<String, BasesError> {
        self.0.hash(path).await
    }
    async fn ensure_dir(&self, path: &str) -> std::result::Result<(), BasesError> {
        self.0.ensure_dir(path).await
    }
    async fn delete(&self, path: &str) -> std::result::Result<(), BasesError> {
        self.0.delete(path).await
    }
}

/// Vault-relative POSIX normalisation, matching what the filesystem backend
/// computes after `path.resolve`.
///
/// The rules `path.resolve` applies, and why each one is here rather than
/// guessed: empty and `.` segments vanish, `..` pops, a `..` with nothing left
/// to pop escapes the root, and a leading `/` is an absolute path that escapes
/// it too. Every one of them decides whether a client gets a note or a refusal,
/// so getting one wrong makes the two backends disagree about paths that are
/// legal in both.
pub fn normalise(rel: &str) -> std::result::Result<String, MemoryVaultError> {
    if rel.starts_with('/') {
        return Err(escapes_root(rel));
    }
    let mut out: Vec<&str> = Vec::new();
    for segment in rel.split('/') {
        if segment.is_empty() || segment == "." {
            continue;
        }
        if segment == ".." {
            if out.pop().is_none() {
                return Err(escapes_root(rel));
            }
            continue;
        }
        out.push(segment);
    }
    Ok(out.join("/"))
}

fn escapes_root(rel: &str) -> MemoryVaultError {
    MemoryVaultError {
        status: 403,
        path: rel.to_string(),
        message: format!("Path escapes the vault root: {rel}"),
    }
}
