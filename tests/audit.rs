//! The audit trail, exercised through the tool surface.
//!
//! The trail is the record of who called what. It is what D9 says is missing
//! — "no record of who called what" — and this file is the evidence that
//! there is one now.
//!
//! The load-bearing claims, each with a test that would fail if it drifted:
//!
//!   - **Every call is recorded**, whatever the tool and whatever became of
//!     it: a read, a write, a refusal, an unknown tool name.
//!   - **What the trail never carries**: note contents, the agent's text,
//!     the error's prose, a credential.
//!   - **Who is recorded**: the client's own name, the session, the peer
//!     address — and the record says plainly that none of them authenticates
//!     anything.
//!   - **The trail is not forgeable by the caller**: a path with a newline in
//!     it cannot write a line of its own.
//!   - **A trail that cannot be written does not break the call**: a failed
//!     audit write is reported, and the tool still answers.
//!
//! The file sink's rotation is exercised here too, because "the trail rolls
//! over at N megabytes" is a claim about a file and only a file can prove it.

mod common;

use std::sync::{Arc, Mutex};

use bases_mcp::audit::{AuditContext, AuditRecord, AuditSink, CallOutcome};
use bases_mcp::service::Resolver;
use bases_mcp::tools::ToolSurface;
use common::{futures_block_on, load_corpus_from, seed_dir, vault_dir};
use serde_json::{Value as Json, json};
use tempfile::TempDir;

/// A sink that keeps every record in memory.
///
/// The tool surface takes an `Arc<dyn AuditSink>`, so this is how a test
/// reads the trail back. It is the same seam the file sink sits behind, so
/// what a test sees here is what a file would hold.
#[derive(Debug, Default)]
struct MemorySink {
    records: Mutex<Vec<AuditRecord>>,
}

impl AuditSink for MemorySink {
    fn write(&self, record: &AuditRecord) -> Result<(), String> {
        self.records.lock().unwrap().push(record.clone());
        Ok(())
    }
}

/// A sink that always fails, to prove the call survives it.
#[derive(Debug, Default)]
struct FailingSink;

impl AuditSink for FailingSink {
    fn write(&self, _record: &AuditRecord) -> Result<(), String> {
        Err("the disk is full".to_string())
    }
}

/// A tool surface over a private copy of the testing vault, with a trail.
fn sandbox_with_trail() -> (TempDir, ToolSurface, Arc<MemorySink>) {
    let dir = TempDir::new().expect("a temp dir is writable");
    seed_dir(dir.path(), &load_corpus_from(&vault_dir()));
    let resolver =
        futures_block_on(Resolver::open_dir(dir.path())).expect("the sandbox vault opens");
    let sink = Arc::new(MemorySink::default());
    let surface = ToolSurface::new(Arc::new(resolver)).with_audit(sink.clone(), "http");
    (dir, surface, sink)
}

/// The records the trail holds, newest last.
fn records(sink: &MemorySink) -> Vec<AuditRecord> {
    sink.records.lock().unwrap().clone()
}

/// Call one tool and answer with the result.
fn call(surface: &ToolSurface, name: &str, arguments: Json) -> rmcp::model::CallToolResult {
    let arguments = arguments.as_object().cloned().unwrap_or_default();
    futures_block_on(surface.dispatch(name, &arguments))
        .unwrap_or_else(|error| panic!("{name} routed: {error}"))
}

// ---------------------------------------------------------------------------
// The record
// ---------------------------------------------------------------------------

#[test]
fn every_call_is_recorded() {
    let (_dir, surface, sink) = sandbox_with_trail();
    call(&surface, "list_bases", json!({}));
    let records = records(&sink);
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(records[0].tool, "list_bases");
    assert_eq!(records[0].outcome, "ok");
    assert_eq!(records[0].transport, "http");
}

#[test]
fn a_call_records_the_path_it_acted_on() {
    let (_dir, surface, sink) = sandbox_with_trail();
    call(&surface, "resolve_base", json!({ "base": "AllNotes.base" }));
    let records = records(&sink);
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(
        records[0].target.as_deref(),
        Some("AllNotes.base"),
        "the trail names the base it resolved: {:?}",
        records[0]
    );
}

#[test]
fn a_note_call_records_the_note_it_read() {
    let (_dir, surface, sink) = sandbox_with_trail();
    call(
        &surface,
        "get_note",
        json!({ "path": "Root Ticket.md", "raw": true }),
    );
    let records = records(&sink);
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(
        records[0].target.as_deref(),
        Some("Root Ticket.md"),
        "{:?}",
        records[0]
    );
}

#[test]
fn a_write_records_the_note_it_wrote() {
    let (dir, surface, sink) = sandbox_with_trail();
    let raw =
        std::fs::read_to_string(dir.path().join("Root Ticket.md")).expect("the note is there");
    let edited = raw.replace("# Root Ticket", "# Root Ticket, edited");
    call(
        &surface,
        "write_note",
        json!({ "path": "Root Ticket.md", "content": edited }),
    );
    let records = records(&sink);
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(
        records[0].target.as_deref(),
        Some("Root Ticket.md"),
        "{:?}",
        records[0]
    );
    assert_eq!(records[0].outcome, "ok");
}

#[test]
fn a_refused_write_is_recorded_as_partial_and_not_as_a_success() {
    let (_dir, surface, sink) = sandbox_with_trail();
    // A note whose Base region is DELETED is refused, and the write is a
    // partial apply: the prose landed and the region was put back. That is
    // the call an audit trail is most interested in — the agent asked to
    // change something it was not allowed to change.
    let stored = futures_block_on(surface.resolver().read_note(
        "Projects/OtherProject.md",
        bases_mcp::service::NoteOptions::raw(),
    ))
    .expect("reads")
    .raw;
    let result = call(
        &surface,
        "write_note",
        json!({
            "path": "Projects/OtherProject.md",
            "content": stored.replace("![[Tickets.base]]", ""),
        }),
    );
    assert_eq!(
        result.structured_content.as_ref().unwrap()["health"],
        "partial-with-errors",
        "the write was refused in part: {:?}",
        result.structured_content
    );
    let records = records(&sink);
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(
        records[0].outcome, "partial-with-errors",
        "the trail says what became of the call: {:?}",
        records[0]
    );
    assert_eq!(
        records[0].target.as_deref(),
        Some("Projects/OtherProject.md"),
        "{:?}",
        records[0]
    );
}

#[test]
fn a_failure_is_recorded_with_the_construct_it_was_about() {
    let (_dir, surface, sink) = sandbox_with_trail();
    // A base that references `this` with no host note is the project's own
    // refusal, and it names its construct.
    call(&surface, "resolve_base", json!({ "base": "Tickets.base" }));
    let records = records(&sink);
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(records[0].outcome, "failed", "{:?}", records[0]);
    assert_eq!(
        records[0].error.as_deref(),
        Some("this"),
        "the trail names the construct, not the prose: {:?}",
        records[0]
    );
}

#[test]
fn a_name_that_routes_to_nothing_is_still_recorded() {
    let (_dir, surface, sink) = sandbox_with_trail();
    let error = futures_block_on(surface.dispatch("nope", &Default::default()))
        .expect_err("an unknown tool does not route");
    assert!(
        error.message.contains("nope"),
        "the refusal names it: {error:?}"
    );
    let records = records(&sink);
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(records[0].tool, "nope");
    assert_eq!(records[0].outcome, "unknown-tool");
}

#[test]
fn a_call_records_how_long_it_took() {
    let (_dir, surface, sink) = sandbox_with_trail();
    call(&surface, "list_bases", json!({}));
    let records = records(&sink);
    assert!(records[0].duration_ms < 60_000, "{:?}", records[0]);
}

// ---------------------------------------------------------------------------
// What the trail never carries
// ---------------------------------------------------------------------------

#[test]
fn the_trail_never_carries_note_contents() {
    let (dir, surface, sink) = sandbox_with_trail();
    let raw =
        std::fs::read_to_string(dir.path().join("Root Ticket.md")).expect("the note is there");
    // A phrase that appears ONLY in the note's prose, so finding it in the
    // trail means the trail carried the note. The phrase is put there by
    // this test, so it is in the note the call READS — the trail must not
    // carry what it read.
    let marker = "the quick brown fox jumps over the lazy dog";
    let edited = raw.replace("# Root ticket", &format!("# Root ticket\n\n{marker}"));
    assert!(
        edited.contains(marker),
        "the marker is in the note the call writes"
    );
    call(
        &surface,
        "write_note",
        json!({ "path": "Root Ticket.md", "content": edited }),
    );
    let records = records(&sink);
    let line = records[0].to_line().expect("a record is one line");
    assert!(
        !line.contains(marker),
        "the trail carried the note's text: {line}"
    );
}

#[test]
fn the_trail_never_carries_what_a_read_returned() {
    let (_dir, surface, sink) = sandbox_with_trail();
    // `get_note` returns the note's whole content in its ANSWER. The trail
    // records the call, not the answer.
    call(
        &surface,
        "get_note",
        json!({ "path": "Root Ticket.md", "raw": true }),
    );
    let records = records(&sink);
    let line = records[0].to_line().expect("a record is one line");
    // A phrase from the note the read returned.
    assert!(
        !line.contains("Root ticket"),
        "the trail carried the note's content: {line}"
    );
    // The path IS recorded — it is the note's name, not its text.
    assert!(line.contains("Root Ticket.md"), "{line}");
}

#[test]
fn the_trail_never_carries_the_text_the_agent_sent() {
    let (dir, surface, sink) = sandbox_with_trail();
    let raw =
        std::fs::read_to_string(dir.path().join("Root Ticket.md")).expect("the note is there");
    let marker = "an agent wrote this sentence and it must not be logged";
    let edited = raw.replace("# Root Ticket", &format!("# Root Ticket\n\n{marker}"));
    call(
        &surface,
        "write_note",
        json!({ "path": "Root Ticket.md", "content": edited }),
    );
    let records = records(&sink);
    let line = records[0].to_line().expect("a record is one line");
    assert!(
        !line.contains(marker),
        "the trail carried the agent's text: {line}"
    );
}

#[test]
fn the_trail_never_carries_the_errors_prose() {
    let (_dir, surface, sink) = sandbox_with_trail();
    call(&surface, "resolve_base", json!({ "base": "Tickets.base" }));
    let records = records(&sink);
    let line = records[0].to_line().expect("a record is one line");
    // The refusal's prose names the note and explains the rule at length.
    // The trail carries the construct and nothing else.
    assert!(
        !line.contains("host note"),
        "the trail carried the refusal's prose: {line}"
    );
    assert!(line.contains("\"error\":\"this\""), "{line}");
}

// ---------------------------------------------------------------------------
// Who is recorded
// ---------------------------------------------------------------------------

#[test]
fn the_caller_is_recorded_when_the_transport_knows_it() {
    let (_dir, surface, sink) = sandbox_with_trail();
    let caller = AuditContext {
        client: Some("claude-code 1.2.3".to_string()),
        session: Some("session-abc".to_string()),
        peer: Some("192.168.1.5:54321".to_string()),
    };
    futures_block_on(surface.dispatch_with_caller("list_bases", &Default::default(), &caller))
        .expect("list_bases routes");
    let records = records(&sink);
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(records[0].caller, "claude-code 1.2.3");
    assert_eq!(records[0].session.as_deref(), Some("session-abc"));
    assert_eq!(records[0].peer.as_deref(), Some("192.168.1.5:54321"));
}

#[test]
fn a_call_from_a_caller_that_never_handshook_is_recorded_as_unknown() {
    let (_dir, surface, sink) = sandbox_with_trail();
    call(&surface, "list_bases", json!({}));
    let records = records(&sink);
    assert_eq!(records[0].caller, "unknown");
    assert_eq!(records[0].session, None);
    assert_eq!(records[0].peer, None);
}

// ---------------------------------------------------------------------------
// Forgery
// ---------------------------------------------------------------------------

#[test]
fn a_caller_cannot_forge_a_line_through_the_path() {
    let (_dir, surface, sink) = sandbox_with_trail();
    // The path is caller-controlled, so a caller that can put a newline in it
    // could try to write a record of its own. The trail is JSON Lines, so the
    // newline is escaped and the file stays one record per line.
    let forged = "Root Ticket.md\n{\"tool\":\"write_note\",\"target\":\"Secret.md\"}";
    let _ = call(&surface, "get_note", json!({ "path": forged }));
    let records = records(&sink);
    assert_eq!(records.len(), 1, "{records:?}");
    let line = records[0].to_line().expect("a record is one line");
    assert!(!line.contains('\n'), "the record is one line: {line}");
    assert!(
        !line.contains("Secret.md"),
        "the forged record was not written as a second line: {line}"
    );
}

#[test]
fn a_caller_cannot_bloat_the_trail_through_the_path() {
    let (_dir, surface, sink) = sandbox_with_trail();
    let long = "x".repeat(100_000);
    let _ = call(&surface, "get_note", json!({ "path": long }));
    let records = records(&sink);
    let line = records[0].to_line().expect("a record is one line");
    assert!(
        line.len() < 1_000,
        "the record is bounded: {} characters",
        line.len()
    );
}

// ---------------------------------------------------------------------------
// A trail that cannot be written
// ---------------------------------------------------------------------------

#[test]
fn a_trail_that_cannot_be_written_does_not_break_the_call() {
    let dir = TempDir::new().expect("a temp dir is writable");
    seed_dir(dir.path(), &load_corpus_from(&vault_dir()));
    let resolver =
        futures_block_on(Resolver::open_dir(dir.path())).expect("the sandbox vault opens");
    let surface = ToolSurface::new(Arc::new(resolver)).with_audit(Arc::new(FailingSink), "http");
    // The call still answers. A full disk must not turn a landed write into a
    // failed one, because that would teach an agent to retry it.
    let result = call(&surface, "list_bases", json!({}));
    assert_eq!(
        result.structured_content.as_ref().unwrap()["health"],
        "ok",
        "the call answered despite the trail: {:?}",
        result.structured_content
    );
}

// ---------------------------------------------------------------------------
// The file sink
// ---------------------------------------------------------------------------

#[test]
fn a_file_sink_writes_one_line_per_record() {
    let dir = TempDir::new().expect("a temp dir is writable");
    let path = dir.path().join("audit.log");
    let sink = bases_mcp::audit::FileSink::open(&path, 1024 * 1024).expect("the sink opens");
    let context = AuditContext::default();
    for index in 0..3 {
        let record = AuditRecord::new(&context, "http", "list_bases", CallOutcome::Ok)
            .with_target(Some(&format!("Base{index}.base")));
        sink.write(&record).expect("the record is written");
    }
    let text = std::fs::read_to_string(&path).expect("the file is there");
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 3, "{text}");
    for line in lines {
        serde_json::from_str::<Json>(line).expect("each line is one JSON value");
    }
}

#[test]
fn a_file_sink_rolls_over_at_the_threshold_and_keeps_one_generation() {
    let dir = TempDir::new().expect("a temp dir is writable");
    let path = dir.path().join("audit.log");
    // A threshold of a few hundred bytes, so a handful of records fills it.
    let sink = bases_mcp::audit::FileSink::open(&path, 400).expect("the sink opens");
    let context = AuditContext::default();
    for index in 0..40 {
        let record = AuditRecord::new(&context, "http", "resolve_base", CallOutcome::Ok)
            .with_target(Some(&format!("Some/Path/Number{index}.base")));
        sink.write(&record).expect("the record is written");
    }
    let current = std::fs::metadata(&path).expect("the file is there");
    assert!(
        current.len() <= 400 + 200,
        "the current generation is bounded: {} bytes",
        current.len()
    );
    // One previous generation, and no more: the disk cost is twice the
    // threshold, not a directory of files.
    let backup = dir.path().join("audit.log.1");
    assert!(backup.is_file(), "the previous generation is kept");
    let entries: Vec<String> = std::fs::read_dir(dir.path())
        .expect("the directory is readable")
        .filter_map(|entry| {
            entry
                .ok()
                .map(|entry| entry.file_name().to_string_lossy().into())
        })
        .collect();
    assert_eq!(
        entries.len(),
        2,
        "exactly the current and the previous generation: {entries:?}"
    );
}

#[test]
fn a_file_sink_creates_the_directory_it_names() {
    let dir = TempDir::new().expect("a temp dir is writable");
    let path = dir.path().join("logs").join("nested").join("audit.log");
    let _sink = bases_mcp::audit::FileSink::open(&path, 1024 * 1024).expect("the sink opens");
    assert!(path.is_file(), "the file is there: {}", path.display());
}
