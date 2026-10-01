//! The six MCP tools, exercised through the handler without a transport.
//!
//! `ToolSurface::dispatch` is the whole tool surface: it takes a name and a JSON
//! object and answers with a `CallToolResult`. That is what these tests call, so
//! nothing here needs a stdio pipe, a peer, or a `LocalSet`, and every assertion
//! is about bytes an agent would actually receive.
//!
//! The load-bearing claims, each with a test that would fail if it drifted:
//!
//!   - **`isError` means the tool could not do its job.** A `write_note` that
//!     restored a Base region and applied everything else is a SUCCESS with
//!     `health: "partial-with-errors"`. An agent that saw `isError` there would
//!     retry a write that already landed.
//!   - **Health is three-state**, and only `failed` says nothing was produced.
//!   - **Two return channels**: every data answer carries the payload as
//!     `structuredContent` AND as a fenced JSON block.
//!   - **The schemas are a contract**: six tools, with the required keys and
//!     defaults the descriptions promise.
//!
//! Nothing here writes to `test/vault`. Writing tools work on a temp copy.

// Each integration test is its own crate, and not every one needs every helper.
// Each integration test is its own crate, and no suite needs every helper here.
#[allow(dead_code)]
mod common;

use std::path::Path;
use std::rc::Rc;

use bases_mcp::config::{ConfigResult, Env, VaultConfig, parse_config};
use bases_mcp::service::Resolver;
use bases_mcp::tools::{ToolSurface, tool_definitions};
use common::{futures_block_on, load_corpus_from, seed_dir, vault_dir};
use serde_json::{Value as Json, json};
use tempfile::TempDir;

/// A tool surface over a private copy of the testing vault.
fn sandbox() -> (TempDir, ToolSurface) {
    let dir = TempDir::new().expect("a temp dir is writable");
    seed_dir(dir.path(), &load_corpus_from(&vault_dir()));
    let resolver =
        futures_block_on(Resolver::open_dir(dir.path())).expect("the sandbox vault opens");
    (dir, ToolSurface::new(Rc::new(resolver)))
}

/// A tool surface over the real testing vault. Only the read tools are called.
fn oracle() -> ToolSurface {
    let resolver = futures_block_on(Resolver::open_dir(vault_dir())).expect("the testing vault opens");
    ToolSurface::new(Rc::new(resolver))
}

/// Call one tool. `Ok` here means the call was routed; the RESULT carries
/// `isError` when the tool itself failed.
fn call(surface: &ToolSurface, name: &str, arguments: Json) -> rmcp::model::CallToolResult {
    let arguments = arguments.as_object().cloned().unwrap_or_default();
    futures_block_on(surface.dispatch(name, &arguments))
        .unwrap_or_else(|error| panic!("{name} routed: {error}"))
}

fn structured(result: &rmcp::model::CallToolResult) -> &Json {
    result.structured_content.as_ref().expect("a data answer carries structuredContent")
}

fn is_ok(result: &rmcp::model::CallToolResult) -> bool {
    result.is_error == Some(false)
}

fn text(result: &rmcp::model::CallToolResult) -> String {
    result
        .content
        .iter()
        .map(|block| block.as_text().map(|text| text.text.clone()).unwrap_or_default())
        .collect::<Vec<_>>()
        .join("\n")
}

fn health_of(result: &rmcp::model::CallToolResult) -> String {
    structured(result)["health"].as_str().expect("a health string").to_string()
}

/// The message of a failed answer, which is the only channel a text-only client sees.
fn failure_message(result: &rmcp::model::CallToolResult) -> String {
    text(result)
}

// ---------------------------------------------------------------------------
// The tool list
// ---------------------------------------------------------------------------

#[test]
fn there_are_exactly_six_tools_and_they_are_the_documented_ones() {
    let tools = tool_definitions();
    let names: Vec<&str> = tools.iter().map(|tool| tool.name.as_ref()).collect();
    assert_eq!(
        names,
        ["list_bases", "resolve_base", "get_note", "write_note", "add_note_to_base", "backlinks"]
    );
}

#[test]
fn every_tool_carries_a_title_and_a_description_that_says_the_load_bearing_thing() {
    for tool in tool_definitions() {
        assert!(tool.title.is_some(), "{} has no title", tool.name);
        let description = tool.description.as_ref().expect("a description");
        assert!(description.len() > 80, "{} describes itself in one clause", tool.name);
    }
}

#[test]
fn resolve_base_says_which_of_its_two_surfaces_is_the_parity_one() {
    let resolve = tool_definitions().into_iter().find(|tool| tool.name == "resolve_base").unwrap();
    let description = resolve.description.unwrap();
    assert!(description.contains("FLAT, CLI-parity surface"), "{description}");
    assert!(description.contains("get_note renders them STRUCTURED"), "{description}");
}

#[test]
fn add_note_to_base_says_it_is_two_calls() {
    let tool = tool_definitions().into_iter().find(|tool| tool.name == "add_note_to_base").unwrap();
    let description = tool.description.unwrap();
    assert!(description.contains("TWO-CALL HANDSHAKE"), "{description}");
    assert!(description.contains("ONLY `draft_id` and `content`"), "{description}");
}

#[test]
fn write_note_says_a_refusal_is_not_a_failure() {
    let tool = tool_definitions().into_iter().find(|tool| tool.name == "write_note").unwrap();
    let description = tool.description.unwrap();
    assert!(description.contains("isError` stays false"), "{description}");
}

#[test]
fn the_input_schemas_are_the_arguments_the_descriptions_promise() {
    let tools = tool_definitions();
    let schema = |name: &str| {
        let tool = tools.iter().find(|tool| tool.name == name).unwrap();
        serde_json::to_value(&tool.input_schema).expect("a schema is JSON")
    };

    let list = schema("list_bases");
    assert_eq!(list["properties"], json!({}), "list_bases takes nothing");
    assert_eq!(list["required"], json!([]));

    let resolve = schema("resolve_base");
    assert_eq!(resolve["required"], json!(["base"]), "only `base` is required");
    assert_eq!(resolve["properties"]["format"]["enum"], json!(["markdown", "json"]));
    assert_eq!(resolve["properties"]["format"]["default"], "markdown");
    assert_eq!(resolve["properties"]["includeAllFormulas"]["default"], false);

    let get_note = schema("get_note");
    assert_eq!(get_note["required"], json!(["path"]));
    assert_eq!(get_note["properties"]["raw"]["default"], false);

    let write = schema("write_note");
    assert_eq!(write["required"], json!(["path", "content"]));

    let add = schema("add_note_to_base");
    assert_eq!(add["required"], json!([]), "the second call sends two of six arguments");
    let properties = add["properties"].as_object().expect("properties");
    for key in ["base", "path", "context", "view", "draft_id", "content"] {
        assert!(properties.contains_key(key), "{key} is declared");
        assert!(
            properties[key]["description"].as_str().is_some_and(|d| !d.is_empty()),
            "{key} is described"
        );
    }

    assert_eq!(schema("backlinks")["required"], json!(["path"]));
}

// ---------------------------------------------------------------------------
// list_bases
// ---------------------------------------------------------------------------

#[test]
fn list_bases_answers_with_both_channels_and_a_count() {
    let surface = oracle();
    let result = call(&surface, "list_bases", json!({}));

    assert!(is_ok(&result));
    assert_eq!(health_of(&result), "ok");
    let bases = structured(&result)["bases"].as_array().expect("an array");
    assert_eq!(bases.len(), 2);
    assert_eq!(bases[1]["path"], "Tickets.base");
    assert_eq!(bases[1]["views"][0]["name"], "All");
    assert_eq!(bases[1]["views"][0]["type"], "cards", "the layout type, as the base spells it");

    // The fenced block is what a model reads; it is the same payload, indented.
    let text = text(&result);
    assert!(text.starts_with("```json\n{\n"), "{text}");
    assert!(text.contains("2 base(s) found."), "the count is stated in prose: {text}");
}

// ---------------------------------------------------------------------------
// resolve_base
// ---------------------------------------------------------------------------

#[test]
fn resolve_base_defaults_to_the_flat_markdown_table() {
    let surface = oracle();
    let result = call(&surface, "resolve_base", json!({ "base": "AllNotes.base" }));

    assert!(is_ok(&result));
    assert_eq!(health_of(&result), "ok");
    let text = text(&result);
    assert!(text.starts_with('|'), "the default format is the flat table: {text}");
    assert!(text.contains("of 4 row(s) -- view \"All\" (table)."), "{text}");
    // Two content blocks: the table, then the health note a text-only client needs.
    assert_eq!(result.content.len(), 2);
}

#[test]
fn resolve_base_markdown_and_json_agree_on_the_row_count() {
    let surface = oracle();
    let markdown =
        call(&surface, "resolve_base", json!({ "base": "AllNotes.base", "format": "markdown" }));
    let json = call(&surface, "resolve_base", json!({ "base": "AllNotes.base", "format": "json" }));

    assert_eq!(structured(&markdown)["rowCount"], 4, "one query, two surfaces");
    assert_eq!(structured(&json)["rowCount"], 4);
    assert_eq!(structured(&json)["base"], "AllNotes.base");
    assert_eq!(structured(&json)["view"], "All");
    assert_eq!(structured(&json)["type"], "table");
    assert_eq!(structured(&json)["total"], 4);
    assert_eq!(structured(&json)["warnings"], json!([]));

    let rows = structured(&json)["rows"].as_array().expect("rows");
    assert_eq!(rows.len(), 4);
    assert!(rows[0].as_object().expect("a row").contains_key("file name"), "keyed by display label");
    // The json surface is a fenced block plus a structured payload, not prose.
    assert!(text(&json).starts_with("```json"), "{}", text(&json));
}

/// `Tickets.base` declares two formulas and its view orders one of them, which
/// makes it the vault's own evidence that `includeAllFormulas` adds exactly the
/// column a plain query cannot show.
#[test]
fn resolve_base_can_add_every_formula_the_base_declares() {
    let surface = oracle();
    let args = json!({
        "base": "Tickets.base",
        "context": "Projects/SomeProject.md",
        "format": "json",
    });
    let without = call(&surface, "resolve_base", args);
    let with = call(
        &surface,
        "resolve_base",
        json!({
            "base": "Tickets.base",
            "context": "Projects/SomeProject.md",
            "format": "json",
            "includeAllFormulas": true
        }),
    );

    let keys = |result: &rmcp::model::CallToolResult| -> Vec<String> {
        result.structured_content.as_ref().expect("payload")["rows"][0]
            .as_object()
            .expect("a row")
            .keys()
            .cloned()
            .collect()
    };
    // `priority_display` is ordered and configured as `Priority`; `task_count`
    // is declared and never ordered, so it is the column the option is for.
    assert!(keys(&without).contains(&"Priority".to_string()), "{:?}", keys(&without));
    assert!(!keys(&without).contains(&"task_count".to_string()), "{:?}", keys(&without));

    let added = keys(&with);
    assert!(added.contains(&"task_count".to_string()), "{added:?}");
    assert_eq!(added.len(), keys(&without).len() + 1, "exactly one column is added: {added:?}");
    // The added column is labelled and stringified by the same rules, so it is
    // indistinguishable from a parity column.
    let row = &structured(&with)["rows"][0];
    assert_eq!(row["task_count"], "1", "both SomeProject tickets carry one task");
}

#[test]
fn a_cell_the_row_does_not_hold_is_null_not_missing_and_not_empty() {
    // `Tickets/Invoice export.md` has no `type`, and the view orders it. Obsidian
    // emits an explicit null, and a summary counts a blank differently from an
    // absent key -- so the difference is the contract.
    let surface = oracle();
    let result = call(
        &surface,
        "resolve_base",
        json!({ "base": "Tickets.base", "context": "Projects/OtherProject.md", "format": "json" }),
    );

    let row = structured(&result)["rows"][0].as_object().expect("a row").clone();
    assert_eq!(row["path"], "Tickets/Invoice export.md");
    assert_eq!(row["type"], Json::Null, "present and empty: {row:?}");
    assert_eq!(row["status"], "done", "and its neighbours are unaffected");
}

#[test]
fn a_this_scoped_base_needs_a_context_and_says_so_when_it_has_none() {
    let surface = oracle();
    let result = call(&surface, "resolve_base", json!({ "base": "Tickets.base" }));

    // The refusal an agent gets, not `[]`.
    assert_eq!(result.is_error, Some(true), "an unbound `this` fails the tool");
    assert_eq!(health_of(&result), "failed");
    assert!(failure_message(&result).to_lowercase().contains("this"), "{}", failure_message(&result));
    assert_eq!(structured(&result)["error"]["construct"], "this");
}

#[test]
fn a_this_scoped_base_resolves_with_a_context() {
    let surface = oracle();
    let result = call(
        &surface,
        "resolve_base",
        json!({ "base": "Tickets.base", "context": "Projects/SomeProject.md", "format": "json" }),
    );
    assert!(is_ok(&result));
    let rows = structured(&result)["rows"].as_array().expect("rows");
    assert_eq!(rows.len(), 2);
    assert_eq!(structured(&result)["context"], "Projects/SomeProject.md");
}

#[test]
fn a_base_that_does_not_exist_is_a_failed_tool_with_the_path_named() {
    let surface = oracle();
    let result = call(&surface, "resolve_base", json!({ "base": "Nope.base" }));

    assert_eq!(result.is_error, Some(true));
    assert_eq!(health_of(&result), "failed");
    assert_eq!(structured(&result)["error"]["note"], "Nope.base");
    assert_eq!(structured(&result)["error"]["message"], "Base file not found: Nope.base");
}

#[test]
fn a_view_that_does_not_exist_names_the_ones_that_do() {
    let surface = oracle();
    let result =
        call(&surface, "resolve_base", json!({ "base": "AllNotes.base", "view": "Nope" }));

    assert_eq!(result.is_error, Some(true));
    assert_eq!(structured(&result)["error"]["view"], "Nope", "the view is a structured field");
    let message = failure_message(&result);
    assert!(message.contains("Available views: All, ByPriority, AsList"), "{message}");
}

#[test]
fn an_argument_that_does_not_fit_the_schema_is_a_failed_tool() {
    let surface = oracle();
    let result = call(&surface, "resolve_base", json!({ "base": "AllNotes.base", "view": 7 }));

    assert_eq!(result.is_error, Some(true));
    assert_eq!(health_of(&result), "failed");
    assert_eq!(structured(&result)["error"]["construct"], "arguments");
}

#[test]
fn an_unknown_format_is_refused_rather_than_defaulted() {
    let surface = oracle();
    let result = call(&surface, "resolve_base", json!({ "base": "AllNotes.base", "format": "yaml" }));

    assert_eq!(result.is_error, Some(true), "a format we cannot render is not a markdown render");
}

// ---------------------------------------------------------------------------
// get_note
// ---------------------------------------------------------------------------

#[test]
fn get_note_answers_with_the_projection_and_its_provenance() {
    let surface = oracle();
    let result = call(&surface, "get_note", json!({ "path": "Projects/SomeProject.md" }));

    assert!(is_ok(&result));
    assert_eq!(health_of(&result), "ok");
    let payload = structured(&result);
    assert_eq!(payload["path"], "Projects/SomeProject.md");
    assert!(payload["content"].as_str().expect("content").contains("base-rendered"));
    assert!(payload["raw"].as_str().expect("raw").contains("![[Tickets.base]]"));

    let regions = payload["regions"].as_array().expect("regions");
    assert_eq!(regions.len(), 1);
    assert_eq!(regions[0]["path"], "Tickets.base");

    assert!(
        text(&result).contains("`content` is a Projection."),
        "the warning names the trap: {}",
        text(&result)
    );
}

#[test]
fn get_note_raw_returns_the_stored_text_and_no_regions() {
    let surface = oracle();
    let result = call(&surface, "get_note", json!({ "path": "Projects/SomeProject.md", "raw": true }));

    assert!(is_ok(&result));
    let payload = structured(&result);
    assert_eq!(payload["regions"], json!([]), "no regions, because nothing was rendered");
    assert_eq!(payload["content"], payload["raw"]);
    assert!(
        !text(&result).contains("Projection"),
        "the warning would tell the agent to do what it deliberately did not do"
    );
}

#[test]
fn a_note_that_does_not_exist_is_a_failed_tool() {
    let surface = oracle();
    let result = call(&surface, "get_note", json!({ "path": "Nope.md" }));

    assert_eq!(result.is_error, Some(true));
    assert_eq!(structured(&result)["error"]["note"], "Nope.md");
}

// ---------------------------------------------------------------------------
// write_note
// ---------------------------------------------------------------------------

#[test]
fn a_plain_write_is_a_success_that_reports_what_changed() {
    let (dir, surface) = sandbox();
    let result = call(
        &surface,
        "write_note",
        json!({
            "path": "Tickets/Invoice export.md",
            "content": "---\ntags:\n  - ticket\n---\n\n# Renamed\n\nBody.\n"
        }),
    );

    assert!(is_ok(&result));
    assert_eq!(health_of(&result), "ok");
    let payload = structured(&result);
    assert_eq!(payload["refusals"], json!([]));
    assert_eq!(payload["removedRegion"], false);
    assert_eq!(payload["applied"]["changed"], true);
    assert!(payload["applied"]["linesAdded"].as_u64().expect("a count") > 0);
    assert!(
        std::fs::read_to_string(dir.path().join("Tickets/Invoice export.md"))
            .expect("the note is on disk")
            .contains("# Renamed")
    );
}

#[test]
fn a_refused_region_is_a_success_with_partial_health_and_no_is_error() {
    // The distinction that matters. An agent that saw `isError` here would
    // conclude the write did not land and retry it -- and the note on disk is
    // already what it asked for, minus the part that is not a legal edit.
    let (dir, surface) = sandbox();
    let stored = futures_block_on(
        surface.resolver().read_note("Projects/OtherProject.md", bases_mcp::service::NoteOptions::raw()),
    )
    .expect("reads")
    .raw;

    let result = call(
        &surface,
        "write_note",
        json!({ "path": "Projects/OtherProject.md", "content": stored.replace("![[Tickets.base]]", "") }),
    );

    assert!(is_ok(&result), "the write itself succeeded");
    assert_eq!(health_of(&result), "partial-with-errors");

    let payload = structured(&result);
    let refusals = payload["refusals"].as_array().expect("refusals");
    assert_eq!(refusals.len(), 1);
    assert_eq!(refusals[0]["region"], 0);
    assert_eq!(refusals[0]["base"], "Tickets.base");
    assert_eq!(refusals[0]["reason"], "The base region was removed.");
    assert!(refusals[0]["guidance"].as_str().expect("guidance").contains("never removed"));
    assert_eq!(payload["removedRegion"], true);

    let warning = text(&result);
    assert!(warning.contains("1 base region(s) were restored, not edited."), "{warning}");
    assert!(warning.contains("deleted outright"), "{warning}");
    assert!(warning.contains("use add_note_to_base"), "{warning}");

    assert!(
        std::fs::read_to_string(dir.path().join("Projects/OtherProject.md"))
            .expect("the note is on disk")
            .contains("![[Tickets.base]]"),
        "the region was put back"
    );
}

#[test]
fn a_projection_round_trip_is_a_clean_success() {
    let (_dir, surface) = sandbox();
    let projected = call(&surface, "get_note", json!({ "path": "Root Project.md" }));
    let content = structured(&projected)["content"].as_str().expect("content").to_string();

    let result = call(&surface, "write_note", json!({ "path": "Root Project.md", "content": content }));

    assert!(is_ok(&result));
    assert_eq!(health_of(&result), "ok", "a designed round trip is not a partial apply");
    assert_eq!(structured(&result)["refusals"], json!([]));
    assert_eq!(structured(&result)["applied"]["changed"], false, "and the note is unchanged");
}

#[test]
fn a_write_to_a_note_that_does_not_exist_is_a_failed_tool() {
    let (_dir, surface) = sandbox();
    let result = call(&surface, "write_note", json!({ "path": "Nope.md", "content": "# x\n" }));

    assert_eq!(result.is_error, Some(true));
    assert_eq!(health_of(&result), "failed");
}

// ---------------------------------------------------------------------------
// add_note_to_base
// ---------------------------------------------------------------------------

#[test]
fn the_first_call_returns_a_draft_and_writes_nothing() {
    let (_dir, surface) = sandbox();
    let result = call(
        &surface,
        "add_note_to_base",
        json!({ "base": "Tickets.base", "path": "Tickets/__tool.md", "context": "Projects/SomeProject.md" }),
    );

    assert!(is_ok(&result));
    assert_eq!(health_of(&result), "ok");
    let payload = structured(&result);
    assert!(payload.get("written").is_none(), "no note was written: {payload}");
    assert_eq!(payload["path"], "Tickets/__tool.md");
    assert_eq!(payload["base"], "Tickets.base");
    assert_eq!(payload["view"], "All");
    assert_eq!(payload["context"], "Projects/SomeProject.md");
    assert_eq!(payload["draft_id"].as_str().expect("an id").len(), 36);
    assert!(payload["content"].as_str().expect("a draft").contains("- ticket"));

    let note = text(&result);
    assert!(note.contains("NOTHING has been written yet"), "{note}");
    assert!(note.contains("only draft_id and content"), "{note}");
    assert!(note.contains("it expires at "), "{note}");
}

#[test]
fn the_second_call_writes_and_says_the_draft_is_consumed() {
    let (_dir, surface) = sandbox();
    let proposed = call(
        &surface,
        "add_note_to_base",
        json!({ "base": "Tickets.base", "path": "Tickets/__tool.md", "context": "Projects/SomeProject.md" }),
    );
    let draft_id = structured(&proposed)["draft_id"].as_str().expect("an id").to_string();
    let content = structured(&proposed)["content"].as_str().expect("a draft").to_string();

    let result = call(
        &surface,
        "add_note_to_base",
        json!({ "draft_id": draft_id, "content": content }),
    );

    assert!(is_ok(&result));
    assert_eq!(structured(&result)["written"], "Tickets/__tool.md");
    assert_eq!(structured(&result)["verified"], true);
    assert_eq!(structured(&result)["draft_id"], draft_id.as_str());
    assert!(text(&result).contains("The draft is consumed."), "{}", text(&result));
}

#[test]
fn a_verify_failure_is_a_failed_tool_that_inlines_the_base_filter() {
    let (_dir, surface) = sandbox();
    let proposed = call(
        &surface,
        "add_note_to_base",
        json!({ "base": "Tickets.base", "path": "Tickets/__tool.md", "context": "Projects/SomeProject.md" }),
    );
    let draft_id = structured(&proposed)["draft_id"].as_str().expect("an id").to_string();

    let result = call(
        &surface,
        "add_note_to_base",
        json!({ "draft_id": draft_id, "content": "---\ntags:\n  - note\n---\n\n# Nope\n" }),
    );

    assert_eq!(result.is_error, Some(true));
    assert_eq!(health_of(&result), "failed");
    let message = failure_message(&result);
    assert!(message.contains("does not match view \"All\" of Tickets.base"), "{message}");
    assert!(message.contains("file.hasTag(\"ticket\")"), "{message}");
    assert!(message.contains("filters:\n  and:"), "the base's own YAML: {message}");
}

#[test]
fn the_commit_path_never_requires_the_first_call_s_fields() {
    // Regression: `base` and `path` are optional because the agent does not
    // resend them. A schema that marked them required would make every client
    // send them, and the agent-facing call would then be the only untested path.
    let (_dir, surface) = sandbox();
    let proposed = call(
        &surface,
        "add_note_to_base",
        json!({ "base": "Tickets.base", "path": "Tickets/__tool.md", "context": "Projects/SomeProject.md" }),
    );
    let draft_id = structured(&proposed)["draft_id"].as_str().expect("an id").to_string();
    let content = structured(&proposed)["content"].as_str().expect("a draft").to_string();

    let result = call(&surface, "add_note_to_base", json!({ "draft_id": draft_id, "content": content }));
    assert!(is_ok(&result), "two arguments is the whole commit call");
}

// ---------------------------------------------------------------------------
// backlinks
// ---------------------------------------------------------------------------

#[test]
fn backlinks_answers_with_a_count_and_one_entry_per_linking_note() {
    let surface = oracle();
    let result = call(&surface, "backlinks", json!({ "path": "Projects/SomeProject.md" }));

    assert!(is_ok(&result));
    assert_eq!(structured(&result)["path"], "Projects/SomeProject.md");
    let links = structured(&result)["backlinks"].as_array().expect("backlinks");
    assert_eq!(links.len(), 2);
    assert_eq!(links[0]["title"], "Add offline mode", "titled by basename without the extension");
    assert!(text(&result).contains("2 note(s) link to Projects/SomeProject.md."), "{}", text(&result));
}

#[test]
fn an_empty_backlink_list_is_ok_not_failed() {
    let surface = oracle();
    let result = call(&surface, "backlinks", json!({ "path": "Root Ticket.md" }));

    assert!(is_ok(&result));
    assert_eq!(structured(&result)["backlinks"], json!([]));
    assert_eq!(health_of(&result), "ok");
}

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

#[test]
fn an_unknown_tool_is_a_protocol_error_rather_than_a_tool_result() {
    let surface = oracle();
    let error = futures_block_on(surface.dispatch("nope", &Default::default()))
        .expect_err("an unroutable request is an error");
    assert!(error.to_string().contains("Unknown tool"), "{error}");
}

#[test]
fn a_missing_arguments_object_is_the_same_as_an_empty_one() {
    let surface = oracle();
    // `list_bases` takes nothing, so an omitted `arguments` must not be an error.
    let result = futures_block_on(surface.dispatch("list_bases", &Default::default()))
        .expect("list_bases routes");
    assert!(is_ok(&result));
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

fn env(pairs: Vec<(&str, &str)>) -> Env {
    pairs.into_iter().map(|(key, value)| (key.to_string(), value.to_string())).collect()
}

fn ok(result: ConfigResult) -> (VaultConfig, Vec<String>) {
    match result {
        ConfigResult::Ok { config, warnings } => (config, warnings),
        ConfigResult::Err { message } => panic!("expected a configuration, got: {message}"),
    }
}

fn refused(result: ConfigResult) -> String {
    match result {
        ConfigResult::Err { message } => message,
        ConfigResult::Ok { config, .. } => panic!("expected a refusal, got {config:?}"),
    }
}

#[test]
fn an_absolute_vault_path_is_taken() {
    let (config, warnings) = ok(parse_config(&env(vec![("BASES_MCP_VAULT", "/tmp/vault")])));
    assert_eq!(config, VaultConfig::Fs { dir: Path::new("/tmp/vault").to_path_buf() });
    assert!(warnings.is_empty());
}

#[test]
fn a_relative_vault_path_is_resolved_to_an_absolute_one() {
    let (config, _) = ok(parse_config(&env(vec![("BASES_MCP_VAULT", "test/vault")])));
    let VaultConfig::Fs { dir } = config else { panic!("an fs vault") };
    assert!(dir.is_absolute(), "{dir:?}");
}

#[test]
fn a_blank_vault_path_is_a_named_mistake_not_an_absent_vault() {
    for name in ["BASES_MCP_VAULT", "BASES_MCP_WEBDAV_URL"] {
        let message = refused(parse_config(&env(vec![(name, "   ")])));
        assert!(message.contains(name), "{message}");
        assert!(message.contains("set but blank"), "{message}");
    }
}

#[test]
fn webdav_url_user_and_password_together_are_accepted() {
    let (config, warnings) = ok(parse_config(&env(vec![
        ("BASES_MCP_WEBDAV_URL", "http://localhost:5000/vault"),
        ("BASES_MCP_WEBDAV_USER", "u"),
        ("BASES_MCP_WEBDAV_PASSWORD", "p"),
    ])));
    assert_eq!(config, VaultConfig::Webdav {
        url: "http://localhost:5000/vault/".to_string(),
        user: "u".to_string(),
        password: "p".to_string(),
    });
    assert!(warnings.is_empty());
}

#[test]
fn the_base_url_gets_a_trailing_slash_so_a_child_name_cannot_fuse_onto_it() {
    for url in ["http://h:5000/vault", "http://h:5000/vault/"] {
        let (config, _) = ok(parse_config(&env(vec![
            ("BASES_MCP_WEBDAV_URL", url),
            ("BASES_MCP_WEBDAV_USER", "u"),
            ("BASES_MCP_WEBDAV_PASSWORD", "p"),
        ])));
        let VaultConfig::Webdav { url: normalised, .. } = config else { panic!("a webdav vault") };
        assert_eq!(normalised, "http://h:5000/vault/", "{url} normalised");
    }
}

#[test]
fn a_url_without_a_password_is_refused_naming_what_is_missing() {
    let message = refused(parse_config(&env(vec![
        ("BASES_MCP_WEBDAV_URL", "http://localhost:5000/vault"),
        ("BASES_MCP_WEBDAV_USER", "u"),
    ])));
    assert!(message.contains("BASES_MCP_WEBDAV_PASSWORD"), "{message}");
    assert!(!message.contains("BASES_MCP_WEBDAV_USER and"), "{message}");
}

#[test]
fn credentials_inside_the_url_are_refused_and_never_echoed() {
    let message = refused(parse_config(&env(vec![
        ("BASES_MCP_WEBDAV_URL", "http://sekret:alsosecret@localhost:5000/vault"),
        ("BASES_MCP_WEBDAV_USER", "u"),
        ("BASES_MCP_WEBDAV_PASSWORD", "p"),
    ])));
    // The password must not appear even in the refusal explaining why.
    assert!(!message.contains("alsosecret"), "{message}");
    assert!(!message.contains("sekret"), "{message}");
    assert!(message.contains("BASES_MCP_WEBDAV_USER"), "{message}");
    assert!(message.contains("***:***@"), "the URL is named, redacted: {message}");
}

#[test]
fn a_non_http_scheme_is_refused() {
    for url in ["ftp://h/vault", "file:///tmp/vault"] {
        let message = refused(parse_config(&env(vec![
            ("BASES_MCP_WEBDAV_URL", url),
            ("BASES_MCP_WEBDAV_USER", "u"),
            ("BASES_MCP_WEBDAV_PASSWORD", "p"),
        ])));
        assert!(message.contains("not a usable WebDAV base URL"), "{url}: {message}");
    }
}

#[test]
fn an_unparseable_url_is_refused() {
    let message = refused(parse_config(&env(vec![
        ("BASES_MCP_WEBDAV_URL", "not a url"),
        ("BASES_MCP_WEBDAV_USER", "u"),
        ("BASES_MCP_WEBDAV_PASSWORD", "p"),
    ])));
    assert!(message.contains("not a usable WebDAV base URL"), "{message}");
}

#[test]
fn a_credential_without_a_url_is_refused_and_says_so() {
    let message = refused(parse_config(&env(vec![
        ("BASES_MCP_WEBDAV_USER", "u"),
        ("BASES_MCP_WEBDAV_PASSWORD", "p"),
    ])));
    assert!(message.contains("without BASES_MCP_WEBDAV_URL"), "{message}");
}

#[test]
fn the_filesystem_vault_wins_and_the_ignored_variables_are_named() {
    let (config, warnings) = ok(parse_config(&env(vec![
        ("BASES_MCP_VAULT", "/tmp/vault"),
        ("BASES_MCP_WEBDAV_URL", "http://h:5000/vault"),
        ("BASES_MCP_WEBDAV_USER", "u"),
        ("BASES_MCP_WEBDAV_PASSWORD", "p"),
    ])));
    assert!(matches!(config, VaultConfig::Fs { .. }));
    assert_eq!(warnings.len(), 1, "and it is not silent");
    assert!(warnings[0].contains("BASES_MCP_WEBDAV_URL"), "{}", warnings[0]);
    assert!(warnings[0].contains("ignored"), "{}", warnings[0]);
}

#[test]
fn no_backend_at_all_is_a_usage_message_never_a_default_vault() {
    let message = refused(parse_config(&Env::new()));
    assert!(message.contains("BASES_MCP_VAULT"), "{message}");
    assert!(message.contains("BASES_MCP_WEBDAV_URL"), "{message}");
    // Critically: it must not name a concrete path to fall back to.
    assert!(!message.contains("test/vault"), "{message}");
}

#[test]
fn an_empty_environment_is_not_treated_as_a_filesystem_vault() {
    assert!(matches!(parse_config(&Env::new()), ConfigResult::Err { .. }));
}

// ---------------------------------------------------------------------------
// Opening the backend
// ---------------------------------------------------------------------------

/// The error a backend configuration produced, or a panic.
///
/// Spelled out rather than `expect_err` because the success type is a
/// `Box<dyn VaultSource>`, which is not `Debug`, and `Result::expect_err`
/// requires it.
fn refused_source(config: &VaultConfig) -> bases_mcp::error::BasesError {
    match bases_mcp::config::open_source(config) {
        Ok(_) => panic!("expected {config:?} to be refused"),
        Err(error) => error,
    }
}

#[test]
fn a_directory_that_does_not_exist_is_refused_before_the_vault_is_indexed() {
    // `FsVaultSource` swallows a failed readdir, so an unrefused miss would
    // index as an EMPTY vault -- indistinguishable from one that genuinely
    // matches nothing, which is the one confusion this server exists to avoid.
    let config = VaultConfig::Fs { dir: Path::new("/nonexistent/bases-mcp/vault").to_path_buf() };
    let error = refused_source(&config);
    assert!(error.message().contains("cannot open vault at"), "{}", error.message());
    assert_eq!(error.construct(), Some("BASES_MCP_VAULT"));
}

#[test]
fn a_path_that_is_not_a_directory_is_refused() {
    let dir = TempDir::new().expect("a temp dir is writable");
    let file = dir.path().join("not-a-dir.md");
    std::fs::write(&file, "# a note\n").expect("writable");

    let config = VaultConfig::Fs { dir: file };
    let error = refused_source(&config);
    assert!(error.message().contains("not a directory"), "{}", error.message());
}

#[test]
fn a_real_directory_opens() {
    let dir = TempDir::new().expect("a temp dir is writable");
    let config = VaultConfig::Fs { dir: dir.path().to_path_buf() };
    assert!(bases_mcp::config::open_source(&config).is_ok());
}

#[test]
fn a_webdav_url_carrying_credentials_is_refused_by_the_backend_too() {
    // The configuration layer refuses it first; the backend refuses it again, so
    // neither can be reached with a password baked into a URL by another caller.
    let config = VaultConfig::Webdav {
        url: "http://sekret:alsosecret@localhost:5000/vault/".to_string(),
        user: "u".to_string(),
        password: "p".to_string(),
    };
    let error = refused_source(&config);
    assert!(!error.message().contains("alsosecret"), "{}", error.message());
}
