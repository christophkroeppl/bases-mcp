//! Property display labels.
//!
//! Obsidian does not render a Property ID as a column header. It renders a
//! *display label* derived from the ID, and that label is what appears both as the
//! `format=json` object key and as the `format=md` table header. Getting this
//! wrong is invisible in the UI and fatal to the parity suite, so the whole rule
//! lives here and every renderer calls it.
//!
//! The labels below were probed directly against `obsidian base:query` on Obsidian
//! 1.13.7 rather than inferred from the docs. The docs publish types and
//! descriptions for `file.*` but no label table, and the labels are not
//! mechanical: `file.name` becomes `file name` while `file.folder` becomes `folder`,
//! dropping the namespace entirely.
//!
//! The fallback is "strip the namespace prefix and otherwise leave the ID alone":
//! `formula.priority_display` labels as `priority_display`, NOT `Priority Display`.
//! Verified with a probe base carrying no `displayName`.
//!
//! An explicit `properties.<id>.displayName` always wins, and is used verbatim --
//! Obsidian does not title-case it either (`PR Priority` stays `PR Priority`).

use crate::base::parse::BaseFile;

/// `file.*` labels that are not derivable from the ID.
///
/// Every entry was confirmed by a live `format=json` probe. Keys absent here fall
/// through to the generic rule and label as their bare segment, which is also
/// correct for the undocumented `file.basename` -> `file base name` only because
/// it is listed explicitly.
pub const FILE_LABELS: [(&str, &str); 14] = [
    ("file.name", "file name"),
    ("file.basename", "file base name"),
    ("file.path", "file path"),
    // The root folder is "/", not "", and a folder drops the namespace prefix.
    ("file.folder", "folder"),
    ("file.ext", "file extension"),
    ("file.size", "file size"),
    ("file.ctime", "created time"),
    ("file.mtime", "modified time"),
    ("file.tags", "file tags"),
    ("file.links", "file links"),
    ("file.embeds", "file embeds"),
    ("file.backlinks", "file backlinks"),
    ("file.properties", "properties"),
    ("file.file", "file"),
];

/// Strip a leading `note.` / `file.` / `formula.` namespace.
pub fn strip_namespace(id: &str) -> &str {
    for prefix in ["note.", "file.", "formula."] {
        if let Some(bare) = id.strip_prefix(prefix) {
            return bare;
        }
    }
    id
}

/// The label Obsidian shows for a property, ignoring any configured `displayName`.
/// Callers must consult [`display_name_for`] first.
pub fn label_for_id(canonical_id: &str) -> &str {
    FILE_LABELS
        .iter()
        .find(|(id, _)| *id == canonical_id)
        .map(|(_, label)| *label)
        .unwrap_or_else(|| strip_namespace(canonical_id))
}

/// The header text for a column: a configured `displayName` when the base sets one
/// for this property, otherwise the probed default label.
///
/// Lookup is by CANONICAL ID only. Probed on Obsidian 1.13.7: a base with
/// `properties: {note.status: {displayName: PrefixedKeyed}}` labels a column ordered
/// as the bare `status`, while the same base keyed as
/// `properties: {status: ...}` is IGNORED and the column labels as `status`. So the
/// prefixed spelling is the one Obsidian matches on, and a bare key is dead config
/// rather than a fallback.
pub fn display_name_for(base: &BaseFile, id: &str) -> String {
    let canonical = canonical_id_of(id);
    base.properties
        .get(&canonical)
        .and_then(|config| config.display_name.clone())
        .unwrap_or_else(|| label_for_id(&canonical).to_string())
}

/// Normalise a Property ID to its `note.`-prefixed canonical spelling.
pub fn canonical_id_of(id: &str) -> String {
    for prefix in ["note.", "file.", "formula."] {
        if id.starts_with(prefix) {
            return id.to_string();
        }
    }
    format!("note.{id}")
}