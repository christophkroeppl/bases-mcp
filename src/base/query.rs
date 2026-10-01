//! The base query pipeline.
//!
//! Stages run in the order Obsidian documents: formulas, then global filters AND
//! view filters, then sort, group, limit. Formulas are evaluated ONCE per note
//! rather than once per (note, column) pair, and are topologically ordered so a
//! formula may reference another formula and see its value.
//!
//! Two ordering rules here are load-bearing and are commented where they
//! happen: an unsorted view orders by `file.name` and not by path, and row order
//! is part of the result rather than an accident of the map iteration.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;

use crate::ast::Node;
use crate::error::{BasesError, Result};
use crate::evaluator::{evaluate, evaluate_expression, EvalContext, ThisContext};
use crate::parser::{parse, try_parse};
use crate::value::{compare, BasesValue};
use crate::vault::source::BASE_EXT;
use crate::vault::Vault;

use super::parse::{select_view, BaseFile, BaseView, Direction, FilterNode, SortEntry};

/// One note that matched, with everything the pipeline computed for it.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedRow {
    pub path: String,
    /// Computed formula values, keyed by formula name.
    pub formula: BTreeMap<String, BasesValue>,
    /// Cached property values, keyed by canonical Property ID.
    pub values: BTreeMap<String, BasesValue>,
}

/// Rows sharing one `groupBy` value.
#[derive(Debug, Clone, PartialEq)]
pub struct QueryGroup {
    /// The group key, as a display string.
    pub key: String,
    pub rows: Vec<ResolvedRow>,
}

/// Everything one view resolved to.
#[derive(Debug, Clone, PartialEq)]
pub struct QueryResult {
    pub base_path: String,
    pub view: BaseView,
    pub rows: Vec<ResolvedRow>,
    /// Present only when the view has a `groupBy`.
    pub groups: Option<Vec<QueryGroup>>,
    /// Pre-limit match count, which is what the UI's "N of M" reports.
    pub total: usize,
    /// The Host note bound to `this`, when one was supplied.
    pub context: Option<String>,
    /// Problems that did not prevent a result, e.g. one bad filter.
    pub warnings: Vec<String>,
}

/// What to resolve, and what to bind `this` to.
#[derive(Debug, Clone, Default)]
pub struct QueryOptions {
    /// The Host note path; binds `this`.
    pub context: Option<String>,
    pub view: Option<String>,
}

/// Resolve a base's rows.
///
/// Throws rather than returning an empty result when a base references `this`
/// and no host was supplied: the Obsidian CLI returns `[]` in that case, and
/// silently mirroring that is exactly what makes the behaviour undebuggable.
pub fn query_base(
    vault: &Rc<Vault>,
    base_path: &str,
    base: &BaseFile,
    options: &QueryOptions,
) -> Result<QueryResult> {
    let view = select_view(base, options.view.as_deref())?;
    let this_value = resolve_host_note(vault, options.context.as_deref())?;

    // Formulas first: they may reference each other, so order them.
    let formula_order = order_formulas(&base.formulas)?;
    let mut compiled: Vec<Node> = Vec::with_capacity(formula_order.len());
    for name in &formula_order {
        let node = parse(&base.formulas[name]).map_err(|error| {
            BasesError::new(format!(
                "Formula \"{name}\" failed to parse: {}",
                error.display_message()
            ))
            .with_construct(name)
            .with_note(base_path)
        })?;
        compiled.push(node);
    }

    // Pre-parse the filters once rather than per note.
    let global_filter = compile_filter(base.filters.as_ref(), "filters", base_path)?;
    let view_filter = compile_filter(
        view.filters.as_ref(),
        &format!("views.{}.filters", view.name),
        base_path,
    )?;

    let mut rows: Vec<ResolvedRow> = Vec::new();
    for note_path in vault.note_paths() {
        let mut ctx = context_for(vault, &note_path, BTreeMap::new(), this_value.as_ref());
        // Every formula, once, for this note. `ctx.formula` is filled as we go so
        // a formula referencing another sees the value already computed for it.
        for (name, node) in formula_order.iter().zip(&compiled) {
            ctx.formula.insert(name.clone(), evaluate(node, &ctx)?);
        }
        if let Some(filter) = &global_filter {
            if !run_filter(filter, &ctx)? {
                continue;
            }
        }
        if let Some(filter) = &view_filter {
            if !run_filter(filter, &ctx)? {
                continue;
            }
        }
        rows.push(ResolvedRow {
            path: note_path,
            formula: ctx.formula,
            values: BTreeMap::new(),
        });
    }

    let total = rows.len();

    // Cache the property values each row needs, after filtering: a note that
    // matched nothing never pays for them.
    let needed = collect_properties(base, view);
    for row in &mut rows {
        let ctx = context_for(vault, &row.path, row.formula.clone(), this_value.as_ref());
        for id in &needed {
            row.values.insert(id.clone(), resolve_property(id, &ctx)?);
        }
    }

    // Sort. `sort` is authoritative; `groupBy` then applies within groups.
    sort_rows(&mut rows, &sort_spec(view));

    let groups = view
        .group_by
        .as_ref()
        .map(|group| group_rows(&rows, &group.property, group.direction));

    // Limit is a TOTAL across all groups, matching the UI, so groups are trimmed in
    // order until the budget runs out rather than each getting the full limit.
    let budget = view
        .limit
        .filter(|limit| *limit >= 0.0)
        .map(|limit| limit as usize);
    let (rows, groups) = apply_limit(rows, groups, budget);

    Ok(QueryResult {
        base_path: base_path.to_string(),
        view: view.clone(),
        rows,
        groups,
        total,
        context: options.context.clone(),
        warnings: Vec::new(),
    })
}

// ---------------------------------------------------------------------------
// `this`
// ---------------------------------------------------------------------------

/// Bind `this` to the Host note, or refuse the value in the one voice every
/// caller uses.
///
/// Absent (`None`) is the one value that is not an error, because it is how a
/// caller says "this base does not need a host note". Every other value must name
/// a note, and each way of failing to is named for what it actually is: a
/// `.base` and a folder both EXIST in the vault, so reporting either as missing
/// sends the agent hunting for a typo in a file it can already see.
pub fn resolve_host_note(vault: &Rc<Vault>, context: Option<&str>) -> Result<Option<ThisContext>> {
    let Some(context) = context else {
        return Ok(None);
    };
    if context.trim().is_empty() {
        return Err(bad_context(context, "is empty"));
    }

    // `resolve` indexes notes only, so a value that lands here resolved to
    // nothing: it is a path, but not to a note.
    let resolved = vault
        .resolve(context)
        .filter(|path| vault.note(path).is_some());
    let Some(resolved) = resolved else {
        return Err(bad_context(context, &describe_non_note(vault, context)));
    };
    let record = vault.note(&resolved).expect("a resolved path is indexed");
    Ok(Some(ThisContext {
        file: vault.file_value(&resolved),
        note: (*record.frontmatter).clone(),
    }))
}

/// What the value turned out to be, for the message that refuses it.
fn describe_non_note(vault: &Vault, context: &str) -> String {
    // A trailing slash names the same folder as its absence, and an agent that got
    // `Projects/` deserves to be told it is a folder rather than that it does not
    // exist -- the same file it just named without the slash.
    let wanted = context.strip_suffix('/').unwrap_or(context);

    // A folder is checked before a base, because `Tickets` is both the folder and
    // the base's stem; the folder is what an agent naming it almost always means,
    // and it is the reading that is true either way.
    let prefix = format!("{wanted}/");
    let is_folder = vault
        .note_paths()
        .iter()
        .chain(vault.base_paths().iter())
        .any(|path| path.starts_with(&prefix));
    if is_folder {
        return format!("\"{context}\" is a folder, and a folder is never a Host note");
    }

    let is_base = vault
        .base_paths()
        .iter()
        .any(|path| path == wanted || *path == format!("{wanted}{BASE_EXT}"));
    if is_base {
        return format!("\"{context}\" is a .base file, and a Base is never a Host note");
    }

    format!("\"{context}\" does not exist in this vault")
}

/// The one refusal, naming the field, the value and the expectation.
///
/// `note` carries the offending path so a client can act on it without parsing
/// prose. The tail is the same for every cause because the fix is the same: pass
/// the Host note, or drop the field when the base does not need one.
fn bad_context(context: &str, what: &str) -> BasesError {
    BasesError::new(format!(
        "`context` {what}. It must be the path of the Host note that binds `this` -- the note a \
         Base is embedded in, e.g. \"Projects/SomeProject.md\" -- or omitted for a Base that does \
         not reference `this`."
    ))
    .with_note(context)
}

// ---------------------------------------------------------------------------
// Filters
// ---------------------------------------------------------------------------

/// A filter tree with every expression already parsed.
enum CompiledFilter {
    Expression(Node),
    And(Vec<CompiledFilter>),
    Or(Vec<CompiledFilter>),
    Not(Vec<CompiledFilter>),
}

fn compile_filter(
    node: Option<&FilterNode>,
    where_: &str,
    base_path: &str,
) -> Result<Option<CompiledFilter>> {
    let Some(node) = node else { return Ok(None) };
    Ok(Some(match node {
        FilterNode::Expression(source) => {
            CompiledFilter::Expression(parse(source).map_err(|error| {
                BasesError::new(format!(
                    "Filter at {where_} failed to parse: {}",
                    error.display_message()
                ))
                .with_note(base_path)
            })?)
        }
        FilterNode::And(children) => {
            CompiledFilter::And(compile_children(children, where_, base_path)?)
        }
        FilterNode::Or(children) => {
            CompiledFilter::Or(compile_children(children, where_, base_path)?)
        }
        FilterNode::Not(children) => {
            CompiledFilter::Not(compile_children(children, where_, base_path)?)
        }
    }))
}

fn compile_children(
    children: &[FilterNode],
    where_: &str,
    base_path: &str,
) -> Result<Vec<CompiledFilter>> {
    children
        .iter()
        .map(|child| compile_filter(Some(child), where_, base_path))
        .map(|compiled| compiled.map(|c| c.expect("a child filter is never absent")))
        .collect()
}

/// Evaluate a compiled filter.
///
/// `not` is NAND -- "none of these are true" -- which is what the docs specify and
/// what makes a six-sibling `not:` exclude a note matching any one of them.
fn run_filter(filter: &CompiledFilter, ctx: &EvalContext) -> Result<bool> {
    match filter {
        CompiledFilter::Expression(node) => Ok(evaluate(node, ctx)?.is_truthy()),
        CompiledFilter::And(children) => {
            for child in children {
                if !run_filter(child, ctx)? {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        CompiledFilter::Or(children) => {
            for child in children {
                if run_filter(child, ctx)? {
                    return Ok(true);
                }
            }
            Ok(false)
        }
        CompiledFilter::Not(children) => {
            for child in children {
                if run_filter(child, ctx)? {
                    return Ok(false);
                }
            }
            Ok(true)
        }
    }
}

// ---------------------------------------------------------------------------
// Formulas
// ---------------------------------------------------------------------------

/// Topologically order formulas so a formula may reference another.
///
/// Cycle detection is here rather than left to the evaluator: a formula set that
/// references itself has no values at all, and evaluating it anyway would either
/// recurse forever or silently read an empty namespace.
///
/// The traversal starts from each name in SORTED order, because
/// [`BaseFile::formulas`] is a `BTreeMap` rather than an insertion-ordered map.
/// The TypeScript original walked its object in declaration order, so the chain
/// it reported for a cycle is a rotation of ours: both name the same formulas and
/// the same closing name, and neither is more correct. Making it declaration
/// ordered would mean an insertion-ordered map in [`BaseFile`] purely to move a
/// rotation inside a refusal message, which is not a trade worth making.
pub fn order_formulas(formulas: &BTreeMap<String, String>) -> Result<Vec<String>> {
    let names: Vec<&str> = formulas.keys().map(String::as_str).collect();
    let deps: BTreeMap<&str, BTreeSet<&str>> = names
        .iter()
        .map(|name| {
            let found = names
                .iter()
                .filter(|other| *other != name && references_formula(&formulas[*name], other))
                .copied()
                .collect();
            (*name, found)
        })
        .collect();

    let mut out: Vec<String> = Vec::with_capacity(names.len());
    let mut done: BTreeSet<&str> = BTreeSet::new();
    for name in &names {
        visit(name, &deps, &mut done, &mut Vec::new(), &mut out)?;
    }
    Ok(out)
}

/// Depth-first, recording the path so a cycle reports the chain that closed it.
///
/// `stack` holds the names on the way down, which is what turns "there is a
/// cycle" into "these formulas are a cycle".
fn visit<'a>(
    name: &'a str,
    deps: &BTreeMap<&'a str, BTreeSet<&'a str>>,
    done: &mut BTreeSet<&'a str>,
    stack: &mut Vec<&'a str>,
    out: &mut Vec<String>,
) -> Result<()> {
    if done.contains(name) {
        return Ok(());
    }
    if stack.contains(&name) {
        let mut chain = stack.clone();
        chain.push(name);
        return Err(BasesError::new(format!(
            "Formulas form a circular reference: {}",
            chain.join(" -> ")
        ))
        .with_construct(name));
    }
    stack.push(name);
    for dep in deps.get(name).into_iter().flatten() {
        visit(dep, deps, done, stack, out)?;
    }
    stack.pop();
    done.insert(name);
    out.push(name.to_string());
    Ok(())
}

/// Does `expr` reference `formula.<name>` (or a bare `name` that is a formula)?
fn references_formula(expr: &str, name: &str) -> bool {
    // The substring test first, because `formula.ppu` parses as a member access
    // whose OBJECT is `formula`, and an AST walk alone would not see the name.
    if expr.contains(&format!("formula.{name}")) {
        return true;
    }
    try_parse(expr).is_some_and(|ast| ast.mentions_formula(name))
}

// ---------------------------------------------------------------------------
// Property IDs
// ---------------------------------------------------------------------------

/// Every Property ID the pipeline must resolve for the row cache.
fn collect_properties(base: &BaseFile, view: &BaseView) -> BTreeSet<String> {
    let mut needed: BTreeSet<String> = base.properties.keys().map(|id| canonical(id)).collect();
    needed.extend(view.order.iter().flatten().map(|id| canonical(id)));
    if let Some(group) = &view.group_by {
        needed.insert(canonical(&group.property));
    }
    needed.extend(view.summaries.iter().flatten().map(|(id, _)| canonical(id)));
    needed.extend(
        view.sort
            .iter()
            .flatten()
            .map(|entry| canonical(&entry.property)),
    );
    needed.insert("file.name".to_string());
    needed.insert("file.path".to_string());
    needed
}

/// Normalise a Property ID: a bare identifier is a note property.
///
/// Obsidian's own UI rewrites bare keys to the prefixed form, so real vaults
/// contain both.
pub fn canonical(id: &str) -> String {
    let trimmed = id.trim();
    for prefix in ["note.", "file.", "formula."] {
        if trimmed.starts_with(prefix) {
            return trimmed.to_string();
        }
    }
    format!("note.{trimmed}")
}

/// Resolve one Property ID against a row's context.
pub fn resolve_property(id: &str, ctx: &EvalContext) -> Result<BasesValue> {
    let prop = canonical(id);
    if let Some(name) = prop.strip_prefix("formula.") {
        return Ok(ctx.formula.get(name).cloned().unwrap_or(BasesValue::Null));
    }
    // `file.*` members are handled natively by the FileValue; expression syntax
    // covers them too (e.g. `file.name`), so reuse the evaluator.
    evaluate_expression(&prop, ctx)
}

/// The evaluation context for one note.
fn context_for(
    vault: &Rc<Vault>,
    path: &str,
    formula: BTreeMap<String, BasesValue>,
    this_value: Option<&ThisContext>,
) -> EvalContext {
    let mut ctx = EvalContext::new(vault.file_value(path));
    ctx.note = vault
        .note(path)
        .map(|record| (*record.frontmatter).clone())
        .unwrap_or_default();
    ctx.formula = formula;
    ctx.this_value = this_value.cloned();
    ctx
}

// ---------------------------------------------------------------------------
// Ordering
// ---------------------------------------------------------------------------

/// The sort a view resolves to, which is never nothing.
///
/// A view with no `sort` is still ordered, and it is NOT path order. Probed on
/// Obsidian 1.13.7: an unsorted view over notes whose names and paths sort
/// differently returns them by `file.name` ascending, regardless of what `order`
/// lists first.
fn sort_spec(view: &BaseView) -> Vec<SortEntry> {
    view.sort
        .clone()
        .filter(|entries| !entries.is_empty())
        .unwrap_or_else(|| {
            vec![SortEntry {
                property: "file.name".to_string(),
                direction: Direction::Asc,
            }]
        })
}

fn sort_rows(rows: &mut [ResolvedRow], sort: &[SortEntry]) {
    rows.sort_by(|a, b| {
        for entry in sort {
            let id = canonical(&entry.property);
            let (Some(av), Some(bv)) = (a.values.get(&id), b.values.get(&id)) else {
                continue;
            };
            // A value that cannot be compared at all is treated as a tie on this
            // key rather than as an error, so one odd cell cannot fail a whole
            // query; the next key, then the path, decides.
            match compare(av, bv) {
                Some(Ordering::Equal) | None => continue,
                Some(ordering) => {
                    return match entry.direction {
                        Direction::Desc => ordering.reverse(),
                        Direction::Asc => ordering,
                    };
                }
            }
        }
        // Deterministic tie-break on path, so equal rows never swap between runs.
        a.path.cmp(&b.path)
    });
}

fn group_rows(rows: &[ResolvedRow], property: &str, direction: Direction) -> Vec<QueryGroup> {
    let id = canonical(property);
    // A `BTreeMap` keys the groups, and `direction` reverses afterwards. Insertion
    // order is therefore irrelevant: the sort is total over distinct keys.
    let mut map: BTreeMap<String, Vec<ResolvedRow>> = BTreeMap::new();
    for row in rows {
        map.entry(group_key(row.values.get(&id)))
            .or_default()
            .push(row.clone());
    }
    let mut groups: Vec<QueryGroup> = map
        .into_iter()
        .map(|(key, mut rows)| {
            // Within each group, the same deterministic path rule as the sort.
            rows.sort_by(|a, b| a.path.cmp(&b.path));
            QueryGroup { key, rows }
        })
        .collect();
    if direction == Direction::Desc {
        groups.reverse();
    }
    groups
}

/// The group key for a value, as a display string.
///
/// `groupBy` ordering is over these strings, so a null and an empty string must
/// land on the same key: they are both "this row has no value here", and putting
/// them in different groups would show one bucket with a header and one without.
fn group_key(value: Option<&BasesValue>) -> String {
    let Some(value) = value else {
        return "(empty)".to_string();
    };
    match value {
        BasesValue::Null => "(empty)".to_string(),
        BasesValue::String(text) if text.is_empty() => "(empty)".to_string(),
        BasesValue::List(items) if items.is_empty() => "(empty)".to_string(),
        BasesValue::List(items) => {
            let mut keys = items
                .iter()
                .map(|item| group_key(Some(item)))
                .collect::<Vec<_>>();
            keys.sort();
            keys.join(", ")
        }
        // A link groups by where it points, so two spellings of one target share a
        // group. Falling back to the written target when unresolved keeps the row
        // somewhere rather than dropping it into "(empty)".
        BasesValue::Link {
            target, resolved, ..
        } => resolved.clone().unwrap_or_else(|| target.clone()),
        // A namespace has no label of its own, so it renders as its content. The
        // TypeScript original stringified it as `[object Object]`, which is a
        // property of `Object.prototype.toString` rather than a decision; the same
        // repair is already recorded for `file.links` in `vault.rs`.
        other => other.to_display_string(),
    }
}

// ---------------------------------------------------------------------------
// Limit
// ---------------------------------------------------------------------------

/// Trim rows to `budget` and report the surviving groups.
///
/// A grouped limit spends ONE budget across every group, so a group that runs out
/// mid-way is kept PARTIAL and the groups after it are dropped. Empty groups are
/// dropped rather than kept as a header with nothing under it.
fn apply_limit(
    rows: Vec<ResolvedRow>,
    groups: Option<Vec<QueryGroup>>,
    budget: Option<usize>,
) -> (Vec<ResolvedRow>, Option<Vec<QueryGroup>>) {
    let Some(budget) = budget else {
        return (rows, groups);
    };
    let Some(groups) = groups else {
        return (rows.into_iter().take(budget).collect(), None);
    };

    let mut kept: Vec<QueryGroup> = Vec::new();
    let mut used = 0usize;
    for group in groups {
        if used >= budget {
            break;
        }
        let kept_rows: Vec<ResolvedRow> = group.rows.into_iter().take(budget - used).collect();
        used += kept_rows.len();
        if !kept_rows.is_empty() {
            kept.push(QueryGroup {
                key: group.key,
                rows: kept_rows,
            });
        }
    }
    let flattened = kept
        .iter()
        .flat_map(|group| group.rows.iter().cloned())
        .collect();
    (flattened, Some(kept))
}
