//! The expression engine.
//!
//! These are the golden values from the TypeScript implementation's suite,
//! transcribed. The point of the port is byte-identical observable behaviour,
//! so the assertions are the same ones the original pinned -- including the
//! quirks that were probed against a live Obsidian rather than inferred.

use std::collections::BTreeMap;
use std::rc::Rc;

use bases_mcp::evaluator::{evaluate_expression, EvalContext, ThisContext};
use bases_mcp::value::{BasesValue, Duration, FileAccessors, FileValue};

/// A one-note vault, enough for link resolution and the file methods.
fn make_ctx(options: Option<AccessorsOptions>) -> EvalContext {
    let opts = options.unwrap_or_default();
    let tags: Rc<Vec<BasesValue>> = Rc::new(
        opts.tags
            .iter()
            .map(|t| BasesValue::String(t.clone()))
            .collect(),
    );
    let links: Rc<Vec<BasesValue>> = Rc::new(
        opts.links
            .iter()
            .map(|t| BasesValue::Link {
                target: t.clone(),
                display: None,
                resolved: Some(t.clone()),
            })
            .collect(),
    );
    let props: Rc<BTreeMap<String, BasesValue>> = Rc::new(opts.note.clone());
    let accessors = FileAccessors {
        tags: Rc::new(move || (*tags).clone()),
        links: Rc::new(move || (*links).clone()),
        embeds: Rc::new(Vec::new),
        backlinks: Rc::new(Vec::new),
        properties: Rc::new(move || (*props).clone()),
        ctime: Rc::new(|| bases_mcp::value::BasesDate::from_millis(1_577_836_800_000)),
        mtime: Rc::new(|| bases_mcp::value::BasesDate::from_millis(1_767_225_600_000)),
        size: Rc::new(|| 42),
        tasks: Rc::new(Vec::new),
        resolve: Rc::new(|target: &str| Some(FileValue::new(target.to_string(), accessors_shim()))),
        links_to: Rc::new(|_: &str| false),
    };
    let file = FileValue::new(
        opts.path.clone().unwrap_or_else(|| "Notes/Alpha.md".into()),
        accessors,
    );

    let mut ctx = EvalContext::new(file);
    ctx.note = opts.note.clone();
    if let Some(this_path) = &opts.this_path {
        let host = FileValue::new(this_path.to_string(), accessors_shim());
        ctx.this_value = Some(ThisContext {
            file: host,
            note: opts.this_note.clone(),
        });
    }
    ctx
}

/// `resolve` needs a `FileAccessors`, and building one that recurses would not
/// terminate. The test never resolves to a note it then reads members of, so a
/// bare shim with no closures that call back is enough.
fn accessors_shim() -> FileAccessors {
    FileAccessors {
        tags: Rc::new(Vec::new),
        links: Rc::new(Vec::new),
        embeds: Rc::new(Vec::new),
        backlinks: Rc::new(Vec::new),
        properties: Rc::new(BTreeMap::new),
        ctime: Rc::new(|| bases_mcp::value::BasesDate::from_millis(0)),
        mtime: Rc::new(|| bases_mcp::value::BasesDate::from_millis(0)),
        size: Rc::new(|| 0),
        tasks: Rc::new(Vec::new),
        resolve: Rc::new(|_: &str| None),
        links_to: Rc::new(|_: &str| false),
    }
}

#[derive(Default, Clone)]
struct AccessorsOptions {
    path: Option<String>,
    tags: Vec<String>,
    links: Vec<String>,
    note: BTreeMap<String, BasesValue>,
    this_path: Option<String>,
    this_note: BTreeMap<String, BasesValue>,
}

impl AccessorsOptions {
    fn at(mut self, path: &str) -> Self {
        self.path = Some(path.to_string());
        self
    }
    fn tagged(mut self, tags: &[&str]) -> Self {
        self.tags = tags.iter().map(|s| s.to_string()).collect();
        self
    }
    fn linked(mut self, links: &[&str]) -> Self {
        self.links = links.iter().map(|s| s.to_string()).collect();
        self
    }
    fn prop(mut self, key: &str, value: &str) -> Self {
        self.note
            .insert(key.to_string(), BasesValue::String(value.to_string()));
        self
    }
    fn host(mut self, path: &str) -> Self {
        self.this_path = Some(path.to_string());
        self
    }
}

fn default_ctx() -> EvalContext {
    make_ctx(None)
}

fn ev(expr: &str) -> Result<BasesValue, bases_mcp::error::BasesError> {
    evaluate_expression(expr, &default_ctx())
}

fn ev_in(expr: &str, ctx: &EvalContext) -> Result<BasesValue, bases_mcp::error::BasesError> {
    evaluate_expression(expr, ctx)
}

fn s(expr: &str) -> String {
    ev(expr).expect("expression evaluates").to_display_string()
}

fn n(expr: &str) -> f64 {
    ev(expr)
        .expect("expression evaluates")
        .as_number()
        .expect("a number")
}

fn b(expr: &str) -> bool {
    match ev(expr).expect("expression evaluates") {
        BasesValue::Bool(b) => b,
        other => panic!("expected a boolean, got {other:?}"),
    }
}

#[test]
fn literals_and_arithmetic() {
    assert_eq!(n("1 + 2"), 3.0);
    assert_eq!(s("\"a\" + \"b\""), "ab");
    assert_eq!(s("1 + \"x\""), "1x");
    assert_eq!(n("2.5.round()"), 3.0);
    assert_eq!(n("(2.5).round()"), 3.0);
    assert_eq!(n("(-5).abs()"), 5.0);
    assert_eq!(s("(3.14159).toFixed(2)"), "3.14");
    assert_eq!(n("7 % 3"), 1.0);
}

#[test]
fn division_by_zero_yields_zero_rather_than_throwing() {
    // Obsidian does not guard this; real vaults guard it themselves.
    assert_eq!(n("1 / 0"), 0.0);
}

#[test]
fn precedence_follows_javascript() {
    assert_eq!(n("1 + 2 * 3"), 7.0);
    assert_eq!(n("(1 + 2) * 3"), 9.0);
    assert!(!b("!true && false"));
}

#[test]
fn this_binds_to_the_host_note() {
    let ctx = make_ctx(Some(
        AccessorsOptions::default().host("Projects/SomeProject.md"),
    ));
    assert_eq!(
        ev_in("this.file.name", &ctx).unwrap().to_display_string(),
        "SomeProject"
    );
    assert_eq!(
        ev_in("this.file.path", &ctx).unwrap().to_display_string(),
        "Projects/SomeProject.md"
    );
    // `this.path` is a File member...
    assert_eq!(
        ev_in("this.path", &ctx).unwrap().to_display_string(),
        "Projects/SomeProject.md"
    );
    // ...and `this.projects` reads the host note's frontmatter.
    let mut opts = AccessorsOptions::default().host("Projects/SomeProject.md");
    opts.this_note
        .insert("projects".into(), BasesValue::String("alpha".into()));
    let ctx2 = make_ctx(Some(opts));
    assert_eq!(
        ev_in("this.projects", &ctx2).unwrap().to_display_string(),
        "alpha"
    );
}

#[test]
fn this_without_a_host_is_a_hard_error() {
    // The project's central divergence: Obsidian returns an empty result here.
    let err = ev("this.file.name").expect_err("must not evaluate");
    assert_eq!(err.construct(), Some("this"));
    assert!(err.message().contains("no host note was supplied"));
}

#[test]
fn this_is_detected_even_behind_a_short_circuit() {
    // A filter like `project.contains(link(this.file.name))` would short-circuit
    // on a missing `project` and quietly yield null without the pre-scan.
    let err = ev("missingprop && this.file.name == \"x\"").expect_err("must not evaluate");
    assert_eq!(err.construct(), Some("this"));
}

#[test]
fn file_members_match_obsidian() {
    let ctx = make_ctx(Some(AccessorsOptions::default().at("Notes/Alpha.md")));
    // `file.name` renders WITHOUT the extension, matching `basename`. Probed on
    // 1.13.7; the docs claim the opposite.
    assert_eq!(
        ev_in("file.name", &ctx).unwrap().to_display_string(),
        "Alpha"
    );
    assert_eq!(
        ev_in("file.basename", &ctx).unwrap().to_display_string(),
        "Alpha"
    );
    assert_eq!(
        ev_in("file.path", &ctx).unwrap().to_display_string(),
        "Notes/Alpha.md"
    );
    assert_eq!(ev_in("file.ext", &ctx).unwrap().to_display_string(), "md");
    assert_eq!(
        ev_in("file.folder", &ctx).unwrap().to_display_string(),
        "Notes"
    );
}

#[test]
fn a_root_note_reports_folder_as_slash() {
    // Probed on 1.13.7: a root note emits `"folder": "/"`, not `""`.
    let ctx = make_ctx(Some(AccessorsOptions::default().at("Alpha.md")));
    assert_eq!(ev_in("file.folder", &ctx).unwrap().to_display_string(), "/");
    assert!(!b_in("file.folder == \"\"", &ctx));
    assert!(b_in("file.folder == \"/\"", &ctx));
}

fn b_in(expr: &str, ctx: &EvalContext) -> bool {
    match ev_in(expr, ctx).expect("expression evaluates") {
        BasesValue::Bool(b) => b,
        other => panic!("expected a boolean, got {other:?}"),
    }
}

#[test]
fn has_tag_matches_nested_tags() {
    let ctx = make_ctx(Some(
        AccessorsOptions::default().tagged(&["#plugin/transformer"]),
    ));
    assert!(b_in("file.hasTag(\"plugin\")", &ctx));
    assert!(b_in("file.hasTag(\"plugin/transformer\")", &ctx));
    assert!(b_in("file.hasTag(\"#plugin\")", &ctx));
    assert!(!b_in("file.hasTag(\"other\")", &ctx));
}

#[test]
fn in_folder_is_recursive_but_folder_equality_is_not() {
    let ctx = make_ctx(Some(AccessorsOptions::default().at("a/b/c/Note.md")));
    assert!(b_in("file.inFolder(\"a\")", &ctx));
    assert!(b_in("file.inFolder(\"a/b\")", &ctx));
    assert!(!b_in("file.folder == \"a\"", &ctx));
    assert!(b_in("file.folder == \"a/b/c\"", &ctx));
}

#[test]
fn a_bare_identifier_is_a_note_property() {
    let ctx = make_ctx(Some(AccessorsOptions::default().prop("status", "active")));
    assert_eq!(ev_in("status", &ctx).unwrap().to_display_string(), "active");
    // And a missing one is null rather than an error.
    assert!(matches!(ev_in("nope", &ctx).unwrap(), BasesValue::Null));
}

#[test]
fn contains_is_type_strict_and_says_so() {
    // The exact string Obsidian produces, which the conformance suite compares.
    let err = ev("\"abc\".contains(1)").expect_err("must be a type error");
    assert_eq!(
        err.display_message(),
        "Type error in \"contains\", parameter \"value\". Expected String not, given Number. (construct: contains)"
    );
}

#[test]
fn an_empty_list_is_falsy() {
    // Which is what makes `if(projects, projects.length, 0)` work as a null guard.
    assert!(!b("list([]).isTruthy()"));
    assert!(b("list([1]).isTruthy()"));
}

#[test]
fn date_is_never_empty() {
    // Obsidian defines `date.isEmpty()` as always false, even when absent.
    assert!(!b("today().isEmpty()"));
    assert!(b("null.isEmpty()"));
    assert!(b("list([]).isEmpty()"));
}

#[test]
fn date_subtraction_yields_a_duration_that_also_numbers() {
    // The docs say milliseconds; the runtime returns a Duration. We support both
    // idioms rather than picking a side. See docs/divergences.md.
    assert_eq!(
        n("number(date(\"2024-03-01\") - date(\"2024-02-01\"))"),
        2_505_600_000.0
    );
    assert_eq!(
        n("(date(\"2024-03-01\") - date(\"2024-02-01\")).days"),
        29.0
    );
    // The documented whole-days idiom.
    assert_eq!(
        n("((number(date(\"2024-03-01\")) - number(date(\"2024-02-01\"))) / 86400000).round(0)"),
        29.0
    );
}

#[test]
fn duration_needs_to_be_on_the_left_to_multiply() {
    assert_eq!(n("number(duration(\"1d\") * 2)"), 172_800_000.0);
    let err = ev("2 * duration(\"1d\")").expect_err("must refuse");
    assert_eq!(err.construct(), Some("*"));
    assert!(err.message().contains("put the duration on the left"));
}

#[test]
fn m_is_a_month_and_m_is_a_minute() {
    // The Moment.js convention Obsidian documents.
    match ev("duration(\"1M\")").expect("parses") {
        BasesValue::Duration(d) => assert_eq!(d.months, 1),
        other => panic!("expected a duration, got {other:?}"),
    }
    match ev("duration(\"1m\")").expect("parses") {
        BasesValue::Duration(d) => assert_eq!(d.minutes(), 1),
        other => panic!("expected a duration, got {other:?}"),
    }
}

#[test]
fn links_compare_by_resolved_target() {
    assert_eq!(s("link(\"SomeProject\")"), "[[SomeProject]]");
    assert_eq!(
        s("link(\"SomeProject\", \"Alias\")"),
        "[[SomeProject|Alias]]"
    );
    // A link whose resolved target matches another note's path compares equal,
    // regardless of how each was spelled.
    let linked = make_ctx(Some(
        AccessorsOptions::default().linked(&["Projects/SomeProject.md"]),
    ));
    assert!(b_in("file.links.contains(link(\"SomeProject\"))", &linked));
    assert!(!b_in(
        "file.links.contains(link(\"OtherProject\"))",
        &linked
    ));
    // And `hasLink` is the same test under Obsidian's other spelling.
    assert!(b_in("file.hasLink(\"SomeProject\")", &linked));
    assert!(!b_in("file.hasLink(\"OtherProject\")", &linked));
}

#[test]
fn list_contains_dispatches_on_the_needle() {
    let ctx = make_ctx(Some(
        AccessorsOptions::default().prop("project", "[[SomeProject]]"),
    ));
    assert!(b_in("list([link(\"A\")]).contains(link(\"A\"))", &ctx));
    assert!(!b_in("list([link(\"A\")]).contains(link(\"B\"))", &ctx));
    assert!(b_in("list([1, 2, 3]).contains(2)", &ctx));
    assert!(!b_in("list([1, 2, 3]).contains(9)", &ctx));
}

#[test]
fn higher_order_methods_bind_value_and_index() {
    assert_eq!(n("list([1, 2, 3]).filter(value > 1).sum()"), 5.0);
    assert_eq!(n("list([1, 2, 3]).map(value * 2).sum()"), 12.0);
    assert!(b("list([1, 2]).some(value == 2)"));
    assert!(b("list([1, 2]).every(value > 0)"));
    assert_eq!(s("list([1, 2, 3]).find(value > 1)"), "2");
}

#[test]
fn reduce_seeds_from_the_second_argument() {
    // Real vaults seed with 0 for a sum and null for the documented max idiom,
    // so the seed must not default to the first element.
    assert_eq!(n("list([1, 2, 3]).reduce(acc + value, 0)"), 6.0);
    assert_eq!(n("list([1, 2, 3]).reduce(acc + value, 10)"), 16.0);
    // A null seed works with `+`. The documented max idiom does NOT, because
    // `if()` is not lazy and `max(null, x)` is a type error -- verified against
    // the TypeScript implementation, which raises the same error.
    assert_eq!(
        n("list([1, 2, 3]).reduce(if(acc == null, 0, acc) + value, null)"),
        6.0
    );
    let strict = ev("max(null, 5)").expect_err("max is type-strict");
    assert_eq!(strict.construct(), Some("max"));
}

#[test]
fn round_is_half_away_from_zero() {
    assert_eq!(n("2.5.round()"), 3.0);
    assert_eq!(n("(-2.5).round()"), -3.0);
    assert_eq!(n("(1.2345).round(2)"), 1.23);
}

#[test]
fn and_or_are_accepted_as_aliases() {
    assert!(!b("true and false"));
    assert!(b("true or false"));
}

#[test]
fn the_not_keyword_has_no_working_position() {
    // Verified against the TypeScript implementation: `not` is lexed as a
    // keyword and the parser has no prefix rule for it, and the infix rule never
    // fires because the operand after `and` is parsed at a precedence that
    // stops at `not`. So `!` is the only usable negation. Recorded as a known
    // gap in the divergence registry rather than silently extended here, because
    // fixing it would change behaviour the parity suite pins.
    let prefix = ev("not false").expect_err("prefix not does not parse");
    assert!(prefix.message().contains("Unexpected token"));
    let infix = ev("true and not false").expect_err("infix not does not parse");
    assert!(infix.message().contains("Unexpected token"));
}

#[test]
fn if_evaluates_every_argument() {
    // Not lazy, in either implementation. It means a guarded expression like
    // `if(x == null, 0, x / x)` still throws on the untaken arm, which is why
    // real vaults guard with `&&` and the `contains` idiom instead.
    let err = ev("if(true, 1, max(null, 5))").expect_err("the untaken arm still runs");
    assert_eq!(err.construct(), Some("max"));
    assert_eq!(ev("if(true, 1, 2)").unwrap().as_number(), Some(1.0));
    assert_eq!(ev("if(false, 1, 2)").unwrap().as_number(), Some(2.0));
}

#[test]
fn not_is_nand_for_a_list_not_a_logical_negation() {
    // `not:` in a filter means "none of these are true". The expression-level
    // `not` is a plain negation; the NAND rule belongs to the filter tree.
    assert!(b("!list([])"));
    assert!(!b("!list([1])"));
}

#[test]
fn member_access_on_a_bare_numeric_literal_works() {
    // Obsidian's own parser rejects both of these (obsidian-help #1095); we
    // follow the documented grammar.
    assert!(b("(1).isTruthy()"));
    assert_eq!(n("(1).abs()"), 1.0);
}

#[test]
fn index_access_works_on_lists_strings_and_files() {
    assert_eq!(s("list([1, 2, 3])[1]"), "2");
    assert_eq!(s("\"hello\"[1]"), "e");
    let ctx = make_ctx(Some(AccessorsOptions::default().at("Notes/Alpha.md")));
    assert_eq!(
        ev_in("file[\"name\"]", &ctx).unwrap().to_display_string(),
        "Alpha"
    );
}

#[test]
fn unknown_functions_are_hard_errors_never_null() {
    let err = ev("nosuchfunction(1)").expect_err("must refuse");
    assert_eq!(err.construct(), Some("nosuchfunction"));
    let err = ev("\"a\".nosuchmethod()").expect_err("must refuse");
    assert_eq!(err.construct(), Some("nosuchmethod"));
    assert!(err.message().contains("is not a method on String"));
}

#[test]
fn duration_fields_are_addressable() {
    let d = ev("duration(\"1d\").days").expect("parses");
    assert_eq!(d.as_number(), Some(1.0));
    let _ = Duration::ZERO;
}

#[test]
fn string_methods_match_the_documented_set() {
    assert!(b("\"hello\".startsWith(\"he\")"));
    assert!(b("\"hello\".endsWith(\"lo\")"));
    assert_eq!(s("\"HELLO\".lower()"), "hello");
    assert_eq!(s("\"  x  \".trim()"), "x");
    assert_eq!(s("\"ab\".reverse()"), "ba");
    assert_eq!(s("\"a,b,c\".split(\",\").length"), "3");
    assert!(b("\"abc\".containsAll(\"a\", \"b\")"));
    assert!(b("\"abc\".containsAny(\"z\", \"b\")"));
}

#[test]
fn object_keys_and_values_work_on_a_namespace() {
    let ctx = make_ctx(Some(AccessorsOptions::default().prop("status", "active")));
    assert_eq!(
        ev_in("note.keys().length", &ctx)
            .unwrap()
            .to_display_string(),
        "1"
    );
    assert_eq!(
        ev_in("note.values()[0]", &ctx).unwrap().to_display_string(),
        "active"
    );
}
