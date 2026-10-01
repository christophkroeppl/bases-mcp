//! The Resolver: the single entry point the MCP tools call.
//!
//! Everything here is pure with respect to the vault contents — it reads
//! through the [`crate::vault::VaultSource`] and never writes, except in
//! [`Resolver::write_note`] and [`Resolver::create_note`].
//!
//! The vault is held as an `Rc<Vault>` rather than owned, for the same reason
//! every `file.*` accessor in [`crate::vault::Vault`] is: those accessors are
//! closures that have to reach back into the index from inside the evaluator.
//! That makes the Resolver `!Send`, which is why the MCP handler is built in
//! rmcp's `local` mode — see `src/tools.rs`.
//!
//! Two rendering surfaces live here and they are NOT interchangeable:
//! [`Resolver::render`] is the flat, CLI-parity `resolve_base --format markdown`
//! output, and the `base-rendered` fence a note's Base regions become is
//! STRUCTURED. Byte parity exists on exactly one of them, and the other is the
//! one an agent reads.

use std::path::Path;
use std::rc::Rc;

use serde_json::{Map, Value as Json, json};

use crate::base::{
    BaseFile, BaseView, QueryOptions, QueryResult, canonical, parse_base, query_base,
};
use crate::drafts::{AddNoteOptions, AddNoteToBaseResult};
use crate::error::{BasesError, Result};
use crate::labels::display_name_for;
use crate::note::{parse_note_with_embeds, serialise};
use crate::render::markdown::{RenderStyle, render_markdown};
use crate::render::project::{
    BaseRegionRef, FenceProvenance, ReconcileResult, RefusedRegion, RenderedRegion, base_regions,
    project, reconcile_note, wrap_in_fence,
};
use crate::value::{BasesValue, strip_extension};
use crate::vault::{FsVaultSource, Vault, VaultSource};

/// One `.base` file, with the views it declares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaseSummary {
    pub path: String,
    pub views: Vec<ViewSummary>,
}

/// One view, by name and layout type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewSummary {
    pub name: String,
    /// The `type:` key, under the name Obsidian calls it a view type.
    pub view_type: String,
}

/// A note, and the Projection that replaces its Base regions.
#[derive(Debug, Clone, PartialEq)]
pub struct NoteView {
    pub path: String,
    /// The note as stored.
    pub raw: String,
    /// The agent-facing Projection, with Base regions rendered.
    pub content: String,
    /// One entry per Base region, in document order.
    pub regions: Vec<FenceProvenance>,
}

/// What `get_note` was asked for.
///
/// The query options ride along because a note's Base regions are resolved with
/// the same machinery `resolve_base` uses, and `this` binds to the note itself
/// unless the caller says otherwise.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NoteOptions {
    /// Return the stored text untouched, with no Projection.
    pub raw: bool,
    /// Host note overriding the note being read.
    pub context: Option<String>,
    /// View overriding the one a region pinned.
    pub view: Option<String>,
}

impl NoteOptions {
    /// Just the projection, for a caller that wants no query options at all.
    pub fn projection() -> Self {
        Self::default()
    }

    /// Just the stored text.
    pub fn raw() -> Self {
        Self { raw: true, ..Self::default() }
    }

    fn query(&self) -> QueryOptions {
        QueryOptions { context: self.context.clone(), view: self.view.clone() }
    }
}

/// A note that links here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Backlink {
    pub path: String,
    pub title: String,
}

/// The outcome of applying an agent's edit to a note.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteNoteResult {
    pub path: String,
    /// The note text as written, with Base regions restored.
    pub text: String,
    /// Base regions the agent tried to change, insert or delete.
    pub refused: Vec<RefusedRegion>,
    /// True when the agent deleted a Base region outright.
    pub removed_region: bool,
}

impl From<(String, ReconcileResult)> for WriteNoteResult {
    fn from((path, result): (String, ReconcileResult)) -> Self {
        Self {
            path,
            text: result.text,
            refused: result.refused,
            removed_region: result.removed_region,
        }
    }
}

/// The Resolver.
#[derive(Debug)]
pub struct Resolver {
    vault: Rc<Vault>,
    drafts: crate::drafts::DraftStore,
}

impl Resolver {
    /// Index a vault from any backend.
    pub async fn open(source: Box<dyn VaultSource>) -> Result<Self> {
        let vault = Rc::new(Vault::new(source));
        vault.load().await?;
        Ok(Self { vault, drafts: crate::drafts::DraftStore::default() })
    }

    /// Index a local vault directory.
    pub async fn open_dir(dir: impl AsRef<Path>) -> Result<Self> {
        let source = FsVaultSource::new(dir)?;
        Self::open(Box::new(source)).await
    }

    /// The index every tool reads through.
    pub fn vault(&self) -> &Rc<Vault> {
        &self.vault
    }

    /// The backend, for writes that must bypass the snapshot.
    pub fn backend(&self) -> &dyn VaultSource {
        self.vault.backend()
    }

    /// The backend, shared, so a verification vault can splice a note into it.
    ///
    /// The overlay the verify step builds wraps the REAL backend rather than a
    /// copy of the listing: link resolution, tag collection and frontmatter
    /// coercion are exactly the machinery a filter like
    /// `project.contains(link(this.file.name))` depends on, and a hand-built
    /// stand-in would be a second, drifting implementation of all three.
    pub fn shared_backend(&self) -> Rc<dyn VaultSource> {
        self.vault.shared_source()
    }

    /// The draft store, for the `add_note_to_base` handshake.
    pub fn drafts(&self) -> &crate::drafts::DraftStore {
        &self.drafts
    }

    /// Re-read the vault after an external change.
    pub async fn reload(&self) -> Result<()> {
        self.vault.reload().await
    }

    // -- reads ---------------------------------------------------------------

    /// Every base in the vault, with the views each declares.
    ///
    /// A base that fails to parse fails the whole listing rather than being
    /// skipped: an agent asking what exists cannot act on a list with a silent
    /// hole in it, and the error names the file.
    pub async fn list_bases(&self) -> Result<Vec<BaseSummary>> {
        let mut out = Vec::new();
        for path in self.vault.base_paths() {
            let text = self.vault.read_text(&path).await?;
            let base = parse_base(&path, &text)?;
            out.push(BaseSummary {
                path,
                views: base
                    .views
                    .iter()
                    .map(|view| ViewSummary { name: view.name.clone(), view_type: view.view_type.clone() })
                    .collect(),
            });
        }
        Ok(out)
    }

    pub async fn load_base(&self, path: &str) -> Result<BaseFile> {
        let resolved = self.resolve_base_path(path)?;
        let text = self.vault.read_text(&resolved).await?;
        parse_base(&resolved, &text)
    }

    /// The vault path of a base, from whatever the caller called it.
    ///
    /// An exact indexed path wins, then link resolution, and then a refusal
    /// naming what was asked for.
    pub fn resolve_base_path(&self, path: &str) -> Result<String> {
        if let Some(exact) = self.vault.base_paths().into_iter().find(|candidate| candidate == path) {
            return Ok(exact);
        }
        match self.vault.resolve(path) {
            Some(resolved) if resolved.ends_with(".base") => Ok(resolved),
            _ => Err(BasesError::new(format!("Base file not found: {path}")).with_note(path)),
        }
    }

    pub async fn view_names(&self, path: &str) -> Result<Vec<String>> {
        Ok(self.load_base(path).await?.views.into_iter().map(|view| view.name).collect())
    }

    pub async fn query(&self, path: &str, options: &QueryOptions) -> Result<QueryResult> {
        let base_path = self.resolve_base_path(path)?;
        let base = self.load_base(&base_path).await?;
        query_base(&self.vault, &base_path, &base, options)
    }

    /// Render a base view as the flat, CLI-parity markdown table.
    ///
    /// This is the `resolve_base --format markdown` surface, so it is `Flat` and
    /// byte-comparable with `obsidian base:query format=md`. It is NOT what a
    /// Base region inside a note becomes; that is `Structured`.
    pub async fn render(&self, path: &str, options: &QueryOptions) -> Result<String> {
        let resolved = self.resolve_base_path(path)?;
        let base = self.load_base(&resolved).await?;
        let result = query_base(&self.vault, &resolved, &base, options)?;
        render_markdown(&base, &result, RenderStyle::Flat)
    }

    /// Resolve a note, rendering each Base region into a provenance fence.
    ///
    /// [`NoteOptions::raw`] returns the stored text untouched, which is the
    /// escape hatch for an agent that wants to edit the note itself.
    pub async fn read_note(&self, path: &str, options: NoteOptions) -> Result<NoteView> {
        let resolved = self.resolve_note_path(path)?;
        let Some(record) = self.vault.note(&resolved) else {
            return Err(BasesError::new(format!("Note not found: {path}")).with_note(path));
        };
        let raw = serialise(&record.parsed.segments);

        if options.raw {
            return Ok(NoteView { path: resolved, raw: raw.clone(), content: raw, regions: Vec::new() });
        }

        // Every region is rendered BEFORE the Projection is assembled, because
        // `project` takes a synchronous substitution and resolving a region
        // reads the target `.base` file. `base_regions` yields the regions in
        // document order, which is the order `project` asks for them in, so the
        // rendered queue lines up with the call sites by position.
        let refs = base_regions(&record.parsed);
        let mut rendered: Vec<RenderedRegion> = Vec::with_capacity(refs.len());
        for region in &refs {
            let body = self.render_region(&record.path, region, &options).await?;
            rendered.push(wrap_in_fence(&body, &provenance_for(region, &options)));
        }
        let regions = rendered.iter().map(|region| region.provenance.clone()).collect();

        let mut queue = rendered.into_iter();
        let content = project(&record.parsed, |_| {
            queue.next().expect("one rendered region per Base region").text.clone()
        });

        Ok(NoteView { path: resolved, raw, content, regions })
    }

    /// Render one Base region.
    ///
    /// An embed resolves the target `.base` file; an inline ```base fence
    /// carries its own YAML. Either way `this` binds to the note the region
    /// lives in, unless the caller overrode it.
    async fn render_region(
        &self,
        note_path: &str,
        region: &BaseRegionRef,
        options: &NoteOptions,
    ) -> Result<String> {
        let host = options.context.clone().unwrap_or_else(|| note_path.to_string());
        let mut query_options = options.query();
        query_options.context = Some(host);

        // A live fence carries its own YAML, so there is no file to resolve.
        let Some(base_path) = &region.base_path else {
            let Some(yaml) = &region.yaml else {
                return Err(BasesError::new(format!(
                    "The base region in {note_path} names no base file and carries no YAML of its \
                     own, so there is nothing to render. A `base-rendered` fence written without a \
                     `path=` attribute is the only shape that reaches this."
                ))
                .with_note(note_path));
            };
            let inline_path = format!("{note_path}#inline");
            let base = parse_base(&inline_path, yaml)?;
            let result = query_base(&self.vault, &inline_path, &base, &query_options)?;
            return render_markdown(&base, &result, RenderStyle::Structured);
        };

        let target = self.resolve_base_path(base_path)?;
        let base = self.load_base(&target).await?;
        // An embedded Base always binds `this` to the note containing it, unless
        // the caller overrode it; a region that pinned a view wins over both.
        query_options.view = region.view_name.clone().or(query_options.view);
        let result = query_base(&self.vault, &target, &base, &query_options)?;
        render_markdown(&base, &result, RenderStyle::Structured)
    }

    /// The vault path of a note, from whatever the caller called it.
    pub fn resolve_note_path(&self, path: &str) -> Result<String> {
        if let Some(exact) = self.vault.note_paths().into_iter().find(|candidate| candidate == path) {
            return Ok(exact);
        }
        match self.vault.resolve(path) {
            Some(resolved) if resolved.ends_with(".md") => Ok(resolved),
            _ => Err(BasesError::new(format!("Note not found: {path}")).with_note(path)),
        }
    }

    /// Inbound links to a note, from the same index that backs every Base.
    ///
    /// A link that appears here and a link that satisfies a Base filter are the
    /// same fact, not two resolvers disagreeing.
    pub fn backlinks(&self, path: &str) -> Result<Vec<Backlink>> {
        let resolved = self.resolve_note_path(path)?;
        Ok(self
            .vault
            .backlinks_for(&resolved)
            .iter()
            .filter_map(|value| match value {
                BasesValue::String(path) => Some(Backlink {
                    path: path.clone(),
                    title: title_of(path),
                }),
                _ => None,
            })
            .collect())
    }

    // -- writes --------------------------------------------------------------

    /// Apply an agent's edit to a note.
    ///
    /// Base regions are restored verbatim and any attempt to change them is
    /// reported. The rest of the note is written as the agent left it.
    pub async fn write_note(&self, path: &str, content: &str) -> Result<WriteNoteResult> {
        let resolved = self.resolve_note_path(path)?;
        let original = self.vault.read_text(&resolved).await?;
        let result = reconcile_note(&resolved, &original, content);

        // Verify the result still parses, so we never write a broken note. The
        // Rust note parser is total — a malformed frontmatter degrades to "no
        // properties" rather than failing — so the invariant worth asserting is
        // the stronger one: the reconciled text segments back to itself.
        let reparsed = parse_note_with_embeds(&resolved, &result.text);
        debug_assert_eq!(
            serialise(&reparsed.segments),
            result.text,
            "a reconciled note must re-segment to the bytes that will be written"
        );

        if result.text != original {
            self.backend().write_text(&resolved, &result.text).await?;
            self.vault.reload().await?;
        }
        Ok((resolved, result).into())
    }

    /// Create or replace a note verbatim. Used only by `add_note_to_base`.
    ///
    /// Only a path WITH a separator has a parent to create. Slicing at the last
    /// `/` unconditionally gives a root-level `Note.md` the "parent" `Note.m`,
    /// and `mkdir` then creates that directory beside the note: invisible to
    /// every query, because a directory is not a note.
    pub async fn create_note(&self, path: &str, content: &str) -> Result<String> {
        if let Some(slash) = path.rfind('/').filter(|slash| *slash > 0) {
            self.backend().ensure_dir(&path[..slash]).await?;
        }
        self.backend().write_text(path, content).await?;
        self.vault.reload().await?;
        Ok(path.to_string())
    }

    /// Add a row to a Base by authoring a note its filter actually matches.
    ///
    /// A row IS a note, so this is a two-call handshake. The first call inverts
    /// the Base's filter into a proposed note and returns a `draft_id`; the
    /// agent edits it and calls again with `draft_id` plus the edited content.
    /// The second call runs the Base's REAL filter against the draft and writes
    /// only on a match, so a note that cannot be a row is never created.
    pub async fn add_note_to_base(&self, options: &AddNoteOptions) -> Result<AddNoteToBaseResult> {
        crate::drafts::add_note_to_base(self, options).await
    }

    /// Serialise rows for `resolve_base --format json`.
    ///
    /// Keyed by DISPLAY LABEL rather than by Property ID, because that is what
    /// Obsidian emits: `file.name` becomes `file name` and a configured
    /// `displayName` wins outright. [`display_name_for`] already consults both
    /// the canonical and the as-written spelling of the ID.
    pub fn to_json(&self, result: &QueryResult, base: &BaseFile) -> Vec<Json> {
        json_rows(base, result, &[])
    }
}

// ---------------------------------------------------------------------------
// JSON rows
// ---------------------------------------------------------------------------

/// One JSON row per resolved row, keyed by display label.
///
/// Shared by [`Resolver::to_json`] and the `includeAllFormulas` branch of
/// `resolve_base`, because Obsidian stringifies every cell the same way on both
/// surfaces and two copies would be free to drift.
pub fn json_rows(base: &BaseFile, result: &QueryResult, extra_formulas: &[String]) -> Vec<Json> {
    let columns = json_columns(base, &result.view);

    result
        .rows
        .iter()
        .map(|row| {
            let mut out = Map::new();
            out.insert("path".to_string(), Json::String(row.path.clone()));
            for id in &columns {
                let canonical_id = canonical(id);
                out.insert(
                    display_name_for(base, &canonical_id),
                    stringified(cell(&row.values, &canonical_id, id)).map_or(Json::Null, Json::String),
                );
            }
            // A formula the view does not order is the one thing an agent cannot
            // otherwise see, so `includeAllFormulas` adds it explicitly —
            // labelled through the same rule and stringified the same way, so
            // the added columns are indistinguishable from the parity ones.
            for name in extra_formulas {
                out.insert(
                    display_name_for(base, &format!("formula.{name}")),
                    stringified(row.formula.get(name)).map_or(Json::Null, Json::String),
                );
            }
            Json::Object(out)
        })
        .collect()
}

/// The columns a JSON row carries, in order.
///
/// `order` is honoured when it is PRESENT, including when it is present and
/// empty: the Obsidian CLI emits exactly the columns the view orders, so an
/// author who wrote `order: []` gets no columns rather than a fallback.
fn json_columns(base: &BaseFile, view: &BaseView) -> Vec<String> {
    match &view.order {
        Some(ids) => ids.clone(),
        None => {
            let mut ids = vec!["file.name".to_string()];
            ids.extend(base.properties.keys().cloned());
            ids
        }
    }
}

/// The value a row holds for a Property ID.
///
/// The `note.`-prefixed fallback is the as-written spelling: a base may write a
/// bare `status` where the row cache is keyed `note.status`, and reading only
/// the canonical key would silently drop the column.
fn cell<'a>(
    values: &'a std::collections::BTreeMap<String, BasesValue>,
    canonical_id: &str,
    raw_id: &str,
) -> Option<&'a BasesValue> {
    values
        .get(canonical_id)
        .filter(|value| !matches!(value, BasesValue::Null))
        .or_else(|| values.get(&format!("note.{raw_id}")))
        .filter(|value| !matches!(value, BasesValue::Null))
}

/// The string form a cell takes in `format=json`.
///
/// Exported because two surfaces emit JSON rows — [`Resolver::to_json`] and the
/// `includeAllFormulas` branch of `resolve_base` — and Obsidian stringifies
/// every value the same way. Two copies would be free to drift, and the drift
/// would only show up as an `includeAllFormulas` response disagreeing with the
/// columns beside it in the same row.
pub fn stringified(value: Option<&BasesValue>) -> Option<String> {
    match value? {
        BasesValue::Null => None,
        BasesValue::List(items) => Some(
            items.iter().map(|item| stringified(Some(item)).unwrap_or_default()).collect::<Vec<_>>().join(", "),
        ),
        // A namespace is a record, and a record has no display form of its own.
        // It is JSON, which is what the TypeScript original emitted — see the
        // note on `to_json_value`.
        other => Some(json_text(other)),
    }
}

/// The JSON text for one value, as `format=json` emits a cell.
///
/// Every Bases value except a namespace and a list is a scalar to JSON, and the
/// TypeScript original reached `JSON.stringify` for exactly those two shapes —
/// a `LinkValue`, `DateValue`, `DurationValue` and `FileValue` are all class
/// instances, so `typeof value === "object"` caught them too. The field names
/// below are the ones those classes had, which is what makes the output
/// comparable with the original rather than merely plausible.
fn json_text(value: &BasesValue) -> String {
    match value {
        BasesValue::Namespace(_) => serde_json::to_string(&to_json_value(value))
            .unwrap_or_else(|_| String::from("{}")),
        other => other.to_display_string(),
    }
}

/// One value as JSON.
pub fn to_json_value(value: &BasesValue) -> Json {
    match value {
        BasesValue::Null => Json::Null,
        BasesValue::Bool(flag) => Json::Bool(*flag),
        BasesValue::Number(number) => json_number(*number),
        BasesValue::String(text) => Json::String(text.clone()),
        BasesValue::List(items) => Json::Array(items.iter().map(to_json_value).collect()),
        BasesValue::Namespace(map) => Json::Object(
            map.iter().map(|(key, item)| (key.clone(), to_json_value(item))).collect(),
        ),
        BasesValue::Link { target, display, resolved } => Json::Object(clean_object([
            ("target", Json::String(target.clone())),
            ("display", display.clone().map_or(Json::Null, Json::String)),
            ("resolvedPath", resolved.clone().map_or(Json::Null, Json::String)),
        ])),
        // `dateOnly` is a TypeScript-only field; `BasesDate` deliberately has
        // no such flag, which changes its derived `Ord` and so the query
        // pipeline's sort. Recorded in `docs/divergences.md`.
        BasesValue::Date(date) => json!({ "ms": date.millis() }),
        BasesValue::Duration(duration) => {
            json!({ "ms": duration.millis, "months": duration.months, "years": duration.years })
        }
        // `accessors` is a bag of functions, which JSON drops, so it is not here
        // for the same reason `undefined` was absent from a link.
        BasesValue::File(file) => Json::Object(clean_object([
            ("path", Json::String(file.path.clone())),
            ("name", Json::String(file.name.clone())),
            ("basename", Json::String(file.basename.clone())),
            ("folder", Json::String(file.folder.clone())),
            ("ext", Json::String(file.ext.clone())),
        ])),
    }
}

/// A number as JSON, integral values without a fractional part.
///
/// `BasesValue::Number` is an `f64` and `serde_json` would emit `1.0` for it, but
/// the Obsidian CLI emits `1`: a `priority: 2` frontmatter value and a
/// `priority: 2.0` one are the same cell, and `format=json` is byte-compared
/// against the CLI. The same rule [`crate::value::format_number`] applies to a
/// rendered cell.
fn json_number(number: f64) -> Json {
    if number.is_finite() && number.fract() == 0.0 && number.abs() < 1e15 {
        return Json::Number((number as i64).into());
    }
    serde_json::Number::from_f64(number).map_or(Json::Null, Json::Number)
}

/// An object with the absent fields removed.
///
/// `JSON.stringify` drops a property whose value is `undefined`, and a resolved
/// link with no path is exactly that: emitting `"resolvedPath": null` instead
/// would be a different document, and one the parity comparison can see.
fn clean_object<const N: usize>(fields: [(&str, Json); N]) -> Map<String, Json> {
    fields
        .into_iter()
        .filter(|(_, value)| !value.is_null())
        .map(|(key, value)| (key.to_string(), value))
        .collect()
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// The provenance a rendered region carries.
///
/// `path` only for an embed or a fence that recorded one, `view` only when the
/// region pinned it, and `context` only when the caller supplied a host note.
/// The shapes are genuinely different, which is why every field is optional.
fn provenance_for(region: &BaseRegionRef, options: &NoteOptions) -> FenceProvenance {
    let mut provenance = FenceProvenance::new();
    if let Some(path) = &region.base_path {
        provenance = provenance.with_path(path.clone());
    }
    if let Some(view) = &region.view_name {
        provenance = provenance.with_view(view.clone());
    }
    if let Some(context) = &options.context {
        provenance = provenance.with_context(context.clone());
    }
    provenance
}

/// A note's title: its basename without the extension.
fn title_of(path: &str) -> String {
    let base = path.rsplit('/').next().unwrap_or(path);
    strip_extension(base)
}
