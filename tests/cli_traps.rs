//! The CLI's traps, pinned.
//!
//! `obsidian base:query` has five behaviours that are silent when they go
//! wrong: nothing in the output says "I truncated you", "I dropped a column"
//! or "that value was a number". The result simply does not contain what you
//! asked for, and a regression in any of them would sail through a suite that
//! only checks the happy path.
//!
//! So each one is asserted here, against a fixture vault built for the purpose
//! (`test/fixtures/cli-traps`). The trap is the assertion; the fixture is
//! arranged so that a broken implementation produces a DIFFERENT answer rather
//! than an absent one.
//!
//! ## What each trap is
//!
//! 1. **`limit:` truncates silently.** The rows past the limit are gone and
//!    nothing says so. The pre-limit count survives in `total`, and that is the
//!    only place the truncation is visible.
//! 2. **The columns come only from `order`.** A property the Base *declares*,
//!    or one the view *sorts* on, is not a column. The CLI emits the columns the
//!    view orders and nothing else.
//! 3. **A header is a display name, and two columns can share one.** The JSON
//!    key is the display label, so two columns labelled `Status` collide on one
//!    key and one of them is silently dropped.
//! 4. **Every value is a string.** Numbers, booleans, lists and links are all
//!    stringified; only an absent value stays null.
//! 5. **Nulls sort last, in BOTH directions.** Not "first ascending": last,
//!    whichever way the sort runs.
//!
//! ## The one thing this file cannot do
//!
//! It cannot compare against the live Obsidian CLI. That CLI talks to the
//! desktop app, which rewrites the `.base` files of whatever vault is open --
//! and the vault the app has open is the parity oracle on `develop`, not this
//! worktree. Running it here would corrupt the oracle for everyone.
//!
//! So these assertions are the documented behaviour, encoded. The live
//! comparison is the deferred UI-validation work (STATUS.md item 14), and it
//! needs a human in the Obsidian UI. Every trap below is marked with where its
//! behaviour comes from, so a human doing that validation knows exactly what to
//! look at.
//!
//! ## What this file writes
//!
//! Nothing. `test/fixtures/` is read-only by construction and `test/vault` is
//! never touched; the oracle stays at exactly [`CORPUS_SIZE`] files.

// Each integration test is its own crate, and no suite needs every helper here.
mod common;

use std::path::Path;
use std::sync::Arc;

use bases_mcp::base::{canonical, parse_base, query_base, BaseFile, QueryOptions, QueryResult};
use bases_mcp::render::markdown::{render_columns, render_markdown, RenderStyle};
use bases_mcp::service::{json_rows, stringified};
use bases_mcp::value::BasesValue;
use bases_mcp::vault::{FsVaultSource, Vault};
use common::{count_files, futures_block_on, vault_dir, CORPUS_SIZE};

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// The fixture vault holding the five traps.
fn traps_vault() -> Arc<Vault> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("test")
        .join("fixtures")
        .join("cli-traps");
    let source = FsVaultSource::new(&dir).expect("the cli-traps fixture is a vault directory");
    let vault = Arc::new(Vault::new(Box::new(source)));
    futures_block_on(vault.load()).expect("the cli-traps fixture is readable");
    vault
}

/// The fixture's Base, parsed.
fn traps_base(vault: &Vault) -> BaseFile {
    let text =
        futures_block_on(vault.read_note("Traps.base")).expect("the fixture base is readable");
    parse_base("Traps.base", &text).expect("the fixture base parses")
}

/// Resolve one view of the fixture Base.
fn query(vault: &Arc<Vault>, view: &str) -> QueryResult {
    let base = traps_base(vault);
    query_base(
        vault,
        "Traps.base",
        &base,
        &QueryOptions {
            context: None,
            view: Some(view.to_string()),
        },
    )
    .expect("the fixture query resolves")
}

/// The row paths of a result, in result order.
fn paths(result: &QueryResult) -> Vec<&str> {
    result.rows.iter().map(|row| row.path.as_str()).collect()
}

/// The JSON rows of a view, as the CLI's `format=json` would key them.
fn json_rows_of(vault: &Arc<Vault>, view: &str) -> Vec<serde_json::Value> {
    let base = traps_base(vault);
    let result = query(vault, view);
    json_rows(&base, &result, &[])
}

/// The flat, CLI-parity markdown table of a view.
fn flat_markdown(vault: &Arc<Vault>, view: &str) -> String {
    let base = traps_base(vault);
    let result = query(vault, view);
    render_markdown(&base, &result, RenderStyle::Flat).expect("the view renders")
}

// ---------------------------------------------------------------------------
// The fixture itself
// ---------------------------------------------------------------------------

/// The fixture vault holds exactly three notes and one Base.
///
/// A fixture that grew would change what every assertion below means, so the
/// count is pinned rather than derived: the same reasoning as the oracle's
/// file count in `tests/base.rs`.
#[test]
fn the_fixture_holds_exactly_three_notes_and_one_base() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("test")
        .join("fixtures")
        .join("cli-traps");
    assert_eq!(count_files(&dir), 4, "three notes and one .base");
}

/// The oracle is untouched by this suite.
#[test]
fn the_testing_vault_is_untouched() {
    assert_eq!(count_files(&vault_dir()), CORPUS_SIZE);
}

// ---------------------------------------------------------------------------
// Trap 1: `limit:` truncates silently
// ---------------------------------------------------------------------------

/// A limit drops the rows past it, and the only trace is `total`.
///
/// `total` is the PRE-limit match count, which is what the UI's "N of M"
/// reports. So the truncation is observable, but only if you look: the row
/// list itself carries no marker saying where it was cut.
#[test]
fn a_limit_truncates_the_rows_and_the_only_trace_is_the_total() {
    let vault = traps_vault();
    let result = query(&vault, "Limited");

    // Three notes match; the limit is two.
    assert_eq!(result.total, 3, "the pre-limit count is the total");
    assert_eq!(
        paths(&result),
        ["Notes/Alpha.md", "Notes/Beta.md"],
        "the limit keeps the FIRST rows in sort order"
    );
    assert_eq!(result.rows.len(), 2);
}

/// A limit of ZERO is no limit at all.
///
/// Obsidian's view parser only keeps a limit that is a NUMBER and POSITIVE:
/// `Number.isNumber(i.limit) && i.limit > 0 && (a.limit = i.limit)`. So
/// `limit: 0` never becomes a limit, and the view keeps every row.
///
/// This is the trap's sharpest edge. A limit of zero is the one value that
/// LOOKS like "keep nothing" and means "keep everything", so an
/// implementation that treated it as a real limit would return an empty table
/// where Obsidian returns the whole set.
#[test]
fn a_limit_of_zero_is_no_limit_at_all() {
    let vault = traps_vault();
    let base = traps_base(&vault);
    // The fixture's `Limited` view has `limit: 2`; a zero-limit view is
    // spelled here rather than added to the fixture, because the fixture has
    // one view per trap and this is the same trap.
    let mut base = base.clone();
    for view in &mut base.views {
        if view.name == "Limited" {
            view.limit = Some(0.0);
        }
    }
    let result = query_base(
        &vault,
        "Traps.base",
        &base,
        &QueryOptions {
            context: None,
            view: Some("Limited".into()),
        },
    )
    .expect("the query resolves");
    assert_eq!(
        result.rows.len(),
        3,
        "`limit: 0` is not a limit: Obsidian keeps every row"
    );
    assert_eq!(result.total, 3);
}

/// A grouped limit spends ONE budget across every group.
///
/// This is the trap's second half: a limit that were applied per group would
/// keep two rows from EACH group, and the total would be more than the limit.
/// The groups here are `active`, `done` and `open`, and the first group alone
/// would eat the whole budget.
#[test]
fn a_grouped_limit_spends_one_budget_across_every_group() {
    let vault = traps_vault();
    let result = query(&vault, "LimitedByGroup");

    assert_eq!(result.total, 3, "the pre-limit count is the total");
    assert_eq!(
        result.rows.len(),
        2,
        "the budget is spent across the groups, not per group"
    );
    let groups = result.groups.as_ref().expect("a grouped result");
    assert!(
        groups.len() <= 2,
        "no group may be kept past the budget: {:?}",
        groups.iter().map(|g| &g.key).collect::<Vec<_>>()
    );
    // The groups that survive hold the first rows in group order.
    assert_eq!(groups[0].key, "active");
    assert_eq!(
        groups[0]
            .rows
            .iter()
            .map(|row| row.path.as_str())
            .collect::<Vec<_>>(),
        ["Notes/Alpha.md"]
    );
}

// ---------------------------------------------------------------------------
// Trap 2: the columns come only from `order`
// ---------------------------------------------------------------------------

/// A column is a column the view ORDERS.
///
/// The Base DECLARES `note.status` and the view SORTS on `note.rank`, and
/// neither is a column: the CLI emits the ordered columns and nothing else.
/// A base that invented columns from the declarations or the sort would show
/// an agent properties nobody asked for.
#[test]
fn the_columns_are_the_ordered_ones_and_only_those() {
    let vault = traps_vault();
    let base = traps_base(&vault);
    let result = query(&vault, "Ordered");

    let columns = render_columns(&base, &result.view);
    let ids: Vec<&str> = columns.iter().map(|column| column.id.as_str()).collect();
    assert_eq!(
        ids,
        ["file.name", "note.kind"],
        "a declared property and a sorted property are not columns"
    );
    assert!(
        !ids.contains(&"note.status"),
        "`note.status` is declared by the Base but not ordered, so it is not a column"
    );
    assert!(
        !ids.contains(&"note.rank"),
        "`note.rank` is sorted on but not ordered, so it is not a column"
    );
}

/// The JSON row carries exactly the ordered columns.
///
/// The same rule on the machine-readable surface, because a JSON row that
/// carried an extra key would break a consumer that reads the row as a
/// record of the columns it asked for.
#[test]
fn a_json_row_carries_exactly_the_ordered_columns() {
    let vault = traps_vault();
    let rows = json_rows_of(&vault, "Ordered");
    let row = rows.first().expect("a row");
    let keys: Vec<&str> = row
        .as_object()
        .expect("a row is an object")
        .keys()
        .map(|k| k.as_str())
        .collect();
    assert_eq!(keys, ["path", "file name", "kind"]);
    assert!(
        !row.as_object()
            .expect("a row is an object")
            .contains_key("status"),
        "the declared `note.status` must not appear"
    );
}

/// `order: []` falls back to the file-name column.
///
/// Obsidian's accessor is `this.order || ["file.name"]`, and an empty array is
/// falsy in JavaScript -- so `order: []` is not "no columns", it is the
/// default. An implementation that treated an empty list as an instruction
/// would render a table with no columns at all, where Obsidian renders one.
#[test]
fn an_empty_order_falls_back_to_the_file_name_column() {
    let vault = traps_vault();
    let mut base = traps_base(&vault);
    for view in &mut base.views {
        if view.name == "Ordered" {
            view.order = Some(Vec::new());
        }
    }
    let result = query_base(
        &vault,
        "Traps.base",
        &base,
        &QueryOptions {
            context: None,
            view: Some("Ordered".into()),
        },
    )
    .expect("the query resolves");
    let columns = render_columns(&base, &result.view);
    assert_eq!(
        columns.iter().map(|c| &c.id).collect::<Vec<_>>(),
        ["file.name"],
        "`order: []` falls back to the default, it does not empty the table"
    );
}

/// The JSON surface falls back the same way the markdown one does.
///
/// The two surfaces build their columns separately, and the CLI builds both
/// from the same `getOrder()`, so the fallback has to be the SAME on both. A
/// JSON surface that fell back to the Base's DECLARED properties would put
/// columns in the row that the markdown table beside it does not have.
#[test]
fn the_json_surface_falls_back_to_the_file_name_column_too() {
    let vault = traps_vault();
    let mut base = traps_base(&vault);
    for view in &mut base.views {
        if view.name == "Ordered" {
            view.order = Some(Vec::new());
        }
    }
    let result = query_base(
        &vault,
        "Traps.base",
        &base,
        &QueryOptions {
            context: None,
            view: Some("Ordered".into()),
        },
    )
    .expect("the query resolves");
    let rows = json_rows(&base, &result, &[]);
    let keys: Vec<&str> = rows[0]
        .as_object()
        .expect("a row is an object")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        ["path", "file name"],
        "the JSON row falls back to the file-name column, like the table does"
    );
}

// ---------------------------------------------------------------------------
// Trap 3: a header is a display name, and two columns can share one
// ---------------------------------------------------------------------------

/// Two columns with the same display name collide on ONE JSON key.
///
/// The JSON key is the display label, not the Property ID, so `note.status`
/// and `formula.status_alias` -- both configured with `displayName: Status` --
/// want the same key. One of them wins and the other is silently dropped:
/// the row still looks complete, because the surviving key has a value.
#[test]
fn two_columns_with_the_same_display_name_collapse_to_one_json_key() {
    let vault = traps_vault();
    let rows = json_rows_of(&vault, "Colliding");
    let row = rows.first().expect("a row");
    let object = row.as_object().expect("a row is an object");

    assert!(
        object.contains_key("Status"),
        "the surviving key is the display name: {:?}",
        object.keys().collect::<Vec<_>>()
    );
    assert_eq!(
        object.keys().filter(|key| *key == "Status").count(),
        1,
        "one key, not two: {:?}",
        object.keys().collect::<Vec<_>>()
    );
    assert!(
        !object.contains_key("status_alias"),
        "the formula's own label is not a key: the collision is on the DISPLAY name"
    );
    // The row is still three columns wide in markdown -- the collision is a
    // JSON problem, not a rendering one.
    let columns = render_columns(&traps_base(&vault), &query(&vault, "Colliding").view);
    assert_eq!(columns.len(), 3, "the table keeps all three columns");
}

/// The value of the surviving key.
///
/// The LAST ordered column with that display name wins: the CLI assigns
/// `row[header] = cell` in `order` order, so a later column overwrites an
/// earlier one. The row keeps its shape either way, which is what makes the
/// collision silent.
#[test]
fn the_colliding_key_keeps_the_value_of_the_column_that_won() {
    let vault = traps_vault();
    let rows = json_rows_of(&vault, "Colliding");
    let object = rows.first().expect("a row").as_object().expect("an object");
    let value = object
        .get("Status")
        .expect("the surviving key")
        .as_str()
        .expect("the surviving key holds a string");
    // `formula.status_alias` is ordered LAST and both columns are labelled
    // `Status`, so the formula's value is the one that survives.
    assert_eq!(value, "note!");
}

/// The markdown table shows BOTH columns, under the SAME header.
///
/// The collision is a JSON-key problem. The markdown table has no keys, so
/// both columns are rendered -- which is why the trap is invisible in `format=md`
/// and only bites a machine reading `format=json`.
#[test]
fn the_markdown_table_keeps_both_colliding_columns() {
    let vault = traps_vault();
    let markdown = flat_markdown(&vault, "Colliding");
    let header = markdown.lines().next().expect("a header row");
    assert_eq!(
        header.matches("Status").count(),
        2,
        "both columns are rendered, under one header: {header}"
    );
}

// ---------------------------------------------------------------------------
// Trap 4: every value is a string
// ---------------------------------------------------------------------------

/// Every cell the CLI emits is a STRING.
///
/// Numbers, booleans, lists and links are all stringified; only an ABSENT
/// value stays null. A consumer that reads `"rank": 1` as a number -- or as a
/// string -- is wrong in opposite directions, and the difference is invisible
/// until the consumer does arithmetic on it.
#[test]
fn every_value_is_a_string() {
    let vault = traps_vault();
    let rows = json_rows_of(&vault, "Strings");
    let object = rows[0].as_object().expect("a row is an object");

    let value = |key: &str| object.get(key).expect("the column is there");
    assert_eq!(
        value("rank"),
        &serde_json::json!("1"),
        "a number is a string"
    );
    assert_eq!(
        value("active"),
        &serde_json::json!("true"),
        "a boolean is a string"
    );
    assert_eq!(
        value("scores"),
        &serde_json::json!("10, 20"),
        "a list is a string, joined with \", \""
    );
    assert_eq!(
        value("related"),
        &serde_json::json!("[[Beta]]"),
        "a link is a string"
    );
    assert_eq!(value("file name"), &serde_json::json!("Alpha"));
}

/// An absent value stays null, and is the ONLY thing that does.
///
/// The distinction between "absent" and "present but empty" is what makes
/// `!= null` the test for "this property has a value at all".
#[test]
fn an_absent_value_stays_null() {
    let vault = traps_vault();
    let rows = json_rows_of(&vault, "Strings");
    let beta = rows
        .iter()
        .find(|row| row["path"] == serde_json::json!("Notes/Beta.md"))
        .expect("the Beta row");
    let object = beta.as_object().expect("a row is an object");

    // `related` is absent from Beta, and null is the only non-string value.
    assert_eq!(object.get("related"), Some(&serde_json::Value::Null));
    // A checkbox that is false is a VALUE, so it is the string "false" and
    // not null.
    assert_eq!(object.get("active"), Some(&serde_json::json!("false")));
}

/// The stringification is the ONE rule, on every surface.
///
/// `format=json` and `format=md` agree: the markdown cell for `rank` is `1`,
/// not `1.0`, and the boolean is `true`. Two copies of the stringification
/// rule would be free to drift, and the drift would show up as the JSON
/// surface disagreeing with the markdown beside it.
#[test]
fn the_markdown_cells_stringify_the_same_way() {
    let vault = traps_vault();
    let markdown = flat_markdown(&vault, "Strings");
    let alpha = markdown
        .lines()
        .find(|line| line.contains("Alpha"))
        .expect("the Alpha row");

    assert!(
        alpha.contains("10, 20"),
        "a list cell joins with \", \": {alpha}"
    );
    assert!(
        alpha.contains("true"),
        "a boolean cell is its text: {alpha}"
    );
    assert!(
        !alpha.contains("1.0"),
        "an integral number has no fractional part: {alpha}"
    );
}

// ---------------------------------------------------------------------------
// Trap 5: nulls sort last, in both directions
// ---------------------------------------------------------------------------

/// A null sorts LAST when the sort ascends.
///
/// Not "first": the row with no value comes after the rows that have one, in
/// the ascending direction too.
#[test]
fn a_null_sorts_last_ascending() {
    let vault = traps_vault();
    let result = query(&vault, "NullsFirst");

    assert_eq!(
        paths(&result),
        ["Notes/Alpha.md", "Notes/Beta.md", "Notes/Gamma.md",],
        "the note with no rank comes last, ascending"
    );
}

/// A null sorts LAST when the sort descends, too.
///
/// This is the half that is easy to get wrong: descending is usually
/// implemented as "reverse the ascending order", which would put the null
/// FIRST. Obsidian keeps it last in both directions, and so must this.
#[test]
fn a_null_sorts_last_descending_too() {
    let vault = traps_vault();
    let result = query(&vault, "NullsLast");

    assert_eq!(
        paths(&result),
        ["Notes/Beta.md", "Notes/Alpha.md", "Notes/Gamma.md",],
        "the note with no rank comes last, descending as well"
    );
}

/// The descending sort really is descending for the rows that HAVE a value.
///
/// Without this, a sort that ignored its direction entirely would satisfy the
/// null-last assertion by accident.
#[test]
fn the_descending_sort_reverses_the_rows_that_have_values() {
    let vault = traps_vault();
    let ascending_result = query(&vault, "NullsFirst");
    let descending_result = query(&vault, "NullsLast");
    let ascending = paths(&ascending_result);
    let descending = paths(&descending_result);

    assert_eq!(ascending[0], "Notes/Alpha.md", "rank 1 first, ascending");
    assert_eq!(descending[0], "Notes/Beta.md", "rank 3 first, descending");
    // The two orders agree about where the null goes, and differ about
    // everything else.
    assert_eq!(ascending.last(), descending.last());
    assert_ne!(ascending, descending);
}

/// A boolean column is the ONE exception to "nulls sort last".
///
/// Obsidian's comparator checks the boolean branch BEFORE the null one, so a
/// null beside a boolean is compared as the boolean it folds into -- `false` --
/// and the direction applies to that comparison like any other. A note with
/// no checkbox therefore sorts with the unchecked ones, not at the bottom.
///
/// Two things follow from the comparator, and both are asserted here:
///
///   * A checkbox sorts `false` BEFORE `true` ascending, because Obsidian
///     compares booleans as -1 and 1 rather than as false and true.
///   * A missing checkbox ties with an unchecked one, because a null is not
///     truthy and so folds to the same -1. The tie is broken by the path, so
///     the two land next to each other rather than at the bottom.
///
/// This is the sort's order of tests, not a rule about booleans: it is the
/// only place where the null branch is not reached first.
#[test]
fn a_missing_checkbox_sorts_with_the_unchecked_ones() {
    let vault = traps_vault();
    let result = query(&vault, "Booleans");

    // `Alpha` is checked, `Beta` is unchecked, and `Gamma` has no `active` at
    // all -- so `Gamma` folds to false and ties with `Beta`.
    assert_eq!(
        paths(&result),
        ["Notes/Beta.md", "Notes/Gamma.md", "Notes/Alpha.md"],
        "unchecked first, the absent one beside it, and the checked one last"
    );
}

// ---------------------------------------------------------------------------
// The rules the traps share
// ---------------------------------------------------------------------------

/// The label rule the collision rests on.
///
/// Two columns share a header because the header is the DISPLAY NAME. The
/// label rule is in `src/labels.rs`; this pins the two halves the collision
/// needs: a configured `displayName` wins verbatim, and it is matched on the
/// CANONICAL id (a bare key is dead config).
#[test]
fn a_configured_display_name_is_the_header_and_verbatim() {
    let vault = traps_vault();
    let base = traps_base(&vault);
    let result = query(&vault, "Colliding");
    let columns = render_columns(&base, &result.view);

    let headers: Vec<&str> = columns.iter().map(|c| c.header.as_str()).collect();
    assert_eq!(headers, ["file name", "Status", "Status"]);
}

/// The formula the collision uses is a formula, and reads a note property.
///
/// `formula.status_alias` is `kind + "!"`, so its value is `note!` where
/// `note.status` is `active`. The two are different values under one header,
/// which is what makes the collision visible rather than theoretical.
#[test]
fn the_colliding_columns_hold_different_values() {
    let vault = traps_vault();
    let result = query(&vault, "Colliding");
    let row = result
        .rows
        .iter()
        .find(|row| row.path == "Notes/Alpha.md")
        .expect("the Alpha row");

    assert_eq!(
        row.values
            .get("note.status")
            .map(BasesValue::to_display_string),
        Some("active".to_string())
    );
    assert_eq!(
        row.values
            .get("formula.status_alias")
            .map(BasesValue::to_display_string),
        Some("note!".to_string())
    );
}

/// A null cell is a null cell, and a stringification of it is null.
///
/// The JSON surface and the stringification helper agree, which is what makes
/// "absent stays null" hold on every path that emits rows.
#[test]
fn a_null_cell_is_null_on_every_surface() {
    let vault = traps_vault();
    let result = query(&vault, "NullsFirst");
    let gamma = result
        .rows
        .iter()
        .find(|row| row.path == "Notes/Gamma.md")
        .expect("the Gamma row");
    let value = gamma.values.get("note.rank").expect("the rank cell");
    assert!(
        matches!(value, BasesValue::Null),
        "the rank cell of a note with no rank is null: {value:?}"
    );
    assert_eq!(stringified(Some(value)), None);
}

/// The canonical-id rule the columns depend on.
///
/// A column's ID is canonicalised, so `status` and `note.status` are the same
/// column and a base may write either. This is what lets the collision's two
/// columns be told apart at all.
#[test]
fn a_bare_property_id_is_the_same_column_as_its_canonical_spelling() {
    assert_eq!(canonical("status"), "note.status");
    assert_eq!(canonical("note.status"), "note.status");
}

// ---------------------------------------------------------------------------
// What the fixture is NOT
// ---------------------------------------------------------------------------

/// The fixture is a fixture, not a second oracle.
///
/// `test/vault` is the parity oracle: it is registered in Obsidian, the CLI
/// can be pointed at it, and it must stay byte-identical. A fixture is never
/// registered in Obsidian -- it is exercised by this suite and by the unit
/// suites, and it is free to grow.
#[test]
fn the_fixture_is_separate_from_the_oracle() {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("test")
        .join("fixtures")
        .join("cli-traps");
    let oracle = vault_dir();

    assert_ne!(fixture, oracle, "a fixture is never the oracle");
    assert!(
        !fixture.starts_with(&oracle),
        "a fixture is never inside the oracle"
    );
    // The oracle holds nine files; the fixture holds four. If they were ever
    // the same directory, this would fail.
    assert_eq!(count_files(&oracle), CORPUS_SIZE);
    assert_eq!(count_files(&fixture), 4);
}

/// The fixture's base declares the views the traps name.
///
/// A fixture whose view was renamed would make every assertion above name a
/// view that does not exist -- and `query` would panic with a message about
/// the missing view rather than about the trap, which is a worse failure.
#[test]
fn the_fixture_declares_a_view_for_each_trap() {
    let vault = traps_vault();
    let base = traps_base(&vault);
    let names: Vec<&str> = base.views.iter().map(|view| view.name.as_str()).collect();
    for expected in [
        "Limited",
        "LimitedByGroup",
        "Ordered",
        "Colliding",
        "Strings",
        "NullsFirst",
        "NullsLast",
        "Booleans",
    ] {
        assert!(
            names.contains(&expected),
            "the fixture lost the {expected} view"
        );
    }
}
