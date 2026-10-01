//! Link resolution, matching Obsidian's rules.
//!
//! The vault index registers each note under four keys — full path, path
//! without extension, basename, basename without extension — and first
//! registration wins. Because the index is built from a sorted path list, that
//! ordering is deterministic, and it is what decides an ambiguous link.
//!
//! There are TWO rules here, and the original's header collapses them into one:
//!
//!   - An EXACT spelling is answered by first registration wins, over a sorted
//!     path list. So `[[Readme]]` with both `Projects/Readme.md` and
//!     `Readme.md` in the vault reaches `Projects/Readme.md` — the first in
//!     sorted order, which is not the shortest path.
//!   - A spelling that matches NO key exactly falls through to a folded
//!     comparison, and THERE the answer is the SHORTEST path, counted in UTF-16
//!     code units. Ties go to the first registration again.
//!
//! Both are pinned separately in `tests/vault_equivalence.rs`, and the second
//! one is Obsidian's documented tie-break.

use std::collections::HashMap;

use unicode_normalization::UnicodeNormalization;

use crate::value::strip_extension;

/// ASCII-fold and lowercase, so `Página` matches `Pagina`.
///
/// NFKD first, then only the combining marks in `U+0300..=U+036F` are dropped.
/// Dropping *every* combining mark would also erase marks a decomposed
/// character legitimately carries, and `to_lowercase()` alone would leave `é`
/// as `é` where JavaScript's `String.prototype.normalize` has already taken it
/// apart — which is why `to_lowercase()` by itself does not match the original.
pub fn fold_key(s: &str) -> String {
    s.nfkd()
        .filter(|c| !is_combining_diacritic(*c))
        .collect::<String>()
        .to_lowercase()
}

/// Whether a character is in the `U+0300..=U+036F` block the original strips.
///
/// The block, and not `is_combining_mark`, because the original's regex is
/// explicit about its range and a Greek `U+1FBD` should survive the fold.
fn is_combining_diacritic(c: char) -> bool {
    matches!(c, '\u{0300}'..='\u{036F}')
}

/// The vault's note index, keyed by every spelling a link may use.
///
/// Insertion-ordered, and the order is load-bearing rather than incidental:
/// [`match_path`]'s folded fallback keeps the FIRST of several equally short
/// candidates, so a hash map's arbitrary iteration order would make ambiguous
/// resolution differ between runs on the same vault. JavaScript's `Map` is
/// insertion-ordered and this has to be too.
#[derive(Debug, Default)]
pub struct PathIndex {
    keys: Vec<String>,
    paths: Vec<String>,
    slots: HashMap<String, usize>,
}

impl PathIndex {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a spelling. First registration wins.
    pub fn insert_if_absent(&mut self, key: &str, path: &str) {
        if key.is_empty() || self.slots.contains_key(key) {
            return;
        }
        self.slots.insert(key.to_string(), self.keys.len());
        self.keys.push(key.to_string());
        self.paths.push(path.to_string());
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.slots.get(key).map(|slot| self.paths[*slot].as_str())
    }

    pub fn len(&self) -> usize {
        self.keys.len()
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// Every registration, in registration order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.keys.iter().zip(self.paths.iter()).map(|(k, p)| (k.as_str(), p.as_str()))
    }
}

/// Resolve a link target against the index.
///
/// Handles the forms Obsidian accepts: `Note`, `Note.md`, `folder/Note` and
/// `folder/Note.md`. Aliases are deliberately NOT consulted — a bare
/// `[[Alias]]` does not resolve in Obsidian.
pub fn match_path(target: &str, by_path: &PathIndex) -> Option<String> {
    if target.is_empty() {
        return None;
    }
    if let Some(direct) = by_path.get(target) {
        return Some(direct.to_string());
    }
    let without_ext = strip_extension(target);
    if let Some(found) = by_path.get(&without_ext) {
        return Some(found.to_string());
    }
    // Fall back to a folded comparison so case and diacritics do not matter.
    let wanted = fold_key(&without_ext);
    let mut best: Option<&str> = None;
    for (key, path) in by_path.iter() {
        if fold_key(&strip_extension(key)) != wanted {
            continue;
        }
        // `map_or(true, ..)` rather than `is_none_or(..)`: the latter is newer than the
        // crate's MSRV, and clippy is right to say so.
        if best.map_or(true, |current| link_length(path) < link_length(current)) {
            best = Some(path);
        }
    }
    best.map(str::to_string)
}

/// The length the original compared candidates with: `String.length`, a count of
/// UTF-16 code units.
///
/// Not `str::len()`, which counts BYTES, and not `chars().count()`, which counts
/// code points. `Café.md` is 7 units and 7 characters but 8 bytes, and the
/// difference decides which of two candidates a folded link picks when both fold
/// to the same name. A note with an emoji in it is where the two spellings part
/// company for real.
fn link_length(path: &str) -> usize {
    path.chars().map(char::len_utf16).sum()
}

/// Whether a link target points at `path`. Used for link equality, which the
/// docs define as "equivalent as long as they point to the same file".
pub fn points_at(link_target: &str, path: &str) -> bool {
    let a = fold_key(&strip_extension(link_target));
    let b = fold_key(&strip_extension(path));
    a == b || a.ends_with(&format!("/{b}")) || b.ends_with(&format!("/{a}"))
}
