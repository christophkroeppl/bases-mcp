//! The nesting limit.
//!
//! One Rust stack overflow is not catchable: the process aborts, the MCP client
//! sees the transport drop with no tool result, the only diagnostic is one line
//! on stderr, and every other tool is dead until someone restarts the server.
//! A `.base` file can arrive hand-edited, written by a plugin, or through a bad
//! merge, and any of those can nest a paren as deep as it likes — so the walks
//! that touch one are bounded, and a walk that runs out is a `BasesError` naming
//! the construct and the number.
//!
//! The measurement behind the number is in `src/depth.rs`. What is asserted
//! here is the behaviour: a named refusal at the boundary, a legitimate deep
//! expression still evaluating, and a filter tree nesting 20+ levels still
//! working, because that is the depth real vaults reach and a limit that broke
//! it would be worse than the crash.
//!
//! Nothing here writes to `test/vault`. It is the parity oracle and stays at
//! exactly [`CORPUS_SIZE`] files.

#[allow(dead_code)]
mod common;

use std::collections::BTreeMap;
use std::rc::Rc;

use bases_mcp::ast::{Literal, Node, NodeKind, Span, UnaryOp};
use bases_mcp::base::{parse_base, query_base, BaseFile, QueryOptions};
use bases_mcp::depth::MAX_DEPTH;
use bases_mcp::error::BasesError;
use bases_mcp::evaluator::{evaluate, EvalContext};
use bases_mcp::parser::parse;
use bases_mcp::value::{BasesValue, FileValue};
use bases_mcp::vault::{FsVaultSource, Vault};
use common::{count_files, futures_block_on, vault_dir, CORPUS_SIZE};

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

fn testing_vault() -> Rc<Vault> {
    let source = FsVaultSource::new(vault_dir()).expect("a vault directory");
    let vault = Rc::new(Vault::new(Box::new(source)));
    futures_block_on(vault.load()).expect("the testing vault is readable");
    vault
}

/// The deepest `and:` nesting a real Base is expected to reach.
///
/// 20 is not a round number chosen for a test: nested filter groups are the one
/// place Bases users actually nest by hand, and 20+ is documented as working. It
/// is the case a limit that is too aggressive breaks first, so it is asserted
/// against the real pipeline rather than against the parser.
const REALISTIC_FILTER_DEPTH: usize = 24;

/// An expression nested `depth` levels: `depth - 1` enclosing groups around a
/// leaf. Parens rather than `!` because a paren group is the shape a generated
/// or merged file actually contains.
fn nested_expression(depth: usize) -> String {
    format!("{}1{}", "(".repeat(depth - 1), ")".repeat(depth - 1))
}

/// A `depth`-deep unary AST built by hand rather than parsed.
///
/// The evaluator's guard is the only thing that can catch this, so it has to be
/// provable with a tree no parser could have produced — which is exactly the
/// hole the guard exists to close.
fn nested_node(depth: usize) -> Node {
    let mut node = Node::new(
        NodeKind::Literal(Literal::Number(1.0)),
        Span { start: 0, end: 1 },
    );
    for _ in 0..depth {
        let span = node.span;
        node = Node::new(
            NodeKind::Unary {
                op: UnaryOp::Negate,
                operand: Box::new(node),
            },
            span,
        );
    }
    node
}

/// A `.base` file whose only filter is one expression.
fn base_with_filter(expression: &str) -> String {
    format!(
        "filters:\n  and:\n    - '{expression}'\nviews:\n  - type: table\n    name: One\n    order:\n      - file.name\n"
    )
}

/// A `.base` file whose filter nests `and:` groups `depth` levels deep.
///
/// The indentation is the shape Obsidian's own writer emits: each group's key is
/// indented two past its `-` marker, and that marker's value is a block sequence
/// indented two past the key.
fn base_with_nested_filters(depth: usize) -> String {
    let mut yaml = String::from("filters:\n  and:\n");
    for level in 2..=depth {
        yaml.push_str(&format!("{}- and:\n", " ".repeat(4 * (level - 1))));
    }
    yaml.push_str(&format!(
        "{}- 'file.ext == \"md\"'\n",
        " ".repeat(4 * depth)
    ));
    yaml.push_str("views:\n  - type: table\n    name: Deep\n    order:\n      - file.name\n");
    yaml
}

/// Resolve a `.base` written as text, returning the refusal.
fn refusal_from(yaml: &str) -> BasesError {
    let vault = testing_vault();
    match parse_base("T.base", yaml) {
        Err(error) => error,
        Ok(base) => match query_base(&vault, "T.base", &base, &QueryOptions::default()) {
            Err(error) => error,
            Ok(result) => panic!("expected a refusal, but the query returned {result:?}"),
        },
    }
}

/// The message a refusal carries.
fn message(error: &BasesError) -> String {
    error.display_message()
}

/// A one-note vault's context, for evaluating a hand-built tree.
fn ctx() -> EvalContext {
    let accessors = bases_mcp::value::FileAccessors {
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
    };
    EvalContext::new(FileValue::new("Note.md".to_string(), accessors))
}

// ---------------------------------------------------------------------------
// The limit refuses rather than aborts
// ---------------------------------------------------------------------------

/// The defect itself: a `.base` nested past the limit is a tool error the agent
/// can read, not a dead process.
///
/// `MAX_DEPTH + 1` rather than a depth that overflows. On the unguarded code
/// this assertion fails as a normal test failure — the query returns rows — and
/// a depth that aborted would have taken the whole test runner with it, which is
/// the thing being fixed and makes for a test that cannot report anything.
#[test]
fn an_expression_past_the_limit_is_a_refusal_not_a_dead_process() {
    let error = refusal_from(&base_with_filter(&nested_expression(MAX_DEPTH + 1)));
    assert_eq!(error.construct(), Some("Expression"), "{}", message(&error));
}

/// The refusal says which construct and what the number is, because neither is
/// actionable alone: one says which Base key to edit, the other says how far back
/// to pull it.
#[test]
fn the_refusal_names_the_construct_and_the_limit() {
    let text = message(&refusal_from(&base_with_filter(&nested_expression(
        MAX_DEPTH + 1,
    ))));
    assert!(text.contains("Expression"), "{text}");
    assert!(text.contains(&MAX_DEPTH.to_string()), "{text}");
}

/// The boundary is exact, and pinned from both sides, because a limit that
/// refused one level early would be indistinguishable from one that is simply
/// too aggressive.
#[test]
fn exactly_the_limit_passes_and_one_more_does_not() {
    assert!(
        parse(&nested_expression(MAX_DEPTH)).is_ok(),
        "the limit itself must still parse"
    );
    let error = parse(&nested_expression(MAX_DEPTH + 1)).expect_err("one past the limit refuses");
    assert_eq!(error.construct(), Some("Expression"), "{}", message(&error));
}

// ---------------------------------------------------------------------------
// The limit is not too aggressive
// ---------------------------------------------------------------------------

/// Fifty levels, which is the deepest legitimate expression this project has
/// been asked to accept, still parses AND evaluates.
///
/// Asserted through evaluation rather than through `parse` alone: a parser can
/// return a tree the evaluator then walks deeper than the limit, and only
/// evaluating proves the whole path.
#[test]
fn a_fifty_level_expression_still_evaluates() {
    let source = nested_expression(50);
    let node = parse(&source).expect("50 levels is within the limit");
    let value = evaluate(&node, &ctx()).expect("and it evaluates");
    assert_eq!(value, BasesValue::Number(1.0));
}

/// A hand-written filter is three to five levels deep, and a formula using
/// `!`, calls and member access mixes all the shapes at once. If the limit were
/// counting something other than nesting, this is what would break.
#[test]
fn an_ordinary_filter_is_nowhere_near_the_limit() {
    let vault = testing_vault();
    let base = parse_base(
        "T.base",
        &base_with_filter(
            r#"!(file.ext == "pdf" || file.name.contains("Alpha")) && file.hasTag("ticket")"#,
        ),
    )
    .expect("an ordinary filter parses");
    let result = query_base(&vault, "T.base", &base, &QueryOptions::default())
        .expect("an ordinary filter is nowhere near the limit");
    // Matching notes, not merely no error. Every note in the Testing vault is
    // `.md`, so a filter narrowing on the absence of an extension is vacuous
    // here, and a vacuous filter would satisfy a limit that refused everything.
    assert!(!result.rows.is_empty(), "the filter matched notes");
}

/// Nested filter groups 20+ deep, which real vaults reach, still resolve.
///
/// Asserted through the full pipeline rather than through `normalise_filters`,
/// because the walk that used to be unbounded here is the one the query runs
/// once per note.
#[test]
fn nested_filter_groups_twenty_deep_still_resolve() {
    let vault = testing_vault();
    let base: BaseFile = parse_base("T.base", &base_with_nested_filters(REALISTIC_FILTER_DEPTH))
        .expect("a 24-deep filter tree parses");
    let result = query_base(&vault, "T.base", &base, &QueryOptions::default())
        .expect("a 24-deep filter tree resolves");
    assert!(
        !result.rows.is_empty(),
        "the depth is not the filter's verdict: rows still match"
    );
}

// ---------------------------------------------------------------------------
// The evaluator is bounded independently of the parser
// ---------------------------------------------------------------------------

/// A tree the parser never built is still refused.
///
/// Load-bearing, and the reason the evaluator has its own guard rather than
/// trusting the parser's: `Node` and `Node::new` are public, so `evaluate` can
/// be handed a tree no limit was applied to. Without the guard this would recurse
/// to the bottom of the thread's stack.
#[test]
fn the_evaluator_refuses_a_tree_no_parser_produced() {
    let error = evaluate(&nested_node(MAX_DEPTH + 1), &ctx())
        .expect_err("a hand-built tree past the limit is refused");
    assert_eq!(error.construct(), Some("Expression"), "{}", message(&error));
}

/// A tree of `depth` nested `.map()` calls over a two-element list, each body a
/// `body_depth`-deep unary chain.
///
/// Built by hand because the point is the arithmetic, not the spelling: a source
/// string would be refused by the parser first and the evaluator's share of the
/// budget would never be exercised.
///
/// Two elements rather than one, so the mapped value is distinguishable from the
/// input. `map` over a one-element list returns a one-element list, so the chain
/// below maps `[1]` to `[1]` and any assertion about the result would hold whether
/// or not the body ever ran.
fn nested_map_chain(depth: usize, body_depth: usize) -> Node {
    let mut node = Node::new(
        NodeKind::List(vec![
            Node::new(
                NodeKind::Literal(Literal::Number(1.0)),
                Span { start: 0, end: 1 },
            ),
            Node::new(
                NodeKind::Literal(Literal::Number(1.0)),
                Span { start: 0, end: 1 },
            ),
        ]),
        Span { start: 0, end: 1 },
    );
    for _ in 0..depth {
        let span = node.span;
        node = Node::new(
            NodeKind::Call {
                callee: Box::new(Node::new(
                    NodeKind::Member {
                        object: Box::new(node),
                        property: "map".to_string(),
                    },
                    span,
                )),
                args: vec![nested_node(body_depth)],
            },
            span,
        );
    }
    node
}

/// The lambda path spends from the budget its call site holds, so a deep body
/// inside a `map` is bounded by the same number rather than restarting per
/// element.
///
/// The arithmetic is the assertion, and it is pinned from three sides. A nested
/// `.map` costs exactly one level, not two: `evaluate_call` destructures a `Member`
/// callee instead of walking it, so only the `Call` nodes are ever spent. Three of
/// them plus a 92-deep body and that body's own literal is `3 + 92 + 1` — exactly
/// on the limit. So the boundary falls where it does, and moving one `map` out of
/// the chain buys the body exactly one level back.
///
/// Had the runner started a fresh budget per element — which is what it would do
/// if the call site's `Depth` were not captured by the runner — the deepest body
/// would be the same at every chain length, and the last assertion below would
/// pass. That is the whole defect this is guarding.
#[test]
fn a_lambda_body_spends_the_budget_its_call_site_holds() {
    // An even number of negations is +1 and an odd one is -1, so the two shapes
    // below differ in sign: that is the body's own depth showing up in the answer.
    assert_eq!(
        evaluate(&nested_map_chain(3, MAX_DEPTH - 4), &ctx()).expect("3 + 92 + 1 is the limit"),
        BasesValue::List(vec![BasesValue::Number(1.0), BasesValue::Number(1.0)]),
        "the shape is what makes the refusal below mean something"
    );
    assert_eq!(
        evaluate(&nested_map_chain(2, MAX_DEPTH - 3), &ctx()).expect("2 + 93 + 1 is the limit"),
        BasesValue::List(vec![BasesValue::Number(-1.0), BasesValue::Number(-1.0)]),
        "one fewer map buys the body exactly one level back"
    );

    let error = evaluate(&nested_map_chain(3, MAX_DEPTH - 3), &ctx())
        .expect_err("3 + 93 + 1 is one level past the limit");
    assert_eq!(error.construct(), Some("Expression"), "{}", message(&error));
}

// ---------------------------------------------------------------------------
// What is deliberately not bounded
// ---------------------------------------------------------------------------

/// A long FLAT expression is not nesting, so it is not refused.
///
/// The distinction the whole limit rests on: `[1, 1, 1, ...]` a hundred thousand
/// wide is one level, and a limit that counted tokens would refuse a formula a
/// person could reasonably write.
#[test]
fn a_very_long_flat_expression_still_parses() {
    let source = format!("[{}].length", vec!["1"; 100_000].join(", "));
    let node = parse(&source).expect("width is not depth");
    assert_eq!(
        evaluate(&node, &ctx()).expect("and it evaluates"),
        BasesValue::Number(100_000.0)
    );
}

/// A deeply nested regex is the lexer's business and the `regex` crate's, and
/// neither is a recursive walk over a shape we control. Asserted so that a future
/// change to the lexer that DOES recurse has to notice this file.
#[test]
fn a_deeply_nested_regex_is_the_regex_crates_refusal_not_ours() {
    let depth = 100_000;
    let source = format!("/{}(a{})/", "(".repeat(depth), ")".repeat(depth));
    let error = parse(&source).expect_err("the regex crate refuses it");
    assert_ne!(error.construct(), Some("Expression"), "{}", message(&error));
}

// ---------------------------------------------------------------------------
// The oracle
// ---------------------------------------------------------------------------

/// The Testing vault is still the Testing vault. Pinned here as well as in
/// `tests/base.rs` because this suite is the one that writes the nastiest
/// expressions, and a note that leaked into `test/vault` would change what
/// `obsidian base:query` returns.
#[test]
fn the_testing_vault_is_untouched() {
    assert_eq!(count_files(&vault_dir()), CORPUS_SIZE);
}
