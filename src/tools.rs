//! The MCP tool surface.
//!
//! Six tools, one Resolver, and no logic of their own: every tool validates its
//! arguments, calls the Resolver, and shapes the answer. Keeping the tools thin
//! is what lets `Resolver` be the single place the engine's behaviour lives, so
//! the CLI parity suite and the MCP surface cannot drift apart — they are the
//! same code path.
//!
//! Three decisions the shape of this file follows from:
//!
//! 1. **Two return channels, chosen by what the payload is.** Data (rows, a
//!    note, a write report) goes out as a fenced JSON block AND as
//!    `structuredContent`, so a text-only client and a machine client both get
//!    what they need from one call. Presentation — `resolve_base`'s markdown —
//!    stays text blocks, because a table is not an object and dressing it as one
//!    would be a lie about its shape.
//! 2. **`isError` means the tool could not do its job.** It is reserved for a
//!    thrown `BasesError`: a bad path, a view that does not exist, an unbound
//!    `this`. A `write_note` that restored a Base region and applied everything
//!    else is a SUCCESS with `health: "partial-with-errors"`, because the note
//!    on disk is now what the agent asked for minus the part that is not a legal
//!    edit. Collapsing the two would teach the agent that a partial apply is a
//!    failure, and it would retry a write that already landed.
//! 3. **Health is three-state everywhere.** Obsidian reports an error AND
//!    correct rows in the same response; we refuse to imitate that, so warnings
//!    demote a result to `partial-with-errors` instead of poisoning it.
//!
//! Descriptions carry what a model cannot infer from the argument names: which
//! surface is CLI-parity and which is not, that `add_note_to_base` is two calls,
//! and that a Base region survives every write.
//!
//! ## Why a hand-written `ServerHandler`
//!
//! rmcp's `#[tool_router]` route was available and rejected for three reasons,
//! each of which would have cost a workaround rather than saved one:
//!
//!   - The schemas are hand-written here because the argument surface is a
//!     contract. `format` is an enum with a default, `includeAllFormulas` is
//!     json-only, and every argument carries prose a model needs. Deriving them
//!     from Rust types moves that prose into doc comments and lets a default or
//!     an enum quietly become a nullable string.
//!   - Every tool here returns a `CallToolResult` and NEVER an error. That is
//!     the whole of decision 2 above, and the macro route's `Result<T, ErrorData>`
//!     return type invites the opposite.
//!   - `dispatch` is a plain `async fn` taking a name and a JSON object, so the
//!     tests exercise the real tool surface without a transport, a peer, or a
//!     `LocalSet`. See `tests/tools.rs`.
//!
//! rmcp's `local` feature is enabled in `Cargo.toml` and is not optional: the
//! Resolver holds an `Rc<Vault>` and every accessor into it is a `RefCell`, so
//! the handler is `!Send` by construction. `local` is what tells rmcp that.

use std::borrow::Cow;
use std::future::Future;
use std::rc::Rc;

use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
    ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerConfig, Tool,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData as McpError, RoleServer, ServerHandler};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::{json, Map, Value as Json};

use crate::base::{QueryOptions, QueryResult};
use crate::drafts::{iso_millis, AddNoteOptions, AddNoteToBaseResult};
use crate::error::BasesError;
use crate::render::markdown::{render_markdown, RenderStyle};
use crate::service::{json_rows, NoteOptions, Resolver};

/// The server name a client sees.
pub const SERVER_NAME: &str = "bases-mcp";
/// The server version a client sees.
pub const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

/// How much of what was asked for actually happened.
///
/// `ok` is a clean result. `partial-with-errors` is a result the caller may still
/// act on, carrying whatever the engine complained about alongside it. `failed`
/// means nothing was produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Health {
    Ok,
    PartialWithErrors,
    Failed,
}

impl Health {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::PartialWithErrors => "partial-with-errors",
            Self::Failed => "failed",
        }
    }
}

/// The MCP server for one vault.
///
/// Takes a Resolver rather than opening one, because a server is one vault in
/// one process and the caller owns the vault lifetime: it decides when the
/// snapshot is reloaded, and this type never touches I/O outside the tools.
#[derive(Debug, Clone)]
pub struct ToolSurface {
    resolver: Rc<Resolver>,
}

impl ToolSurface {
    pub fn new(resolver: Rc<Resolver>) -> Self {
        Self { resolver }
    }

    pub fn resolver(&self) -> &Rc<Resolver> {
        &self.resolver
    }

    /// Run one tool by name.
    ///
    /// The one place a tool is allowed to fail. A handler describes intent, not
    /// recovery: every one of them returns a `CallToolResult`, so a malformed
    /// Base, a missing note or an unbound `this` comes back as something the
    /// agent can read rather than as a protocol error that takes the server down.
    ///
    /// `Err` is reserved for the request itself being unroutable — an unknown
    /// tool name — which is a JSON-RPC concern rather than a tool result.
    pub async fn dispatch(
        &self,
        name: &str,
        arguments: &Map<String, Json>,
    ) -> Result<CallToolResult, McpError> {
        match name {
            "list_bases" => Ok(attempt(self.list_bases()).await),
            "resolve_base" => Ok(attempt(self.resolve_base(arguments)).await),
            "get_note" => Ok(attempt(self.get_note(arguments)).await),
            "write_note" => Ok(attempt(self.write_note(arguments)).await),
            "add_note_to_base" => Ok(attempt(self.add_note_to_base(arguments)).await),
            "backlinks" => Ok(attempt(self.backlinks(arguments)).await),
            other => Err(McpError::invalid_params(
                format!(
                    "Unknown tool \"{other}\". Call tools/list for the six this server serves."
                ),
                None,
            )),
        }
    }

    // -- the six tools -------------------------------------------------------

    async fn list_bases(&self) -> Result<CallToolResult, BasesError> {
        let bases = self.resolver.list_bases().await?;
        let count = bases.len();
        let payload = json!({
            "bases": bases
                .iter()
                .map(|base| json!({
                    "path": base.path,
                    "views": base.views
                        .iter()
                        .map(|view| json!({ "name": view.name, "type": view.view_type }))
                        .collect::<Vec<_>>(),
                }))
                .collect::<Vec<_>>(),
            "health": Health::Ok.as_str(),
        });
        Ok(data(
            payload,
            Some(format!(
                "Every base queries the whole vault. {count} base(s) found."
            )),
        ))
    }

    async fn resolve_base(
        &self,
        arguments: &Map<String, Json>,
    ) -> Result<CallToolResult, BasesError> {
        let args: ResolveBaseArgs = parse_args(arguments)?;
        let path = self.resolver.resolve_base_path(&args.base)?;
        let base = self.resolver.load_base(&path).await?;
        // One query for both formats. `Resolver::render` is these two lines; it
        // is inlined so the `QueryResult` survives to the health field, where its
        // warnings decide ok vs partial-with-errors. The markdown is
        // byte-identical either way.
        let options = QueryOptions {
            context: args.context.clone(),
            view: args.view.clone(),
        };
        let result = self.resolver.query(&path, &options).await?;
        let health = health_of(&result);

        if args.format == OutputFormat::Markdown {
            let mut call = CallToolResult::success(vec![
                ContentBlock::text(render_markdown(&base, &result, RenderStyle::Flat)?),
                ContentBlock::text(health_note(&result, health)),
            ]);
            call.structured_content = Some(summarise(&result, health));
            return Ok(call);
        }

        // `includeAllFormulas` adds a column for every formula the Base
        // declares, not just those in the view's `order`. It applies to json
        // only: markdown is the parity surface and shows exactly `order`.
        let extra: Vec<String> = if args.include_all_formulas {
            base.formulas.keys().cloned().collect()
        } else {
            Vec::new()
        };
        let mut payload = summarise(&result, health);
        if let Json::Object(fields) = &mut payload {
            fields.insert(
                "rows".to_string(),
                Json::Array(json_rows(&base, &result, &extra)),
            );
        }
        Ok(data(payload, None))
    }

    async fn get_note(&self, arguments: &Map<String, Json>) -> Result<CallToolResult, BasesError> {
        let args: GetNoteArgs = parse_args(arguments)?;
        let note = self
            .resolver
            .read_note(
                &args.path,
                NoteOptions {
                    raw: args.raw,
                    ..NoteOptions::default()
                },
            )
            .await?;
        let payload = json!({
            "path": note.path,
            "raw": note.raw,
            "content": note.content,
            "regions": note.regions.iter().map(fence_provenance).collect::<Vec<_>>(),
            "health": Health::Ok.as_str(),
        });
        // `raw: true` returns the stored text untouched, so the Projection
        // warning would be telling the agent to do something it deliberately did
        // not do.
        let warning = (!args.raw).then(|| {
            "`content` is a Projection. Edit `raw`, not `content`, and send that to write_note."
                .to_string()
        });
        Ok(data(payload, warning))
    }

    async fn write_note(
        &self,
        arguments: &Map<String, Json>,
    ) -> Result<CallToolResult, BasesError> {
        let args: WriteNoteArgs = parse_args(arguments)?;
        let before = self
            .resolver
            .read_note(&args.path, NoteOptions::raw())
            .await?;
        let result = self.resolver.write_note(&args.path, &args.content).await?;
        let health = if result.refused.is_empty() {
            Health::Ok
        } else {
            Health::PartialWithErrors
        };
        let payload = json!({
            "path": result.path,
            "applied": change_summary(&before.raw, &result.text),
            "refusals": result
                .refused
                .iter()
                .map(|region| json!({
                    "region": region.index,
                    "base": region.base_path,
                    "reason": region.reason,
                    "guidance": region.guidance,
                }))
                .collect::<Vec<_>>(),
            "removedRegion": result.removed_region,
            "health": health.as_str(),
        });
        Ok(data(
            payload,
            refusal_note(result.refused.len(), result.removed_region),
        ))
    }

    async fn add_note_to_base(
        &self,
        arguments: &Map<String, Json>,
    ) -> Result<CallToolResult, BasesError> {
        let args: AddNoteOptions = parse_args(arguments)?;
        let result = self.resolver.add_note_to_base(&args).await?;
        let payload = match &result {
            AddNoteToBaseResult::Proposal(proposal) => json!({
                "draft_id": proposal.draft_id,
                "path": proposal.path,
                "content": proposal.content,
                "expires_at": proposal.expires_at,
                "base": proposal.base,
                "view": proposal.view,
                "context": proposal.context,
                "health": Health::Ok.as_str(),
            }),
            AddNoteToBaseResult::Commit(commit) => json!({
                "written": commit.written,
                "draft_id": commit.draft_id,
                "verified": commit.verified,
                "health": Health::Ok.as_str(),
            }),
        };
        Ok(data(payload, handshake_note(&result)))
    }

    async fn backlinks(&self, arguments: &Map<String, Json>) -> Result<CallToolResult, BasesError> {
        let args: BacklinksArgs = parse_args(arguments)?;
        let path = self.resolver.resolve_note_path(&args.path)?;
        let backlinks = self.resolver.backlinks(&path)?;
        let count = backlinks.len();
        let payload = json!({
            "path": path,
            "backlinks": backlinks
                .iter()
                .map(|backlink| json!({ "path": backlink.path, "title": backlink.title }))
                .collect::<Vec<_>>(),
            "health": Health::Ok.as_str(),
        });
        Ok(data(
            payload,
            Some(format!("{count} note(s) link to {path}.")),
        ))
    }
}

impl ServerHandler for ToolSurface {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(SERVER_NAME, SERVER_VERSION))
    }

    fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<ListToolsResult, McpError>> + '_ {
        std::future::ready(Ok(ListToolsResult::with_all_items(tool_definitions())))
    }

    fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<CallToolResponse, McpError>> + '_ {
        let arguments = request.arguments.unwrap_or_default();
        async move {
            self.dispatch(&request.name, &arguments)
                .await
                .map(CallToolResponse::from)
        }
    }
}

// ---------------------------------------------------------------------------
// Arguments
// ---------------------------------------------------------------------------

/// `resolve_base`'s arguments.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ResolveBaseArgs {
    /// Path to the `.base` file, as listed by list_bases.
    base: String,
    /// View name. Defaults to the base's first view.
    view: Option<String>,
    /// Host note path binding `this`. Required for a base that references `this`.
    context: Option<String>,
    /// `markdown` for the flat CLI-parity table, `json` for rows keyed by
    /// display label.
    #[serde(default)]
    format: OutputFormat,
    /// json only: add a column for every formula the base declares.
    #[serde(default)]
    include_all_formulas: bool,
}

/// Which of `resolve_base`'s two surfaces to render.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
enum OutputFormat {
    /// The flat, CLI-parity markdown table.
    #[default]
    Markdown,
    /// Rows keyed by display label.
    Json,
}

/// `get_note`'s arguments.
#[derive(Debug, Deserialize)]
struct GetNoteArgs {
    /// Path to the `.md` note.
    path: String,
    /// Return the stored text with base regions intact instead of the Projection.
    #[serde(default)]
    raw: bool,
}

/// `write_note`'s arguments.
#[derive(Debug, Deserialize)]
struct WriteNoteArgs {
    /// Path to the `.md` note to write.
    path: String,
    /// The agent's edited note. Send the `raw` text from get_note, never a
    /// Projection.
    content: String,
}

/// `backlinks`' arguments.
#[derive(Debug, Deserialize)]
struct BacklinksArgs {
    /// Path to the `.md` note.
    path: String,
}

/// Turn a JSON object into typed arguments, or report the mismatch.
///
/// A `BasesError`, so a bad argument reaches the agent through the same envelope
/// as every other engine failure: `isError` with `health: "failed"` and a
/// message naming what was wrong.
fn parse_args<T: DeserializeOwned>(arguments: &Map<String, Json>) -> Result<T, BasesError> {
    serde_json::from_value(Json::Object(arguments.clone())).map_err(|error| {
        BasesError::new(format!("Invalid arguments: {error}")).with_construct("arguments")
    })
}

// ---------------------------------------------------------------------------
// Results
// ---------------------------------------------------------------------------

/// The counts every `resolve_base` answer carries, whatever the format.
fn summarise(result: &QueryResult, health: Health) -> Json {
    json!({
        "base": result.base_path,
        "view": result.view.name,
        "type": result.view.view_type,
        "context": result.context,
        "rowCount": result.rows.len(),
        "total": result.total,
        "health": health.as_str(),
        "warnings": result.warnings,
    })
}

/// Warnings demote a result rather than failing it.
fn health_of(result: &QueryResult) -> Health {
    if result.warnings.is_empty() {
        Health::Ok
    } else {
        Health::PartialWithErrors
    }
}

/// A one-line read of the health field, for a reader who sees only the text.
fn health_note(result: &QueryResult, health: Health) -> String {
    if health == Health::Ok {
        return format!(
            "{rows} of {total} row(s) -- view \"{view}\" ({view_type}).",
            rows = result.rows.len(),
            total = result.total,
            view = result.view.name,
            view_type = result.view.view_type
        );
    }
    format!(
        "health: {health}. {rows} of {total} row(s) resolved; the base reported:\n{warnings}",
        health = health.as_str(),
        rows = result.rows.len(),
        total = result.total,
        warnings = result
            .warnings
            .iter()
            .map(|warning| format!("- {warning}"))
            .collect::<Vec<_>>()
            .join("\n")
    )
}

/// How much of the agent's text survived reconciliation.
///
/// A multiset count over lines, not a diff: it answers "did my edit land and how
/// big was it", which is the question an agent has after a partial apply.
/// Anything finer would mean owning a diff algorithm in a tool whose real job is
/// reporting refusals.
fn change_summary(before: &str, after: &str) -> Json {
    let old_lines = line_counts(before);
    let new_lines = line_counts(after);
    json!({
        "changed": before != after,
        "linesAdded": surplus(&new_lines, &old_lines),
        "linesRemoved": surplus(&old_lines, &new_lines),
    })
}

fn line_counts(text: &str) -> std::collections::HashMap<&str, usize> {
    let mut counts = std::collections::HashMap::new();
    for line in text.split('\n') {
        *counts.entry(line).or_insert(0) += 1;
    }
    counts
}

/// How many lines of `from` have no counterpart in `against`.
fn surplus(
    from: &std::collections::HashMap<&str, usize>,
    against: &std::collections::HashMap<&str, usize>,
) -> usize {
    from.iter()
        .map(|(line, count)| count.saturating_sub(against.get(line).copied().unwrap_or(0)))
        .sum()
}

/// The refusal count, stated plainly, so it cannot be skimmed past.
fn refusal_note(count: usize, removed: bool) -> Option<String> {
    if count == 0 {
        return None;
    }
    let mut parts = vec![format!("{count} base region(s) were restored, not edited.")];
    if removed {
        parts.push("One of them had been deleted outright.".to_string());
    }
    parts
        .push("Rows come from notes: use add_note_to_base to create one that matches.".to_string());
    Some(parts.join(" "))
}

/// Which half of the handshake came back, named so the agent cannot mistake it.
fn handshake_note(result: &AddNoteToBaseResult) -> Option<String> {
    match result {
        AddNoteToBaseResult::Commit(commit) => {
            Some(format!("Wrote {}. The draft is consumed.", commit.written))
        }
        AddNoteToBaseResult::Proposal(proposal) => Some(format!(
            "Draft {id} for {path}. NOTHING has been written yet. Edit the proposed note and call \
             again with only draft_id and content; it expires at {expires}.",
            id = proposal.draft_id,
            path = proposal.path,
            expires = iso_millis(proposal.expires_at)
        )),
    }
}

// ---------------------------------------------------------------------------
// Envelopes
// ---------------------------------------------------------------------------

/// A data result, on both channels.
///
/// `structuredContent` is what a machine client reads; the fenced block is what
/// a model reads. Emitting both costs a second copy of the payload and saves
/// every consumer from parsing markdown to get at a row.
fn data(payload: Json, warning: Option<String>) -> CallToolResult {
    let body = serde_json::to_string_pretty(&payload).unwrap_or_else(|_| payload.to_string());
    let mut text = format!("```json\n{body}\n```");
    if let Some(warning) = warning {
        text.push('\n');
        text.push_str(&warning);
    }
    let mut call = CallToolResult::success(vec![ContentBlock::text(text)]);
    call.structured_content = Some(payload);
    call
}

/// A `BasesError` as a tool result.
///
/// The engine's own voice — it names the construct, the note, the view or the
/// property that failed, and those fields are the only thing that makes a bad
/// Base fixable. They are lifted onto the result so a client can act on them
/// without scraping the prose.
fn failure(error: &BasesError) -> CallToolResult {
    let mut detail = context_of(error);
    detail.insert(
        "message".to_string(),
        Json::String(error.message().to_string()),
    );

    let mut call = CallToolResult::error(vec![ContentBlock::text(error.message())]);
    call.structured_content =
        Some(json!({ "health": Health::Failed.as_str(), "error": Json::Object(detail) }));
    call
}

/// The error fields that are present, and only those.
fn fence_provenance(provenance: &crate::render::project::FenceProvenance) -> Json {
    serde_json::to_value(provenance).unwrap_or(Json::Null)
}

fn context_of(error: &BasesError) -> Map<String, Json> {
    let mut context = Map::new();
    for (key, value) in [
        ("construct", error.construct()),
        ("property", error.property()),
        ("view", error.view()),
        ("note", error.note()),
    ] {
        if let Some(value) = value {
            context.insert(key.to_string(), Json::String(value.to_string()));
        }
    }
    if let Some(position) = error.position() {
        context.insert("position".to_string(), Json::from(position));
    }
    context
}

/// Run a tool body, turning anything it throws into a structured tool result.
async fn attempt<F>(work: F) -> CallToolResult
where
    F: std::future::Future<Output = Result<CallToolResult, BasesError>>,
{
    match work.await {
        Ok(call) => call,
        Err(error) => failure(&error),
    }
}

// ---------------------------------------------------------------------------
// Tool declarations
// ---------------------------------------------------------------------------

/// Every tool this server serves, with its input schema.
///
/// Six tools, and the order they are declared in is the order `tools/list`
/// reports them in: the two discovery tools, then the two note tools, then the
/// row tool, then the link tool.
pub fn tool_definitions() -> Vec<Tool> {
    vec![
        tool(
            "list_bases",
            "List bases",
            "List every `.base` file in the vault with its views and layout types. A base \
             queries the WHOLE vault -- there is no `from` clause -- so this is the only way to \
             find out what exists before resolving one. Pass a `view` name from this list \
             verbatim to resolve_base; with no `view`, resolve_base uses the base's first view.",
            json!({ "type": "object", "properties": {}, "required": [] }),
        ),
        tool(
            "resolve_base",
            "Resolve a base view",
            concat!(
                "Resolve one view of a base into rows. Two surfaces, and the difference \
                 matters:\n",
                "- format=markdown is the FLAT, CLI-parity surface: byte-comparable with ",
                "`obsidian base:query format=md`, which collapses EVERY view type into one \
                 centred table and drops group headers and summaries.\n",
                "- Bases embedded in a note are the opposite -- get_note renders them \
                 STRUCTURED, with group headers and a summaries footer. Use markdown here to \
                 read a whole table; use get_note to read a base in the note it lives in.\n",
                "`context` is the host note that binds `this`. A base whose filter references \
                 `this` REQUIRES it: with no host note this tool fails rather than returning the \
                 empty result the Obsidian CLI returns, because an empty result is \
                 indistinguishable from a base that genuinely matches nothing.\n",
                "`includeAllFormulas` adds a column for every formula the base declares, not \
                 just those in the view's `order`. It applies to format=json only -- markdown is \
                 the parity surface and shows exactly `order`.\n",
                "`health` is `ok`, `partial-with-errors` (rows resolved, but the base reported \
                 warnings) or `failed`."
            ),
            json!({
                "type": "object",
                "properties": {
                    "base": {
                        "type": "string",
                        "description": "Path to the `.base` file, as listed by list_bases."
                    },
                    "view": {
                        "type": "string",
                        "description": "View name. Defaults to the base's first view."
                    },
                    "context": {
                        "type": "string",
                        "description": "Host note path binding `this`. Required for a base that references `this`."
                    },
                    "format": {
                        "type": "string",
                        "enum": ["markdown", "json"],
                        "default": "markdown",
                        "description": "`markdown` for the flat CLI-parity table, `json` for rows keyed by display label."
                    },
                    "includeAllFormulas": {
                        "type": "boolean",
                        "default": false,
                        "description": "json only: add a column for every formula the base declares."
                    }
                },
                "required": ["base"]
            }),
        ),
        tool(
            "get_note",
            "Get a note",
            concat!(
                "Read a note as a Projection: every base region is replaced by a \
                 `base-rendered` fence carrying its `path`, `view` and `context`, and `regions` \
                 lists that provenance -- one entry per base region, in document order. Each \
                 embedded base binds `this` to the note it lives in, so no host note has to be \
                 supplied here.\n",
                "This Projection is NEVER written back to disk. Obsidian treats a `base` fence \
                 as live YAML and would reject rendered markdown inside one. To edit a note, read \
                 it with raw=true, edit THAT, and send it to write_note.\n",
                "`raw: true` returns the stored text untouched and an empty `regions`."
            ),
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Path to the `.md` note." },
                    "raw": {
                        "type": "boolean",
                        "default": false,
                        "description": "Return the stored text with base regions intact instead of the Projection."
                    }
                },
                "required": ["path"]
            }),
        ),
        tool(
            "write_note",
            "Write a note",
            concat!(
                "Apply an edit to a note. Every non-base change is written. Base regions are \
                 restored byte-for-byte, and any attempt to change, insert or delete one is \
                 REFUSED and reported in `refused` -- each entry carries a reason and guidance \
                 for what to do instead.\n",
                "A refusal means that part of your edit did NOT land: the base region was put \
                 back as it was, while the rest of your edit was applied. `health` is \
                 `partial-with-errors` whenever anything was refused, and `isError` stays false \
                 because the write itself succeeded.\n",
                "Rows come from notes, not from the base file. To add a row, use \
                 add_note_to_base -- it authors a note whose properties satisfy the base's \
                 filter."
            ),
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Path to the `.md` note to write." },
                    "content": {
                        "type": "string",
                        "description": "The agent's edited note. Send the `raw` text from get_note, never a Projection."
                    }
                },
                "required": ["path", "content"]
            }),
        ),
        tool(
            "add_note_to_base",
            "Add a note to a base",
            concat!(
                "Add a row to a base by creating a note its filter actually matches. A row IS a \
                 note, so this is a TWO-CALL HANDSHAKE, never one:\n",
                "1. Call with `base` and `path` (plus `context` when the base references `this`). \
                 You get back a proposed note and a `draft_id`. NOTHING has been written.\n",
                "2. Edit the proposed note, then call again with ONLY `draft_id` and `content`.\n",
                "The second call runs the base's real filter against your frontmatter and writes \
                 only on a match, so a note that could never be a row is never created. On a \
                 mismatch the error names the expressions that failed and inlines the base's own \
                 filter; nothing is written and the draft stays live for 30 minutes, so you can \
                 correct the frontmatter and resend the same `draft_id`.\n",
                "Branch on the result: `written` present means the note was created; absent means \
                 you are holding a draft to edit."
            ),
            json!({
                "type": "object",
                "properties": {
                    "base": {
                        "type": "string",
                        "description": "Path to the `.base` the note has to match. First call only."
                    },
                    "path": {
                        "type": "string",
                        "description": "Vault-relative `.md` path to create. First call only."
                    },
                    "context": {
                        "type": "string",
                        "description": "Host note binding `this`. First call only; required for a scoped base."
                    },
                    "view": {
                        "type": "string",
                        "description": "View whose filters the note must satisfy."
                    },
                    "draft_id": {
                        "type": "string",
                        "description": "The id from the first call. Sending it selects the commit path."
                    },
                    "content": {
                        "type": "string",
                        "description": "Your edited note. Second call only."
                    }
                },
                "required": []
            }),
        ),
        tool(
            "backlinks",
            "Backlinks",
            "Inbound links to a note, from the same resolver that backs every base -- so a link \
             that appears here and a link that satisfies a base filter are the same fact, not two \
             resolvers disagreeing. An empty list means nothing links here; that is an answer, \
             not an error.",
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Path to the `.md` note." }
                },
                "required": ["path"]
            }),
        ),
    ]
}

fn tool(
    name: &'static str,
    title: &'static str,
    description: &'static str,
    input_schema: Json,
) -> Tool {
    let schema = input_schema.as_object().cloned().unwrap_or_default();
    let mut declared = Tool::new_with_raw(name, Some(Cow::Borrowed(description)), schema);
    declared.title = Some(title.to_string());
    declared
}
