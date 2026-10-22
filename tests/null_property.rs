//! A null property DROPS the row, the way the Obsidian CLI drops it.
//!
//! The construct: a filter that dereferences a property the note does not
//! have, e.g. `type.contains("task")` against a note with no `type`.
//!
//! Obsidian's rule, read out of the Bases evaluator Obsidian ships in
//! `resources/obsidian.asar`. Its call site resolves the receiver and then
//!
//!     if(s=r.getValue(e)||gW.value, !(t=KW.findForValue(s,i)) && s===gW.value) return gW.value
//!
//! so a null receiver with an unknown method answers the null value rather
//! than raising "no such method". Obsidian's filter predicate is
//! `this.getValue(e).isTruthy()`, and its null value answers `isTruthy()`
//! false -- so the expression evaluates to null, a null filter is falsy, and a
//! falsy filter drops the row. No error is reported.
//!
//! Before this, the same construct raised `Type error: "contains" is not a
//! method on null`, which failed the WHOLE query rather than one row.
//!
//! The CLI comparison itself is the deferred UI validation (STATUS.md item
//! 14): the Obsidian CLI talks to the desktop app, whose one open vault is
//! `develop`'s `test/vault` -- the parity oracle -- and any `obsidian`
//! command makes the app rewrite that vault's `.base` files. It needs a human
//! in the Obsidian UI.

use bases_mcp::base::QueryOptions;
use bases_mcp::service::Resolver;

// Each integration test is its own crate, and no suite needs every helper here.
mod common;
use common::futures_block_on;

/// A vault with two notes: one has a `type`, one does not.
fn vault_with_and_without_type() -> tempfile::TempDir {
    let dir = tempfile::TempDir::new().expect("a temp dir is writable");
    let root = dir.path();
    std::fs::write(
        root.join("WithType.md"),
        "---\ntags:\n  - ticket\ntype: task\nstatus: active\n---\n\n# With type\n",
    )
    .expect("the note is written");
    std::fs::write(
        root.join("WithoutType.md"),
        "---\ntags:\n  - ticket\nstatus: done\n---\n\n# Without type\n",
    )
    .expect("the note is written");
    dir
}

fn base_filtering_on_type() -> &'static str {
    "filters:\n  and:\n    - file.hasTag(\"ticket\")\n    - type.contains(\"task\")\n\
     views:\n  - type: table\n    name: All\n    order:\n      - file.name\n"
}

#[test]
fn a_filter_on_a_null_property_drops_that_row_and_keeps_the_rest() {
    let dir = vault_with_and_without_type();
    std::fs::write(dir.path().join("Filter.base"), base_filtering_on_type())
        .expect("the base is written");

    let resolver = futures_block_on(Resolver::open_dir(dir.path())).expect("the vault opens");
    let result = futures_block_on(resolver.query("Filter.base", &QueryOptions::default()))
        .expect("a null property drops a row, it does not fail the query");

    let paths: Vec<&str> = result.rows.iter().map(|r| r.path.as_str()).collect();
    assert_eq!(
        paths,
        ["WithType.md"],
        "the note without a `type` is dropped, and the one with it survives"
    );
}

#[test]
fn a_filter_on_a_null_property_drops_the_row_wherever_it_sits() {
    // The same construct as a VIEW filter, which is a different code path from
    // the base-level one.
    let dir = vault_with_and_without_type();
    std::fs::write(
        dir.path().join("ViewFiltered.base"),
        "filters:\n  and:\n    - file.hasTag(\"ticket\")\n\
         views:\n  - type: table\n    name: All\n    filters:\n      and:\n        - type.contains(\"task\")\n    \
         order:\n      - file.name\n",
    )
    .expect("the base is written");

    let resolver = futures_block_on(Resolver::open_dir(dir.path())).expect("the vault opens");
    let result = futures_block_on(resolver.query("ViewFiltered.base", &QueryOptions::default()))
        .expect("queries");

    let paths: Vec<&str> = result.rows.iter().map(|r| r.path.as_str()).collect();
    assert_eq!(paths, ["WithType.md"], "a view filter drops the row too");
}

#[test]
fn a_formula_on_a_null_property_is_null_rather_than_an_error() {
    // The same construct as a FORMULA. A null formula cell is the third
    // observable form of the same rule.
    let dir = vault_with_and_without_type();
    std::fs::write(
        dir.path().join("Formula.base"),
        "filters:\n  and:\n    - file.hasTag(\"ticket\")\n\
         formulas:\n  has_task: type.contains(\"task\")\n\
         views:\n  - type: table\n    name: All\n    order:\n      - file.name\n      - formula.has_task\n",
    )
    .expect("the base is written");

    let resolver = futures_block_on(Resolver::open_dir(dir.path())).expect("the vault opens");
    let result = futures_block_on(resolver.query("Formula.base", &QueryOptions::default()))
        .expect("queries");

    assert_eq!(result.rows.len(), 2, "a null formula does not drop the row");
    let with = result
        .rows
        .iter()
        .find(|r| r.path == "WithType.md")
        .expect("the note with a type is a row");
    let without = result
        .rows
        .iter()
        .find(|r| r.path == "WithoutType.md")
        .expect("the note without a type is a row");
    assert_eq!(
        with.formula.get("has_task").map(|v| v.to_display_string()),
        Some("true".to_string())
    );
    assert!(
        !without
            .formula
            .get("has_task")
            .is_some_and(|v| v.is_truthy()),
        "the formula is null on the note without a `type`"
    );
}
