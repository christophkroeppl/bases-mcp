//! The Projection, and the reconciler that takes an edited one apart.
//!
//! A Projection is the agent-facing rendering of a note: each Base region
//! replaced by a ```base-rendered fence carrying its provenance. The fence
//! language is deliberately NOT `base` -- Obsidian treats a ```base fence as
//! live base YAML, so rendered markdown inside one would fail to parse. Any
//! other language renders as an inert code block, which is exactly what a fence
//! the agent is about to hand back has to be.
//!
//! A Projection is NEVER written to disk. Writing it would leave a dead copy of
//! the rendered rows in the note and destroy the live region. Patches are
//! applied by RECONCILING the agent's text against the original segments, never
//! by writing the projection, and that is what [`reconcile_note`] does.
//!
//! It works off [`crate::note`]'s segmentation rather than re-parsing anything:
//! a Base region is already a byte span there, and re-finding it with a second
//! parser is how a note gets written back with one of its regions silently
//! dropped.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use crate::note::{fence_attrs, is_base_region, parse_note_with_embeds, ParsedNote, Segment};

/// The fence language for a rendered base. Deliberately not `base`.
///
/// The spelling is duplicated from [`crate::note::RENDER_FENCE_LANG`] rather than
/// imported, because the note parser has to recognise this fence before the
/// renderer can emit it. `tests/render.rs` pins the two together, so a rename
/// cannot leave them disagreeing.
pub const RENDER_FENCE: &str = "base-rendered";

/// Where a rendered region came from, and how much of it there was.
///
/// Every field is optional because the shapes are genuinely different: a live
/// inline fence has a view but no Base path, a file embed has a path and
/// possibly a view, and only a host-bound render has a context.
///
/// Serialized for `get_note`'s `regions`, which is the provenance an agent needs
/// to reason about a fence it is about to hand back. Absent fields are skipped
/// rather than emitted as null, because a fence info string never carried them
/// and an agent comparing the two must not see a difference that is not there.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct FenceProvenance {
    /// The `.base` file the region came from, when it was a file embed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// The view name rendered.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub view: Option<String>,
    /// The host note bound to `this`, when one was supplied.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
    /// Rows the view resolved to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rows: Option<usize>,
}

impl FenceProvenance {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_path(mut self, path: impl Into<String>) -> Self {
        self.path = Some(path.into());
        self
    }

    pub fn with_view(mut self, view: impl Into<String>) -> Self {
        self.view = Some(view.into());
        self
    }

    pub fn with_context(mut self, context: impl Into<String>) -> Self {
        self.context = Some(context.into());
        self
    }

    pub fn with_rows(mut self, rows: usize) -> Self {
        self.rows = Some(rows);
        self
    }
}

/// The rendered body that stands in for one Base region.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedRegion {
    /// The fence block, ready to splice into a Projection. Carries no trailing
    /// newline; [`project`] adds it.
    pub text: String,
    pub provenance: FenceProvenance,
}

/// Escape a value for the quoted attribute form of a fence info string.
///
/// Backslash first: escaping the quote first would double-escape the backslash
/// that escaping the quote introduced.
fn escape_attr(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Build the fence info string, e.g. `base-rendered path="T.base" view="All"`.
///
/// A bare language, then the attributes in a fixed order, so the same render
/// always produces the same bytes.
pub fn fence_info(provenance: &FenceProvenance) -> String {
    let mut parts = vec![RENDER_FENCE.to_string()];
    for (key, value) in [
        ("path", &provenance.path),
        ("view", &provenance.view),
        ("context", &provenance.context),
    ] {
        if let Some(value) = value {
            parts.push(format!("{key}=\"{}\"", escape_attr(value)));
        }
    }
    if let Some(rows) = provenance.rows {
        parts.push(format!("rows=\"{rows}\""));
    }
    parts.join(" ")
}

/// Parse a fence info string back into provenance, or `None` for any other.
///
/// A `rows` attribute that is not a number is dropped rather than recorded as
/// zero: a count nobody can read is worse than no count.
pub fn parse_fence_info(info: &str) -> Option<FenceProvenance> {
    let trimmed = info.trim();
    if !trimmed.starts_with(RENDER_FENCE) {
        return None;
    }
    let mut out = FenceProvenance::new();
    for (key, value) in fence_attrs(trimmed) {
        match key.as_str() {
            "path" => out.path = Some(value),
            "view" => out.view = Some(value),
            "context" => out.context = Some(value),
            "rows" => out.rows = value.parse().ok(),
            // An unknown attribute is not provenance this port knows how to
            // round-trip, and guessing would put it in the wrong field.
            _ => {}
        }
    }
    Some(out)
}

/// Wrap rendered markdown in a provenance fence.
pub fn wrap_in_fence(body: &str, provenance: &FenceProvenance) -> RenderedRegion {
    let text = [
        format!("```{}", fence_info(provenance)),
        body.trim_end_matches('\n').to_string(),
        "```".to_string(),
    ]
    .join("\n");
    RenderedRegion {
        text,
        provenance: provenance.clone(),
    }
}

/// Identifies one Base region within a note.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaseRegionRef {
    /// Index of the region among the note's Base regions.
    pub index: usize,
    /// The `.base` file, for an embed or a rendered fence. `None` for a live
    /// inline fence, which carries YAML instead of a path.
    pub base_path: Option<String>,
    /// The `#View` selector the region pinned, if any.
    pub view_name: Option<String>,
    /// Inline base YAML, for a ```base fence.
    pub yaml: Option<String>,
    /// True for a ```base-rendered fence: a region handed back by a Projection.
    pub rendered: bool,
    /// Byte span in the original note.
    pub start: usize,
    /// Byte span end in the original note.
    pub end: usize,
}

/// Identify one Base region.
///
/// A rendered fence carries the Base it came from on its info string, which is
/// what lets it be paired with the region it replaced. A live ```base fence
/// carries its YAML instead and is matched on that.
fn segment_ref(segment: &Segment) -> BaseRegionRef {
    BaseRegionRef {
        index: 0,
        base_path: segment.base_path().map(str::to_string),
        view_name: segment.view_name().map(str::to_string),
        yaml: segment.yaml().map(str::to_string),
        rendered: segment.is_rendered(),
        start: segment.start(),
        end: segment.end(),
    }
}

/// Every Base region in a note, in document order and 0-indexed.
pub fn base_regions(note: &ParsedNote) -> Vec<BaseRegionRef> {
    note.segments
        .iter()
        .filter(|segment| is_base_region(segment))
        .enumerate()
        .map(|(index, segment)| {
            let mut region = segment_ref(segment);
            region.index = index;
            region
        })
        .collect()
}

/// Build the Projection of a note by substituting each Base region.
///
/// `render` receives a region's identity and returns the markdown that stands in
/// for it -- [`wrap_in_fence`]'s output, for the Projection proper. It is a
/// closure rather than an argument list because resolving a region reads the
/// target `.base` file, which is the caller's business, not this module's.
pub fn project<F>(note: &ParsedNote, mut render: F) -> String
where
    F: FnMut(&BaseRegionRef) -> String,
{
    let mut regions = base_regions(note).into_iter();
    let mut out = String::new();
    for (at, segment) in note.segments.iter().enumerate() {
        if !is_base_region(segment) {
            out.push_str(segment.raw());
            continue;
        }
        let region = regions.next().expect("every Base region has a ref");
        out.push_str(&render(&region));
        if !ends_its_own_line(&out, note.segments.get(at + 1)) {
            out.push('\n');
        }
    }
    out
}

/// Whether a rendered region leaves the cursor at a line boundary.
///
/// A Base region occupies whole lines, so its replacement has to end with a
/// newline or it runs into the prose after it. It does NOT get one added when
/// the boundary is already there, and it usually is: the prose following a
/// region begins with the newline that ended the region's own line, and a region
/// at the end of a note has no prose after it at all. Adding one anyway left a
/// stray newline in every Projection, and since `write_note` reconciles rather
/// than writes, that newline came back out as a spurious edit to the host note.
fn ends_its_own_line(out: &str, next: Option<&Segment>) -> bool {
    if out.ends_with('\n') {
        return true;
    }
    match next {
        None => true,
        Some(following) => following.raw().starts_with('\n'),
    }
}

/// A Base region an agent tried to change, and what to do instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefusedRegion {
    /// The region's index among the edited note's Base regions.
    pub index: usize,
    /// The base the region belongs to, when it is an embed.
    pub base_path: Option<String>,
    pub reason: String,
    /// Actionable guidance, including what the agent should do instead.
    pub guidance: String,
}

/// The outcome of reconciling an edited note against the stored original.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReconcileResult {
    /// The note text to write, with Base regions restored.
    pub text: String,
    /// Base regions the agent tried to change.
    pub refused: Vec<RefusedRegion>,
    /// True when the agent deleted a Base region outright.
    pub removed_region: bool,
}

/// Reconcile an agent's edited note against the stored original.
///
/// Rules, in order:
///  1. A Base region present in both and unchanged is kept as-is.
///  2. A Base region present in both but modified is RESTORED to the original.
///     For an embed or a live fence the attempt is also reported -- adding rows
///     to a rendered base is not a supported edit: rows come from notes, not from
///     the base. A rendered fence is restored SILENTLY, because round-tripping a
///     Projection is the designed flow.
///  3. A Base region the agent DELETED is restored, and reported.
///  4. A Base region the agent ADDED is dropped, and reported.
///  5. All non-base changes are applied.
///
/// The result therefore applies the agent's real edits while guaranteeing the
/// Base regions survive byte for byte.
pub fn reconcile_note(note_path: &str, original: &str, edited: &str) -> ReconcileResult {
    let before = parse_note_with_embeds(note_path, original);
    let after = parse_note_with_embeds(note_path, edited);

    let original_regions = base_regions(&before);
    let edited_regions = base_regions(&after);

    // Each edited region paired with the ORIGINAL region it replaced, and the
    // originals claimed so far.
    let mut pairs: BTreeMap<usize, &str> = BTreeMap::new();
    let mut claimed: BTreeSet<usize> = BTreeSet::new();

    // First pass, by position: an embed and a live fence are identified by their
    // own text, so two of them at the same index are the same region.
    for (index, region) in edited_regions.iter().enumerate() {
        let Some(original_region) = original_regions.get(index) else {
            continue;
        };
        if !same_region(region, original_region) {
            continue;
        }
        pairs.insert(index, &original[original_region.start..original_region.end]);
        claimed.insert(index);
    }

    // Second pass, by Base path: a rendered fence whose position shifted is
    // matched on the `path=` attribute in its info string. That attribute is the
    // ONLY link back to the region it came from -- the fence body is rendered
    // rows, not YAML -- so matching on position alone would mispair a note whose
    // regions were reordered, and would restore one region's text into another's
    // place.
    for (index, region) in edited_regions.iter().enumerate() {
        if pairs.contains_key(&index) || !region.rendered {
            continue;
        }
        let Some(path) = region.base_path.as_deref() else {
            continue;
        };
        let hit = original_regions
            .iter()
            .enumerate()
            .find(|(other, candidate)| {
                !claimed.contains(other) && candidate.base_path.as_deref() == Some(path)
            });
        let Some((other, hit_ref)) = hit else {
            continue;
        };
        pairs.insert(index, &original[hit_ref.start..hit_ref.end]);
        claimed.insert(other);
    }

    let missing: Vec<&BaseRegionRef> = original_regions
        .iter()
        .enumerate()
        .filter(|(index, _)| !claimed.contains(index))
        .map(|(_, region)| region)
        .collect();

    // Rebuild: walk the edited segments, restore the paired regions, and drop
    // anything the agent added.
    let mut refused: Vec<RefusedRegion> = Vec::new();
    let mut out = String::new();
    let mut regions = edited_regions.iter();
    for segment in &after.segments {
        if !is_base_region(segment) {
            out.push_str(segment.raw());
            continue;
        }
        let region = regions.next().expect("every Base region has a ref");
        let Some(original_raw) = pairs.get(&region.index) else {
            refused.push(RefusedRegion {
                index: region.index,
                base_path: embed_base_path(segment),
                reason: "A new base region was inserted.".to_string(),
                guidance: "Adding a base region to a note is not supported by default. Edit the \
.base file itself, then re-read this note."
                    .to_string(),
            });
            continue;
        };
        // A rendered fence is replaced SILENTLY. Round-tripping a Projection --
        // read the note, edit the prose, write it back -- is the DESIGNED flow,
        // so reporting a refusal every time would mark the happy path
        // `partial-with-errors` and teach an agent to ignore refusals, which
        // costs more than it buys. We cannot tell an untouched fence from an
        // edited one here anyway: the rendered rows were never the source of
        // truth, and the live region is restored byte for byte either way.
        if !segment.is_rendered() && *original_raw != segment.raw() {
            refused.push(RefusedRegion {
                index: region.index,
                base_path: embed_base_path(segment),
                reason: "The rendered base region was modified.".to_string(),
                guidance: "Rows in a base come from notes, not from the base file. To add a row, \
create a note whose properties satisfy the base's filter, then re-read the host note. Use \
add_note_to_base to be guided through creating a matching note."
                    .to_string(),
            });
        }
        out.push_str(original_raw);
    }

    // Re-insert any region the agent deleted, at its original position.
    let removed_region = !missing.is_empty();
    let mut text = out;
    for region in missing {
        let raw = original[region.start..region.end].to_string();
        text = match newline_at_or_after(&text, region.start) {
            // Spliced in just past the newline the region's own offset pointed
            // at, so it lands on its own line as it did.
            Some(anchor) => format!("{}\n{raw}\n{}", &text[..anchor + 1], &text[anchor + 1..]),
            None => format!("{text}\n{raw}\n"),
        };
        refused.push(RefusedRegion {
            index: region.index,
            base_path: region.base_path.clone(),
            reason: "The base region was removed.".to_string(),
            guidance: "Base regions are never removed by a note edit. Restore the embed, or \
delete the base region deliberately outside this tool."
                .to_string(),
        });
    }

    ReconcileResult {
        text,
        refused,
        removed_region,
    }
}

/// Do two regions name the same thing?
///
/// The Base has to match when both sides name one, and the pinned view has to
/// match either way -- `![[T.base#All]]` and `![[T.base#ByPriority]]` are two
/// different regions of the same file.
///
/// Two INLINE regions with no Base behind them are the awkward case. A live
/// fence carries its YAML and a rendered fence carries no path, so the YAML
/// check cannot pair them and neither can the path check in the second pass: it
/// needs a `path=` attribute that an inline region never has. Position plus the
/// pinned view is the only identity left, and it is enough -- the region either
/// came from exactly here or it did not come from this note at all.
fn same_region(edited: &BaseRegionRef, original: &BaseRegionRef) -> bool {
    if edited.view_name != original.view_name {
        return false;
    }
    match (&edited.base_path, &original.base_path) {
        (Some(edited), Some(original)) => edited == original,
        (None, None) => true,
        _ => false,
    }
}

/// The `.base` file a region belongs to, when it is an embed.
///
/// An inline fence has no file behind it: a live one carries YAML instead, and a
/// rendered one is only ever an intermediate form on its way back to the region
/// it came from, so naming a Base for it would point at something the agent
/// never touched.
fn embed_base_path(segment: &Segment) -> Option<String> {
    segment.base_embed().map(|embed| embed.base_path.clone())
}

/// The byte offset of the first newline at or after `from`, or `None`.
///
/// Byte offsets, because `from` is a byte offset: the testing vault's umlauts
/// make a character offset wrong here, and `text[from..]` would panic rather
/// than merely disagree.
fn newline_at_or_after(text: &str, from: usize) -> Option<usize> {
    let mut start = from.min(text.len());
    while start < text.len() && !text.is_char_boundary(start) {
        start += 1;
    }
    text.get(start..)?.find('\n').map(|offset| start + offset)
}
