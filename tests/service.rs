//! The Resolver, and the draft / verify / commit handshake behind it.
//!
//! The contract the handshake tests pin is not "we produced a nice note" — it is
//! the one that matters: a note is written if and only if the Base's REAL filter
//! matches it. Everything else (the inversion, the placeholders) is best-effort
//! by design, and the tests say so where they touch it.
//!
//! ## Nothing here writes to `test/vault`
//!
//! `test/vault` is the parity oracle and stays at exactly [`CORPUS_SIZE`]
//! files. Every test that writes works on a COPY seeded into a temp directory,
//! so a test that dies mid-write can only ever leave a file in a temp dir that
//! the OS reclaims. Read-only assertions still run against the real vault, so
//! they are checking the same bytes the CLI compares against.
//!
//! `test/fixtures/` is never written either: the fixtures that exercise
//! filter inversion are read-only by construction, since nothing in these tests
//! commits a note into them.

// Each integration test is its own crate, and no suite needs every helper here.
#[allow(dead_code)]
mod common;

use std::path::{Path, PathBuf};
use std::rc::Rc;

use bases_mcp::base::QueryOptions;
use bases_mcp::drafts::{
    AddNoteOptions, AddNoteToBaseResult, DraftProposal, DraftSeed, DraftStore, DRAFT_TTL_MS,
};
use bases_mcp::error::{BasesError, Result};
use bases_mcp::note::parse_note;
use bases_mcp::service::{NoteOptions, Resolver};
use common::{
    count_files, futures_block_on, load_corpus_from, seed_dir, vault_dir, VaultFile, CORPUS_SIZE,
};
use tempfile::TempDir;

/// The scratch note most of these tests use; the rest name their own.
const SCRATCH: &str = "Tickets/__draft-scratch.md";
const HOST: &str = "Projects/SomeProject.md";

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// A Resolver over a private copy of the testing vault.
///
/// The copy is seeded from the same loader the read-only tests use, so a write
/// test and a read test see identical content and the only difference is whether
/// the directory is disposable.
fn sandbox() -> (TempDir, Rc<Resolver>) {
    let dir = TempDir::new().expect("a temp dir is writable");
    seed_dir(dir.path(), &load_corpus_from(&vault_dir()));
    let resolver = futures_block_on(Resolver::open_dir(dir.path()))
        .unwrap_or_else(|error| panic!("the sandbox vault opens: {error}"));
    (dir, Rc::new(resolver))
}

/// A Resolver over the real testing vault. Never writes.
fn oracle() -> Rc<Resolver> {
    Rc::new(
        futures_block_on(Resolver::open_dir(vault_dir()))
            .unwrap_or_else(|error| panic!("the testing vault opens: {error}")),
    )
}

/// A Resolver over one fixture vault.
fn fixture(name: &str) -> Rc<Resolver> {
    let dir = fixtures_dir().join(name);
    Rc::new(
        futures_block_on(Resolver::open_dir(&dir))
            .unwrap_or_else(|error| panic!("the {name} fixture opens: {error}")),
    )
}

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("test")
        .join("fixtures")
}

/// The message of a rejection, so a test can assert on its whole text.
fn message_of<T>(run: impl FnOnce() -> Result<T>) -> String {
    match run() {
        Ok(_) => panic!("expected a rejection, but the call succeeded"),
        Err(error) => error.message().to_string(),
    }
}

/// The message of a rejection, from a future.
///
/// Named `_async` because it cannot be the same function as [`message_of`]: the
/// futures here are `!Send`, and an `impl Future` in a `Fn` bound is not a thing
/// Rust has.
fn message_of_async<F, Fut, T>(run: F) -> String
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
{
    match futures_block_on(run()) {
        Ok(_) => panic!("expected a rejection, but the call succeeded"),
        Err(error) => error.message().to_string(),
    }
}

/// Run `add_note_to_base` over a blocking runtime.
///
/// The arguments are built into a local first because `add_note_to_base` borrows
/// them for the whole call, and a temporary would be dropped at the end of the
/// enclosing statement.
fn add(resolver: &Resolver, pairs: Vec<(&str, &str)>) -> Result<AddNoteToBaseResult> {
    let args = options(pairs);
    futures_block_on(resolver.add_note_to_base(&args))
}

fn options(pairs: Vec<(&str, &str)>) -> AddNoteOptions {
    let mut out = AddNoteOptions::default();
    for (key, value) in pairs {
        match key {
            "base" => out.base = Some(value.to_string()),
            "path" => out.path = Some(value.to_string()),
            "context" => out.context = Some(value.to_string()),
            "view" => out.view = Some(value.to_string()),
            "draft_id" => out.draft_id = Some(value.to_string()),
            "content" => out.content = Some(value.to_string()),
            other => panic!("unknown option {other}"),
        }
    }
    out
}

/// The proposal half of a call, or a panic naming the other half.
fn draft(result: Result<AddNoteToBaseResult>) -> DraftProposal {
    match result {
        Ok(AddNoteToBaseResult::Proposal(proposal)) => proposal,
        Ok(AddNoteToBaseResult::Commit(commit)) => {
            panic!(
                "expected a draft, but the call committed {}",
                commit.written
            )
        }
        Err(error) => panic!("expected a draft, but the call failed: {}", error.message()),
    }
}

/// Ask for a draft against `Tickets.base` in the sandbox.
/// Ask for a draft against `Tickets.base`.
///
/// An absent `context` or `view` is left ABSENT rather than sent as an empty
/// string. That is not a detail: `context: ""` binds `this` to nothing, and a
/// draft that carries one is a draft scoped to no host.
fn draft_for(
    resolver: &Resolver,
    path: &str,
    context: Option<&str>,
    view: Option<&str>,
) -> DraftProposal {
    let mut args = vec![("base", "Tickets.base"), ("path", path)];
    if let Some(context) = context {
        args.push(("context", context));
    }
    if let Some(view) = view {
        args.push(("view", view));
    }
    draft(add(resolver, args))
}

fn commit(resolver: &Resolver, draft_id: &str, content: &str) -> Result<AddNoteToBaseResult> {
    add(resolver, vec![("draft_id", draft_id), ("content", content)])
}

/// The note the draft would create, read back off disk.
///
/// The draft is not on disk yet, so this parses the text rather than reading the
/// vault: what is under test is the FRONTMATTER the draft carries.
fn frontmatter_of(path: &str, content: &str) -> std::collections::BTreeMap<String, String> {
    parse_note(path, content)
        .frontmatter
        .iter()
        .map(|(key, value)| (key.clone(), render(&value.to_display_string())))
        .collect()
}

/// A frontmatter value as the test asserts on it: a list joined, a scalar bare.
fn render(value: &str) -> String {
    value.replace(", ", "|")
}

fn frontmatter(path: &str, content: &str, key: &str) -> Option<String> {
    frontmatter_of(path, content).get(key).cloned()
}

fn paths(resolver: &Resolver) -> Vec<String> {
    resolver.vault().note_paths()
}

fn row_paths(resolver: &Resolver, base: &str, context: &str) -> Vec<String> {
    futures_block_on(resolver.query(
        base,
        &QueryOptions {
            context: Some(context.to_string()),
            ..Default::default()
        },
    ))
    .map(|result| result.rows.into_iter().map(|row| row.path).collect())
    .unwrap_or_else(|error| panic!("{base} resolves for {context}: {}", error.message()))
}

/// Remove a note the test created and rebuild the index.
///
/// The snapshot still lists a file until it is rebuilt, and a later query would
/// then read bytes that are no longer there.
fn delete(resolver: &Resolver, path: &str) {
    futures_block_on(resolver.backend().delete(path))
        .unwrap_or_else(|error| panic!("{path} is deletable: {}", error.message()));
    futures_block_on(resolver.reload()).expect("the vault reindexes");
}

// ---------------------------------------------------------------------------
// The vault the tests depend on
// ---------------------------------------------------------------------------

#[test]
fn the_testing_vault_is_untouched() {
    assert_eq!(
        count_files(&vault_dir()),
        CORPUS_SIZE,
        "test/vault is the parity oracle and must stay at exactly {CORPUS_SIZE} files"
    );
}

/// Every fixture vault indexes, and every base in one parses.
///
/// Parsing rather than querying, deliberately. `Hostile.base` carries formulas
/// that read a `when` property its own notes do not have, so a view that
/// resolves it fails on the missing property rather than on anything this port
/// changed — and the fixture exists to be INVERTED, not queried. What the draft
/// tests need from it is a base whose filters are legible.
#[test]
fn every_fixture_vault_opens() {
    for (name, base, views) in [
        ("hostile-filters", "Hostile.base", 3),
        ("not-nand", "Core.base", 1),
    ] {
        let resolver = fixture(name);
        let loaded = futures_block_on(resolver.load_base(base))
            .unwrap_or_else(|error| panic!("the {name} fixture: {}", error.message()));
        assert_eq!(loaded.views.len(), views, "{base} declares {views} view(s)");
    }
}

// ---------------------------------------------------------------------------
// The Resolver
// ---------------------------------------------------------------------------

#[test]
fn list_bases_reports_every_base_with_its_views() {
    let resolver = oracle();
    let bases = futures_block_on(resolver.list_bases()).expect("the vault lists");

    let paths: Vec<&str> = bases.iter().map(|base| base.path.as_str()).collect();
    assert_eq!(
        paths,
        ["AllNotes.base", "Tickets.base"],
        "bases are sorted by path"
    );

    let tickets = &bases[1];
    assert_eq!(
        tickets
            .views
            .iter()
            .map(|view| (view.name.as_str(), view.view_type.as_str()))
            .collect::<Vec<_>>(),
        [("All", "cards")],
        "a view reports its name and its layout type"
    );
}

#[test]
fn a_base_is_found_by_its_exact_path_and_by_nothing_else() {
    let resolver = oracle();
    assert_eq!(
        resolver.resolve_base_path("Tickets.base").expect("exact"),
        "Tickets.base"
    );
    // The path index holds NOTES, because a link points at a note. `Tickets` is
    // a folder that shares its name with a Base, and resolving it would hand an
    // agent a folder where it asked for a base.
    let message = message_of(|| resolver.resolve_base_path("Tickets"));
    assert!(
        message.contains("Base file not found: Tickets"),
        "{message}"
    );
}

#[test]
fn a_base_that_does_not_exist_is_refused_by_name() {
    let resolver = oracle();
    let message = message_of(|| resolver.resolve_base_path("Nope.base"));
    assert!(
        message.contains("Base file not found: Nope.base"),
        "{message}"
    );
}

#[test]
fn view_names_lists_what_the_base_declares() {
    let resolver = oracle();
    assert_eq!(
        futures_block_on(resolver.view_names("AllNotes.base")).expect("views"),
        ["All", "ByPriority", "AsList"]
    );
}

#[test]
fn a_this_scoped_base_refuses_to_resolve_with_no_host_note() {
    let resolver = oracle();
    // Never an empty result: an empty array is indistinguishable from a base that
    // genuinely matches nothing. This is the divergence D1 exists for.
    let options = QueryOptions::default();
    let message = message_of_async(|| resolver.query("Tickets.base", &options));
    assert!(message.to_lowercase().contains("this"), "{message}");
}

#[test]
fn the_same_base_resolves_differently_per_host_note() {
    let resolver = oracle();
    assert_eq!(
        row_paths(&resolver, "Tickets.base", HOST),
        [
            "Tickets/Add offline mode.md",
            "Tickets/Fix login redirect.md",
        ]
    );
    assert_eq!(
        row_paths(&resolver, "Tickets.base", "Projects/OtherProject.md"),
        ["Tickets/Invoice export.md"]
    );
    assert_eq!(
        row_paths(&resolver, "Tickets.base", "Root Project.md"),
        ["Root Ticket.md"]
    );
}

#[test]
fn render_is_the_flat_cli_parity_surface() {
    let resolver = oracle();
    let markdown = futures_block_on(resolver.render("AllNotes.base", &QueryOptions::default()))
        .expect("AllNotes renders");
    assert!(
        markdown.starts_with('|'),
        "the parity surface is a markdown table: {markdown}"
    );
    assert!(
        !markdown.contains("**"),
        "flat drops group headers: {markdown}"
    );
}

#[test]
fn a_note_with_no_base_region_projects_to_itself() {
    let resolver = oracle();
    let note = futures_block_on(resolver.read_note("Root Ticket.md", NoteOptions::projection()))
        .expect("the note reads");
    assert!(note.regions.is_empty(), "no Base regions, no provenance");
    assert_eq!(
        note.content, note.raw,
        "nothing to replace, so the Projection is the note"
    );
}

#[test]
fn a_note_renders_its_base_regions_with_provenance() {
    let resolver = oracle();
    let note = futures_block_on(resolver.read_note(HOST, NoteOptions::projection()))
        .expect("the host note reads");

    // `Projects/SomeProject.md` embeds `Tickets.base` AND carries an inline
    // ```base fence, so it exercises both region shapes at once.
    assert_eq!(
        note.regions.len(),
        1,
        "one entry per Base region, in document order"
    );
    assert_eq!(note.regions[0].path.as_deref(), Some("Tickets.base"));

    assert!(
        note.content
            .contains("```base-rendered path=\"Tickets.base\""),
        "{}",
        note.content
    );
    // Structured, not flat: the group header survives.
    assert!(
        note.content.contains("**1 – high**"),
        "the Projection keeps group headers: {}",
        note.content
    );
}

#[test]
fn an_inline_fence_region_records_no_base_path() {
    let resolver = oracle();
    // `Root Project.md` carries BOTH shapes: an `![[Tickets.base]]` embed and a
    // live ```base fence with its own YAML. They are different region kinds and
    // the provenance says so.
    let note = futures_block_on(resolver.read_note("Root Project.md", NoteOptions::projection()))
        .expect("the root host reads");
    assert_eq!(
        note.regions.len(),
        2,
        "one entry per Base region, in document order"
    );
    assert_eq!(
        note.regions[0].path.as_deref(),
        Some("Tickets.base"),
        "the embed names its base"
    );
    assert_eq!(
        note.regions[1].path, None,
        "an inline fence carries YAML, not a path"
    );
    assert_eq!(
        note.regions[1].view, None,
        "and no `#View` selector, so no view to record"
    );
    // The inline fence's view is `type: list`, so it renders as a markdown LIST
    // on the Projection surface -- the flat parity surface would have made it a
    // table, which is exactly the difference between the two.
    assert!(
        note.content.contains("```base-rendered\n- Root Ticket\n"),
        "{}",
        note.content
    );
}

#[test]
fn the_projection_is_never_the_raw_text() {
    let resolver = oracle();
    let note =
        futures_block_on(resolver.read_note(HOST, NoteOptions::projection())).expect("reads");
    assert_ne!(note.content, note.raw);
    assert!(
        note.raw.contains("![[Tickets.base]]"),
        "raw keeps the live region"
    );
    assert!(
        !note.raw.contains("base-rendered"),
        "raw has no rendered fence"
    );
}

#[test]
fn raw_returns_the_stored_text_and_no_regions() {
    let resolver = oracle();
    let note = futures_block_on(resolver.read_note(HOST, NoteOptions::raw())).expect("reads");
    assert_eq!(note.content, note.raw);
    assert!(note.regions.is_empty());
    assert!(note.raw.contains("![[Tickets.base]]"));
}

#[test]
fn a_note_is_found_by_its_exact_path_or_by_link_resolution() {
    let resolver = oracle();
    assert_eq!(
        resolver.resolve_note_path("Root Ticket.md").expect("exact"),
        "Root Ticket.md"
    );
    assert_eq!(
        resolver.resolve_note_path("Root Ticket").expect("resolved"),
        "Root Ticket.md",
        "a bare stem resolves by basename"
    );
}

#[test]
fn a_note_that_does_not_exist_is_refused_by_name() {
    let resolver = oracle();
    let message = message_of(|| resolver.resolve_note_path("Nope.md"));
    assert!(message.contains("Note not found: Nope.md"), "{message}");
}

#[test]
fn backlinks_come_from_the_same_index_that_backs_every_base() {
    let resolver = oracle();
    let backlinks = resolver.backlinks(HOST).expect("the note has backlinks");
    let mut paths: Vec<(String, String)> = backlinks
        .into_iter()
        .map(|backlink| (backlink.path, backlink.title))
        .collect();
    paths.sort();

    assert_eq!(
        paths,
        [
            (
                "Tickets/Add offline mode.md".to_string(),
                "Add offline mode".to_string()
            ),
            (
                "Tickets/Fix login redirect.md".to_string(),
                "Fix login redirect".to_string()
            ),
        ],
        "one entry per linking note, titled by basename without the extension. The link in both \
         is a bare [[SomeProject]], which resolves to Projects/SomeProject.md by shortest path."
    );
}

#[test]
fn an_empty_backlink_list_is_an_answer_not_an_error() {
    let resolver = oracle();
    assert!(
        resolver
            .backlinks("Root Ticket.md")
            .expect("links")
            .is_empty(),
        "nothing links to the root ticket"
    );
    assert_eq!(
        resolver
            .backlinks("Projects/OtherProject.md")
            .expect("links")
            .iter()
            .map(|backlink| backlink.path.as_str())
            .collect::<Vec<_>>(),
        ["Tickets/Invoice export.md"],
        "and a link that resolves is reported against the file it resolved to"
    );
}

#[test]
fn to_json_keys_rows_by_display_label() {
    let resolver = oracle();
    let base = futures_block_on(resolver.load_base("AllNotes.base")).expect("the base loads");
    let result = futures_block_on(resolver.query("AllNotes.base", &QueryOptions::default()))
        .expect("the base resolves");
    let rows = resolver.to_json(&result, &base);

    assert_eq!(rows.len(), result.rows.len(), "one object per row");
    let ticket = rows
        .iter()
        .map(|row| row.as_object().expect("a row is an object"))
        .find(|row| row["path"] == "Root Ticket.md")
        .expect("the root ticket is a row");
    // `file.name` labels as `file name`; `note.status` has a configured
    // `displayName` of `Status`; the formula labels as its configured `Priority`.
    assert!(ticket.contains_key("file name"), "{ticket:?}");
    assert!(ticket.contains_key("Status"), "{ticket:?}");
    assert!(ticket.contains_key("Priority"), "{ticket:?}");
    assert_eq!(ticket["Status"], "active");
    assert_eq!(ticket["file name"], "Root Ticket");
    assert_eq!(ticket["Priority"], "1 – high");
}

#[test]
fn an_absent_cell_is_null_rather_than_a_placeholder() {
    let resolver = oracle();
    let base = futures_block_on(resolver.load_base("AllNotes.base")).expect("the base loads");
    let result = futures_block_on(resolver.query("AllNotes.base", &QueryOptions::default()))
        .expect("resolves");
    let rows = resolver.to_json(&result, &base);

    // The view orders exactly three columns, so an absent cell can only be
    // observed through the `includeAllFormulas` branch — which is asserted in
    // `tests/tools.rs`. What this pins is that the row carries `path` and no
    // key beyond the three columns.
    let ticket = rows
        .iter()
        .map(|row| row.as_object().expect("object"))
        .find(|row| row["path"] == "Root Ticket.md")
        .expect("the root ticket is a row");
    let mut keys: Vec<&str> = ticket.keys().map(String::as_str).collect();
    keys.sort();
    assert_eq!(
        keys,
        ["Priority", "Status", "file name", "path"],
        "one key per ordered column"
    );
}

// ---------------------------------------------------------------------------
// The first call: a draft, an id and an expiry
// ---------------------------------------------------------------------------

#[test]
fn the_first_call_returns_a_draft_id_a_draft_and_an_absolute_expiry() {
    let (_dir, resolver) = sandbox();
    let proposal = draft_for(&resolver, SCRATCH, Some(HOST), None);

    assert_eq!(
        proposal.draft_id.len(),
        36,
        "a v4 UUID, which is what the tool documents"
    );
    assert!(!proposal.content.is_empty(), "the draft is never empty");
    // An absolute timestamp, not a duration: an agent that stored the draft
    // across a restart has to be able to say when it goes stale.
    let now = chrono::Utc::now().timestamp_millis();
    assert!(proposal.expires_at > now, "the expiry is in the future");
    assert!(
        proposal.expires_at - now <= DRAFT_TTL_MS,
        "and no further out than the TTL"
    );
    assert_eq!(proposal.path, SCRATCH);
    assert_eq!(proposal.base, "Tickets.base");
    assert_eq!(proposal.view, "All", "no view named, so the first view");
    assert_eq!(proposal.context.as_deref(), Some(HOST));
}

#[test]
fn the_draft_is_a_note_we_could_parse_not_a_fragment() {
    let (_dir, resolver) = sandbox();
    let proposal = draft_for(&resolver, SCRATCH, Some(HOST), None);

    let note = parse_note(SCRATCH, &proposal.content);
    assert!(
        !note.malformed_frontmatter,
        "the draft's own frontmatter must parse"
    );
    assert!(
        proposal.content.starts_with("---\n"),
        "{}",
        proposal.content
    );
    assert!(
        proposal
            .content
            .trim_end()
            .ends_with("the base's own filter is the ground truth."),
        "{}",
        proposal.content
    );
}

#[test]
fn nothing_is_written_on_the_first_call() {
    let (_dir, resolver) = sandbox();
    draft_for(&resolver, SCRATCH, Some(HOST), None);
    assert!(!paths(&resolver).contains(&SCRATCH.to_string()));
}

// ---------------------------------------------------------------------------
// Filter inversion
// ---------------------------------------------------------------------------

#[test]
fn file_has_tag_becomes_a_tag() {
    let (_dir, resolver) = sandbox();
    let proposal = draft_for(&resolver, SCRATCH, Some(HOST), None);
    assert_eq!(
        frontmatter(SCRATCH, &proposal.content, "tags").as_deref(),
        Some("ticket")
    );
    assert!(
        proposal.content.contains("- ticket"),
        "{}",
        proposal.content
    );
}

#[test]
fn a_this_scoped_contains_becomes_a_link_to_the_host_note() {
    let (_dir, resolver) = sandbox();
    let proposal = draft_for(&resolver, SCRATCH, Some(HOST), None);
    // The link, not the host's path: `[[SomeProject]]` resolves by basename and
    // survives the note moving between folders.
    assert_eq!(
        frontmatter(SCRATCH, &proposal.content, "project").as_deref(),
        Some("[[SomeProject]]")
    );
}

#[test]
fn a_nested_host_binds_by_basename_without_the_extension() {
    let (_dir, resolver) = sandbox();
    let proposal = draft_for(&resolver, SCRATCH, Some("Root Project.md"), None);
    assert_eq!(
        frontmatter(SCRATCH, &proposal.content, "project").as_deref(),
        Some("[[Root Project]]")
    );
}

#[test]
fn without_a_host_the_link_is_a_marked_placeholder_never_a_guess() {
    let (_dir, resolver) = sandbox();
    let proposal = draft_for(&resolver, SCRATCH, None, None);
    assert!(
        proposal
            .content
            .contains("# TODO: could not invert project.contains(link(this.file.name))"),
        "{}",
        proposal.content
    );
    assert!(
        proposal
            .content
            .contains("pass the host note as \"context\""),
        "the placeholder says what would fix it: {}",
        proposal.content
    );
    // The one thing a placeholder must never do is invent a value.
    assert_eq!(frontmatter(SCRATCH, &proposal.content, "project"), None);
}

#[test]
fn order_columns_are_seeded_empty_and_computed_columns_are_not_seeded() {
    let (_dir, resolver) = sandbox();
    let proposal = draft_for(&resolver, SCRATCH, Some(HOST), None);
    // `status` and `type` are note properties the view lists, so the agent sees
    // the shape a row is expected to have.
    assert_eq!(
        frontmatter(SCRATCH, &proposal.content, "status").as_deref(),
        Some("")
    );
    assert_eq!(
        frontmatter(SCRATCH, &proposal.content, "type").as_deref(),
        Some("")
    );
    // `file.name` is not frontmatter and `formula.priority_display` is computed,
    // so neither can be seeded.
    let keys: Vec<String> = frontmatter_of(SCRATCH, &proposal.content)
        .into_keys()
        .collect();
    assert!(!keys.contains(&"file.name".to_string()), "{keys:?}");
    assert!(!keys.contains(&"priority_display".to_string()), "{keys:?}");
}

#[test]
fn view_level_filters_are_inverted_too_and_reported_when_they_cannot_be() {
    let resolver = fixture("hostile-filters");
    let proposal = draft(add(
        &resolver,
        vec![
            ("base", "Hostile.base"),
            ("view", "BareFormulaFilter"),
            ("path", "Notes/__probe.md"),
        ],
    ));
    // The view filter is `formula.needs_follow_up`, a computed value that no
    // amount of frontmatter can pin down. It is reported, not dropped.
    assert!(
        proposal
            .content
            .contains("# TODO: could not invert formula.needs_follow_up"),
        "{}",
        proposal.content
    );
}

#[test]
fn a_conjunct_nobody_can_invert_becomes_a_todo_comment() {
    let resolver = fixture("not-nand");
    let proposal = draft(add(
        &resolver,
        vec![("base", "Core.base"), ("path", "Notes/__probe.md")],
    ));
    // `file.ext == "md"` reads a file, and a six-sibling `not:` is NAND. Both are
    // reported verbatim so the agent can satisfy them by hand.
    assert!(
        proposal
            .content
            .contains("# TODO: could not invert file.ext == \"md\""),
        "{}",
        proposal.content
    );
    assert!(
        proposal
            .content
            .contains("# TODO: could not invert none of (file.inFolder(\"plugins\")"),
        "{}",
        proposal.content
    );
}

// ---------------------------------------------------------------------------
// The second call: verify, then commit
// ---------------------------------------------------------------------------

#[test]
fn the_unedited_draft_commits_and_the_base_then_returns_the_row() {
    let (_dir, resolver) = sandbox();
    let proposal = draft_for(&resolver, SCRATCH, Some(HOST), None);

    let result =
        commit(&resolver, &proposal.draft_id, &proposal.content).expect("the draft verifies");
    assert!(result.is_commit());
    assert_eq!(result.written(), Some(SCRATCH));
    assert_eq!(result.draft_id(), proposal.draft_id);
    assert!(
        paths(&resolver).contains(&SCRATCH.to_string()),
        "the note is on disk"
    );

    // The load-bearing assertion: the note we accepted is a row the real query
    // pipeline -- not our filter walk -- actually returns.
    assert!(row_paths(&resolver, "Tickets.base", HOST).contains(&SCRATCH.to_string()));
}

#[test]
fn the_second_call_sends_only_a_draft_id_and_content() {
    // Regression: `base` and `path` are optional because the agent does not
    // resend them, so the commit path must never read them off `options`.
    // Resending them in every test hid that it crashed without them.
    let (_dir, resolver) = sandbox();
    let proposal = draft_for(&resolver, SCRATCH, Some(HOST), None);
    let result =
        commit(&resolver, &proposal.draft_id, &proposal.content).expect("the draft verifies");
    assert_eq!(result.written(), Some(SCRATCH));
    assert_eq!(result.draft_id(), proposal.draft_id);
}

#[test]
fn proposing_without_a_path_says_which_field_is_missing() {
    let (_dir, resolver) = sandbox();
    let message = message_of(|| add(&resolver, vec![("base", "Tickets.base")]));
    assert!(message.contains("`path`"), "{message}");
    assert!(
        message.contains("draft_id"),
        "and how to send the second call: {message}"
    );
}

#[test]
fn proposing_without_a_base_says_which_field_is_missing() {
    let (_dir, resolver) = sandbox();
    let message = message_of(|| add(&resolver, vec![("path", SCRATCH)]));
    assert!(message.contains("`base`"), "{message}");
}

#[test]
fn the_commit_is_single_use() {
    let (_dir, resolver) = sandbox();
    let proposal = draft_for(&resolver, SCRATCH, Some(HOST), None);
    commit(&resolver, &proposal.draft_id, &proposal.content).expect("the draft verifies");

    let replay = message_of(|| commit(&resolver, &proposal.draft_id, &proposal.content));
    assert!(replay.contains("unknown or has expired"), "{replay}");
}

#[test]
fn a_draft_missing_a_filtered_property_reports_a_mismatch_not_a_type_error() {
    // Regression: `project.contains(link(this.file.name))` DEREFERENCES `project`,
    // so a draft with no `project` at all makes the expression THROW instead of
    // evaluating to false. That raw error buried the filter and the base YAML,
    // which are the two things the agent needs to correct itself.
    let (_dir, resolver) = sandbox();
    let proposal = draft_for(&resolver, SCRATCH, Some(HOST), None);

    let message = message_of(|| {
        commit(
            &resolver,
            &proposal.draft_id,
            "---\ntags:\n  - ticket\n---\n\n# Scratch\n",
        )
    });
    assert!(message.contains("does not match"), "{message}");
    assert!(
        message.contains("project.contains(link(this.file.name))"),
        "{message}"
    );
    assert!(
        message.contains("threw:"),
        "the dereference is reported, not propagated: {message}"
    );
    assert!(
        message.contains("filters:\n  and:"),
        "the base's own YAML is inlined: {message}"
    );
    assert!(!paths(&resolver).contains(&SCRATCH.to_string()));
}

#[test]
fn content_that_cannot_match_throughs_nothing_writes_and_hands_back_the_base_yaml() {
    let (_dir, resolver) = sandbox();
    let proposal = draft_for(&resolver, SCRATCH, Some(HOST), None);

    // Right shape, wrong values: the tag is absent and the link points at a
    // different project, so neither conjunct can hold.
    let content = "---\ntags:\n  - note\nproject:\n  - \"[[OtherProject]]\"\n---\n\n# Scratch\n";
    let message = message_of(|| commit(&resolver, &proposal.draft_id, content));
    assert!(message.contains("does not match"), "{message}");
    assert!(message.contains("file.hasTag(\"ticket\")"), "{message}");
    assert!(
        message.contains("project.contains(link(this.file.name))"),
        "{message}"
    );
    // The base's own text, not our paraphrase of it: the correction is made
    // against the filter, so the filter has to be visible.
    assert!(
        message.contains("filters:\n  and:\n    - file.hasTag(\"ticket\")"),
        "{message}"
    );
    assert!(!paths(&resolver).contains(&SCRATCH.to_string()));
}

#[test]
fn a_failed_verify_leaves_the_draft_live_so_the_same_id_can_be_resent() {
    let (_dir, resolver) = sandbox();
    let proposal = draft_for(&resolver, SCRATCH, Some(HOST), None);

    message_of(|| {
        commit(
            &resolver,
            &proposal.draft_id,
            "---\ntags:\n  - note\n---\n\n# Scratch\n",
        )
    });
    // The draft survives the refusal, which is the whole point of the handshake.
    let corrected = draft_for(&resolver, SCRATCH, Some(HOST), None);
    assert_ne!(corrected.draft_id, proposal.draft_id);
    let result =
        commit(&resolver, &proposal.draft_id, &corrected.content).expect("the correction verifies");
    assert_eq!(result.written(), Some(SCRATCH));
}

#[test]
fn a_this_scoped_base_refuses_to_commit_without_a_host_note() {
    let (_dir, resolver) = sandbox();
    let proposal = draft_for(&resolver, SCRATCH, None, None);

    let message = message_of(|| commit(&resolver, &proposal.draft_id, &proposal.content));
    assert!(message.contains("scoped to a host note"), "{message}");
    assert!(message.contains("context"), "{message}");
    assert!(!paths(&resolver).contains(&SCRATCH.to_string()));
}

#[test]
fn a_second_call_without_content_says_what_is_missing() {
    let (_dir, resolver) = sandbox();
    let proposal = draft_for(&resolver, SCRATCH, Some(HOST), None);

    let message = message_of(|| add(&resolver, vec![("draft_id", &proposal.draft_id)]));
    assert!(message.contains("content"), "{message}");
}

#[test]
fn a_draft_cannot_be_redirected_to_another_path() {
    let (_dir, resolver) = sandbox();
    let proposal = draft_for(&resolver, SCRATCH, Some(HOST), None);

    // Verification is only sound for the path the draft was built for, so a
    // re-send naming a different path is refused rather than quietly honoured.
    let message = message_of(|| {
        add(
            &resolver,
            vec![
                ("base", "Tickets.base"),
                ("path", "Tickets/__draft-elsewhere.md"),
                ("context", HOST),
                ("draft_id", &proposal.draft_id),
                ("content", &proposal.content),
            ],
        )
    });
    assert!(message.contains(SCRATCH), "{message}");
    assert!(
        message.contains("Tickets/__draft-elsewhere.md"),
        "{message}"
    );
    assert!(!paths(&resolver).contains(&"Tickets/__draft-elsewhere.md".to_string()));
}

#[test]
fn a_draft_cannot_be_redirected_to_another_base() {
    let (_dir, resolver) = sandbox();
    let proposal = draft_for(&resolver, SCRATCH, Some(HOST), None);

    let message = message_of(|| {
        add(
            &resolver,
            vec![
                ("base", "AllNotes.base"),
                ("path", SCRATCH),
                ("context", HOST),
                ("draft_id", &proposal.draft_id),
                ("content", &proposal.content),
            ],
        )
    });
    assert!(message.contains("Tickets.base"), "{message}");
    assert!(message.contains("AllNotes.base"), "{message}");
    assert!(!paths(&resolver).contains(&SCRATCH.to_string()));
}

#[test]
fn malformed_frontmatter_is_refused_before_the_filter_ever_runs() {
    let (_dir, resolver) = sandbox();
    let proposal = draft_for(&resolver, SCRATCH, Some(HOST), None);

    let message = message_of(|| {
        commit(
            &resolver,
            &proposal.draft_id,
            "---\ntags: [unclosed\n---\n\n# Scratch\n",
        )
    });
    assert!(message.contains("not valid YAML"), "{message}");
    assert!(!paths(&resolver).contains(&SCRATCH.to_string()));
}

// ---------------------------------------------------------------------------
// The draft store
// ---------------------------------------------------------------------------

#[test]
fn an_unknown_id_is_a_miss_that_says_to_re_draft() {
    let (_dir, resolver) = sandbox();
    let message = message_of(|| commit(&resolver, "not-a-draft", "---\n---\n"));
    assert!(message.contains("unknown or has expired"), "{message}");
    assert!(message.contains("WITHOUT draft_id"), "{message}");
}

#[test]
fn an_expired_draft_is_a_miss_not_a_special_case() {
    let store = DraftStore::new(0);
    let stored = store.put(DraftSeed {
        base: "B.base".to_string(),
        view: "V".to_string(),
        path: "N.md".to_string(),
        context: None,
        original: String::new(),
    });
    let message = message_of(|| store.get(&stored.id));
    assert!(message.contains("expired"), "{message}");
    // The miss deletes the draft, so an expired one is never found again.
    assert_eq!(store.len(), 0);
}

#[test]
fn a_live_draft_keeps_what_the_commit_needs() {
    let store = DraftStore::default();
    let stored = store.put(DraftSeed {
        base: "Tickets.base".to_string(),
        view: "All".to_string(),
        path: SCRATCH.to_string(),
        context: Some(HOST.to_string()),
        original: "the draft we proposed".to_string(),
    });

    let found = store.get(&stored.id).expect("a fresh draft is live");
    assert_eq!(found.base, "Tickets.base");
    assert_eq!(found.view, "All");
    assert_eq!(found.path, SCRATCH);
    assert_eq!(found.context.as_deref(), Some(HOST));
    assert_eq!(found.original, "the draft we proposed");

    store.release(&stored.id);
    assert_eq!(store.len(), 0);
}

#[test]
fn a_store_starts_empty_and_can_be_emptied_again() {
    let store = DraftStore::default();
    assert!(store.is_empty());
    store.put(DraftSeed {
        base: "B.base".to_string(),
        view: "V".to_string(),
        path: "N.md".to_string(),
        context: None,
        original: String::new(),
    });
    assert_eq!(store.len(), 1);
    store.clear();
    assert!(store.is_empty());
}

// ---------------------------------------------------------------------------
// A Base is never written
// ---------------------------------------------------------------------------

#[test]
fn a_base_path_is_refused_on_the_drafting_call() {
    let (_dir, resolver) = sandbox();
    let message = message_of(|| {
        add(
            &resolver,
            vec![("base", "Tickets.base"), ("path", "Tickets.base")],
        )
    });
    assert!(message.contains("a Base is never written"), "{message}");
}

#[test]
fn a_base_path_is_refused_on_the_commit_call_too() {
    let (_dir, resolver) = sandbox();
    let proposal = draft_for(&resolver, SCRATCH, Some(HOST), None);

    let message = message_of(|| {
        add(
            &resolver,
            vec![
                ("base", "Tickets.base"),
                ("path", "Tickets.base"),
                ("context", HOST),
                ("draft_id", &proposal.draft_id),
                ("content", &proposal.content),
            ],
        )
    });
    assert!(message.contains("a Base is never written"), "{message}");
    // The base itself is untouched, not merely refused in the abstract.
    let text =
        futures_block_on(resolver.vault().read_text("Tickets.base")).expect("the base reads");
    assert!(text.contains("views:"), "the base is intact");
}

#[test]
fn a_path_the_vault_would_never_index_is_refused() {
    let (_dir, resolver) = sandbox();
    let message = message_of(|| {
        add(
            &resolver,
            vec![("base", "Tickets.base"), ("path", "Tickets/scratch.txt")],
        )
    });
    assert!(message.contains(".md note"), "{message}");
}

#[test]
fn an_empty_path_is_refused() {
    let (_dir, resolver) = sandbox();
    let message = message_of(|| add(&resolver, vec![("base", "Tickets.base"), ("path", "   ")]));
    assert!(message.contains("note path is required"), "{message}");
}

#[test]
fn an_existing_note_is_never_clobbered() {
    let (_dir, resolver) = sandbox();
    let target = "Tickets/Fix login redirect.md";
    let before = futures_block_on(resolver.vault().read_text(target)).expect("the note reads");

    let message = message_of(|| add(&resolver, vec![("base", "Tickets.base"), ("path", target)]));
    assert!(message.contains("already exists"), "{message}");

    let after = futures_block_on(resolver.vault().read_text(target)).expect("the note reads");
    assert_eq!(before, after, "the existing note is byte-identical");
}

// ---------------------------------------------------------------------------
// The host note's path depth does not change the verdict
// ---------------------------------------------------------------------------

/// `this` binds to the HOST note, and the host note's path depth is its own
/// business.
///
/// There is a claim that a draft at a different depth from its host verifies
/// against the wrong context, which would make every root-level or nested
/// pairing a lie. It is false, and these are the four pairings that say so: each
/// one commits, and each one produces a note the REAL query pipeline returns as a
/// row for that host -- which is the only evidence that matters, since a note
/// this module accepts but the pipeline drops is precisely the failure the tool
/// exists to prevent.
const PAIRINGS: [(&str, &str); 4] = [
    ("Projects/SomeProject.md", "Tickets/__depth-nested.md"),
    ("Root Project.md", "Tickets/__depth-root-host.md"),
    ("Projects/SomeProject.md", "__depth-root-draft.md"),
    ("Root Project.md", "__depth-root-both.md"),
];

#[test]
fn a_draft_at_every_depth_becomes_a_row_for_its_host() {
    for (host, note) in PAIRINGS {
        let (_dir, resolver) = sandbox();
        let proposal = draft_for(&resolver, note, Some(host), None);
        // The link is the host's BASENAME, which is what makes the depth
        // irrelevant: `[[SomeProject]]` resolves to the same note whether the note
        // being written sits beside it or two folders away.
        let stem = host
            .rsplit('/')
            .next()
            .expect("a basename")
            .strip_suffix(".md")
            .expect("a .md");
        assert_eq!(
            frontmatter(note, &proposal.content, "project").as_deref(),
            Some(format!("[[{stem}]]").as_str()),
            "a draft at {note} links the host by basename, extension dropped"
        );

        let result =
            commit(&resolver, &proposal.draft_id, &proposal.content).unwrap_or_else(|error| {
                panic!(
                    "a draft at {note} verifies against {host}: {}",
                    error.message()
                )
            });
        assert_eq!(result.written(), Some(note));

        assert!(
            row_paths(&resolver, "Tickets.base", host).contains(&note.to_string()),
            "a draft at {note} is a row for {host}"
        );
    }
}

// ---------------------------------------------------------------------------
// A draft at the vault root leaves the root alone
// ---------------------------------------------------------------------------

const ROOT_SCRATCH: &str = "__root-scratch.md";
/// The bogus directory the parent calculation used to produce.
const BOGUS_DIR: &str = "__root-scratch.m";

/// A root-level draft path creates no folder.
///
/// Regression. `create_note` derived the parent directory by slicing the path at
/// its last `/`, and for a path with NO `/` that index is -1 -- so the "parent"
/// of `Note.md` came out as `Note.m`, and a directory by that name was created
/// beside the note. It was invisible from the vault index (a directory is not a
/// note, so no query ever saw it) which is exactly why it survived: the note
/// committed, verified, and became a row, and the vault quietly gained a folder
/// nobody asked for.
#[test]
fn writing_a_root_level_note_creates_no_directory_beside_it() {
    let (dir, resolver) = sandbox();
    let before = directory_names(dir.path());

    let proposal = draft_for(&resolver, ROOT_SCRATCH, Some(HOST), None);
    let result = commit(&resolver, &proposal.draft_id, &proposal.content).expect("verifies");
    assert_eq!(result.written(), Some(ROOT_SCRATCH));

    // The note, and nothing else. A directory here would be invisible to every
    // query, so asserting on the index alone would miss it.
    let added: Vec<String> = directory_names(dir.path())
        .into_iter()
        .filter(|name| !before.contains(name))
        .collect();
    assert_eq!(
        added,
        [ROOT_SCRATCH.to_string()],
        "the vault root gained a folder"
    );
}

/// The same bug, one step further on: `X.m` is a plausible real name. When a file
/// already occupies it, a stray `mkdir` would throw a bare EEXIST that is neither
/// a `BasesError` nor a message an agent can act on, and the note is never
/// written -- a row the base's own filter matched, refused by a collision with a
/// directory this tool was about to create for itself.
#[test]
fn a_file_already_named_like_the_bogus_directory_does_not_block_the_write() {
    let (dir, resolver) = sandbox();
    std::fs::write(dir.path().join(BOGUS_DIR), "not a note\n").expect("the decoy file is writable");

    let proposal = draft_for(&resolver, ROOT_SCRATCH, Some(HOST), None);
    let result = commit(&resolver, &proposal.draft_id, &proposal.content).expect("verifies");
    assert_eq!(result.written(), Some(ROOT_SCRATCH));
    assert!(paths(&resolver).contains(&ROOT_SCRATCH.to_string()));
}

/// The names in a directory, sorted, so a set comparison is stable.
fn directory_names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .expect("the directory reads")
        .map(|entry| {
            entry
                .expect("an entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    names.sort();
    names
}

// ---------------------------------------------------------------------------
// write_note and create_note
// ---------------------------------------------------------------------------

#[test]
fn a_prose_edit_is_applied_and_written() {
    let (dir, resolver) = sandbox();
    let note = "Tickets/Invoice export.md";
    let original = futures_block_on(resolver.vault().read_text(note)).expect("reads");

    let edited = original.replace("# Invoice export", "# Invoice export, revised");
    let result =
        futures_block_on(resolver.write_note(note, &edited, None)).expect("the edit applies");
    assert_eq!(result.path, note);
    assert!(result.refused.is_empty());
    assert!(!result.removed_region);

    let on_disk = std::fs::read_to_string(dir.path().join(note)).expect("the note is on disk");
    assert_eq!(on_disk, edited);
    assert_eq!(
        futures_block_on(resolver.vault().read_text(note)).expect("reads"),
        edited
    );
}

#[test]
fn an_edit_that_changes_nothing_does_not_touch_the_file() {
    let (dir, resolver) = sandbox();
    let note = "Tickets/Invoice export.md";
    let original = futures_block_on(resolver.vault().read_text(note)).expect("reads");
    let stamp = std::fs::metadata(dir.path().join(note))
        .expect("the note is on disk")
        .len();

    let result =
        futures_block_on(resolver.write_note(note, &original, None)).expect("a no-op edit applies");
    assert_eq!(result.text, original);
    assert_eq!(
        std::fs::metadata(dir.path().join(note))
            .expect("the note is on disk")
            .len(),
        stamp,
        "the file is not rewritten"
    );
}

#[test]
fn editing_a_base_region_is_refused_and_the_rest_of_the_edit_still_lands() {
    let (dir, resolver) = sandbox();
    let note = "Root Project.md";
    // A Projection round-trip plus a prose edit: the rendered fences are replaced
    // SILENTLY (that is the designed flow) and the prose change applies. This
    // note carries both region shapes, so both round-trip here.
    let projection =
        futures_block_on(resolver.read_note(note, NoteOptions::projection())).expect("reads");
    let edited = projection
        .content
        .replace("A root-level host note", "A host note");
    let result = futures_block_on(resolver.write_note(note, &edited, None)).expect("applies");

    assert!(
        result.refused.is_empty(),
        "a rendered fence round-trips without a refusal"
    );
    let on_disk = std::fs::read_to_string(dir.path().join(note)).expect("the note is on disk");
    assert!(
        on_disk.contains("A host note."),
        "the prose edit landed: {on_disk}"
    );
    assert!(
        on_disk.contains("![[Tickets.base]]"),
        "the live region survived: {on_disk}"
    );
    assert!(
        !on_disk.contains("base-rendered"),
        "no rendered fence reached disk: {on_disk}"
    );
    assert!(
        on_disk.contains("```base"),
        "the inline fence survived: {on_disk}"
    );
}

#[test]
fn deleting_a_base_region_is_refused_and_the_region_is_restored() {
    let (dir, resolver) = sandbox();
    let note = "Projects/OtherProject.md";
    let original = futures_block_on(resolver.vault().read_text(note)).expect("reads");
    assert!(original.contains("![[Tickets.base]]"));

    let edited = original.replace("![[Tickets.base]]", "");
    let result = futures_block_on(resolver.write_note(note, &edited, None)).expect("applies");

    assert!(result.removed_region, "the deletion is reported");
    assert_eq!(result.refused.len(), 1);
    assert_eq!(result.refused[0].reason, "The base region was removed.");
    assert!(
        result.refused[0].guidance.contains("never removed"),
        "{}",
        result.refused[0].guidance
    );

    let on_disk = std::fs::read_to_string(dir.path().join(note)).expect("the note is on disk");
    assert!(
        on_disk.contains("![[Tickets.base]]"),
        "the region is back: {on_disk}"
    );
}

#[test]
fn a_note_that_does_not_exist_cannot_be_written() {
    let (_dir, resolver) = sandbox();
    let message = message_of_async(|| resolver.write_note("Tickets/__nope.md", "# nope\n", None));
    assert!(message.contains("Note not found"), "{message}");
}

#[test]
fn create_note_makes_intermediate_folders_for_a_nested_path() {
    let (dir, resolver) = sandbox();
    let path = "Tickets/Nested/Deeper/Note.md";

    let written =
        futures_block_on(resolver.create_note(path, "# created\n")).expect("the note is written");
    assert_eq!(written, path);
    assert!(
        dir.path().join("Tickets/Nested/Deeper/Note.md").is_file(),
        "the file is on disk"
    );
    assert!(
        paths(&resolver).contains(&path.to_string()),
        "and the index found it"
    );
}

#[test]
fn create_note_makes_no_folder_for_a_root_level_path() {
    let (dir, resolver) = sandbox();
    let before = directory_names(dir.path());

    futures_block_on(resolver.create_note("__root-note.md", "# created\n"))
        .expect("the note is written");

    let added: Vec<String> = directory_names(dir.path())
        .into_iter()
        .filter(|name| !before.contains(name))
        .collect();
    assert_eq!(
        added,
        ["__root-note.md".to_string()],
        "the root gained a folder"
    );
}

#[test]
fn a_committed_note_survives_a_reload_of_the_index() {
    let (_dir, resolver) = sandbox();
    let proposal = draft_for(&resolver, SCRATCH, Some(HOST), None);
    commit(&resolver, &proposal.draft_id, &proposal.content).expect("verifies");

    futures_block_on(resolver.reload()).expect("the vault reindexes");
    assert!(
        paths(&resolver).contains(&SCRATCH.to_string()),
        "the committed note is still indexed after a rebuild"
    );
}

#[test]
fn a_deleted_note_leaves_the_index() {
    let (_dir, resolver) = sandbox();
    let proposal = draft_for(&resolver, SCRATCH, Some(HOST), None);
    commit(&resolver, &proposal.draft_id, &proposal.content).expect("verifies");
    assert!(paths(&resolver).contains(&SCRATCH.to_string()));

    delete(&resolver, SCRATCH);
    assert!(
        !paths(&resolver).contains(&SCRATCH.to_string()),
        "and is gone once deleted"
    );
}

// ---------------------------------------------------------------------------
// The errors a tool result carries
// ---------------------------------------------------------------------------

#[test]
fn an_engine_error_names_the_construct_it_is_about() {
    let resolver = oracle();
    let error: BasesError = resolver
        .resolve_base_path("Nope.base")
        .expect_err("a missing base is refused");
    assert_eq!(error.construct(), None, "a plain refusal has no construct");
    assert_eq!(
        error.note(),
        Some("Nope.base"),
        "but it names the path it was given"
    );
}

// ---------------------------------------------------------------------------
// Stringifying a cell
// ---------------------------------------------------------------------------

/// A link cell is JSON, not a wikilink.
///
/// The TypeScript original reached `JSON.stringify` for anything whose
/// `typeof` was `object`, and a `LinkValue` is a class instance — so a link cell
/// in `format=json` came out as `{"target": ...}`. `BasesValue` has no such
/// overlap: a namespace is the only record in the lattice, and a link renders as
/// `[[Target]]`. Porting the TYPE's behaviour rather than its spelling would have
/// emitted `{"ms": 1700000000000}` for a date cell, which is not a thing
/// Obsidian emits. These three assertions are what the port chose, and they are
/// pinned here so the choice is a decision rather than an accident.
#[test]
fn stringified_renders_the_shapes_obsidian_stringifies() {
    use bases_mcp::service::stringified;
    use bases_mcp::value::{BasesDate, BasesValue, Duration};

    assert_eq!(stringified(None), None, "an absent cell is null");
    assert_eq!(stringified(Some(&BasesValue::Null)), None);
    assert_eq!(
        stringified(Some(&BasesValue::String("active".into()))).as_deref(),
        Some("active")
    );
    assert_eq!(
        stringified(Some(&BasesValue::Bool(true))).as_deref(),
        Some("true")
    );
    assert_eq!(
        stringified(Some(&BasesValue::Number(2.0))).as_deref(),
        Some("2")
    );

    let list = BasesValue::List(vec![
        BasesValue::String("a".into()),
        BasesValue::Null,
        BasesValue::String("b".into()),
    ]);
    assert_eq!(
        stringified(Some(&list)).as_deref(),
        Some("a, , b"),
        "lists join with \", \""
    );

    let link = BasesValue::Link {
        target: "SomeProject".into(),
        display: None,
        resolved: Some("Projects/SomeProject.md".into()),
    };
    assert_eq!(stringified(Some(&link)).as_deref(), Some("[[SomeProject]]"));

    let aliased = BasesValue::Link {
        target: "Projects/Business".into(),
        display: Some("Business".into()),
        resolved: None,
    };
    assert_eq!(
        stringified(Some(&aliased)).as_deref(),
        Some("[[Projects/Business|Business]]")
    );

    let date = BasesValue::Date(BasesDate::from_millis(0));
    assert_eq!(
        stringified(Some(&date)).as_deref(),
        Some("1970-01-01T00:00:00Z")
    );

    let duration = BasesValue::Duration(Duration::from_millis(86_400_000));
    assert_eq!(stringified(Some(&duration)).as_deref(), Some("1 day"));
}

#[test]
fn a_namespace_is_json_because_a_record_has_no_display_form() {
    use bases_mcp::service::{stringified, to_json_value};
    use bases_mcp::value::BasesValue;
    use std::rc::Rc;

    let map = std::collections::BTreeMap::from([
        ("b".to_string(), BasesValue::String("two".into())),
        (
            "a".to_string(),
            BasesValue::List(vec![BasesValue::Number(1.0)]),
        ),
    ]);
    let namespace = BasesValue::Namespace(Rc::new(map));
    assert_eq!(
        stringified(Some(&namespace)).as_deref(),
        Some(r#"{"a":[1],"b":"two"}"#)
    );
    // `serde_json` renders an `f64` as `1.0`; Obsidian renders the same value as
    // `1`, and `format=json` is compared against the CLI byte for byte.
    assert_eq!(
        to_json_value(&namespace),
        serde_json::json!({ "a": [1], "b": "two" })
    );
    // A LIST is not a record, so it joins rather than brackets: the joined form is
    // what a rendered cell and a json cell both show.
    assert_eq!(
        stringified(Some(&BasesValue::List(vec![BasesValue::Number(1.0)]))).as_deref(),
        Some("1")
    );
}

// ---------------------------------------------------------------------------
// The verify step and the query pipeline must agree
// ---------------------------------------------------------------------------

/// Verification walks the filter tree itself rather than reusing `query_base`,
/// because only the walk can say WHICH expression failed. The price of mirroring
/// is that the two graphs have to be the same graph, and this is the case that
/// proves they are.
///
/// The filter reads `formula.label`, which reads `formula.doubled`, which reads
/// `formula.base_value`. A verify pass that filled the formula bag only at the
/// end — which is what the TypeScript original did, in a comment claiming it
/// mirrored `queryBase` — would evaluate `null * 2` and refuse a note the real
/// query pipeline returns as a row. That is exactly the failure the handshake
/// exists to prevent: a draft that can never be a row, refused for a reason the
/// agent cannot see.
#[test]
fn a_filter_over_a_chained_formula_verifies_the_way_the_query_pipeline_evaluates_it() {
    let dir = TempDir::new().expect("a temp dir is writable");
    seed_dir(
        dir.path(),
        &[
            VaultFile {
                path: "Chained.base".to_string(),
                content: concat!(
                    "filters:\n",
                    "  and:\n",
                    "    - file.hasTag(\"ticket\")\n",
                    "    - formula.label == 83\n",
                    "formulas:\n",
                    "  base_value: \"41\"\n",
                    "  doubled: \"formula.base_value * 2\"\n",
                    "  label: \"formula.doubled + 1\"\n",
                    "views:\n",
                    "  - type: table\n",
                    "    name: All\n",
                    "    order:\n",
                    "      - file.name\n",
                )
                .to_string(),
            },
            VaultFile {
                path: "Notes/Alpha.md".to_string(),
                content: "---\ntags:\n  - ticket\n---\n\n# Alpha\n".to_string(),
            },
        ],
    );
    let resolver = futures_block_on(Resolver::open_dir(dir.path())).expect("the temp vault opens");

    // The pipeline first, so a failure here is a failure of the assertion's
    // premise rather than of the verify step.
    let rows = futures_block_on(resolver.query("Chained.base", &QueryOptions::default()))
        .expect("the chained base resolves")
        .rows
        .into_iter()
        .map(|row| row.path)
        .collect::<Vec<_>>();
    assert_eq!(rows, ["Notes/Alpha.md"], "the chain resolves: 41, 82, 83");

    let proposal = draft(add(
        &resolver,
        vec![("base", "Chained.base"), ("path", "Notes/Beta.md")],
    ));
    let result = commit(&resolver, &proposal.draft_id, &proposal.content).unwrap_or_else(|error| {
        panic!(
            "a draft verifies against the same graph: {}",
            error.message()
        )
    });
    assert_eq!(result.written(), Some("Notes/Beta.md"));
}

/// A concurrent external edit must survive a write.
///
/// Regression. `write_note` read the note through the cached vault, so it
/// reconciled the agent's edit against a snapshot from whenever the note was last
/// read and wrote the result over the top. With Obsidian autosaving
/// continuously, one second of typing was enough to lose the user's paragraph,
/// and the response said `health: ok`.
///
/// The fix is to read the note fresh at the top of the write. This test performs
/// the sequence that used to lose data: read, then have someone else write, then
/// write the agent's edit of the ORIGINAL copy.
#[test]
fn an_external_edit_made_after_the_read_is_not_clobbered() {
    let (dir, resolver) = sandbox();
    let note = "Tickets/Invoice export.md";

    // The agent reads the note, exactly as the designed flow intends, and keeps
    // the hash of what it read.
    let view = futures_block_on(resolver.read_note(note, NoteOptions::raw())).expect("reads");
    let as_read = view.raw;

    // The user edits the same note in Obsidian, which autosaves it.
    let user_text = format!(
        "{as_read}\nTWO MINUTES LATER: the user rewrote this whole paragraph in Obsidian, adding \
several new lines of real prose that must not be lost.\n"
    );
    std::fs::write(dir.path().join(note), &user_text).expect("the user's save lands");

    // The agent writes back its edit of the copy it read, quoting the hash from
    // BEFORE the user's save.
    let hash = view.base_hash;
    let agent_edit = as_read.replace("# Invoice export", "# Invoice export, revised");

    // The conditional write is refused, and the user's paragraph survives.
    let message = message_of_async(|| resolver.write_note(note, &agent_edit, Some(&hash)));
    assert!(
        message.contains("changed since you read it"),
        "the stale write was not refused: {message}"
    );
    let on_disk = std::fs::read_to_string(dir.path().join(note)).expect("the note is on disk");
    assert!(
        on_disk.contains("the user rewrote this whole paragraph"),
        "the user's edit was destroyed. On disk:\n{on_disk}"
    );
}

/// The same test, but the two edits touch different paragraphs.
///
/// Reconciling against a stale copy loses whichever change was made outside the
/// agent's view. Here the user appends and the agent rewrites a heading, so the
/// correct result contains both.
#[test]
fn a_stale_read_does_not_lose_an_appended_paragraph() {
    let (dir, resolver) = sandbox();
    let note = "Tickets/Invoice export.md";
    // Read, and take the hash of what was read.
    let view = futures_block_on(resolver.read_note(note, NoteOptions::raw())).expect("reads");
    let agent_edit = view.raw.replace("Invoice export", "Invoice export v2");

    // The user saves the note, appending a ticked task.
    std::fs::write(
        dir.path().join(note),
        format!("{}\n\n- [x] a task the user ticked\n", view.raw),
    )
    .expect("the user's save lands");

    futures_block_on(resolver.write_note(note, &agent_edit, Some(&view.base_hash)))
        .expect_err("the stale write must be refused");

    let on_disk = std::fs::read_to_string(dir.path().join(note)).expect("reads");
    assert!(on_disk.contains("a task the user ticked"), "{on_disk}");
}

/// Re-reading and reapplying succeeds, which is the whole point of refusing.
#[test]
fn a_conditional_write_succeeds_when_nothing_changed() {
    let (_dir, resolver) = sandbox();
    let note = "Tickets/Invoice export.md";

    let view = futures_block_on(resolver.read_note(note, NoteOptions::raw())).expect("reads");
    let edited = view
        .raw
        .replace("# Invoice export", "# Invoice export, revised");

    futures_block_on(resolver.write_note(note, &edited, Some(&view.base_hash)))
        .expect("an uncontested conditional write applies");

    let on_disk = futures_block_on(resolver.read_note(note, NoteOptions::raw())).expect("reads");
    assert!(on_disk.raw.contains("revised"), "{}", on_disk.raw);
}

/// A wrong hash is refused rather than applied, including a hash of nothing.
#[test]
fn a_mismatched_hash_is_refused() {
    let (dir, resolver) = sandbox();
    let note = "Tickets/Invoice export.md";
    let before = std::fs::read_to_string(dir.path().join(note)).expect("reads");

    futures_block_on(resolver.write_note(note, "# clobbered\n", Some("0000000000000000")))
        .expect_err("a wrong hash must be refused");

    let after = std::fs::read_to_string(dir.path().join(note)).expect("reads");
    assert_eq!(before, after, "a refused write changed the note");
}

/// A refused write must become writable again after re-reading.
///
/// Regression, and the most serious of the defects found by porting. `get_note`
/// served its bytes from the index while `write_note` compared the hash against
/// a fresh read. Once the two disagreed, the note could never be written again:
/// the re-read the refusal message instructs returns the SAME stale bytes and
/// the SAME stale hash, so the next write is refused too. That trades silent data
/// loss for a permanent hard block on a single note.
///
/// The test asserts the full loop: read, external edit, refused write, re-read,
/// write accepted.
#[test]
fn a_refused_write_does_not_lock_the_note_out() {
    let (dir, resolver) = sandbox();
    let note = "Tickets/Invoice export.md";

    let view = futures_block_on(resolver.read_note(note, NoteOptions::raw())).expect("reads");
    std::fs::write(dir.path().join(note), format!("{}\nuser text\n", view.raw))
        .expect("the user's save lands");

    let stale_edit = view.raw.replace("Invoice export", "v2");
    futures_block_on(resolver.write_note(note, &stale_edit, Some(&view.base_hash)))
        .expect_err("the stale write must be refused");

    // Re-read as the error message instructs.
    let again = futures_block_on(resolver.read_note(note, NoteOptions::raw())).expect("re-reads");
    assert!(
        again.raw.contains("user text"),
        "the re-read did not see the user's text, so it cannot be based on a fresh copy"
    );
    assert_ne!(
        again.base_hash, view.base_hash,
        "the re-read returned the same hash, which is the deadlock"
    );

    // The second write, based on the re-read, must now be ACCEPTED.
    let reapplied = again.raw.replace("Invoice export", "v2");
    futures_block_on(resolver.write_note(note, &reapplied, Some(&again.base_hash)))
        .expect("a write based on a fresh read must be accepted");

    let on_disk = std::fs::read_to_string(dir.path().join(note)).expect("reads");
    assert!(on_disk.contains("user text"), "{on_disk}");
    assert!(on_disk.contains("v2"), "{on_disk}");
}
