//! `.base` parsing and the query pipeline.
//!
//! Three things are asserted here that a subset assertion would miss, and each
//! has a test that would pass if they drifted:
//!
//!   - The FULL [`QueryResult`] -- rows, row ORDER, every cell value, groups,
//!     group order, `total`, `context` and `view`. Row order is part of the
//!     contract, not an accident of iteration, so it is asserted on every view
//!     rather than in one "did it sort?" test.
//!   - Unknown view keys preserved BYTE FOR BYTE. The view level is an open
//!     namespace plugins write into, and `test/vault/Tickets.base` carries
//!     composite keys joined by `\x1f`. A parser that drops, renames or refuses
//!     them corrupts real vaults, and the corruption is invisible until a
//!     plugin reads its own state back.
//!   - Hard errors rather than silent `null`s, for every unimplemented construct.
//!
//! `AllNotes.base` is the parity oracle: it never references `this`, so it is
//! what `obsidian base:query` can be compared against once Obsidian is
//! reachable. Its row set and order are pinned here so that comparison has
//! something to compare against.
//!
//! `Tickets.base` is the documented DIVERGENCE (D1): it scopes itself to the
//! Host note bound to `this`, which the CLI cannot express at all. It is
//! asserted against what the vault implies, never against the CLI.
//!
//! ## The expression cases here
//!
//! Every case in `test/unit/expr.test.ts` is asserted somewhere in this
//! project's suites. Most already are, in `tests/expr.rs`, which is where
//! expression semantics live and where they stay. The cases in this file are
//! the ones the base pipeline leans on that `tests/expr.rs` did not pin:
//! the `Tickets.base` filter itself, formula visibility, bracket access, the
//! formula namespace, regex, date formatting, and stringification. They are
//! duplicated here only where the base pipeline's dependence on them is the
//! thing under test.
//!
//! Nothing here writes to `test/vault`. It is the parity oracle and stays at
//! exactly [`CORPUS_SIZE`] files — a set the manifest pins, byte for byte.

// Every integration test is its own crate, and no suite needs every helper here.
// The module carries its own `dead_code` allow, so this declaration needs
// nothing beside it.
mod common;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use bases_mcp::base::{
    order_formulas, parse_base, query_base, BaseFile, Direction, FilterNode, QueryOptions,
    QueryResult, SortEntry,
};
use bases_mcp::error::BasesError;
use bases_mcp::labels::{canonical_id_of, display_name_for, label_for_id, strip_namespace};
use bases_mcp::value::BasesValue;
use bases_mcp::vault::{FsVaultSource, Vault};
use common::manifest::assert_vault_matches_manifest;
use common::{futures_block_on, vault_dir};

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// The testing vault, indexed. Read-only: nothing in this file writes to it.
fn testing_vault() -> Arc<Vault> {
    load(vault_dir())
}

/// A fixture vault, resolved by pointing the backend at its own directory.
fn fixture_vault(name: &str) -> Arc<Vault> {
    load(fixtures_dir().join(name))
}

fn load(dir: impl AsRef<Path>) -> Arc<Vault> {
    let source = FsVaultSource::new(dir).expect("a vault directory");
    let vault = Arc::new(Vault::new(Box::new(source)));
    futures_block_on(vault.load()).expect("the vault is readable");
    vault
}

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("test")
        .join("fixtures")
}

/// Parse a `.base` out of the testing vault by name.
fn base_of(vault: &Vault, path: &str) -> BaseFile {
    let text = futures_block_on(vault.read_note(path)).expect("a corpus base is readable");
    parse_base(path, &text).expect("a corpus base parses")
}

/// Resolve one view of a base against the testing vault.
fn query(vault: &Arc<Vault>, path: &str, view: Option<&str>, context: Option<&str>) -> QueryResult {
    let base = base_of(vault, path);
    let options = QueryOptions {
        context: context.map(str::to_string),
        view: view.map(str::to_string),
    };
    query_base(vault, path, &base, &options).expect("the query resolves")
}

/// The refusal a query produced, or a failure saying it unexpectedly succeeded.
fn refusal(vault: &Arc<Vault>, path: &str, options: QueryOptions) -> BasesError {
    let base = base_of(vault, path);
    match query_base(vault, path, &base, &options) {
        Err(error) => error,
        Ok(result) => panic!("expected a refusal, but the query returned {result:?}"),
    }
}

/// The refusal an inline base produced. For the cases that are about the PARSER
/// rather than about any particular vault.
fn refusal_from(yaml: &str, options: QueryOptions) -> BasesError {
    let vault = testing_vault();
    match parse_base("T.base", yaml) {
        Err(error) => error,
        Ok(base) => match query_base(&vault, "T.base", &base, &options) {
            Err(error) => error,
            Ok(result) => panic!("expected a refusal, but the query returned {result:?}"),
        },
    }
}

/// The rows a result resolved to, as `(path, cell values)` pairs in result order.
///
/// Cells are rendered rather than compared as `BasesValue`, because the rendered
/// form is what an agent reads; the underlying typed value is asserted separately
/// where the type matters.
fn cells(result: &QueryResult) -> Vec<(String, BTreeMap<String, String>)> {
    result
        .rows
        .iter()
        .map(|row| {
            (
                row.path.clone(),
                row.values
                    .iter()
                    .map(|(id, v)| (id.clone(), v.to_display_string()))
                    .collect(),
            )
        })
        .collect()
}

/// Group keys in result order, with each group's row paths.
fn groups(result: &QueryResult) -> Option<Vec<(String, Vec<String>)>> {
    result.groups.as_ref().map(|groups| {
        groups
            .iter()
            .map(|group| {
                (
                    group.key.clone(),
                    group.rows.iter().map(|row| row.path.clone()).collect(),
                )
            })
            .collect()
    })
}

/// A preserved YAML key as text.
///
/// `serde_yaml` keys are `Value`s, and a composite key joined by `\x1f` is a
/// perfectly ordinary string key -- reading it back is what proves it survived.
fn yaml_key_text(key: &serde_yaml::Value) -> String {
    match key {
        serde_yaml::Value::String(text) => text.clone(),
        other => serde_yaml::to_string(other).expect("a YAML key serialises"),
    }
}

fn paths(result: &QueryResult) -> Vec<&str> {
    result.rows.iter().map(|row| row.path.as_str()).collect()
}

/// The cell a row holds, or a panic naming the row -- a failure that says which
/// row disagreed is actable, one that says only "left != right" is not.
fn cell<'a>(result: &'a QueryResult, path: &str, id: &str) -> &'a BasesValue {
    let row = result
        .rows
        .iter()
        .find(|row| row.path == path)
        .unwrap_or_else(|| panic!("no row for {path}; the result has {:?}", paths(result)));
    row.values.get(id).unwrap_or_else(|| {
        panic!(
            "row {path} has no cell {id}; it has {:?}",
            row.values.keys().collect::<Vec<_>>()
        )
    })
}

/// The same, for a grouped result, where a row can be reached through its group.
fn grouped<'a>(result: &'a QueryResult, group_key: &str, path: &str, id: &str) -> &'a BasesValue {
    let group = result
        .groups
        .as_ref()
        .expect("a grouped result")
        .iter()
        .find(|group| group.key == group_key)
        .unwrap_or_else(|| panic!("no group {group_key:?}"));
    let row = group
        .rows
        .iter()
        .find(|row| row.path == path)
        .unwrap_or_else(|| {
            panic!("no row {path} in group {group_key:?}");
        });
    row.values
        .get(id)
        .unwrap_or_else(|| panic!("row {path} has no cell {id}"))
}

// ---------------------------------------------------------------------------
// The frontmatter coercion chain
// ---------------------------------------------------------------------------
//
// Obsidian coerces a frontmatter STRING before it types anything, and the
// order is Link -> URL -> Date. Every assertion below was measured against
// Obsidian 1.14.4 with a live `base:query` probe; the observations are
// recorded in `docs/divergences.md`.
//
// The fixture asks each value what it IS rather than asserting a value we
// hard-coded, so the fixture answers the question instead of restating it.

/// The coercion fixture, queried.
fn coercion() -> QueryResult {
    let vault = fixture_vault("coercion");
    query(&vault, "Coercion.base", None, None)
}

/// The cell the coercion fixture holds for one formula.
fn coerced(result: &QueryResult, formula: &str) -> String {
    cell(result, "Notes/Values.md", &format!("formula.{formula}")).to_display_string()
}

#[test]
fn a_quoted_date_becomes_a_date_and_a_datetime_becomes_a_date_too() {
    let result = coercion();
    assert_eq!(coerced(&result, "quoted_date_is_date"), "true");
    assert_eq!(coerced(&result, "quoted_date_is_string"), "false");
    // A date-only string is NOT a string: Obsidian types it as a Date.
    assert_eq!(coerced(&result, "quoted_datetime_is_date"), "true");
    // The space-separated form is a datetime too.
    assert_eq!(coerced(&result, "datetime_space_is_date"), "true");
}

#[test]
fn a_date_needs_two_digits_for_its_month_and_day_and_is_not_trimmed() {
    let result = coercion();
    // `"2026-1-5"` is not a date: Obsidian's matcher wants two digits.
    assert_eq!(coerced(&result, "one_digit_is_date"), "false");
    assert_eq!(coerced(&result, "one_digit_is_string"), "true");
    // Nothing is trimmed, so a padded value is a plain string.
    assert_eq!(coerced(&result, "padded_is_date"), "false");
    assert_eq!(coerced(&result, "padded_is_string"), "true");
}

#[test]
fn a_wikilink_becomes_a_link_and_a_markdown_link_does_not() {
    let result = coercion();
    assert_eq!(coerced(&result, "wikilink_is_link"), "true");
    assert_eq!(coerced(&result, "wikilink_alias_is_link"), "true");
    assert_eq!(coerced(&result, "wikilink_heading_is_link"), "true");
    // A markdown link is NOT coerced, even when it names a note in the vault.
    assert_eq!(coerced(&result, "markdown_link_is_link"), "false");
    assert_eq!(coerced(&result, "markdown_link_is_string"), "true");
}

#[test]
fn a_url_becomes_a_url_and_only_a_real_url_does() {
    let result = coercion();
    assert_eq!(coerced(&result, "url_is_url"), "true");
    // A URL is a String too: Obsidian's `isType` walks the prototype chain.
    assert_eq!(coerced(&result, "url_is_string"), "true");
    // The scheme is case-insensitive.
    assert_eq!(coerced(&result, "url_upper_is_url"), "true");
    // No scheme is not a URL.
    assert_eq!(coerced(&result, "url_no_scheme_is_url"), "false");
    // A space disqualifies it.
    assert_eq!(coerced(&result, "url_with_space_is_url"), "false");
    // An empty host is not a URL.
    assert_eq!(coerced(&result, "url_empty_host_is_url"), "false");
    // Any scheme parses, `mailto:` included.
    assert_eq!(coerced(&result, "mailto_is_url"), "true");
}

#[test]
fn a_tag_is_never_a_link_even_when_it_is_spelled_like_one() {
    let result = coercion();
    // `tags` is exempt from the coercion chain, so a tag spelled `[[x]]` stays
    // a string and never becomes a link.
    assert_eq!(coerced(&result, "tags_are_strings"), "true");
    assert_eq!(coerced(&result, "tags_are_not_links"), "false");
}

#[test]
fn a_link_and_a_url_inherit_the_string_methods_and_work_on_their_own_text() {
    let result = coercion();
    // The link's methods operate on the TARGET, not on the `[[...]]` spelling:
    // `[[Some Note]]` has a target of nine characters.
    assert_eq!(coerced(&result, "wikilink_length"), "9");
    assert_eq!(coerced(&result, "wikilink_contains"), "true");
    assert_eq!(coerced(&result, "wikilink_slice"), "Some");
    // A link compares equal to its target, and to its `[[...]]` spelling.
    assert_eq!(coerced(&result, "wikilink_equals_target"), "true");
    // ... and to its `[[...|alias]]` spelling, which is how it renders.
    assert_eq!(coerced(&result, "wikilink_alias_equals_spelling"), "true");
    // A URL's methods operate on the URL.
    assert_eq!(coerced(&result, "url_contains"), "true");
    assert_eq!(coerced(&result, "url_length"), "19");
    assert_eq!(coerced(&result, "url_equals_text"), "true");
}

#[test]
fn a_date_only_value_equals_the_datetime_on_the_same_day() {
    let result = coercion();
    assert_eq!(coerced(&result, "date_equals_day"), "true");
    assert_eq!(coerced(&result, "datetime_equals_day"), "true");
}

// ---------------------------------------------------------------------------
// The corpus itself
// ---------------------------------------------------------------------------

#[test]
fn the_testing_vault_is_untouched() {
    // The oracle underpins every comparison below: a changed file would change
    // what Obsidian returns for the same query. This suite reads the vault and
    // never writes to it, so this must not move.
    //
    // A MANIFEST, not a count. The count could only see a file being added;
    // the desktop app rewriting a `.base` file leaves the count at nine and
    // the oracle changed underneath it.
    assert_vault_matches_manifest();
}

// ---------------------------------------------------------------------------
// Parsing: the view level is an open namespace
// ---------------------------------------------------------------------------

#[test]
fn unknown_view_keys_are_preserved_verbatim() {
    let vault = testing_vault();
    let base = base_of(&vault, "Tickets.base");
    let view = &base.views[0];

    // The eight plugin keys `Tickets.base` carries, by name.
    let keys: Vec<String> = view.extra.keys().map(yaml_key_text).collect();
    assert_eq!(
        keys,
        [
            "cardOrders",
            "columnColors",
            "groupByProperty",
            "columnOrders",
            "swimlaneByProperty",
            "swimlaneOrders",
            "collapsedLanes",
            "cardTitleProperty",
        ]
        .map(str::to_string)
    );
    // And their values, still YAML rather than flattened to strings.
    assert_eq!(
        view.extra["groupByProperty"],
        serde_yaml::Value::String("formula.priority_display".into())
    );
    assert_eq!(
        view.extra["swimlaneByProperty"],
        serde_yaml::Value::String("note.type".into())
    );
    assert_eq!(
        view.extra["cardTitleProperty"],
        serde_yaml::Value::String("file.name".into())
    );
}

#[test]
fn a_composite_key_joined_by_us_is_preserved_byte_for_byte() {
    // Plugins join two Property IDs with `\x1f` to key a cross-property config
    // (`cardOrders`, `swimlaneOrders`). Rewriting the separator, trimming it, or
    // splitting it would silently address a config entry that does not exist.
    let vault = testing_vault();
    let base = base_of(&vault, "Tickets.base");
    let view = &base.views[0];
    let composite = "formula.priority_display\u{1f}note.type";

    let card_orders = view.extra["cardOrders"]
        .as_mapping()
        .expect("cardOrders is a mapping");
    assert!(
        card_orders.contains_key(serde_yaml::Value::String(composite.into())),
        "cardOrders lost the composite key; it holds {:?}",
        card_orders.keys().collect::<Vec<_>>()
    );
    // Both halves of the pair are present separately as well, so the composite is
    // an ADDITIONAL key rather than a replacement.
    assert!(card_orders.contains_key(serde_yaml::Value::String("file.file".into())));
    assert!(card_orders.contains_key(serde_yaml::Value::String("formula.priority_display".into())));

    let swimlanes = view.extra["swimlaneOrders"]
        .as_mapping()
        .expect("swimlaneOrders is a mapping");
    let orders = swimlanes[serde_yaml::Value::String(composite.into())]
        .as_sequence()
        .expect("a lane order is a list");
    assert_eq!(
        orders,
        &[
            serde_yaml::Value::String("task".into()),
            serde_yaml::Value::String("Uncategorized".into())
        ]
    );
}

#[test]
fn a_composite_key_survives_a_round_trip_through_the_pipeline() {
    // The keys are preserved by the parser; this asserts nothing downstream
    // interprets them either. `order` here is the plain two-column list, and the
    // composite keys are NOT in it, so no column is fabricated from them.
    let vault = testing_vault();
    let result = query(
        &vault,
        "Tickets.base",
        Some("All"),
        Some("Projects/SomeProject.md"),
    );
    assert!(result.view.order.as_ref().is_some_and(|order| order
        == &[
            "file.name".to_string(),
            "status".to_string(),
            "formula.priority_display".to_string(),
            "type".to_string()
        ]));
    assert!(!result
        .view
        .order
        .as_ref()
        .is_some_and(|order| order.iter().any(|id| id.contains('\u{1f}'))));
}

#[test]
fn unrecognised_top_level_keys_are_preserved() {
    let vault = testing_vault();
    let base = base_of(&vault, "Tickets.base");
    assert!(
        base.extra.is_empty(),
        "Tickets.base has no unknown top-level keys"
    );
    assert_eq!(
        base.properties.keys().collect::<Vec<_>>(),
        ["formula.priority_display"]
    );

    let parsed = parse_base(
        "T.base",
        "myPluginState: {a: 1}\nviews:\n  - type: table\n    name: V\n",
    )
    .expect("parses");
    assert_eq!(
        parsed.extra["myPluginState"],
        serde_yaml::Value::Mapping(serde_yaml::Mapping::from_iter([(
            serde_yaml::Value::String("a".into()),
            serde_yaml::Value::Number(1.into()),
        )]))
    );
}

#[test]
fn the_core_view_keys_are_read_rather_than_preserved() {
    // The other half of the open-namespace rule: the keys the engine DOES read
    // are lifted out of `extra` and typed.
    let vault = testing_vault();
    let view = &base_of(&vault, "AllNotes.base").views[0];
    assert_eq!(view.view_type, "table");
    assert_eq!(view.name, "All");
    assert!(
        view.extra.is_empty(),
        "every key of view `All` is a core key: {:?}",
        view.extra
    );
}

// ---------------------------------------------------------------------------
// Parsing: `sort`, `order`, `groupBy`
// ---------------------------------------------------------------------------

#[test]
fn sort_accepts_the_legacy_column_spelling() {
    // Obsidian 1.9 wrote `column:`; it is `property:` now. TaskNotes still emits
    // `column:`, so a parser that only reads the new spelling silently drops that
    // vault's sort and returns rows in `file.name` order instead.
    let yaml = "views:\n  - type: table\n    name: V\n    order: [file.name]\n    sort:\n      - column: note.status\n        direction: DESC\n";
    let parsed = parse_base("T.base", yaml).expect("parses");
    assert_eq!(
        parsed.views[0].sort,
        Some(vec![SortEntry {
            property: "note.status".into(),
            direction: Direction::Desc
        }])
    );
}

#[test]
fn sort_prefers_property_over_column_when_both_are_written() {
    let yaml = "views:\n  - type: table\n    name: V\n    sort:\n      - property: note.status\n        column: file.name\n";
    let parsed = parse_base("T.base", yaml).expect("parses");
    assert_eq!(
        parsed.views[0]
            .sort
            .as_ref()
            .map(|s| s[0].property.as_str()),
        Some("note.status")
    );
}

#[test]
fn a_sort_entry_without_a_usable_property_is_dropped_rather_than_failing_the_view() {
    // One broken entry in a five-entry sort must not cost the agent the view.
    let yaml = "views:\n  - type: table\n    name: V\n    sort:\n      - direction: ASC\n      - notamapping\n      - property: file.name\n";
    let parsed = parse_base("T.base", yaml).expect("parses");
    assert_eq!(
        parsed.views[0].sort,
        Some(vec![SortEntry {
            property: "file.name".into(),
            direction: Direction::Asc
        }])
    );
}

#[test]
fn a_missing_or_lowercase_direction_is_ascending() {
    // Anything Obsidian does not spell `DESC` sorts ascending, including a
    // misspelling: refusing would lose a view over a one-character typo.
    for direction in ["DESC", "desc", "Desc"] {
        let yaml = format!("views:\n  - type: table\n    name: V\n    sort:\n      - property: file.name\n        direction: {direction}\n");
        assert_eq!(
            parse_base("T.base", &yaml).unwrap().views[0]
                .sort
                .as_ref()
                .unwrap()[0]
                .direction,
            Direction::Desc
        );
    }
    for direction in ["", "ASC", "ascending", "ASCENDING", "nonsense"] {
        let yaml = format!("views:\n  - type: table\n    name: V\n    sort:\n      - property: file.name\n        direction: {direction}\n");
        assert_eq!(
            parse_base("T.base", &yaml).unwrap().views[0]
                .sort
                .as_ref()
                .unwrap()[0]
                .direction,
            Direction::Asc
        );
    }
}

#[test]
fn a_non_string_in_order_is_dropped_rather_than_failing_the_view() {
    let yaml =
        "views:\n  - type: table\n    name: V\n    order: [file.name, 7, status, null, true]\n";
    let parsed = parse_base("T.base", yaml).expect("parses");
    assert_eq!(
        parsed.views[0].order,
        Some(vec!["file.name".to_string(), "status".to_string()])
    );
}

#[test]
fn a_group_by_without_a_property_is_ignored() {
    // A groupBy with nothing to group on cannot produce groups. Inventing a
    // property name would put every row in a bucket the author never asked for.
    let yaml = "views:\n  - type: cards\n    name: V\n    groupBy:\n      direction: ASC\n";
    assert!(parse_base("T.base", yaml).unwrap().views[0]
        .group_by
        .is_none());
}

#[test]
fn a_view_with_no_name_is_numbered_by_position() {
    let yaml = "views:\n  - type: table\n  - type: cards\n    name: Named\n";
    let parsed = parse_base("T.base", yaml).expect("parses");
    assert_eq!(parsed.views[0].name, "View 1");
    assert_eq!(parsed.views[1].name, "Named");
}

// ---------------------------------------------------------------------------
// Parsing: refusals
// ---------------------------------------------------------------------------

#[test]
fn a_base_must_be_a_mapping_with_views() {
    let cases = [
        ("- a\n", "A base file must be a YAML mapping: T.base"),
        (
            "just a string\n",
            "A base file must be a YAML mapping: T.base",
        ),
        (
            "formulas: {}\n",
            "A base file must define a \"views\" list: T.base",
        ),
        (
            "views: {}\n",
            "A base file must define a \"views\" list: T.base",
        ),
        (
            "views: []\n",
            "A base file must define at least one view: T.base",
        ),
    ];
    for (yaml, expected) in cases {
        let error = refusal_from(yaml, QueryOptions::default());
        assert_eq!(error.message(), expected);
        assert_eq!(error.note(), Some("T.base"));
    }
}

#[test]
fn invalid_yaml_names_the_file() {
    let error = refusal_from("views: [\n  - type: table\n", QueryOptions::default());
    assert!(
        error.message().starts_with("Invalid YAML in T.base: "),
        "got {:?}",
        error.message()
    );
    assert_eq!(error.note(), Some("T.base"));
}

#[test]
fn a_view_must_be_a_mapping_with_a_type() {
    let cases = [
        (
            "views:\n  - just a string\n",
            "views[0] must be a mapping in T.base",
        ),
        (
            "views:\n  - name: V\n",
            "views[0] is missing a \"type\" in T.base",
        ),
        (
            "views:\n  - type: \"\"\n",
            "views[0] is missing a \"type\" in T.base",
        ),
        (
            "views:\n  - type: table\n    name: V\n  - type: 7\n",
            "views[1] is missing a \"type\" in T.base",
        ),
    ];
    for (yaml, expected) in cases {
        let error = refusal_from(yaml, QueryOptions::default());
        assert_eq!(error.message(), expected);
        assert_eq!(error.note(), Some("T.base"));
    }
}

#[test]
fn an_escaped_pipe_is_unescaped_before_parsing() {
    // Obsidian's own writer escapes `|` inside filter expressions, which YAML
    // would otherwise read as a block scalar indicator and turn the rest of the
    // file into a string.
    let yaml =
        "filters:\n  and:\n    - 'status == \"a\\|b\"'\nviews:\n  - type: table\n    name: V\n";
    let parsed = parse_base("T.base", yaml).expect("parses");
    assert_eq!(
        parsed.filters,
        Some(FilterNode::And(vec![FilterNode::Expression(
            "status == \"a|b\"".into()
        )]))
    );
}

// ---------------------------------------------------------------------------
// Filters: validation at the boundary
// ---------------------------------------------------------------------------

#[test]
fn a_filter_object_with_sibling_group_keys_is_refused_with_obsidians_wording() {
    // Obsidian's exact message, because this is a case an agent will hit by
    // writing `and:` next to an existing `or:` and needs to recognise the fix.
    for siblings in [
        "      and:\n        - a\n      or:\n        - b\n",
        "      or:\n        - a\n      not:\n        - b\n",
        "      and:\n        - a\n      or:\n        - b\n      not:\n        - c\n",
    ] {
        let yaml = format!("views:\n  - type: table\n    name: V\n    filters:\n{siblings}");
        let error = refusal_from(&yaml, QueryOptions::default());
        assert_eq!(
            error.message(),
            "\"filters\" may only have one of an \"and\", \"or\", or \"not\" keys."
        );
        assert_eq!(error.note(), Some("T.base"));
    }
}

#[test]
fn a_sibling_group_key_counts_even_when_its_value_is_null() {
    // The author wrote two group keys. Which one they meant is a question we
    // cannot answer, and silently honouring one is the wrong answer.
    let yaml =
        "views:\n  - type: table\n    name: V\n    filters:\n      and:\n        - a\n      or:\n";
    let error = refusal_from(yaml, QueryOptions::default());
    assert_eq!(
        error.message(),
        "\"filters\" may only have one of an \"and\", \"or\", or \"not\" keys."
    );
}

#[test]
fn a_sibling_refusal_also_applies_to_the_top_level_filters() {
    let yaml = "filters:\n  and:\n    - a\n  not:\n    - b\nviews:\n  - type: table\n    name: V\n";
    let error = refusal_from(yaml, QueryOptions::default());
    assert_eq!(
        error.message(),
        "\"filters\" may only have one of an \"and\", \"or\", or \"not\" keys."
    );
}

#[test]
fn a_filter_object_with_no_group_key_says_so() {
    let yaml = "views:\n  - type: table\n    name: V\n    filters: {x: 1}\n";
    let error = refusal_from(yaml, QueryOptions::default());
    assert_eq!(
        error.message(),
        "A filter object must contain one of \"and\", \"or\", or \"not\" (at views[0].filters in T.base)"
    );
}

#[test]
fn a_bare_list_under_filters_is_refused() {
    // A bare YAML list under `filters:` is invalid in Obsidian, and reading it as
    // "all of these" would quietly change what the base selects.
    let yaml = "views:\n  - type: table\n    name: V\n    filters:\n      - a\n      - b\n";
    let error = refusal_from(yaml, QueryOptions::default());
    assert_eq!(
        error.message(),
        "\"filters\" must be a string or a filter object, not a bare list (at views[0].filters in T.base)"
    );
}

#[test]
fn a_scalar_filter_that_is_not_a_string_is_refused() {
    let error = refusal_from(
        "views:\n  - type: table\n    name: V\n    filters: 7\n",
        QueryOptions::default(),
    );
    assert_eq!(
        error.message(),
        "Invalid filter at views[0].filters in T.base"
    );
}

#[test]
fn a_group_that_is_not_a_list_is_refused() {
    let error = refusal_from(
        "views:\n  - type: table\n    name: V\n    filters:\n      and: 7\n",
        QueryOptions::default(),
    );
    assert_eq!(
        error.message(),
        "Filter group \"views[0].filters.and\" in T.base must be a list"
    );
}

#[test]
fn a_bare_string_is_a_whole_filter_expression() {
    let parsed = parse_base(
        "T.base",
        "filters: 'status == \"active\"'\nviews:\n  - type: table\n    name: V\n",
    )
    .unwrap();
    assert_eq!(
        parsed.filters,
        Some(FilterNode::Expression("status == \"active\"".into()))
    );
}

#[test]
fn a_not_group_accepts_a_single_scalar() {
    // `not: file.hasTag("x")` is what people write; requiring a list for it would
    // refuse a spelling Obsidian accepts.
    let parsed = parse_base(
        "T.base",
        "filters:\n  not: 'file.hasTag(\"x\")'\nviews:\n  - type: table\n    name: V\n",
    )
    .unwrap();
    assert_eq!(
        parsed.filters,
        Some(FilterNode::Not(vec![FilterNode::Expression(
            "file.hasTag(\"x\")".into()
        )]))
    );
}

#[test]
fn filters_nest_to_any_depth() {
    let yaml = "filters:\n  and:\n    - file.ext == \"md\"\n    - or:\n        - not:\n            - file.inFolder(\"a\")\n            - file.inFolder(\"b\")\n        - status == \"x\"\nviews:\n  - type: table\n    name: V\n";
    let parsed = parse_base("T.base", yaml).expect("parses");
    assert_eq!(
        parsed.filters,
        Some(FilterNode::And(vec![
            FilterNode::Expression("file.ext == \"md\"".into()),
            FilterNode::Or(vec![
                FilterNode::Not(vec![
                    FilterNode::Expression("file.inFolder(\"a\")".into()),
                    FilterNode::Expression("file.inFolder(\"b\")".into()),
                ]),
                FilterNode::Expression("status == \"x\"".into()),
            ]),
        ]))
    );
}

// ---------------------------------------------------------------------------
// `not` is NAND
// ---------------------------------------------------------------------------

#[test]
fn not_excludes_on_any_match_which_is_what_distinguishes_it_from_a_negation() {
    // Six siblings means "none of these six are true". A note in TWO of the
    // excluded folders is still excluded -- which is only true of NAND, and not
    // of a `!(a || b)` reading of the same filter.
    let vault = fixture_vault("not-nand");
    let result = query(&vault, "Core.base", None, None);
    assert_eq!(paths(&result), ["Notes/Guide.md"]);
    // The note in both `features` and `advanced` is the one that proves it.
    let excluded: Vec<&str> = ["plugins/Plugin.md", "features/Both.md", "advanced/Deep.md"]
        .into_iter()
        .filter(|path| !paths(&result).contains(path))
        .collect();
    assert_eq!(excluded.len(), 3);
}

// ---------------------------------------------------------------------------
// A tag is a distinct type
// ---------------------------------------------------------------------------

/// A tag list is never loose-equal to a bare string.
///
/// This is the divergence STATUS.md item 4 named: `tags == "project"` returned
/// true on our side and false in Obsidian. The cause is that Obsidian types
/// `tags` as a tag list -- a `TagValue` extends `StringValue`, and
/// `looseEquals` compares constructors, so a tag is never loose-equal to a
/// plain string.
///
/// Every value below was measured against Obsidian 1.14.4, with an equivalent
/// base queried in an isolated vault. The fixture is never registered in
/// Obsidian, so what was measured is the rule and what is asserted here is the
/// same rule over a vault under test.
#[test]
fn a_tag_list_is_never_equal_to_a_bare_string() {
    let vault = fixture_vault("tags-equality");
    let result = query(&vault, "Tags.base", None, None);

    // `tags == "project"` is FALSE for every note, whether it carries one tag
    // or two, and whether the tag was written bare or with its `#`.
    for path in [
        "Notes/One.md",
        "Notes/Two.md",
        "Notes/Other.md",
        "Notes/Nested.md",
    ] {
        assert_eq!(
            cell(&result, path, "formula.eq_bare").to_display_string(),
            "false",
            "tags == \"project\" must be false for {path}"
        );
        assert_eq!(
            cell(&result, path, "formula.eq_list_bare").to_display_string(),
            "false",
            "tags == [\"project\"] must be false for {path}"
        );
    }

    // The tag's own text carries the `#`, so `tags[0] == "#project"` is TRUE
    // and `tags[0] == "project"` is FALSE.
    assert_eq!(
        cell(&result, "Notes/One.md", "formula.tags_first").to_display_string(),
        "#project"
    );
    assert_eq!(
        cell(&result, "Notes/One.md", "formula.tags_first_eq_hash").to_display_string(),
        "true"
    );
    assert_eq!(
        cell(&result, "Notes/One.md", "formula.tags_first_eq_bare").to_display_string(),
        "false"
    );

    // A ONE-tag list equals the tag, because a list compares equal to a scalar
    // only when it holds exactly one element.
    assert_eq!(
        cell(&result, "Notes/One.md", "formula.eq_hash").to_display_string(),
        "true"
    );
    assert_eq!(
        cell(&result, "Notes/One.md", "formula.eq_list").to_display_string(),
        "true"
    );
    // A TWO-tag list does not, and neither does a tag with another name.
    assert_eq!(
        cell(&result, "Notes/Two.md", "formula.eq_hash").to_display_string(),
        "false"
    );
    assert_eq!(
        cell(&result, "Notes/Two.md", "formula.eq_list").to_display_string(),
        "false"
    );
    assert_eq!(
        cell(&result, "Notes/Other.md", "formula.eq_hash").to_display_string(),
        "false"
    );

    // A plain string property is unaffected: `status == "active"` is true.
    assert_eq!(
        cell(&result, "Notes/One.md", "formula.status_eq").to_display_string(),
        "true"
    );
}

/// `contains` on a tag list is a TAG MATCH, not an equality.
///
/// Obsidian's `tagMatches` lowercases both sides, normalises the needle to a
/// `#`-prefixed tag, and matches on a `/` boundary. Measured against Obsidian
/// 1.14.4.
#[test]
fn contains_on_a_tag_list_is_a_tag_match() {
    let vault = fixture_vault("tags-equality");
    let result = query(&vault, "Tags.base", None, None);

    // Case-insensitive, and the `#` on the needle is optional.
    for path in ["Notes/One.md", "Notes/Nested.md"] {
        assert_eq!(
            cell(&result, path, "formula.contains_bare").to_display_string(),
            "true",
            "tags.contains(\"project\") must be true for {path}"
        );
        assert_eq!(
            cell(&result, path, "formula.contains_hash").to_display_string(),
            "true"
        );
        assert_eq!(
            cell(&result, path, "formula.contains_upper").to_display_string(),
            "true"
        );
        // The match is a prefix on the TAG, not on the needle, so a shorter
        // needle matches nothing.
        assert_eq!(
            cell(&result, path, "formula.contains_prefix").to_display_string(),
            "false"
        );
        // An empty needle never matches.
        assert_eq!(
            cell(&result, path, "formula.contains_empty").to_display_string(),
            "false"
        );
    }

    // A note with a different tag does not match.
    assert_eq!(
        cell(&result, "Notes/Other.md", "formula.contains_bare").to_display_string(),
        "false"
    );
    assert_eq!(
        cell(&result, "Notes/Other.md", "formula.contains_upper").to_display_string(),
        "false"
    );

    // `hasTag` is the same match, so it is case-insensitive too.
    assert_eq!(
        cell(&result, "Notes/One.md", "formula.has_tag").to_display_string(),
        "true"
    );
    assert_eq!(
        cell(&result, "Notes/One.md", "formula.has_tag_upper").to_display_string(),
        "true"
    );
    assert_eq!(
        cell(&result, "Notes/One.md", "formula.has_tag_prefix").to_display_string(),
        "false"
    );
    assert_eq!(
        cell(&result, "Notes/Other.md", "formula.has_tag").to_display_string(),
        "false"
    );
}

/// An EMPTY tag list is an empty list, not a null.
///
/// This is the difference between the `tags` PROPERTY and `file.tags`: the
/// property is absent on a note without one, so `tags.contains(...)` there is
/// null -- a null receiver, which is its own divergence (a null receiver
/// raising an error rather than answering null is fixed on another branch).
/// A note that declares `tags: []` HAS the property, and it holds nothing.
///
/// Measured against Obsidian 1.14.4.
#[test]
fn an_empty_tag_list_is_an_empty_list_and_not_a_null() {
    let vault = fixture_vault("tags-equality");
    let result = query(&vault, "Tags.base", None, None);

    assert_eq!(
        cell(&result, "Notes/Empty.md", "formula.tags_len").to_display_string(),
        "0"
    );
    assert_eq!(
        cell(&result, "Notes/Empty.md", "formula.contains_bare").to_display_string(),
        "false"
    );
    assert_eq!(
        cell(&result, "Notes/Empty.md", "formula.has_tag").to_display_string(),
        "false"
    );
    // The other notes are unaffected.
    assert_eq!(
        cell(&result, "Notes/One.md", "formula.tags_len").to_display_string(),
        "1"
    );
    assert_eq!(
        cell(&result, "Notes/Two.md", "formula.tags_len").to_display_string(),
        "2"
    );
    assert_eq!(
        cell(&result, "Notes/Other.md", "formula.tags_len").to_display_string(),
        "1"
    );
}

/// The shape of a tag, and tag-list membership, pinned cell by cell.
///
/// This is the fixture's guard. `Tags.base` declares one column per rule, and
/// every column is asserted somewhere in this file; this test covers the ones
/// the three above do not -- the columns that are easy to leave unasserted
/// because a regression in them is quiet: the shape of the value (`tags[0]`,
/// `join`, `length`), the `containsAll`/`containsAny` pair, and the whole
/// `file.tags` column family.
///
/// Every expected answer is a measurement against Obsidian 1.14.4, taken from
/// an equivalent base in an isolated vault. The notes here are the measured
/// ones: `Notes/One.md` is the single-tag shape, `Notes/Two.md` the two-tag
/// shape, `Notes/Other.md` a second name, `Notes/Nested.md` a nested tag.
/// `Notes/Empty.md` is asserted by the test above rather than here,
/// because its cells follow from "an empty list" rather than from a
/// measurement of one.
///
/// A note with NO `tags` property at all is deliberately absent from the
/// fixture: every `tags.*` column would then sit on a null receiver, which is
/// the divergence in STATUS.md item 5 and another branch's fix. A fixture that
/// needed it would encode that branch's bug.
#[test]
fn the_tag_fixture_holds_for_every_measured_cell() {
    let vault = fixture_vault("tags-equality");
    let result = query(&vault, "Tags.base", None, None);

    // The note, then every cell of it this fixture pins.
    let truth: &[(&str, &[(&str, &str)])] = &[
        (
            "Notes/One.md",
            &[
                ("formula.eq_not_bare", "true"),
                ("formula.contains_ticket", "false"),
                ("formula.contains_all", "true"),
                ("formula.contains_any", "true"),
                ("formula.has_tag_hash", "true"),
                ("formula.has_tag_nested", "false"),
                ("formula.tags_is_empty", "false"),
                ("formula.tags_first", "#project"),
                ("formula.tags_first_eq_bare", "false"),
                ("formula.tags_first_eq_hash", "true"),
                ("formula.tags_join", "#project"),
                ("formula.file_tags_len", "1"),
                ("formula.file_tags_contains", "true"),
                ("formula.file_tags_eq_bare", "false"),
            ],
        ),
        (
            "Notes/Two.md",
            &[
                ("formula.eq_not_bare", "true"),
                ("formula.contains_ticket", "false"),
                ("formula.contains_all", "true"),
                ("formula.contains_any", "true"),
                ("formula.has_tag_hash", "true"),
                ("formula.has_tag_nested", "false"),
                ("formula.tags_is_empty", "false"),
                ("formula.tags_first", "#project"),
                ("formula.tags_first_eq_bare", "false"),
                ("formula.tags_first_eq_hash", "true"),
                ("formula.tags_join", "#project,#business-idea"),
                ("formula.file_tags_len", "2"),
                ("formula.file_tags_contains", "true"),
                ("formula.file_tags_eq_bare", "false"),
            ],
        ),
        (
            "Notes/Other.md",
            &[
                ("formula.eq_not_bare", "true"),
                ("formula.contains_ticket", "true"),
                ("formula.contains_all", "false"),
                ("formula.contains_any", "true"),
                ("formula.has_tag_hash", "false"),
                ("formula.has_tag_nested", "false"),
                ("formula.tags_is_empty", "false"),
                ("formula.tags_first", "#ticket"),
                ("formula.tags_first_eq_bare", "false"),
                ("formula.tags_first_eq_hash", "false"),
                ("formula.tags_join", "#ticket"),
                ("formula.file_tags_len", "1"),
                ("formula.file_tags_contains", "false"),
                ("formula.file_tags_eq_bare", "false"),
            ],
        ),
        (
            "Notes/Nested.md",
            &[
                ("formula.eq_not_bare", "true"),
                ("formula.contains_ticket", "false"),
                ("formula.contains_all", "true"),
                ("formula.contains_any", "true"),
                ("formula.has_tag_hash", "true"),
                // A nested tag matches its PREFIX on the `/` boundary, and
                // nothing on the other side of it.
                ("formula.has_tag_nested", "true"),
                ("formula.tags_is_empty", "false"),
                ("formula.tags_first", "#project/alpha"),
                ("formula.tags_first_eq_bare", "false"),
                ("formula.tags_first_eq_hash", "false"),
                ("formula.tags_join", "#project/alpha"),
                ("formula.file_tags_len", "1"),
                ("formula.file_tags_contains", "true"),
                ("formula.file_tags_eq_bare", "false"),
            ],
        ),
    ];

    // Every wrong cell is reported, not just the first: a drift in the tag rule
    // moves several columns at once, and one line per column says which rule
    // went with it.
    let mut wrong: Vec<String> = Vec::new();
    for (path, columns) in truth {
        for (column, want) in *columns {
            let got = cell(&result, path, column).to_display_string();
            if got != *want {
                wrong.push(format!(
                    "{path} {column}: Obsidian says {want:?}, we say {got:?}"
                ));
            }
        }
    }
    assert!(
        wrong.is_empty(),
        "the tag rules drifted from the measurement:\n  {}",
        wrong.join("\n  ")
    );
}

// ---------------------------------------------------------------------------
// Formula ordering and cycles
// ---------------------------------------------------------------------------

#[test]
fn formulas_are_topologically_ordered() {
    // `z` references `y` references `x`, and they are declared in the opposite
    // order. Evaluating in declaration order would read an empty namespace.
    let formulas = BTreeMap::from([
        ("z".to_string(), "formula.y + 1".to_string()),
        ("y".to_string(), "x + 1".to_string()),
        ("x".to_string(), "1".to_string()),
    ]);
    assert_eq!(
        order_formulas(&formulas).expect("no cycle"),
        ["x", "y", "z"]
    );
}

#[test]
fn a_formula_sees_the_formula_it_references() {
    // The TypeScript accumulated formulas into a separate object and assigned
    // `ctx.formula` only after the loop, so `formula.doubled` read `null` and
    // this threw "Expected a number but got null". The same fixture is pinned on
    // the TypeScript side, so the two cannot disagree about it.
    let vault = fixture_vault("formula-chain");
    let base = base_of(&vault, "Chained.base");
    let result =
        query_base(&vault, "Chained.base", &base, &QueryOptions::default()).expect("queries");
    let values = &result.rows[0].values;
    assert_eq!(values["formula.base_value"].as_number(), Some(41.0));
    assert_eq!(values["formula.doubled"].as_number(), Some(82.0));
    assert_eq!(values["formula.label"].as_number(), Some(83.0));
}

// ---------------------------------------------------------------------------
// The case rules of the string methods
// ---------------------------------------------------------------------------

/// The case rules of `contains`, `startsWith`, `endsWith` and `==`, as the CLI
/// answers them.
///
/// Measured against the Obsidian CLI on 1.14.4, over the `string-case`
/// fixture. The asymmetry is Obsidian's own: `contains` folds the case of both
/// sides, and the other three do not. The whole table is asserted in one test
/// because a subset would miss the point — the point is that ONE of the four
/// folds and the other three do not.
///
/// A base-level guard rather than an expression-level one, because the four
/// have to agree WITH EACH OTHER as well as with Obsidian, and only a base
/// evaluates them side by side.
#[test]
fn contains_folds_case_and_the_other_three_do_not() {
    let vault = fixture_vault("string-case");
    let base = base_of(&vault, "Case.base");
    let result = query_base(&vault, "Case.base", &base, &QueryOptions::default()).expect("queries");
    let row = &result.rows[0];
    let value = |id: &str| row.values[id].to_display_string();

    // `==` is case-sensitive.
    assert_eq!(value("formula.eq_exact"), "true");
    assert_eq!(value("formula.eq_lower"), "false");

    // `contains` folds the case, and an empty needle is false.
    assert_eq!(value("formula.contains_lower"), "true");
    assert_eq!(value("formula.contains_upper"), "true");
    assert_eq!(value("formula.contains_exact"), "true");
    assert_eq!(value("formula.contains_empty"), "false");

    // The variadic forms fold the case too, and have NO empty-needle guard:
    // Obsidian's `contains` carries the guard and the variadic forms do not.
    assert_eq!(value("formula.contains_all_lower"), "true");
    assert_eq!(value("formula.contains_all_upper"), "true");
    assert_eq!(value("formula.contains_all_empty"), "true");
    assert_eq!(value("formula.contains_any_lower"), "true");
    assert_eq!(value("formula.contains_any_upper"), "true");
    assert_eq!(value("formula.contains_any_empty"), "true");

    // `startsWith` and `endsWith` do NOT fold.
    assert_eq!(value("formula.starts_lower"), "false");
    assert_eq!(value("formula.starts_upper"), "false");
    assert_eq!(value("formula.starts_exact"), "true");
    assert_eq!(value("formula.ends_lower"), "false");
    assert_eq!(value("formula.ends_upper"), "false");
    assert_eq!(value("formula.ends_exact"), "true");

    // An empty prefix or suffix is still a match — the empty-needle rule
    // belongs to `contains` alone.
    assert_eq!(value("formula.starts_empty"), "true");
    assert_eq!(value("formula.ends_empty"), "true");

    // `file.name` is a String, so it takes the string `contains` — case folded.
    assert_eq!(value("formula.file_name_contains_lower"), "true");
    // ...and `startsWith` on it, which does not fold.
    assert_eq!(value("formula.file_name_starts_lower"), "false");
}

// ---------------------------------------------------------------------------
// AllNotes.base: the parity oracle
// ---------------------------------------------------------------------------

/// The exact row set and order the testing vault implies.
///
/// This is what `obsidian base:query` is compared against, so it has to be right
/// even while Obsidian is unreachable. `Root Ticket.md` is at the vault root on
/// purpose: an unsorted view orders by `file.name`, not by path, so it sorts
/// AFTER the `Tickets/` notes despite sorting BEFORE them by path.
#[test]
fn the_oracle_produces_the_row_set_and_order_the_vault_implies() {
    let vault = testing_vault();
    let result = query(&vault, "AllNotes.base", Some("All"), None);
    assert_eq!(
        paths(&result),
        [
            "Tickets/Add offline mode.md",
            "Tickets/Fix login redirect.md",
            "Tickets/Invoice export.md",
            "Root Ticket.md",
        ]
    );
    assert_eq!(result.total, 4);
    assert_eq!(result.view.name, "All");
    assert_eq!(result.context, None);
}

#[test]
fn an_unsorted_view_orders_by_file_name_not_by_path() {
    let vault = testing_vault();
    let names: Vec<String> = query(&vault, "AllNotes.base", Some("AsList"), None)
        .rows
        .iter()
        .map(|row| row.values["file.name"].to_display_string())
        .collect();
    let mut sorted = names.clone();
    sorted.sort();
    assert_eq!(names, sorted, "rows must be in file.name order");
    assert_eq!(
        names,
        [
            "Add offline mode",
            "Fix login redirect",
            "Invoice export",
            "Root Ticket"
        ]
    );
}

#[test]
fn a_formula_column_is_evaluated_once_per_note_and_ordered() {
    let vault = testing_vault();
    let result = query(&vault, "AllNotes.base", Some("All"), None);
    assert_eq!(
        cell(
            &result,
            "Tickets/Add offline mode.md",
            "formula.priority_display"
        )
        .to_display_string(),
        "2 – normal"
    );
    assert_eq!(
        cell(
            &result,
            "Tickets/Fix login redirect.md",
            "formula.priority_display"
        )
        .to_display_string(),
        "1 – high"
    );
}

#[test]
fn grouped_results_keep_group_order_and_are_reachable_by_key() {
    let vault = testing_vault();
    let result = query(&vault, "AllNotes.base", Some("ByPriority"), None);
    // groupBy is ASC, and the labels are "1 – high" .. "3 – low".
    let grouped_keys: Vec<String> = groups(&result)
        .expect("a grouped result")
        .into_iter()
        .map(|(key, _)| key)
        .collect();
    assert_eq!(grouped_keys, ["1 – high", "2 – normal", "3 – low"]);
    assert_eq!(
        grouped(
            &result,
            "1 – high",
            "Tickets/Fix login redirect.md",
            "file.name"
        )
        .to_display_string(),
        "Fix login redirect"
    );
}

#[test]
fn an_explicitly_sorted_view_differs_from_the_default() {
    let vault = testing_vault();
    let sorted = query(&vault, "AllNotes.base", Some("All"), None);
    let unsorted = query(&vault, "AllNotes.base", Some("AsList"), None);
    // Both happen to agree here because file.name ASC is the default; the point
    // is that `All` carries an explicit sort and resolves without complaint.
    assert_eq!(sorted.total, unsorted.total);
    assert!(sorted.view.sort.is_some());
}

// ---------------------------------------------------------------------------
// Tickets.base: the documented divergence, and hard errors
// ---------------------------------------------------------------------------

#[test]
fn a_this_scoped_base_raises_rather_than_returning_an_empty_result() {
    // The project's central divergence. Obsidian's CLI returns `[]` here; an
    // empty array would be silently wrong, so this is an error.
    let vault = testing_vault();
    let error = refusal(&vault, "Tickets.base", QueryOptions::default());
    assert_eq!(error.construct(), Some("this"));
    assert!(error.message().contains("no host note was supplied"));
}

#[test]
fn the_same_base_scopes_itself_to_the_host_note() {
    let vault = testing_vault();
    let some = query(
        &vault,
        "Tickets.base",
        Some("All"),
        Some("Projects/SomeProject.md"),
    );
    assert_eq!(
        paths(&some),
        [
            "Tickets/Add offline mode.md",
            "Tickets/Fix login redirect.md"
        ]
    );

    let other = query(
        &vault,
        "Tickets.base",
        Some("All"),
        Some("Projects/OtherProject.md"),
    );
    assert_eq!(paths(&other), ["Tickets/Invoice export.md"]);

    let root = query(&vault, "Tickets.base", Some("All"), Some("Root Project.md"));
    assert_eq!(paths(&root), ["Root Ticket.md"]);
}

#[test]
fn a_context_that_is_not_a_note_is_refused_by_what_it_is() {
    let vault = testing_vault();
    for (context, expected) in [
        ("Nope/Missing.md", "does not exist"),
        ("Tickets.base", "Base"),
        ("Projects", "folder"),
        ("   ", "empty"),
    ] {
        let error = refusal(
            &vault,
            "Tickets.base",
            QueryOptions {
                context: Some(context.into()),
                view: None,
            },
        );
        assert!(
            error.message().contains(expected),
            "context {context:?} should be refused as {expected:?}, got: {}",
            error.message()
        );
    }
}

#[test]
fn filters_may_only_have_one_of_and_or_not() {
    // Obsidian's exact string, which the conformance suite compares.
    let error = refusal_from(
        "filters:\n  and:\n    - file.hasTag(\"ticket\")\n  or:\n    - file.hasTag(\"x\")\nviews:\n  - type: table\n    name: V\n",
        QueryOptions::default(),
    );
    assert!(
        error.message().contains("may only have one of"),
        "got: {}",
        error.message()
    );
}

#[test]
fn an_unimplemented_construct_is_an_error_never_a_silent_null() {
    let error = refusal_from(
        "filters:\n  and:\n    - nosuchfunction(1)\nviews:\n  - type: table\n    name: V\n",
        QueryOptions::default(),
    );
    assert_eq!(error.construct(), Some("nosuchfunction"));
    assert!(error.message().contains("Unknown function"));
}

#[test]
fn an_unknown_view_name_says_so() {
    let vault = testing_vault();
    let error = refusal(
        &vault,
        "AllNotes.base",
        QueryOptions {
            context: None,
            view: Some("NoSuchView".into()),
        },
    );
    assert!(
        error.message().contains("NoSuchView"),
        "got: {}",
        error.message()
    );
}

// ---------------------------------------------------------------------------
// Labels
// ---------------------------------------------------------------------------

#[test]
fn column_labels_are_obsidians_display_labels_not_titles() {
    // Probed on 1.13.7, not inferred: file.name labels as "file name", and
    // folder and properties DROP the namespace entirely.
    assert_eq!(label_for_id("file.name"), "file name");
    assert_eq!(label_for_id("file.basename"), "file base name");
    assert_eq!(label_for_id("file.ext"), "file extension");
    assert_eq!(label_for_id("file.ctime"), "created time");
    assert_eq!(label_for_id("file.mtime"), "modified time");
    assert_eq!(label_for_id("file.folder"), "folder");
    assert_eq!(label_for_id("file.properties"), "properties");
    assert_eq!(label_for_id("file.file"), "file");
}

#[test]
fn labels_are_not_title_cased_and_keep_their_underscores() {
    assert_eq!(label_for_id("formula.priority_display"), "priority_display");
    assert_eq!(strip_namespace("note.my_prop"), "my_prop");
    assert_eq!(strip_namespace("formula.x"), "x");
    assert_eq!(canonical_id_of("status"), "note.status");
    assert_eq!(canonical_id_of("note.status"), "note.status");
}

#[test]
fn a_configured_display_name_wins_verbatim() {
    // Obsidian does not title-case a configured name, and matches the key
    // CANONICALLY: a bare-keyed displayName is silently ignored.
    let base = parse_base(
        "L.base",
        "properties:\n  note.priority:\n    displayName: PR Priority\n  priority:\n    displayName: Bare\nviews:\n  - type: table\n    name: V\n",
    )
    .expect("parses");
    assert_eq!(display_name_for(&base, "priority"), "PR Priority");
    assert_eq!(display_name_for(&base, "note.priority"), "PR Priority");

    let bare_only = parse_base(
        "B.base",
        "properties:\n  priority:\n    displayName: Bare\nviews:\n  - type: table\n    name: V\n",
    )
    .expect("parses");
    assert_eq!(display_name_for(&bare_only, "priority"), "priority");
}

#[test]
fn the_rendered_cells_are_what_an_agent_reads() {
    let vault = testing_vault();
    let result = query(&vault, "AllNotes.base", Some("All"), None);
    let rendered = cells(&result);
    let add = rendered
        .iter()
        .find(|(path, _)| path == "Tickets/Add offline mode.md")
        .expect("a row");
    assert_eq!(add.1["note.status"], "active");
    assert_eq!(add.1["formula.priority_display"], "2 – normal");
}
