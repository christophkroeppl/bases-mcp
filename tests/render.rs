//! Markdown rendering, and the Projection round-trip.
//!
//! Three things are pinned here.
//!
//! The first is the FLAT / STRUCTURED split. `flat` is byte-compared against
//! `obsidian base:query format=md`, so its centring, its column widths and the
//! fact that it drops `groupBy` and `summaries` are all load-bearing: they are
//! what makes it a parity surface rather than a rendering style. `structured` is
//! the Projection, keeps what the CLI throws away, and left-aligns instead. These
//! tests exist so neither surface drifts into the other, so most of them assert
//! EXACT output rather than looking for a substring.
//!
//! The second is that a ```base-rendered fence never reaches disk. It is an
//! intermediate form on its way back to the region it replaced, and Obsidian
//! would keep it as an inert block -- a dead copy of the rendered rows sitting in
//! the note. Round-tripping an untouched Projection has to be silent, or the
//! designed flow comes back `partial-with-errors` and an agent learns to ignore
//! refusals.
//!
//! The third is that `reconcile_note` never changes or removes a Base region,
//! including the awkward one: an inline ```base fence carries no `path=`, so
//! pairing falls back to position plus the pinned view. `Root Project.md` in the
//! testing vault has one of those and is the case that caught it.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use chrono::TimeZone;
use chrono::Utc;

use bases_mcp::base::{BaseFile, BaseView, PropertyConfig, QueryGroup, QueryResult, ResolvedRow};
use bases_mcp::note::{is_base_region, parse_note_with_embeds, RENDER_FENCE_LANG};
use bases_mcp::render::markdown::{display_width, render_columns, render_markdown, RenderStyle};
use bases_mcp::render::project::{
    base_regions, fence_info, parse_fence_info, project, reconcile_note, wrap_in_fence,
    FenceProvenance, RENDER_FENCE,
};
use bases_mcp::value::{BasesDate, BasesValue};
use serde_yaml::Mapping;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// A Base with nothing declared. Every label below is then a probed default.
fn base() -> BaseFile {
    BaseFile {
        filters: None,
        formulas: BTreeMap::new(),
        properties: BTreeMap::new(),
        summaries: BTreeMap::new(),
        views: Vec::new(),
        extra: Mapping::new(),
    }
}

/// A view whose columns are `file.name` and `status`. Only the layout and the
/// column order are stated, because those are all a renderer reads.
fn view(view_type: &str) -> BaseView {
    BaseView {
        view_type: view_type.to_string(),
        name: "V".to_string(),
        limit: None,
        filters: None,
        order: Some(vec!["file.name".to_string(), "status".to_string()]),
        group_by: None,
        sort: None,
        summaries: None,
        extra: Mapping::new(),
    }
}

fn row(path: &str, name: &str, status: &str) -> ResolvedRow {
    ResolvedRow {
        path: path.to_string(),
        formula: BTreeMap::new(),
        values: BTreeMap::from([
            (
                "file.name".to_string(),
                BasesValue::String(name.to_string()),
            ),
            (
                "note.status".to_string(),
                BasesValue::String(status.to_string()),
            ),
        ]),
    }
}

const BETA: &str = "Beta";
const ALPHA: &str = "Alpha";

fn rows() -> Vec<ResolvedRow> {
    vec![
        row("Tickets/B.md", BETA, "active"),
        row("Root A.md", ALPHA, "done"),
    ]
}

/// The two groups, keyed the way a `groupBy` on a formula renders them.
fn groups() -> Vec<QueryGroup> {
    vec![
        QueryGroup {
            key: "1 – high".to_string(),
            rows: vec![rows()[0].clone()],
        },
        QueryGroup {
            key: "2 – normal".to_string(),
            rows: vec![rows()[1].clone()],
        },
    ]
}

/// A resolved query over [`rows`], rendered as `view_type`.
fn result(view_type: &str) -> QueryResult {
    grouped(view_type, None)
}

/// The same, with groups.
fn grouped(view_type: &str, groups: Option<Vec<QueryGroup>>) -> QueryResult {
    let all = rows();
    QueryResult {
        base_path: "T.base".to_string(),
        view: view(view_type),
        rows: match &groups {
            None => all.clone(),
            Some(groups) => groups.iter().flat_map(|g| g.rows.clone()).collect(),
        },
        total: all.len(),
        groups,
        context: None,
        warnings: Vec::new(),
    }
}

/// The same, with the view's summaries set.
fn summarised(view_type: &str, summaries: &[(&str, &str)]) -> QueryResult {
    let mut result = result(view_type);
    result.view.summaries = Some(
        summaries
            .iter()
            .map(|(id, spec)| (id.to_string(), spec.to_string()))
            .collect(),
    );
    result
}

/// The same, but the rows are replaced -- for columns the two fixed rows do not
/// carry.
fn with_rows(view_type: &str, order: &[&str], rows: Vec<ResolvedRow>) -> QueryResult {
    let mut result = result(view_type);
    result.view.order = Some(order.iter().map(|id| id.to_string()).collect());
    result.total = rows.len();
    result.rows = rows;
    result
}

/// A resolved row holding one numeric column, with `null` for an absent one.
fn numeric_row(path: &str, value: Option<f64>) -> ResolvedRow {
    ResolvedRow {
        path: path.to_string(),
        formula: BTreeMap::new(),
        values: BTreeMap::from([(
            "note.n".to_string(),
            value.map_or(BasesValue::Null, BasesValue::Number),
        )]),
    }
}

/// The summary footer's last line, which is the whole summarised value.
fn summary_line(markdown: &str) -> &str {
    markdown
        .lines()
        .last()
        .expect("a render always emits lines")
}

fn render(base: &BaseFile, result: &QueryResult, style: RenderStyle) -> String {
    render_markdown(base, result, style).expect("the fixture only uses built-in summaries")
}

/// The flat surface for any view type: one centred table, whatever the view is.
const FLAT_TABLE: &str = r#"| file name | status |
| --------- | ------ |
|   Beta    | active |
|   Alpha   |  done  |"#;

/// The same view, `structured`: same table, left-aligned, no centring.
const STRUCTURED_TABLE: &str = r#"| file name | status |
| --------- | ------ |
| Beta      | active |
| Alpha     | done   |"#;

/// A grouped structured table: a bold header and a table per group.
const STRUCTURED_GROUPED: &str = r#"**1 – high**

| file name | status |
| --------- | ------ |
| Beta      | active |

**2 – normal**

| file name | status |
| --------- | ------ |
| Alpha     | done   |
"#;

/// A grouped structured list.
const STRUCTURED_GROUPED_LIST: &str = r#"**1 – high**

- Beta
  - status: active

**2 – normal**

- Alpha
  - status: done
"#;

/// A structured table with a `Unique` footer, sharing the table's column widths.
const SUMMARISED_TABLE: &str = r#"| file name | status |
| --------- | ------ |
| Beta      | active |
| Alpha     | done   |

| file name | status |
| --------- | ------ |
|           | 2      |"#;

// ---------------------------------------------------------------------------
// Flat: the format=md parity surface
// ---------------------------------------------------------------------------

#[test]
fn flat_renders_one_centred_table_for_every_view_type() {
    // `list` is a TABLE here, not a markdown list. That is the whole point of
    // the split: this surface reproduces the CLI, which has one table layout.
    for view_type in ["table", "cards", "kanban", "list", "map"] {
        let markdown = render(&base(), &result(view_type), RenderStyle::Flat);
        assert_eq!(markdown, FLAT_TABLE, "{view_type} is not the flat table");
        assert!(
            !markdown.contains("- Alpha"),
            "{view_type} leaked a list bullet"
        );
        assert!(markdown.contains(ALPHA));
    }
}

#[test]
fn flat_drops_group_headers() {
    let markdown = render(
        &base(),
        &grouped("table", Some(groups())),
        RenderStyle::Flat,
    );
    assert_eq!(markdown, FLAT_TABLE, "the groups changed the flat table");
    assert!(
        !markdown.contains("**"),
        "a group header survived into the export"
    );
}

#[test]
fn an_unknown_view_type_degrades_to_a_table_on_both_surfaces() {
    // The view type is a documented open namespace that plugins own, so an
    // unrecognised one degrades rather than failing the whole render.
    for style in [RenderStyle::Flat, RenderStyle::Structured] {
        let markdown = render(&base(), &result("gallery"), style);
        assert!(
            markdown.starts_with('|'),
            "{style:?} did not fall back to a table"
        );
        assert!(markdown.contains(ALPHA));
    }
}

#[test]
fn flat_centres_cells_and_the_pad_is_load_bearing() {
    let markdown = render(&base(), &result("table"), RenderStyle::Flat);
    // Centring puts a pad BEFORE the first cell's text, so two or more spaces
    // separate the pipe from "Alpha". Left alignment would leave exactly one --
    // which is why the exact string above is the real assertion and this is only
    // here to name what is being protected.
    let alpha_line = markdown.lines().nth(3).expect("the Alpha row");
    assert!(
        alpha_line.starts_with("|   Alpha"),
        "the pad before Alpha is missing: {alpha_line:?}"
    );
}

// ---------------------------------------------------------------------------
// Structured: the Projection surface
// ---------------------------------------------------------------------------

#[test]
fn structured_keeps_group_headers() {
    let markdown = render(
        &base(),
        &grouped("table", Some(groups())),
        RenderStyle::Structured,
    );
    assert_eq!(markdown, STRUCTURED_GROUPED);
    assert!(markdown.contains("**1 – high**"));
    assert!(markdown.contains("**2 – normal**"));
}

#[test]
fn structured_renders_a_list_view_as_a_list() {
    // The nested line uses the same display label the table header would.
    let expected = "- Beta\n  - status: active\n- Alpha\n  - status: done";
    for view_type in ["list", "map"] {
        let markdown = render(&base(), &result(view_type), RenderStyle::Structured);
        assert_eq!(markdown, expected, "{view_type} is not a list");
        assert!(markdown.contains("- Alpha"));
        assert!(markdown.contains("status: done"));
    }
}

#[test]
fn structured_renders_a_grouped_list_as_a_list() {
    let markdown = render(
        &base(),
        &grouped("list", Some(groups())),
        RenderStyle::Structured,
    );
    assert_eq!(markdown, STRUCTURED_GROUPED_LIST);
}

#[test]
fn structured_renders_cards_kanban_and_their_aliases_as_tables() {
    for view_type in ["cards", "kanban", "board", "table"] {
        let markdown = render(&base(), &result(view_type), RenderStyle::Structured);
        assert_eq!(markdown, STRUCTURED_TABLE, "{view_type} is not a table");
    }
}

#[test]
fn structured_left_aligns_cells_instead_of_centring_them() {
    let markdown = render(&base(), &result("table"), RenderStyle::Structured);
    assert_eq!(markdown, STRUCTURED_TABLE);
    // No leading pad before the first cell's text: `| Alpha` and nothing more.
    assert!(markdown
        .lines()
        .nth(3)
        .expect("the Alpha row")
        .starts_with("| Alpha"));
}

#[test]
fn an_empty_order_falls_back_to_file_name() {
    // An `order` of zero columns is as absent as no `order` at all: Obsidian
    // shows `file.name` plus the Base's own properties either way, so there is
    // always a primary column for a list to bullet on.
    let mut result = result("list");
    result.view.order = Some(Vec::new());
    // The Base declares no properties, so `file.name` is the only column and
    // every row is a bare bullet.
    assert_eq!(
        render(&base(), &result, RenderStyle::Structured),
        "- Beta\n- Alpha"
    );
}

#[test]
fn a_blank_primary_cell_renders_as_untitled() {
    let mut result = result("list");
    result.rows[0].values.remove("file.name");
    assert!(render(&base(), &result, RenderStyle::Structured).starts_with("- (untitled)"));
}

// ---------------------------------------------------------------------------
// Summaries
// ---------------------------------------------------------------------------

#[test]
fn the_footer_aligns_with_the_table_it_summarises() {
    // This was a real bug: the footer was measured on its own, came out narrower
    // than the table, and squeezed itself into the columns above. It has to be
    // measured together with the rows.
    let result = summarised("table", &[("note.status", "Unique")]);
    let markdown = render(&base(), &result, RenderStyle::Structured);
    assert_eq!(markdown, SUMMARISED_TABLE);

    let header_width = markdown.lines().next().expect("a header").chars().count();
    for line in markdown.lines().filter(|line| !line.is_empty()) {
        assert_eq!(
            line.chars().count(),
            header_width,
            "a line is out of alignment: {line:?}"
        );
    }
}

#[test]
fn every_numeric_builtin_reduces_the_column() {
    // 1, 2, 4 over three rows plus one `null`, which is absent rather than zero.
    let rows = vec![
        numeric_row("A.md", Some(1.0)),
        numeric_row("B.md", Some(2.0)),
        numeric_row("C.md", Some(4.0)),
        numeric_row("D.md", None),
    ];
    let expected = [
        ("Average", "| 2.333 |"),
        ("Sum", "| 7   |"),
        ("Min", "| 1   |"),
        ("Max", "| 4   |"),
        ("Range", "| 3   |"),
        ("Median", "| 2   |"),
        ("Stddev", "| 1.247 |"),
        ("Unique", "| 3   |"),
        ("Filled", "| 3   |"),
        ("Empty", "| 0   |"),
        ("Count", "| 3   |"),
    ];
    let base = base();
    for (spec, line) in expected {
        let result = with_rows("table", &["note.n"], rows.clone());
        let result = summarised_onto(result, &[("note.n", spec)]);
        let markdown = render(&base, &result, RenderStyle::Structured);
        assert_eq!(summary_line(&markdown), line, "{spec} summarised wrongly");
    }
}

#[test]
fn booleans_and_dates_summarise_on_their_own_terms() {
    // A date is not a number -- `Number("2024-12-20")` is `NaN` -- so every
    // numeric summary of a date column is blank, and `Earliest`/`Latest` are the
    // two that read the instant rather than the rendered text.
    let day = |month: u32, day: u32| {
        let instant = Utc
            .with_ymd_and_hms(2024, month, day, 0, 0, 0)
            .single()
            .expect("a date in range has one instant in UTC");
        BasesValue::Date(BasesDate::from_millis(instant.timestamp_millis()))
    };
    let row = |path: &str, checked: Option<bool>, due: Option<BasesValue>| {
        let mut values = BTreeMap::new();
        values.insert(
            "note.flag".to_string(),
            checked.map_or(BasesValue::Null, BasesValue::Bool),
        );
        values.insert("note.due".to_string(), due.unwrap_or(BasesValue::Null));
        ResolvedRow {
            path: path.to_string(),
            formula: BTreeMap::new(),
            values,
        }
    };
    let rows = vec![
        row("A.md", Some(true), Some(day(12, 20))),
        row("B.md", Some(false), Some(day(1, 3))),
        row("C.md", None, None),
    ];
    // A date CELL renders as full RFC 3339 here, where the TypeScript's
    // `DateValue` carried a `dateOnly` flag and rendered a bare `YYYY-MM-DD`.
    // `BasesDate` keeps a fixed offset instead, so the instant is unambiguous and
    // the time component survives. The SUMMARY cell is a bare date either way:
    // a summary of timestamps is unreadable at a glance.
    let expected = r#"| flag  | due                  |
| ----- | -------------------- |
| true  | 2024-12-20T00:00:00Z |
| false | 2024-01-03T00:00:00Z |
|       |                      |

| flag  | due                  |
| ----- | -------------------- |
| 1     | 2024-01-03           |"#;

    let result = with_rows("table", &["note.flag", "note.due"], rows.clone());
    let result = summarised_onto(
        result,
        &[("note.due", "Earliest"), ("note.flag", "Checked")],
    );
    assert_eq!(render(&base(), &result, RenderStyle::Structured), expected);

    for (column, spec, cell) in [
        ("note.flag", "Checked", "| 1     |                      |"),
        ("note.flag", "Unchecked", "| 1     |                      |"),
        ("note.flag", "Filled", "| 2     |                      |"),
        ("note.flag", "Empty", "| 0     |                      |"),
        ("note.due", "Earliest", "|       | 2024-01-03           |"),
        ("note.due", "Latest", "|       | 2024-12-20           |"),
        // Not a span: a date is not a number, so a Range over one has nothing to
        // take the difference of and is blank rather than a millisecond figure.
        ("note.due", "Range", "|       |                      |"),
    ] {
        let result = with_rows("table", &["note.flag", "note.due"], rows.clone());
        let result = summarised_onto(result, &[(column, spec)]);
        assert_eq!(
            summary_line(&render(&base(), &result, RenderStyle::Structured)),
            cell,
            "{spec}"
        );
    }
}

#[test]
fn earliest_and_latest_are_implemented_not_merely_advertised() {
    // Both are named in the unsupported-summary error message, so they have to
    // actually resolve rather than reach that error.
    for spec in ["Earliest", "Latest"] {
        let result = summarised("table", &[("file.name", spec)]);
        assert!(
            render_markdown(&base(), &result, RenderStyle::Structured).is_ok(),
            "{spec}"
        );
    }
}

#[test]
fn an_unknown_summary_name_hard_errors() {
    let result = summarised("table", &[("note.status", "NotASummaryName")]);
    let error = render_markdown(&base(), &result, RenderStyle::Structured)
        .expect_err("an unknown summary is not a blank cell");
    assert!(error.message().contains("not implemented"), "{error}");
    assert!(
        error.message().contains("Average, Min, Max"),
        "the message must list the built-ins"
    );
    assert_eq!(
        error.construct(),
        Some("NotASummaryName"),
        "the error must name the summary"
    );
}

#[test]
fn a_summary_for_a_column_the_view_does_not_render_is_inert() {
    // Summaries are keyed on the canonical ID, so `status` and `note.status` name
    // the same column -- and a key naming no rendered column is ignored.
    let result = summarised("table", &[("status", "Unique"), ("note.owner", "Count")]);
    assert_eq!(
        render(&base(), &result, RenderStyle::Structured),
        SUMMARISED_TABLE
    );
}

// ---------------------------------------------------------------------------
// Column headers
// ---------------------------------------------------------------------------

#[test]
fn headers_use_obsidians_display_labels_not_property_ids() {
    let base = labelled(&[("note.status", "Status")]);
    let markdown = render(&base, &result("table"), RenderStyle::Flat);
    assert_eq!(
        markdown,
        r#"| file name | Status |
| --------- | ------ |
|   Beta    | active |
|   Alpha   |  done  |"#
    );
    // Not `File Name`, and not the Property ID.
    assert!(!markdown.contains("File Name"));
    assert!(!markdown.contains("note.status"));
}

#[test]
fn a_bare_display_name_key_is_dead_config() {
    // Obsidian matches a configured `displayName` on the CANONICAL spelling only.
    // A base keyed as `properties: {status: ...}` is ignored, not treated as a
    // fallback -- probed on Obsidian 1.13.7, and fatal to the parity suite.
    let base = labelled(&[("status", "Ignored")]);
    assert_eq!(
        render(&base, &result("table"), RenderStyle::Flat),
        FLAT_TABLE
    );
}

#[test]
fn an_unordered_view_lists_the_bases_own_properties() {
    let base = BaseFile {
        properties: BTreeMap::from([
            ("note.status".to_string(), PropertyConfig::default()),
            ("note.owner".to_string(), PropertyConfig::default()),
        ]),
        ..base()
    };
    let mut result = result("table");
    result.view.order = None;
    // `BaseFile::properties` is a sorted map, so an unordered view lists its
    // properties in key order rather than in the order the file declared them.
    // Only views with no `order` are affected, and Obsidian always writes one.
    assert_eq!(
        render(&base, &result, RenderStyle::Flat),
        r#"| file name | owner | status |
| --------- | ----- | ------ |
|   Beta    |       | active |
|   Alpha   |       |  done  |"#
    );
}

#[test]
fn render_columns_canonicalises_the_property_id() {
    let base = labelled(&[("note.status", "Status")]);
    let columns = render_columns(&base, &view("table"));
    assert_eq!(
        columns
            .iter()
            .map(|c| (c.id.as_str(), c.header.as_str()))
            .collect::<Vec<_>>(),
        [("file.name", "file name"), ("note.status", "Status")]
    );
}

// ---------------------------------------------------------------------------
// Display width
// ---------------------------------------------------------------------------

#[test]
fn east_asian_characters_and_emoji_are_two_columns_and_marks_are_none() {
    for (text, width) in [
        ("", 0),
        ("abc", 3),
        ("日本語", 6),
        ("한글", 4),
        ("ファイル", 8),
        ("🎉", 2),
        // The ZWJ is zero and each half is wide, so the whole sequence is four.
        ("👩‍💻", 4),
        // `e` plus a combining acute, plus the base glyph: one column.
        ("é", 1),
        // A variation selector is zero, so a text-presentation emoji is one.
        ("a️", 1),
        ("‑‐", 2),
        ("α", 1),
    ] {
        assert_eq!(display_width(text), width, "{text:?}");
    }
}

#[test]
fn a_wide_cell_widens_its_column_by_its_rendered_width() {
    // Width, not character count: the CJK title is three characters and six
    // columns, so the whole table has to be six columns wide or it will not line
    // up in a reader's terminal.
    let mut wide = row("W.md", "日本語", "active");
    wide.values
        .insert("file.name".into(), BasesValue::String("日本語".into()));
    let result = with_rows("table", &["file.name", "note.status"], vec![wide]);
    let markdown = render(&base(), &result, RenderStyle::Flat);
    assert_eq!(
        markdown,
        r#"| file name | status |
| --------- | ------ |
|  日本語   | active |"#
    );
}

// ---------------------------------------------------------------------------
// The fence, and the note parser that has to recognise it
// ---------------------------------------------------------------------------

#[test]
fn the_renderer_emits_the_language_the_note_parser_reads() {
    // These two spellings live in different modules on purpose -- the parser
    // cannot import the renderer, which depends on it -- so this is the only
    // thing keeping them together.
    assert_eq!(RENDER_FENCE, RENDER_FENCE_LANG);
    assert_ne!(
        RENDER_FENCE, "base",
        "a rendered fence must never look live"
    );
}

#[test]
fn provenance_round_trips_through_the_info_string() {
    let provenance = FenceProvenance::new()
        .with_path("Tickets.base")
        .with_view("All")
        .with_context("Projects/SomeProject.md")
        .with_rows(3);
    let info = fence_info(&provenance);

    assert_eq!(
        info,
        r#"base-rendered path="Tickets.base" view="All" context="Projects/SomeProject.md" rows="3""#
    );
    assert_eq!(parse_fence_info(&info), Some(provenance));
    assert_eq!(
        parse_fence_info("base view=\"All\""),
        None,
        "a live fence is not provenance"
    );
    assert_eq!(
        parse_fence_info("base"),
        None,
        "a live fence is not provenance"
    );
    assert_eq!(
        parse_fence_info("ts"),
        None,
        "an ordinary code fence is not provenance"
    );
    assert_eq!(parse_fence_info(""), None);
    // A row count nobody can read is dropped rather than recorded as zero.
    assert_eq!(
        parse_fence_info(r#"base-rendered rows="many""#),
        Some(FenceProvenance::new()),
        "an unreadable row count is no row count"
    );
}

#[test]
fn a_quoted_attribute_cannot_break_out_of_its_own_string() {
    // The info string is a whole line of markdown, so a note path carrying a
    // quote or a backslash has to be escaped or the fence ends early and the rest
    // of the path becomes body text.
    let provenance = FenceProvenance::new().with_path("a\"b\\c.base");
    let info = fence_info(&provenance);
    assert_eq!(info, r#"base-rendered path="a\"b\\c.base""#);
    // Reading it back truncates at the escaped quote: an attribute value cannot
    // contain an escaped quote, because the attribute pattern stops at the first
    // one. The escaping is for the fence, not for a lossless round trip.
    assert_eq!(
        parse_fence_info(&info)
            .expect("still provenance")
            .path
            .as_deref(),
        Some("a\\")
    );
    // A path with no quote in it round-trips exactly.
    let plain = FenceProvenance::new()
        .with_path("a b/c.base")
        .with_view("All");
    assert_eq!(parse_fence_info(&fence_info(&plain)), Some(plain));
}

#[test]
fn a_wrapped_body_loses_only_its_trailing_newlines() {
    let region = wrap_in_fence(
        "| a |\n| b |\n\n\n",
        &FenceProvenance::new().with_path("T.base"),
    );
    assert_eq!(
        region.text,
        "```base-rendered path=\"T.base\"\n| a |\n| b |\n```"
    );
    assert_eq!(region.provenance.path.as_deref(), Some("T.base"));
}

// ---------------------------------------------------------------------------
// Reconciling a Projection
// ---------------------------------------------------------------------------

/// A host note with one embedded base, as `test/vault` has.
const HOST: &str = "---\nstatus: active\n---\n\n## Tickets\n\n![[Tickets.base]]\n";

/// What `get_note` returns for [`HOST`]: the embed rendered into a fence.
const PROJECTION: &str = concat!(
    "---\n",
    "status: active\n",
    "---\n",
    "\n",
    "## Tickets\n",
    "\n",
    "```base-rendered path=\"Tickets.base\"\n",
    "| file name |\n",
    "| --- |\n",
    "| Fix login redirect |\n",
    "```\n",
);

#[test]
fn a_base_rendered_fence_is_a_base_region_not_prose() {
    let note = parse_note_with_embeds("Host.md", PROJECTION);
    let regions = base_regions(&note);
    assert_eq!(regions.len(), 1);
    let prose: String = note
        .segments
        .iter()
        .filter(|s| !is_base_region(s))
        .map(|s| s.raw())
        .collect();
    assert!(
        !prose.contains("base-rendered"),
        "the fence leaked into prose: {prose}"
    );
}

#[test]
fn the_fence_keeps_the_base_path_and_view_from_its_info_string() {
    let note = parse_note_with_embeds("Host.md", PROJECTION);
    let region = &base_regions(&note)[0];

    assert_eq!(region.base_path.as_deref(), Some("Tickets.base"));
    assert_eq!(region.view_name, None, "no view was pinned");
    assert!(region.rendered);
    // A rendered fence carries no YAML, so it can never be mistaken for live.
    assert_eq!(region.yaml, None);
}

#[test]
fn the_rendered_fence_is_replaced_by_the_live_region_not_written_to_disk() {
    let result = reconcile_note("Host.md", HOST, PROJECTION);

    assert!(
        !result.text.contains("base-rendered"),
        "the fence reached disk: {}",
        result.text
    );
    assert!(
        !result.text.contains("Fix login redirect"),
        "rendered rows reached disk"
    );
    assert!(result.text.contains("![[Tickets.base]]"));
}

#[test]
fn an_untouched_round_trip_reports_no_refusal() {
    // Round-tripping a Projection is the DESIGNED flow. Reporting a refusal here
    // would mark the happy path `partial-with-errors` and train an agent to
    // ignore refusals, which costs more than it buys.
    let result = reconcile_note("Host.md", HOST, PROJECTION);

    assert_eq!(result.refused, Vec::new());
    assert!(!result.removed_region);
    // The strongest form of the invariant: read, write back, nothing moved.
    assert_eq!(result.text, HOST);
}

#[test]
fn prose_edits_around_the_region_still_apply() {
    let edited = PROJECTION.replace("## Tickets", "## Tickets (edited)");
    let result = reconcile_note("Host.md", HOST, &edited);

    assert!(result.text.contains("## Tickets (edited)"));
    assert!(result.text.contains("![[Tickets.base]]"));
    assert!(!result.text.contains("base-rendered"));
    assert_eq!(result.refused, Vec::new());
}

#[test]
fn an_agent_editing_the_rendered_rows_cannot_change_the_region() {
    let edited = PROJECTION.replace(
        "| Fix login redirect |",
        "| Fix login redirect |\n| Sneaky row |",
    );
    let result = reconcile_note("Host.md", HOST, &edited);

    assert_eq!(result.text, HOST);
    // Silence here too: a tampered rendered fence is indistinguishable from an
    // untouched one, and the live region is restored either way.
    assert_eq!(result.refused, Vec::new());
}

#[test]
fn a_rendered_fence_is_matched_by_base_path_not_by_position() {
    // The note has the region first; the Projection reorders it behind some
    // prose. Position matching would pair it with nothing and restore wrongly.
    let reordered = concat!(
        "---\n",
        "status: active\n",
        "---\n",
        "\n",
        "## Tickets\n",
        "\n",
        "intro prose\n",
        "\n",
        "```base-rendered path=\"Tickets.base\"\n",
        "| file name |\n",
        "```\n",
        "\n",
    );
    let result = reconcile_note("Host.md", HOST, reordered);

    assert!(result.text.contains("intro prose"));
    assert!(result.text.contains("![[Tickets.base]]"));
    assert!(!result.text.contains("base-rendered"));
    assert_eq!(result.refused, Vec::new());
}

#[test]
fn an_original_region_is_claimed_once_so_a_duplication_cannot_swap_them() {
    // The path pass has to consume each original as it pairs it. Without that,
    // two rendered fences naming the same Base would both restore the SAME
    // original, and `One` would be lost from the note entirely.
    let original = "---\n---\n\n![[One.base]]\n\n![[Two.base]]\n";
    let fence = |path: &str| format!("```base-rendered path=\"{path}\"\nrows\n```\n");
    let edited = format!("---\n---\n\n{}{}\n", fence("Two.base"), fence("Two.base"));
    let result = reconcile_note("Host.md", original, &edited);

    assert_eq!(
        result.text.matches("![[Two.base]]").count(),
        1,
        "the duplicate was not dropped: {}",
        result.text
    );
    assert!(
        result.text.contains("![[One.base]]"),
        "the unclaimed original was lost"
    );
    assert!(result.removed_region);
    let reasons: Vec<&str> = result.refused.iter().map(|r| r.reason.as_str()).collect();
    assert_eq!(
        reasons,
        [
            "A new base region was inserted.",
            "The base region was removed."
        ]
    );
}

#[test]
fn deleting_a_region_is_still_refused_loudly() {
    let result = reconcile_note("Host.md", HOST, &HOST.replace("![[Tickets.base]]\n", ""));

    assert!(result.removed_region);
    assert!(
        result.text.contains("![[Tickets.base]]"),
        "the embed was not restored"
    );
    assert_eq!(result.refused.len(), 1);
    let refusal = &result.refused[0];
    assert_eq!(refusal.reason, "The base region was removed.");
    assert_eq!(refusal.base_path.as_deref(), Some("Tickets.base"));
}

#[test]
fn an_added_region_is_dropped_and_reported() {
    let edited = format!("{HOST}\n![[Intruder.base]]\n");
    let result = reconcile_note("Host.md", HOST, &edited);

    assert!(
        !result.text.contains("Intruder"),
        "an added region reached disk"
    );
    assert_eq!(result.refused.len(), 1);
    assert_eq!(result.refused[0].reason, "A new base region was inserted.");
    assert_eq!(
        result.refused[0].base_path.as_deref(),
        Some("Intruder.base")
    );
}

#[test]
fn editing_a_live_fence_is_restored_and_refused_loudly() {
    // A live ```base fence is the one shape whose bytes an agent COULD have
    // edited meaningfully, and it is restored with the loud refusal.
    let original = "---\n---\n\n```base\nviews: []\n```\n";
    let edited = "---\n---\n\n```base\nviews:\n  - type: table\n```\n";
    let result = reconcile_note("Host.md", original, edited);

    assert_eq!(result.text, original);
    assert_eq!(result.refused.len(), 1);
    assert_eq!(
        result.refused[0].reason,
        "The rendered base region was modified."
    );
    assert_eq!(
        result.refused[0].base_path, None,
        "an inline fence has no Base file to name"
    );
}

#[test]
fn every_refusal_explains_where_rows_come_from() {
    // The guidance has to be actionable: an agent told only "no" will try again
    // the same way.
    let added = reconcile_note("Host.md", HOST, &format!("{HOST}\n![[Intruder.base]]\n"));
    assert!(
        added.refused[0].guidance.contains(".base file itself"),
        "{:?}",
        added.refused[0]
    );

    let original = "---\n---\n\n```base\nviews: []\n```\n";
    let tampered = reconcile_note("Host.md", original, "---\n---\n\n```base\nviews:\n```\n");
    assert!(
        tampered.refused[0]
            .guidance
            .contains("come from notes, not from the base file"),
        "{:?}",
        tampered.refused[0]
    );

    let deleted = reconcile_note("Host.md", HOST, &HOST.replace("![[Tickets.base]]\n", ""));
    assert!(deleted.refused[0]
        .guidance
        .contains("never removed by a note edit"));
}

// ---------------------------------------------------------------------------
// Building a Projection
// ---------------------------------------------------------------------------

#[test]
fn project_substitutes_every_region_and_leaves_the_prose_alone() {
    let note = parse_note_with_embeds("Host.md", HOST);
    let seen: Vec<(Option<String>, Option<String>, bool)> = base_regions(&note)
        .iter()
        .map(|r| (r.base_path.clone(), r.view_name.clone(), r.yaml.is_some()))
        .collect();
    assert_eq!(seen, [(Some("Tickets.base".to_string()), None, false)]);

    let rendered = project(&note, |region| {
        wrap_in_fence(
            "| rendered |",
            &FenceProvenance::new().with_path(region.base_path.clone().unwrap_or_default()),
        )
        .text
    });
    assert_eq!(
        rendered,
        "---\nstatus: active\n---\n\n## Tickets\n\n```base-rendered path=\"Tickets.base\"\n| rendered |\n```\n"
    );
}

/// A Projection must not splice a bare LF into a CRLF Host note.
///
/// A Base region on a CRLF line stops before the `\r` (see `split_base_embeds`), so
/// the prose segment after it begins with `\r\n` — which ends a line exactly as a
/// `\n` does. Treating that as mid-line output is what a newline-after-every-region
/// rule would do, and the Projection is what an agent copies its edits out of, so a
/// bare LF here comes straight back as an edit on the next write.
///
/// Projection-only: nothing here reaches disk, and the fence's own body is built
/// with `\n` whatever the note uses.
#[test]
fn a_projection_of_a_crlf_note_contains_no_lone_lf() {
    let note = parse_note_with_embeds(
        "Host.md",
        "# Host\r\n\r\nIntro.\r\n\r\n![[T.base]]\r\n\r\nEnd.\r\n",
    );
    let projected = project(&note, |region| {
        wrap_in_fence(
            "| rendered |",
            &FenceProvenance::new().with_path(region.base_path.clone().unwrap_or_default()),
        )
        .text
    });

    assert_eq!(
        projected,
        "# Host\r\n\r\nIntro.\r\n\r\n```base-rendered path=\"T.base\"\n| rendered |\n```\r\n\r\nEnd.\r\n"
    );
    assert_eq!(
        lone_lf(&projected),
        2,
        "only the fence's own two joins may use LF: {projected:?}"
    );
    assert_eq!(
        stray_cr(&projected),
        0,
        "a doubled carriage return reached the Projection: {projected:?}"
    );
}

#[test]
fn base_regions_are_indexed_in_document_order() {
    let text = "---\n---\n\n![[A.base]]\n\n![[B.base#Pinned]]\n\n```base\nviews: []\n```\n";
    let note = parse_note_with_embeds("Host.md", text);
    let regions = base_regions(&note);

    assert_eq!(regions.len(), 3);
    assert_eq!(
        regions
            .iter()
            .map(|r| (
                r.index,
                r.base_path.as_deref(),
                r.view_name.as_deref(),
                r.rendered
            ))
            .collect::<Vec<_>>(),
        [
            (0, Some("A.base"), None, false),
            (1, Some("B.base"), Some("Pinned"), false),
            (2, None, None, false),
        ]
    );
    // Every region spans real bytes, which is what makes restoring it exact.
    for region in &regions {
        assert!(
            region.end > region.start,
            "region {} is empty",
            region.index
        );
    }
}

// ---------------------------------------------------------------------------
// The testing vault
// ---------------------------------------------------------------------------

/// A path inside the testing vault, which is read and never written.
fn vault_file(name: &str) -> String {
    fs::read_to_string(Path::new("test/vault").join(name)).expect("the testing vault is readable")
}

/// `Root Project.md` as `get_note` renders it: an embed AND an inline ```base
/// fence, each replaced by a provenance fence.
///
/// This is the case that makes the reconciler's pairing rule load-bearing. The
/// inline fence records no `path=`, because an inline base has no `.base` file,
/// so it can only be paired by position and pinned view. Without that rule the
/// second region pairs with nothing, is reported as inserted, and the live fence
/// is reported as removed -- on a clean round-trip.
const VAULT_PROJECTION: &str = r#"---
tags:
  - business-idea
  - project
type:
  - "[[Geschäftsidee]]"
categories:
  - "[[Projects]]"
status: active
priority: high
description: Eine Projektnotiz direkt im Vault-Root, ohne Ordner.
---

## Tickets

A root-level host note. `file.folder` is `"/"` here, not the empty string —
Obsidian reports the vault root that way — and `this.file.name` resolves to
`Root Project.md`, so the embedded base must scope itself to this note just like
the two hosts under `Projects/` do.

```base-rendered path="Tickets.base" context="Root Project.md"
| file name | priority |
| --- | --- |
| Root Ticket | 1 – high |
```

## Inline

The same scoping, expressed with an inline fence instead of an embed. Obsidian
binds `this` to the containing note, so this resolves identically.

```base-rendered context="Root Project.md"
- Root Ticket
  - status: active
```
"#;

/// Rebuild the Projection of a vault host note, using the renderer as the
/// caller would: one provenance fence per region.
fn project_vault_host(path: &str, original: &str) -> String {
    let note = parse_note_with_embeds(path, original);
    project(&note, |region| {
        let provenance = match &region.base_path {
            Some(base_path) => FenceProvenance::new()
                .with_path(base_path)
                .with_context(path),
            // An inline base has no file to name, so the fence carries the host
            // note alone -- and that is what makes it hard to pair.
            None => FenceProvenance::new().with_context(path),
        };
        wrap_in_fence(
            match region.yaml.is_some() {
                true => "- Root Ticket\n  - status: active",
                false => "| file name | priority |\n| --- | --- |\n| Root Ticket | 1 – high |\n",
            },
            &provenance,
        )
        .text
    })
}

#[test]
fn a_host_note_with_an_embed_and_an_inline_base_reconciles_byte_identically() {
    let original = vault_file("Root Project.md");
    let projection = project_vault_host("Root Project.md", &original);
    assert_eq!(
        projection, VAULT_PROJECTION,
        "the Projection drifted from `get_note`"
    );

    let result = reconcile_note("Root Project.md", &original, &projection);
    assert_eq!(
        result.refused,
        Vec::new(),
        "a clean round-trip refused something"
    );
    assert!(
        !result.removed_region,
        "a clean round-trip removed a region"
    );
    // Byte for byte, umlauts and all -- the note never came off disk.
    assert_eq!(result.text, original);
}

#[test]
fn a_edited_inline_region_in_a_vault_host_cannot_reach_disk() {
    let original = vault_file("Root Project.md");
    let edited = project_vault_host("Root Project.md", &original)
        .replace("- Root Ticket\n  - status: active", "- Sneaky row");

    let result = reconcile_note("Root Project.md", &original, &edited);
    assert_eq!(result.text, original);
    assert_eq!(
        result.refused,
        Vec::new(),
        "a rendered fence is replaced silently"
    );
}

#[test]
fn a_tampered_projection_of_every_vault_host_still_round_trips() {
    // Whatever an agent does to the rendered rows, the live regions come back
    // exactly as they were.
    for path in vault_notes() {
        let (name, original) = path;
        let edited = project_vault_host(&name, &original)
            .replace("| Root Ticket", "| Sneaky row\n| Root Ticket");
        let result = reconcile_note(&name, &original, &edited);

        assert_eq!(result.text, original, "{name} did not round-trip");
        assert!(
            !result.text.contains("base-rendered"),
            "{name} leaked a fence to disk"
        );
        assert!(
            !result.text.contains("Sneaky"),
            "{name} kept a rendered row"
        );
    }
}

#[test]
fn the_testing_vault_is_the_fixture_the_renderer_is_written_against() {
    // `AllNotes.base` is the parity oracle: three views spanning all three
    // structured layouts, and summaries that exist only to prove the two
    // surfaces diverge on purpose.
    let all_notes = vault_file("AllNotes.base");
    assert!(all_notes.contains("name: All"), "the table view moved");
    assert!(
        all_notes.contains("name: ByPriority"),
        "the cards view moved"
    );
    assert!(all_notes.contains("name: AsList"), "the list view moved");
    assert!(
        all_notes.contains("groupBy:"),
        "the groupBy view lost its grouping"
    );
    assert!(
        all_notes.contains("note.status: Unique"),
        "the summary fixture moved"
    );

    // `Tickets.base` is the divergence: it scopes itself with `this`. Its view
    // level also carries plugin keys, including a `\x1f`-joined composite, which
    // must survive parsing verbatim.
    let tickets = vault_file("Tickets.base");
    assert!(
        tickets.contains("link(this.file.name)"),
        "the `this` scoping moved"
    );
    assert!(
        tickets.contains("file.tasks"),
        "the `file.tasks` extension moved"
    );
    // Written escaped in the YAML and decoded by the parser, so the file on disk
    // carries the spelling rather than a raw control byte.
    assert!(
        tickets.contains(r"formula.priority_display\x1fnote.type"),
        "the composite plugin key moved"
    );
    assert!(tickets.contains("cardOrders:"), "the plugin keys moved");
}

/// Every `.md` file in the testing vault, read-only.
fn vault_notes() -> Vec<(String, String)> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        let mut entries: Vec<PathBuf> = fs::read_dir(dir)
            .expect("the testing vault is readable")
            .map(|entry| entry.expect("a directory entry").path())
            .collect();
        entries.sort();
        for entry in entries {
            if entry.is_dir() {
                walk(&entry, out);
            } else if entry.extension().is_some_and(|ext| ext == "md") {
                out.push(entry);
            }
        }
    }

    let mut files = Vec::new();
    walk(Path::new("test/vault"), &mut files);
    assert!(
        files.len() >= 7,
        "expected the testing vault's notes, found {}",
        files.len()
    );
    files
        .into_iter()
        .map(|path| {
            let text = fs::read_to_string(&path).expect("a note is valid UTF-8");
            (path.to_string_lossy().replace('\\', "/"), text)
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Helpers that keep the fixture declarations readable
// ---------------------------------------------------------------------------

/// A Base whose `properties` map is the given `ID -> displayName` pairs.
fn labelled(properties: &[(&str, &str)]) -> BaseFile {
    BaseFile {
        properties: properties
            .iter()
            .map(|(id, display_name)| {
                (
                    id.to_string(),
                    PropertyConfig {
                        display_name: Some(display_name.to_string()),
                        extra: Mapping::new(),
                    },
                )
            })
            .collect(),
        ..base()
    }
}

/// Attach summaries to a result built by [`with_rows`].
fn summarised_onto(mut result: QueryResult, summaries: &[(&str, &str)]) -> QueryResult {
    result.view.summaries = Some(
        summaries
            .iter()
            .map(|(id, spec)| (id.to_string(), spec.to_string()))
            .collect(),
    );
    result
}

/// A restore must not corrupt the agent's prose.
///
/// Regression. The insertion point for a deleted region was a byte offset taken
/// from the ORIGINAL text and then indexed into the EDITED text. Once the agent
/// had added or removed anything above the region, that offset pointed at an
/// arbitrary character — routinely the middle of a sentence it had just written,
/// with the embed spliced in between the halves.
///
/// Anchoring on the other regions' positions rather than on an offset is what
/// fixes it, and the assertion has to be about the prose rather than merely that
/// the embed is present, which is all the existing test checked.
#[test]
fn a_restore_does_not_split_a_sentence_the_agent_just_wrote() {
    let original = "# Host\n\nIntro line.\n\n![[T.base]]\n\nTrailing prose.\n";
    // The agent deletes the region and writes a long new paragraph above it, so
    // every byte after the deletion point shifts.
    let edited = "# Host\n\nIntro line.\n\nA brand new paragraph inserted by the agent, \
long enough to shift every byte\n\nTrailing prose.\n";

    let result = reconcile_note("Host.md", original, edited);

    assert!(result.removed_region);
    assert!(
        result.text.contains("![[T.base]]"),
        "the embed was not restored"
    );
    assert!(
        !result
            .text
            .contains("shift every byte\n![[T.base]]\n offset"),
        "the embed was spliced into the middle of the sentence"
    );
    for phrase in [
        "A brand new paragraph inserted by the agent, long enough to shift every byte",
        "Intro line.",
        "Trailing prose.",
    ] {
        assert!(
            result.text.contains(phrase),
            "the agent's prose lost a line: {phrase:?}\n---\n{}",
            result.text
        );
    }
    // The embed sits between the prose blocks, as a whole line.
    let embed_line = result
        .text
        .lines()
        .position(|line| line == "![[T.base]]")
        .expect("the embed is on its own line");
    let intro_line = result
        .text
        .lines()
        .position(|line| line.starts_with("A brand new paragraph"))
        .expect("the new paragraph survives");
    assert!(
        embed_line > intro_line,
        "the embed must not land above the prose the agent wrote"
    );
}

/// Two deleted regions, restored in their original order and not stacked together.
#[test]
fn two_deleted_regions_keep_their_order() {
    let original = "# Host\n\nA\n\n![[One.base]]\n\nMiddle prose.\n\n![[Two.base]]\n\nEnd prose.\n";
    let edited = "# Host\n\nA\n\nOne short new line.\n\nMiddle prose.\n\nEnd prose.\n";

    let result = reconcile_note("Host.md", original, edited);

    assert!(result.removed_region);
    assert_eq!(result.refused.len(), 2);
    assert!(result.text.contains("![[One.base]]"));
    assert!(result.text.contains("![[Two.base]]"));
    let one = result.text.find("![[One.base]]").expect("restored");
    let two = result.text.find("![[Two.base]]").expect("restored");
    assert!(one < two, "the regions came back in the wrong order");
    assert!(
        result.text.contains("One short new line."),
        "the agent's edit was lost"
    );
}

/// A note that is nothing but a deleted region still round-trips.
#[test]
fn a_region_only_note_still_round_trips() {
    let original = "![[Only.base]]\n";
    let result = reconcile_note("Host.md", original, "");

    assert!(result.removed_region);
    assert_eq!(result.text, "![[Only.base]]\n");
}

/// A restored Base region must be spliced in with the note's OWN line ending.
///
/// Regression, and the second half of the CRLF work in `src/note.rs` (c4af62f).
/// That commit made a Base region visible in a CRLF note at all; this is what
/// happens once it is visible again. `push_line` appended `\n` to the joins it
/// generates, so a deleted region restored into a CRLF Host note came back with a
/// lone LF beside two CRLF lines. Git, diff tools and Obsidian's own line-ending
/// handling all read such a file as corrupt or as wholly rewritten, and it
/// reaches disk whether or not the caller looked at it.
///
/// An inline ```base fence is the shape that shows it, because the region's byte
/// span stops at the closing fence and carries no `\r` of its own for a bare `\n`
/// to accidentally complete. An `![[X.base]]` embed used to span its line's `\r`,
/// so the same call on an embed produced CRLF by accident rather than by
/// construction — and once the span was corrected to stop before the `\r`, an
/// embed joined the fence as a shape this catches for real.
#[test]
fn a_restored_region_is_spliced_into_a_crlf_note_with_crlf() {
    let original = "# Host\r\n\r\nIntro.\r\n\r\n```base\r\nviews: []\r\n```\r\n\r\nEnd.\r\n";
    // The agent deletes the inline fence and the blank lines around it, so the
    // restore has to generate its own terminator rather than inherit one.
    let edited = "# Host\r\n\r\nIntro.\r\n\r\nEnd.\r\n";

    let result = reconcile_note("Host.md", original, edited);

    assert!(result.removed_region);
    assert!(
        result
            .refused
            .iter()
            .any(|refusal| refusal.reason.contains("was removed")),
        "the deletion must still be reported: {:?}",
        result.refused
    );
    assert!(
        result.text.contains("```base\r\nviews: []\r\n```"),
        "the inline Base region was not restored: {:?}",
        result.text
    );
    for phrase in ["Intro.", "End."] {
        assert!(result.text.contains(phrase), "prose lost: {phrase:?}");
    }
    assert_eq!(
        lone_lf(&result.text),
        0,
        "a bare LF was spliced into a CRLF note: {:?}",
        result.text
    );
    assert_eq!(
        stray_cr(&result.text),
        0,
        "a doubled carriage return was spliced into a CRLF note: {:?}",
        result.text
    );
}

/// Every restore shape in a CRLF Host note comes out pure CRLF — and still parses.
///
/// `push_line` generates a join on both sides of the restored region, so each side
/// is its own way to splice a bare terminator in. The leading join needs text that
/// does not already end in one, which is why one shape ends without a newline. The
/// trailing join fires whenever the region's own bytes stop before its terminator,
/// which an inline fence always does, an embed at end of file does, and — since the
/// span was corrected to stop before the `\r` of its line — a Base region on a
/// terminated line does too.
///
/// The re-parse is the half that matters. A `\r` absorbed into the region and then
/// joined with the terminator produces `\r\r\n`, which no amount of counting bare
/// LFs detects: it contains no lone `\n` at all. The note is byte-visible, contains
/// the embed, and reports `health: ok` — but the region cannot match a second time,
/// so the next read returns `regions: []` and the following deletion of that
/// invisible region is neither refused nor reported. Silent loss, and the counting
/// assertion that should have caught it looked only in the other direction.
#[test]
fn every_restore_shape_in_a_crlf_note_is_pure_crlf() {
    for (label, original, edited) in [
        (
            "an inline fence between prose",
            "# Host\r\n\r\nIntro.\r\n\r\n```base\r\nviews: []\r\n```\r\n\r\nEnd.\r\n",
            "# Host\r\n\r\nIntro.\r\n\r\nEnd.\r\n",
        ),
        (
            "an embed at end of file",
            "# Host\r\n\r\nIntro.\r\n\r\n![[Only.base]]",
            "# Host\r\n\r\nIntro.\r\n",
        ),
        (
            "edited text ending without a newline",
            "# Host\r\n\r\nIntro.\r\n\r\n```base\r\nviews: []\r\n```\r\n\r\nEnd.\r\n",
            "# Host\r\n\r\nIntro.\r\n\r\nEnd.",
        ),
        (
            "a Base region on a terminated line",
            "# Host\r\n\r\nIntro.\r\n\r\n![[Only.base]]\r\n",
            "# Host\r\n\r\nIntro.\r\n",
        ),
        (
            "an indented Base region on a terminated line",
            "# Host\r\n\r\nIntro.\r\n\r\n  ![[Only.base]]  \r\n\r\nEnd.\r\n",
            "# Host\r\n\r\nIntro.\r\n\r\nEnd.\r\n",
        ),
    ] {
        let result = reconcile_note("Host.md", original, edited);
        assert!(
            result.removed_region,
            "{label}: the deletion went unreported"
        );
        assert_eq!(
            lone_lf(&result.text),
            0,
            "{label}: a bare LF was spliced into a CRLF note: {:?}",
            result.text
        );
        assert_eq!(
            stray_cr(&result.text),
            0,
            "{label}: a doubled carriage return was spliced into a CRLF note: {:?}",
            result.text
        );

        // A region the next read cannot see is the failure that costs the note.
        let stored = base_regions(&parse_note_with_embeds("Host.md", original));
        let restored = base_regions(&parse_note_with_embeds("Host.md", &result.text));
        assert_eq!(
            (stored.len(), restored.len()),
            (1, 1),
            "{label}: the restored Base region is not the one that was stored: {:?}",
            result.text
        );
        // Verbatim, and possibly a terminator longer: a region that ended the
        // note had none, so the restore has to invent one and the next parse
        // reads it as part of the region.
        assert!(
            result.text[restored[0].start..restored[0].end]
                .starts_with(&original[stored[0].start..stored[0].end]),
            "{label}: the stored region's bytes were not restored verbatim: {:?}",
            result.text
        );
    }
}

/// How many `\n` in `text` are not the second half of a `\r\n`.
fn lone_lf(text: &str) -> usize {
    let bytes = text.as_bytes();
    (0..bytes.len())
        .filter(|&index| bytes[index] == b'\n' && (index == 0 || bytes[index - 1] != b'\r'))
        .count()
}

/// How many `\r` in `text` are not the first half of a `\r\n`.
///
/// The other half of [`lone_lf`], and the half that was missing. `\r\r\n` is a
/// doubled carriage return — a line ending that is neither LF nor CRLF, that no
/// line-ending detector recognises and that most editors silently rewrite — and it
/// contains no lone `\n`, so counting LFs alone passes straight through it.
fn stray_cr(text: &str) -> usize {
    let bytes = text.as_bytes();
    (0..bytes.len())
        .filter(|&index| bytes[index] == b'\r' && bytes.get(index + 1) != Some(&b'\n'))
        .count()
}
