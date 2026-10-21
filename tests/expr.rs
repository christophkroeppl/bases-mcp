//! The expression engine.
//!
//! These are the golden values from the TypeScript implementation's suite,
//! transcribed. The point of the port is byte-identical observable behaviour,
//! so the assertions are the same ones the original pinned -- including the
//! quirks that were probed against a live Obsidian rather than inferred.

use std::collections::BTreeMap;
use std::sync::Arc;

use bases_mcp::evaluator::{evaluate_expression, EvalContext, ThisContext};
use bases_mcp::value::{BasesValue, Duration, FileAccessors, FileValue};

/// A one-note vault, enough for link resolution and the file methods.
fn make_ctx(options: Option<AccessorsOptions>) -> EvalContext {
    let opts = options.unwrap_or_default();
    let tags: Arc<Vec<BasesValue>> = Arc::new(
        opts.tags
            .iter()
            .map(|t| BasesValue::String(t.clone()))
            .collect(),
    );
    let links: Arc<Vec<BasesValue>> = Arc::new(
        opts.links
            .iter()
            .map(|t| BasesValue::Link {
                target: t.clone(),
                display: None,
                resolved: Some(t.clone()),
            })
            .collect(),
    );
    let props: Arc<BTreeMap<String, BasesValue>> = Arc::new(opts.note.clone());
    let accessors = FileAccessors {
        tags: Arc::new(move || (*tags).clone()),
        links: Arc::new(move || (*links).clone()),
        embeds: Arc::new(Vec::new),
        backlinks: Arc::new(Vec::new),
        properties: Arc::new(move || (*props).clone()),
        ctime: Arc::new(|| bases_mcp::value::BasesDate::from_millis(1_577_836_800_000)),
        mtime: Arc::new(|| bases_mcp::value::BasesDate::from_millis(1_767_225_600_000)),
        size: Arc::new(|| 42),
        tasks: Arc::new(Vec::new),
        resolve: Arc::new(|target: &str| {
            Some(FileValue::new(target.to_string(), accessors_shim()))
        }),
        links_to: Arc::new(|_: &str| false),
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
        tags: Arc::new(Vec::new),
        links: Arc::new(Vec::new),
        embeds: Arc::new(Vec::new),
        backlinks: Arc::new(Vec::new),
        properties: Arc::new(BTreeMap::new),
        ctime: Arc::new(|| bases_mcp::value::BasesDate::from_millis(0)),
        mtime: Arc::new(|| bases_mcp::value::BasesDate::from_millis(0)),
        size: Arc::new(|| 0),
        tasks: Arc::new(Vec::new),
        resolve: Arc::new(|_: &str| None),
        links_to: Arc::new(|_: &str| false),
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

    /// Frontmatter holding typed values, for the coercion tests. Typed props
    /// cannot go through `prop`, which only builds strings.
    fn props(pairs: &[(&str, BasesValue)]) -> Self {
        Self {
            note: pairs
                .iter()
                .map(|(k, v)| ((*k).to_string(), v.clone()))
                .collect(),
            ..Self::default()
        }
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

/// A null RECEIVER answers null, whatever the method is called.
///
/// Obsidian's rule, read out of the Bases evaluator Obsidian ships in
/// `resources/obsidian.asar`: its call site resolves the receiver and then
///
///     if(s=r.getValue(e)||gW.value, !(t=KW.findForValue(s,i)) && s===gW.value) return gW.value
///
/// so a null receiver with an unknown method answers the null value instead
/// of raising "no such method". This is what makes a null property DROP a row
/// instead of failing the whole query -- Obsidian's filter predicate is
/// `this.getValue(e).isTruthy()`, and its null value is falsy.
///
/// The four universal methods are the exception, and they are defined on null.
#[test]
fn a_null_receiver_answers_null_whatever_the_method_is_called() {
    let ctx = make_ctx(Some(AccessorsOptions::default().prop("status", "active")));
    for expr in [
        "missingprop.contains(\"x\")",
        "missingprop.containsAll(\"a\")",
        "missingprop.containsAny(\"a\")",
        "missingprop.startsWith(\"x\")",
        "missingprop.endsWith(\"x\")",
        "missingprop.lower()",
        "missingprop.abs()",
        "missingprop.bogus()",
        "missingprop.count()",
        "missingprop.filter(value == \"active\")",
    ] {
        assert!(
            matches!(ev_in(expr, &ctx), Ok(BasesValue::Null)),
            "{expr} on a null receiver is null, not an error"
        );
    }
}

/// The universal methods are defined ON null and answer normally.
///
/// Obsidian registers exactly these four for a null receiver -- `isEmpty` on
/// Null itself, and `toString`/`isTruthy`/`isType` on Any, which Null extends
/// -- and looks them up by walking that prototype chain, so they are found
/// before the null-receiver fallback can answer for them.
#[test]
fn the_universal_methods_answer_on_a_null_receiver() {
    let ctx = make_ctx(Some(AccessorsOptions::default().prop("status", "active")));
    assert_eq!(
        ev_in("missingprop.toString()", &ctx)
            .unwrap()
            .to_display_string(),
        "null"
    );
    assert!(!b_in("missingprop.isTruthy()", &ctx));
    assert!(b_in("missingprop.isEmpty()", &ctx));
    assert!(b_in("missingprop.isType(\"null\")", &ctx));
}

/// A receiver that is NOT null still errors on an unknown method.
///
/// The fallback is keyed on the receiver being null, so it cannot swallow the
/// "no such method" error for a real receiver -- which is Obsidian's rule too:
/// its null check is on the receiver itself, and a non-null receiver falls
/// through to `Cannot find function "x" on type <type>`.
#[test]
fn a_non_null_receiver_with_an_unknown_method_still_errors() {
    let ctx = make_ctx(Some(AccessorsOptions::props(&[
        ("s", BasesValue::String("active".into())),
        ("n", BasesValue::Number(42.0)),
        ("b", BasesValue::Bool(true)),
    ])));
    for expr in ["s.bogus()", "n.bogus()", "b.bogus()", "list(s).bogus()"] {
        let err = ev_in(expr, &ctx).expect_err("a non-null receiver still errors");
        assert!(
            err.display_message().contains("is not a method on"),
            "{expr} names the type it failed on: {}",
            err.display_message()
        );
    }
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

/// The argument rule, read out of Obsidian's Bases evaluator in
/// `resources/obsidian.asar` (installer 1.12.4). Before a function runs, its
/// call site checks each argument against the parameter table:
///
///     if(t.optional&&e===gW.value) T.push(null);
///     else { if(!t.type.some(t=>e instanceof t)){ if(e===gW.value) return !1; throw ... } T.push(e) }
///     ...
///     if(x<S.length)for(h=x;h<S.length;h++) if(!S[h].optional&&!S[h].variadic) throw new Error(SK.msgErrorNotEnoughArguments({function:i}))
///
/// An argument that was never passed is therefore an error in a required
/// position, and the message is Obsidian's own i18n string
/// (`msgErrorNotEnoughArguments` in `i18n.js`): `Cannot call function "x", not
/// enough arguments.` A null that WAS passed is not an error: it answers the
/// whole call null in a required position, and it is absent in an optional one.
#[test]
fn a_missing_required_argument_is_not_enough_arguments() {
    for (expr, function) in [
        ("\"active\".contains()", "contains"),
        ("\"active\".startsWith()", "startsWith"),
        ("\"active\".endsWith()", "endsWith"),
        ("\"active\".repeat()", "repeat"),
        ("\"active\".slice()", "slice"),
        ("\"active\".split()", "split"),
        ("\"active\".replace()", "replace"),
        // `replace`'s replacement is required in Obsidian, so this is the case
        // that used to delete the match instead of refusing.
        ("\"active\".replace(\"a\")", "replace"),
        ("list([1, 2]).contains()", "contains"),
        ("list([1, 2, 3]).slice()", "slice"),
        ("link(\"Note\").contains()", "contains"),
    ] {
        let err = ev(expr).expect_err("a missing required argument must be an error");
        assert_eq!(
            err.display_message(),
            format!("Cannot call function \"{function}\", not enough arguments. (construct: {function})"),
            "{expr}"
        );
    }
}

/// A null that was passed in a required position answers null -- it is not an
/// error and not a coerced value.
#[test]
fn a_null_required_argument_answers_null() {
    for expr in [
        "\"active\".contains(null)",
        "\"active\".startsWith(null)",
        "\"active\".endsWith(null)",
        "\"active\".repeat(null)",
        "\"active\".slice(null, 2)",
        "\"active\".split(null)",
        "\"active\".replace(null, \"x\")",
        "\"active\".replace(\"a\", null)",
        "\"active\".containsAll(\"a\", null)",
        "\"active\".containsAny(null)",
        "list([1, 2, 3]).slice(null, 2)",
        // Link extends String in Obsidian, so this resolves to String's
        // `contains` with its plain-String `value` parameter.
        "link(\"Note\").contains(null)",
    ] {
        assert!(
            matches!(
                ev(expr).expect("a null argument is not an error"),
                BasesValue::Null
            ),
            "{expr} answers null"
        );
    }
}

/// A null in an OPTIONAL position is absent, not null and not zero.
///
/// Obsidian's tables: `slice` is `[self, start, end]` with `end` optional, and
/// `split` is `[self, separator, n]` with `n` optional.
#[test]
fn a_null_optional_argument_is_absent() {
    assert_eq!(s("\"active\".slice(0, null)"), "active");
    assert_eq!(n("\"a,b,c\".split(\",\", null).length"), 3.0);
    assert_eq!(s("list([1, 2, 3]).slice(0, null).length"), "3");
    // And the argument can be left out entirely, which is the same thing.
    assert_eq!(s("\"active\".slice(0)"), "active");
    assert_eq!(n("\"a,b,c\".split(\",\").length"), 3.0);
}

/// A variadic position is a sequence of required arguments, so a null anywhere
/// in it answers the call null -- while zero arguments are allowed and mean
/// `every`/`some` over an empty list.
#[test]
fn a_null_variadic_argument_answers_null() {
    assert!(b("\"abc\".containsAll()"));
    assert!(!b("\"abc\".containsAny()"));
    for expr in [
        "\"abc\".containsAll(null)",
        "\"abc\".containsAll(\"a\", null)",
        "\"abc\".containsAny(null)",
        "\"abc\".containsAny(\"a\", null)",
    ] {
        assert!(
            matches!(
                ev(expr).expect("a null argument is not an error"),
                BasesValue::Null
            ),
            "{expr} answers null"
        );
    }
}

/// `list.contains` takes the `text` union, which accepts null, so a null needle
/// is compared like any other value rather than answering null. Its comparison
/// is the list's own `includes`, which is why a list holding null matches.
#[test]
fn a_null_needle_in_a_list_is_compared_rather_than_answering_null() {
    assert!(!b("list([1, 2]).contains(null)"));
    assert!(b("list([null]).contains(null)"));
}

/// `contains` is CASE-INSENSITIVE, while `==`, `startsWith` and `endsWith`
/// are case-SENSITIVE.
///
/// Measured against the Obsidian CLI on 1.14.4, and the asymmetry is
/// Obsidian's: `contains` folds the case of BOTH sides, while the other two
/// are plain `startsWith`/`endsWith` with no folding.
///
/// The temptation is to "make all four agree" by folding `startsWith` and
/// `endsWith` too — and that would introduce a divergence rather than remove
/// one. `==` and `startsWith`/`endsWith` already agree with Obsidian, and
/// `contains` is the one that did not.
#[test]
fn contains_folds_case_and_the_other_string_methods_do_not() {
    // `contains` folds both sides.
    assert!(b("\"Hello World\".contains(\"hello\")"));
    assert!(b("\"Hello World\".contains(\"HELLO\")"));
    assert!(b("\"Hello World\".contains(\"Hello World\")"));
    assert!(!b("\"Hello World\".contains(\"goodbye\")"));

    // `startsWith` and `endsWith` do NOT fold.
    assert!(!b("\"Hello World\".startsWith(\"hello\")"));
    assert!(!b("\"Hello World\".startsWith(\"HELLO\")"));
    assert!(b("\"Hello World\".startsWith(\"Hello\")"));
    assert!(!b("\"Hello World\".endsWith(\"world\")"));
    assert!(!b("\"Hello World\".endsWith(\"WORLD\")"));
    assert!(b("\"Hello World\".endsWith(\"World\")"));

    // `==` does not fold either.
    assert!(!b("\"Hello World\" == \"hello world\""));
    assert!(b("\"Hello World\" == \"Hello World\""));
}

/// The variadic forms fold the case too, and have NO empty-needle guard.
///
/// The single-needle `contains("")` is false; `containsAll("")` and
/// `containsAny("")` are true. That is not a subtlety of ours — Obsidian's
/// `contains` carries a `"" !== needle` guard and the variadic forms do not.
#[test]
fn contains_all_and_any_fold_case_and_keep_the_empty_needle() {
    assert!(b("\"Hello World\".containsAll(\"hello\", \"world\")"));
    assert!(b("\"Hello World\".containsAll(\"HELLO\", \"WORLD\")"));
    assert!(!b("\"Hello World\".containsAll(\"hello\", \"goodbye\")"));
    assert!(b("\"Hello World\".containsAny(\"zzz\", \"hello\")"));
    assert!(b("\"Hello World\".containsAny(\"zzz\", \"HELLO\")"));
    assert!(!b("\"Hello World\".containsAny(\"zzz\", \"goodbye\")"));

    // The empty needle: guarded on `contains` alone.
    assert!(!b("\"Hello World\".contains(\"\")"));
    assert!(b("\"Hello World\".containsAll(\"\")"));
    assert!(b("\"Hello World\".containsAny(\"\")"));
}

/// A property whose value is a LINK inherits the string `contains` rules.
///
/// A Link in Obsidian is a subclass of String whose data is the RAW TARGET,
/// so `linkish.contains("hello")` folds the case and `linkish.contains("")` is
/// false — the string rules — while `linkish.contains("[")` is false, because
/// the brackets belong to the rendering rather than to the data.
///
/// `startsWith` is NOT a Link method in Obsidian (a Link is a String
/// subclass, so it inherits the String methods, and the Bases expression
/// language dispatches on the DECLARED type) — our implementation refuses it
/// too, and the CLI says the same: "Cannot find function startsWith on type
/// List" for a list, and a Link answers on its own terms.
#[test]
fn a_link_receiver_inherits_the_string_contains_rules() {
    let ctx = make_ctx(Some(AccessorsOptions::props(&[(
        "linkish",
        BasesValue::Link {
            target: "Hello World".into(),
            display: None,
            resolved: Some("Hello World".into()),
        },
    )])));

    assert!(b_in("linkish.contains(\"hello\")", &ctx));
    assert!(b_in("linkish.contains(\"HELLO\")", &ctx));
    assert!(b_in("linkish.contains(\"Hello World\")", &ctx));
    // The raw target, not the `[[...]]` rendering.
    assert!(!b_in("linkish.contains(\"[\")", &ctx));
    assert!(!b_in("linkish.contains(\"\")", &ctx));
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

/// Cross-type coercion, measured against `obsidian base:query` on Obsidian
/// 1.13.7 rather than inferred.
///
/// Obsidian's `DataValue.looseEquals` compares raw data with JavaScript `==`, so
/// a string coerces to a number and a checkbox is 1 or 0. These are the CLI's
/// answers, one note per property.
#[test]
fn scalars_coerce_across_types_like_javascript() {
    let ctx = make_ctx(Some(AccessorsOptions::props(&[
        ("s", BasesValue::String(String::new())),
        ("n", BasesValue::Number(5.0)),
        ("b", BasesValue::Bool(true)),
        ("bf", BasesValue::Bool(false)),
        ("abc", BasesValue::String("abc".into())),
    ])));

    // A string that spells a number equals that number, and the empty string is
    // 0, which is why `"" == 0` and `"" == false` both hold.
    assert!(b_in("s == 0", &ctx));
    assert!(b_in("s == false", &ctx));
    assert!(!b_in("s == 1", &ctx));
    assert!(b_in("n == \"5\"", &ctx));
    // A string that will not convert is never equal to a number.
    assert!(!b_in("abc == 5", &ctx));
    assert!(!b_in("abc == 0", &ctx));
    // A checkbox is 1 or 0, so it crosses with a number but not with its own
    // spelling: `true == "true"` is false, because "true" is NaN as a number.
    assert!(b_in("b == 1", &ctx));
    assert!(!b_in("b == 2", &ctx));
    assert!(!b_in("b == \"true\"", &ctx));
    assert!(b_in("bf == 0", &ctx));
    assert!(!b_in("bf == 1", &ctx));
}

/// Ordering coerces a number against a string the same way `==` does.
#[test]
fn ordering_coerces_a_number_against_a_string() {
    let ctx = make_ctx(Some(AccessorsOptions::props(&[
        ("n", BasesValue::Number(5.0)),
        ("abc", BasesValue::String("abc".into())),
    ])));

    assert!(b_in("n > \"3\"", &ctx));
    assert!(b_in("n >= \"3\"", &ctx));
    assert!(b_in("n < \"10\"", &ctx));
    assert!(!b_in("n > 10", &ctx));
    // ToNumber("abc") is NaN, so every operator against it is false.
    assert!(!b_in("abc > 1", &ctx));
    assert!(!b_in("abc < 1", &ctx));
    assert!(!b_in("abc >= 1", &ctx));
    assert!(!b_in("abc <= 1", &ctx));
}

/// A null operand makes an ordering falsy rather than true.
///
/// `compare` ranks Null below everything, because a sort needs nulls to go
/// somewhere. Obsidian's ComparisonExpr returns Null instead, and null is not
/// truthy, so `due < today()` drops a note with no `due` rather than including it.
#[test]
fn an_ordering_against_null_is_falsy() {
    let ctx = make_ctx(Some(AccessorsOptions::props(&[(
        "n",
        BasesValue::Number(5.0),
    )])));

    assert!(b_in("n < 10", &ctx));
    assert!(!b_in("missing < 10", &ctx));
    assert!(!b_in("n < missing", &ctx));
    assert!(!b_in("missing >= 0", &ctx));
    assert!(!b_in("missing <= 0", &ctx));
}

/// Only null equals null: an empty string and an empty list both have values.
///
/// This is what makes `!= null` the test for "this property is present at all",
/// which is the idiom the corpus and user bases rely on.
#[test]
fn only_null_equals_null() {
    let ctx = make_ctx(Some(AccessorsOptions::props(&[
        ("empty", BasesValue::String(String::new())),
        ("lst", BasesValue::List(vec![])),
    ])));

    assert!(b_in("missing == null", &ctx));
    assert!(!b_in("empty == null", &ctx));
    assert!(b_in("empty != null", &ctx));
    assert!(!b_in("lst == null", &ctx));
    assert!(b_in("lst != null", &ctx));
}

/// A list equals a bare scalar only when it holds exactly one element.
///
/// Regression guard for the flattening rule: wrapping the pair in lists instead
/// re-enters the same branch forever, because the wrapper is itself a list.
#[test]
fn a_list_equals_a_scalar_only_when_it_has_one_element() {
    let ctx = make_ctx(Some(AccessorsOptions::props(&[
        (
            "one",
            BasesValue::List(vec![BasesValue::String("a".into())]),
        ),
        (
            "two",
            BasesValue::List(vec![
                BasesValue::String("a".into()),
                BasesValue::String("b".into()),
            ]),
        ),
        ("empty", BasesValue::List(vec![])),
    ])));

    assert!(b_in("one == \"a\"", &ctx));
    assert!(!b_in("one == \"b\"", &ctx));
    assert!(!b_in("two == \"a\"", &ctx));
    assert!(b_in("two != \"a\"", &ctx));
    assert!(!b_in("empty == 0", &ctx));
    assert!(b_in("empty != 0", &ctx));
    assert!(!b_in("one == 1", &ctx));
}

// ---------------------------------------------------------------------------
// Date equality: a DAY on either side compares by calendar day
//
// `make_ctx` pins the note's mtime to 2026-01-01T00:00Z, and `today()` reads the
// clock from that mtime, so these assertions are about the rule rather than
// about the day the suite runs on.
// ---------------------------------------------------------------------------

#[test]
fn a_date_time_equals_today_on_the_day_it_falls_on() {
    let ctx = make_ctx(None);
    assert!(b_in("date(\"2026-01-01T17:30\") == today()", &ctx));
    assert!(b_in("today() == date(\"2026-01-01T17:30\")", &ctx));
}

#[test]
fn a_date_only_value_equals_today() {
    let ctx = make_ctx(None);
    assert!(b_in("date(\"2026-01-01\") == today()", &ctx));
}

#[test]
fn a_date_on_another_day_does_not_equal_today() {
    let ctx = make_ctx(None);
    assert!(!b_in("date(\"2026-01-02T09:00\") == today()", &ctx));
    assert!(!b_in("date(\"2025-12-31\") == today()", &ctx));
}

#[test]
fn two_moments_on_the_same_day_are_not_equal() {
    let ctx = make_ctx(None);
    assert!(!b_in(
        "date(\"2026-01-01T17:30\") == date(\"2026-01-01T18:30\")",
        &ctx
    ));
    assert!(b_in(
        "date(\"2026-01-01T17:30\") == date(\"2026-01-01T17:30\")",
        &ctx
    ));
}

#[test]
fn a_day_equals_any_moment_inside_it() {
    let ctx = make_ctx(None);
    assert!(b_in(
        "date(\"2026-01-01\") == date(\"2026-01-01T00:00:00\")",
        &ctx
    ));
    assert!(b_in(
        "date(\"2026-01-01T23:59:59\") == date(\"2026-01-01\")",
        &ctx
    ));
}

#[test]
fn a_date_on_another_day_is_not_equal() {
    let ctx = make_ctx(None);
    assert!(b_in("date(\"2026-01-02T09:00\") != today()", &ctx));
    assert!(!b_in("date(\"2026-01-01T09:00\") != today()", &ctx));
}

#[test]
fn tomorrow_is_a_day_and_equals_that_day() {
    let ctx = make_ctx(None);
    assert!(b_in("date(\"2026-01-02T09:00\") == today() + \"1d\"", &ctx));
    assert!(b_in("date(\"2026-01-02\") == today() + \"1d\"", &ctx));
    assert!(!b_in(
        "date(\"2026-01-03T09:00\") == today() + \"1d\"",
        &ctx
    ));
}

/// `contains` on a list of dates takes the same day rule `==` does, in both
/// directions, and no needle type is a type error.
///
/// Obsidian 1.14.4's bundle registers `list.contains` with
/// `params: [self: List, value: "text" union over Any]` and
/// `operate(e, t) { return e.includes(t) }`; `ListValue.includes` is
/// `aK.looseEquals(e, this.get(n))`, and `DateValue.looseEquals` compares
/// `date.getTime()` when BOTH sides carry a time and `dateOnly()` otherwise.
/// So a date-only needle matches every moment on its day, and a Date needle is
/// never rejected. The TypeScript tree's exact-millisecond Date arm is the
/// divergence from that, not this.
///
/// The String-needle half is here too: it reaches the same equality through
/// `values_equal`, so the two needle spellings must agree with the operator.
#[test]
fn contains_reads_a_date_needle_the_way_equality_does() {
    let ctx = make_ctx(None);
    // Two moments compare by instant.
    assert!(b_in(
        "[date(\"2026-01-05T17:30\")].contains(date(\"2026-01-05T17:30\"))",
        &ctx
    ));
    assert!(!b_in(
        "[date(\"2026-01-05T17:30\")].contains(date(\"2026-01-05T17:31\"))",
        &ctx
    ));
    // A DAY on either side matches every moment on that day.
    assert!(b_in(
        "[date(\"2026-01-05T17:30\")].contains(date(\"2026-01-05\"))",
        &ctx
    ));
    assert!(b_in(
        "[date(\"2026-01-05\")].contains(date(\"2026-01-05T17:30\"))",
        &ctx
    ));
    // A day is still not a blanket match.
    assert!(!b_in(
        "[date(\"2026-01-05T17:30\")].contains(date(\"2026-01-06\"))",
        &ctx
    ));
    // The string spelling of the same day agrees, and a word is not a date.
    assert!(b_in(
        "[date(\"2026-01-05T17:30\")].contains(\"2026-01-05\")",
        &ctx
    ));
    assert!(!b_in(
        "[date(\"2026-01-05T17:30\")].contains(\"not a date\")",
        &ctx
    ));
    // The variadic forms share `includes`, so they must agree with it.
    assert!(b_in(
        "[date(\"2026-01-05T17:30\")].containsAll([date(\"2026-01-05\")])",
        &ctx
    ));
    // And the operator agrees with the list: same two values, same answer.
    assert!(b_in(
        "date(\"2026-01-05T17:30\") == date(\"2026-01-05\")",
        &ctx
    ));
}
