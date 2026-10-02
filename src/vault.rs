//! The vault layer: the one place vault content is read from and written to, and
//! the index over it.
//!
//! Two backends live in the submodules — the local filesystem and a WebDAV
//! client — plus an in-memory fake in `tests/` used as the equivalence oracle.
//! Everything above [`source::VaultSource`] is pure, so the backends are
//! interchangeable by construction, and `tests/vault_equivalence.rs` is what
//! makes that a claim rather than a hope.
//!
//! Two behaviours deliberately differ between backends, and both are commented
//! where they happen rather than papered over here: an unreadable directory is
//! skipped by the filesystem source and refused by the WebDAV one, and a
//! `delete` of a file that was never there is tolerated by both for different
//! reasons. `docs/divergences.md` is where such a difference is recorded.
//!
//! # The index
//!
//! Link resolution follows Obsidian rather than intuition, and there are two
//! rules behind it, neither of which is what you would guess:
//!
//!  - A bare `[[Alias]]` does NOT resolve. `aliases` feeds the link *suggester*,
//!    which emits the piped form `[[Real Name|Alias]]`. Verified on 1.13.7.
//!  - An exact spelling is answered by FIRST REGISTRATION WINS, over an index
//!    built from a sorted path list. A spelling that matches no key exactly falls
//!    through to a folded comparison, and THERE the answer is the shortest path.
//!    The original's own comments merge the two into one sentence, which its code
//!    does not do; both are pinned separately in `tests/vault_equivalence.rs`.
//!
//! The index lives behind `RefCell` and the vault is handed out as an `Rc<Vault>`,
//! because every `file.*` accessor is a closure that has to reach back into the
//! index from inside the evaluator. A `&mut self` API and an `Rc` are not
//! compatible, and `Rc` is the direction the rest of the value layer already went
//! in (`FileValue` and `FileAccessors` are `Rc` based).

pub mod fs;
pub mod resolve;
pub mod source;
pub mod webdav;

pub use fs::{content_hash, FsVaultSource};
pub use resolve::{fold_key, match_path, points_at, PathIndex};
pub use source::{
    is_base_path, is_indexable, is_note_path, normalise_line_endings, FileStat, SourceKind,
    VaultSource,
};
pub use webdav::{
    dav_refusal, parse_dav_date, parse_multistatus, vault_path_from_href, vault_relative_path,
    DavOperation, DavRequest, DavRequestOptions, DavResource, DavResponse, Depth, WebdavError,
    WebdavMethod, WebdavTransport, WebdavVaultOptions, WebdavVaultSource,
};

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::OnceLock;

use chrono::{DateTime, FixedOffset};
use regex::Regex;

use crate::error::Result;
use crate::note::{parse_note_with_embeds, ParsedNote, TaskItem, WikiLink};
use crate::value::{strip_extension, BasesDate, BasesValue, FileAccessors, FileValue};

/// One indexed note, with its frontmatter already coerced.
#[derive(Debug, Clone)]
pub struct NoteRecord {
    pub path: String,
    pub parsed: ParsedNote,
    /// The frontmatter with link-shaped values turned into links.
    ///
    /// `Rc` because every `file.properties` access hands the evaluator a whole
    /// map and the index holds the same one for every row of a query.
    pub frontmatter: Rc<BTreeMap<String, BasesValue>>,
    pub size: u64,
    pub mtime: DateTime<FixedOffset>,
}

/// The index.
pub struct Vault {
    /// `Rc`, not `Box`, so a caller that needs the backend can keep reading
    /// through it while an overlay vault is built over the same listing. The
    /// verify step behind `add_note_to_base` needs exactly that, and moving the
    /// box out is not an option — the index is still reading from it.
    source: Rc<dyn VaultSource>,
    /// `Rc` per record so `note()` can hand one out without holding the map
    /// borrowed. The map is a `BTreeMap` rather than an insertion-ordered one
    /// because `list()` is sorted on every backend, so key order IS registration
    /// order -- and registration order is what breaks link ties.
    notes: RefCell<BTreeMap<String, Rc<NoteRecord>>>,
    bases: RefCell<Vec<String>>,
    by_path: RefCell<PathIndex>,
    ready: Cell<bool>,
}

impl std::fmt::Debug for Vault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Vault")
            .field("source", &self.source.kind())
            .field("notes", &self.notes.borrow().len())
            .field("bases", &self.bases.borrow().len())
            .finish()
    }
}

impl Vault {
    pub fn new(source: Box<dyn VaultSource>) -> Self {
        Self::with_source(Rc::from(source))
    }

    /// Index a vault whose backend is already shared.
    pub fn with_source(source: Rc<dyn VaultSource>) -> Self {
        Self {
            source,
            notes: RefCell::new(BTreeMap::new()),
            bases: RefCell::new(Vec::new()),
            by_path: RefCell::new(PathIndex::new()),
            ready: Cell::new(false),
        }
    }

    pub fn source_kind(&self) -> SourceKind {
        self.source.kind()
    }

    /// The underlying backend, for writes that must bypass the snapshot.
    pub fn backend(&self) -> &dyn VaultSource {
        self.source.as_ref()
    }

    /// The backend, shared.
    ///
    /// For an overlay index built over the same listing — see
    /// [`crate::drafts`], which splices one unsaved note into the real backend
    /// so verification exercises the real link resolution rather than a stand-in.
    pub fn shared_source(&self) -> Rc<dyn VaultSource> {
        Rc::clone(&self.source)
    }

    /// Build the index. Safe to call repeatedly; use [`Vault::reload`] to force a
    /// rebuild after writes.
    pub async fn load(&self) -> Result<()> {
        if self.ready.get() {
            return Ok(());
        }
        self.rebuild().await
    }

    pub async fn reload(&self) -> Result<()> {
        self.rebuild().await
    }

    /// Rebuild from the source's current listing.
    ///
    /// The order inside this method is load-bearing and the comment on
    /// [`coerce_frontmatter`] explains why: frontmatter is coerced BEFORE the
    /// path index is populated, so a link written in a property resolves to
    /// nothing. That is the original's behaviour and it is preserved here rather
    /// than fixed, because `file.links` re-resolves the same links at query time
    /// and the parity suite was calibrated against it.
    async fn rebuild(&self) -> Result<()> {
        let paths = self.source.list().await?;

        let mut notes: BTreeMap<String, Rc<NoteRecord>> = BTreeMap::new();
        let mut bases: Vec<String> = Vec::new();
        for path in paths {
            if is_base_path(&path) {
                bases.push(path);
                continue;
            }
            if !is_note_path(&path) {
                continue;
            }
            let text = self.source.read_note(&path).await?;
            let stat = self.source.stat(&path).await?;
            let parsed = parse_note_with_embeds(&path, &text);
            let frontmatter = coerce_frontmatter(&parsed.frontmatter, self);
            notes.insert(
                path.clone(),
                Rc::new(NoteRecord {
                    path,
                    parsed,
                    frontmatter: Rc::new(frontmatter),
                    size: stat.size,
                    mtime: stat.mtime,
                }),
            );
        }

        // Index every path under the four keys Obsidian's resolver accepts: full
        // path, path without extension, and the basename with and without its
        // extension.
        let mut by_path = PathIndex::new();
        for path in notes.keys() {
            register(&mut by_path, path, path);
            register(&mut by_path, path, &strip_extension(path));
            let base = path.rsplit('/').next().unwrap_or(path);
            register(&mut by_path, path, base);
            register(&mut by_path, path, &strip_extension(base));
        }
        bases.sort();

        *self.notes.borrow_mut() = notes;
        *self.bases.borrow_mut() = bases;
        *self.by_path.borrow_mut() = by_path;
        self.ready.set(true);
        Ok(())
    }

    /// Every indexed note, in registration order.
    pub fn note_paths(&self) -> Vec<String> {
        self.notes.borrow().keys().cloned().collect()
    }

    pub fn base_paths(&self) -> Vec<String> {
        self.bases.borrow().clone()
    }

    pub fn note(&self, path: &str) -> Option<Rc<NoteRecord>> {
        self.notes.borrow().get(path).cloned()
    }

    /// A note's text with CRLF normalised to LF — see [`VaultSource::read_note`].
    pub async fn read_note(&self, path: &str) -> Result<String> {
        self.source.read_note(path).await
    }

    /// Resolve a link target to a vault path, or `None` when unresolved.
    pub fn resolve(&self, target: &str) -> Option<String> {
        match_path(target, &self.by_path.borrow())
    }

    /// A `FileValue` with accessors bound to this vault.
    ///
    /// Takes `&Rc<Self>` because every accessor is a closure that has to reach
    /// the index, and a closure cannot borrow from a plain `&self` that does not
    /// outlive it. That is why the vault is shared rather than owned.
    pub fn file_value(self: &Rc<Self>, path: &str) -> FileValue {
        // Every accessor is spelled out rather than built by a helper, because each
        // one has a different return type and a helper would have to be generic over
        // a type it cannot name.
        let tags = {
            let vault = Rc::clone(self);
            let path = path.to_string();
            Rc::new(move || vault.tags_for(&path)) as _
        };
        let links = {
            let vault = Rc::clone(self);
            let path = path.to_string();
            Rc::new(move || vault.links_for(&path)) as _
        };
        let embeds = {
            let vault = Rc::clone(self);
            let path = path.to_string();
            Rc::new(move || vault.embeds_for(&path)) as _
        };
        let backlinks = {
            let vault = Rc::clone(self);
            let path = path.to_string();
            Rc::new(move || vault.backlinks_for(&path)) as _
        };
        let properties = {
            let vault = Rc::clone(self);
            let path = path.to_string();
            Rc::new(move || vault.properties_for(&path)) as _
        };
        // `file.ctime` and `file.mtime` read the SAME value, deliberately: a note's
        // creation time is recorded by neither backend, and a `FileStat` carries one
        // instant. Wired to one accessor rather than two so the coupling is visible
        // here rather than implied by two identical bodies.
        let mtime = {
            let vault = Rc::clone(self);
            let path = path.to_string();
            Rc::new(move || vault.instant_for(&path)) as _
        };
        let ctime = Rc::clone(&mtime);
        let size = {
            let vault = Rc::clone(self);
            let path = path.to_string();
            Rc::new(move || vault.size_for(&path)) as _
        };
        let tasks = {
            let vault = Rc::clone(self);
            let path = path.to_string();
            Rc::new(move || vault.tasks_for(&path)) as _
        };
        let vault = Rc::clone(self);
        let resolve = Rc::new(move |target: &str| {
            vault
                .resolve(target)
                .map(|resolved| vault.file_value(&resolved))
        });

        let accessors = FileAccessors {
            tags,
            links,
            embeds,
            backlinks,
            properties,
            ctime,
            mtime,
            size,
            tasks,
            resolve,
            // Obsidian's `file.linksTo` asks whether the OTHER file links here,
            // which is the question `file.backlinks` already answers. The resolver
            // reaches for backlinks, so this stays false rather than paying for a
            // second pass over the index.
            links_to: Rc::new(|_| false),
        };
        FileValue::new(path.to_string(), accessors)
    }

    /// A `FileValue` for `path`, or `None` when the note does not exist.
    pub fn file_for(self: &Rc<Self>, path: &str) -> Option<FileValue> {
        self.note(path).map(|_| self.file_value(path))
    }

    /// `file.tags` covers frontmatter and body, and every element keeps its `#`
    /// prefix -- confirmed against `base:query format=json` on Obsidian 1.13.7,
    /// which emits `"Tags": "#Contacts, #Kontakte"`.
    pub fn tags_for(&self, path: &str) -> Vec<BasesValue> {
        let Some(note) = self.note(path) else {
            return Vec::new();
        };
        let mut out: Vec<BasesValue> = Vec::new();
        // `tags` may be a single string or a list.
        for tag in note
            .frontmatter
            .get("tags")
            .map(BasesValue::to_list)
            .unwrap_or_default()
        {
            let BasesValue::String(text) = &tag else {
                continue;
            };
            // A frontmatter tag may itself be `business-idea` or `#x`; normalise to
            // the `#`-prefixed display form.
            let clean = text.trim().trim_start_matches('#');
            if !clean.is_empty() {
                out.push(BasesValue::String(format!("#{clean}")));
            }
        }
        for tag in &note.parsed.inline_tags {
            out.push(BasesValue::String(format!("#{tag}")));
        }
        dedupe(out)
    }

    /// `file.links` includes links found in frontmatter as well as the body.
    pub fn links_for(&self, path: &str) -> Vec<BasesValue> {
        let Some(note) = self.note(path) else {
            return Vec::new();
        };
        let mut out: Vec<BasesValue> = Vec::new();
        for link in &note.parsed.links {
            if link.embedded {
                continue;
            }
            out.push(self.link_value(&link.target, link.display.as_deref()));
        }
        for key in ["link", "links", "related", "projects", "project"] {
            let Some(value) = note.frontmatter.get(key) else {
                continue;
            };
            for item in value.to_list() {
                let BasesValue::String(text) = &item else {
                    continue;
                };
                out.push(self.link_value(&strip_brackets(text), None));
            }
        }
        dedupe(out)
    }

    pub fn embeds_for(&self, path: &str) -> Vec<BasesValue> {
        let Some(note) = self.note(path) else {
            return Vec::new();
        };
        note.parsed
            .embeds
            .iter()
            .map(|embed: &WikiLink| self.link_value(&embed.target, embed.display.as_deref()))
            .collect()
    }

    /// `file.backlinks` is indexed, not read live -- see `docs/divergences.md`.
    pub fn backlinks_for(&self, path: &str) -> Vec<BasesValue> {
        let target = strip_extension(path);
        let mut out: Vec<BasesValue> = Vec::new();
        // The path list is taken out of the borrow before the loop, because
        // `links_for` reads the same map this iteration walks.
        for other in self.note_paths() {
            if other == path {
                continue;
            }
            // One hit is enough: a note that links here twice is still one
            // backlink, and `any` is what stops at the first.
            if self.links_for(&other).iter().any(|link| {
                link.link_target()
                    .is_some_and(|hit| strip_extension(hit) == target)
            }) {
                out.push(BasesValue::String(other));
            }
        }
        out
    }

    /// `file.tasks` is a documented extension, not part of the official surface.
    pub fn tasks_for(&self, path: &str) -> Vec<BasesValue> {
        let Some(note) = self.note(path) else {
            return Vec::new();
        };
        note.parsed.tasks.iter().map(task_value).collect()
    }

    /// Wrap a link target as a link, resolving it when possible.
    pub fn link_value(&self, target: &str, display: Option<&str>) -> BasesValue {
        BasesValue::Link {
            target: target.to_string(),
            display: display.map(str::to_string),
            resolved: self.resolve(target),
        }
    }

    /// `file.properties` for a note, or an empty map for one that is not indexed.
    fn properties_for(&self, path: &str) -> BTreeMap<String, BasesValue> {
        self.note(path)
            .map(|note| (*note.frontmatter).clone())
            .unwrap_or_default()
    }

    /// The instant `file.mtime` and `file.ctime` both report, or the epoch for a
    /// note that is not indexed.
    fn instant_for(&self, path: &str) -> BasesDate {
        let mtime = self.note(path).map(|note| note.mtime).unwrap_or_else(epoch);
        BasesDate(mtime)
    }

    fn size_for(&self, path: &str) -> u64 {
        self.note(path).map(|note| note.size).unwrap_or(0)
    }
}

/// First registration wins, which is what breaks a tie in link resolution.
fn register(index: &mut PathIndex, path: &str, key: &str) {
    index.insert_if_absent(key, path);
}

fn epoch() -> DateTime<FixedOffset> {
    DateTime::from_timestamp_millis(0)
        .expect("the epoch is representable")
        .fixed_offset()
}

/// Drop repeats, keeping the first occurrence.
///
/// Keyed on the value's own rendered text, which is what every kind of value
/// here renders to except one: the original keyed on `String(value)`, and
/// `String()` of a link object is `"[object Object]"`, so its `file.links` and
/// `file.embeds` collapsed to a single element however many links a note had.
/// Keying on the rendered text is the evident intent, and the corpus never has
/// two links on one note, so no assertion in either suite can tell the two
/// implementations apart.
fn dedupe(values: Vec<BasesValue>) -> Vec<BasesValue> {
    let mut seen: BTreeMap<String, ()> = BTreeMap::new();
    let mut out: Vec<BasesValue> = Vec::new();
    for value in values {
        if seen.insert(value.to_display_string(), ()).is_none() {
            out.push(value);
        }
    }
    out
}

/// Convert frontmatter values that Obsidian treats as links.
///
/// A property value written as `[[Some Note]]` or `[[Some Note|Alias]]` is a Link
/// object in Bases, compared by resolved target rather than by text. Without this
/// coercion `project.contains(link(this.file.name))` silently fails, because one
/// side would be a link and the other a plain string.
///
/// Called BEFORE the path index is populated, so `resolved` is always `None` for
/// these links. That is the original's order and it is preserved: `file.links`
/// re-resolves the same targets at query time, and the parity suite was
/// calibrated against that. Changing it here would change query output, so it is
/// a change for the divergence registry rather than a cleanup.
pub fn coerce_frontmatter(
    data: &BTreeMap<String, BasesValue>,
    vault: &Vault,
) -> BTreeMap<String, BasesValue> {
    data.iter()
        .map(|(key, value)| (key.clone(), coerce_value(value, vault)))
        .collect()
}

fn coerce_value(value: &BasesValue, vault: &Vault) -> BasesValue {
    match value {
        BasesValue::List(items) => {
            BasesValue::List(items.iter().map(|item| coerce_value(item, vault)).collect())
        }
        BasesValue::String(text) => {
            let trimmed = text.trim();
            if let Some((target, display)) = wikilink(trimmed) {
                return vault.link_value(&target, display.as_deref());
            }
            if let Some((label, target)) = mdlink(trimmed) {
                if !has_scheme(&target) && !target.starts_with('#') {
                    let bare = target.strip_suffix(".md").unwrap_or(&target);
                    // `mdlink[1] || null` in the original: an EMPTY label is no
                    // label.
                    let display = if label.is_empty() {
                        None
                    } else {
                        Some(label.as_str())
                    };
                    return vault.link_value(bare, display);
                }
            }
            value.clone()
        }
        other => other.clone(),
    }
}

/// `[[Target]]`, `[[Target|Display]]` and `[[Target#Subpath]]`.
fn wikilink(text: &str) -> Option<(String, Option<String>)> {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    let captures = PATTERN
        .get_or_init(|| {
            Regex::new(r"^\[\[([^\]|#]+)(?:#[^\]|]*)?(?:\|([^\]]*))?\]\]$")
                .expect("the wikilink pattern is valid")
        })
        .captures(text)?;
    let target = captures.get(1)?.as_str().to_string();
    let display = captures.get(2).map(|m| m.as_str().to_string());
    Some((target, display))
}

/// `[Label](target)`.
fn mdlink(text: &str) -> Option<(String, String)> {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    let captures = PATTERN
        .get_or_init(|| {
            Regex::new(r"^\[([^\]]*)\]\(([^)\s]+)\)$").expect("the mdlink pattern is valid")
        })
        .captures(text)?;
    Some((
        captures.get(1)?.as_str().to_string(),
        captures.get(2)?.as_str().to_string(),
    ))
}

/// Whether a markdown-link target is a URL, which is never a note in this vault.
///
/// One or more ASCII letters, then `://`, case-insensitively. A relative target
/// with no `://` at all is not a scheme however word-like it is, which is why the
/// separator has to be found rather than assumed.
fn has_scheme(target: &str) -> bool {
    let Some((scheme, _)) = target.split_once("://") else {
        return false;
    };
    !scheme.is_empty() && scheme.chars().all(|c| c.is_ascii_alphabetic())
}

/// `[[Target]]` reduced to the target, for a frontmatter list entry.
fn strip_brackets(text: &str) -> String {
    let trimmed = text.trim();
    match wikilink(trimmed) {
        Some((target, _)) => target,
        None => trimmed.to_string(),
    }
}

/// A task's shape follows the Obsidian CLI's `tasks format=json` output:
/// `{ status, text, file, line }`.
///
/// `file` and `line` are dropped by the original, which is why a task round-trips
/// through a query as three fields rather than four.
pub fn task_value(task: &TaskItem) -> BasesValue {
    let mut out = BTreeMap::new();
    out.insert(
        "status".to_string(),
        BasesValue::String(if task.checked {
            "x".to_string()
        } else {
            " ".to_string()
        }),
    );
    out.insert("text".to_string(), BasesValue::String(task.text.clone()));
    out.insert("completed".to_string(), BasesValue::Bool(task.checked));
    BasesValue::Namespace(Rc::new(out))
}
