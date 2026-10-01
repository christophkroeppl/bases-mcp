//! Tree-walking evaluator.
//!
//! The semantics here are the part of this project most likely to drift from
//! Obsidian, so each non-obvious rule is commented with the reason. Several
//! encode documented behaviour that Obsidian's own runtime gets wrong
//! (obsidian-help #1095); those are flagged in `docs/divergences.md`.
//!
//! Evaluation is single-threaded and reference-counted. Nothing here allocates a
//! task, and no call into the vault is `async`: a `FileAccessors` closure is a
//! synchronous read of an already-loaded snapshot, so an expression is a pure
//! function of its context and can be tested without a vault at all.

use std::collections::BTreeMap;
use std::rc::Rc;

use crate::ast::{BinOp, Node, NodeKind, UnaryOp};
use crate::error::{BasesError, MissingThisContext, Result};
use crate::parser::parse;
use crate::stdlib::{get_global, get_method, MethodFn};
use crate::value::{compare, values_equal, BasesDate, BasesValue, Duration, FileValue};

/// The host note `this` binds to.
#[derive(Debug, Clone)]
pub struct ThisContext {
    pub file: FileValue,
    pub note: BTreeMap<String, BasesValue>,
}

/// Everything an expression can see.
#[derive(Clone)]
pub struct EvalContext {
    /// This note's frontmatter.
    pub note: BTreeMap<String, BasesValue>,
    pub file: FileValue,
    /// Computed formula columns.
    pub formula: BTreeMap<String, BasesValue>,
    /// The host note. `None` means no host was supplied, and any reference to
    /// `this` is then a hard error.
    pub this_value: Option<ThisContext>,
    /// Lambda parameters (`value`, `index`, `acc`).
    pub bindings: Option<BTreeMap<String, BasesValue>>,
}

impl std::fmt::Debug for EvalContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EvalContext")
            .field("note", &self.note)
            .field("file", &self.file.path)
            .field("formula", &self.formula)
            .field("has_this", &self.this_value.is_some())
            .finish()
    }
}

impl EvalContext {
    pub fn new(file: FileValue) -> Self {
        Self {
            note: BTreeMap::new(),
            file,
            formula: BTreeMap::new(),
            this_value: None,
            bindings: None,
        }
    }

    pub fn with_bindings(mut self, bindings: BTreeMap<String, BasesValue>) -> Self {
        self.bindings = Some(bindings);
        self
    }
}

pub fn evaluate(node: &Node, ctx: &EvalContext) -> Result<BasesValue> {
    match &node.kind {
        NodeKind::Literal(l) => Ok(l.clone().into()),
        NodeKind::List(items) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                out.push(evaluate(item, ctx)?);
            }
            Ok(BasesValue::List(out))
        }
        NodeKind::Identifier(name) => resolve_identifier(name, ctx),
        NodeKind::Unary { op, operand } => evaluate_unary(*op, operand, ctx),
        NodeKind::Binary { op, left, right } => evaluate_binary(*op, left, right, ctx),
        NodeKind::Call { callee, args } => evaluate_call(callee, args, ctx),
        NodeKind::Member { object, property } => evaluate_member(object, property, ctx),
        NodeKind::Index { object, index } => evaluate_index(object, index, ctx),
    }
}

pub fn evaluate_expression(source: &str, ctx: &EvalContext) -> Result<BasesValue> {
    let ast = parse(source)?;
    // Pre-scan for `this`. Without this, a filter like
    // `project.contains(link(this.file.name))` would short-circuit on a missing
    // `project` and quietly evaluate to null -- exactly the silent empty result
    // this project exists to avoid.
    if ctx.this_value.is_none() {
        if let Some(reference) = find_this_reference(&ast) {
            return Err(MissingThisContext { base: reference }.into());
        }
    }
    evaluate(&ast, ctx)
}

/// The first `this` reference in the tree, as text, or `None`.
pub fn find_this_reference(node: &Node) -> Option<String> {
    match &node.kind {
        NodeKind::Identifier(name) => (name == "this").then(|| "this".to_string()),
        NodeKind::Member { object, property } => {
            if let NodeKind::Identifier(name) = &object.kind {
                if name == "this" {
                    return Some(format!("this.{property}"));
                }
            }
            find_this_reference(object)
        }
        NodeKind::Binary { left, right, .. } => {
            find_this_reference(left).or_else(|| find_this_reference(right))
        }
        NodeKind::Unary { operand, .. } => find_this_reference(operand),
        NodeKind::Call { callee, args } => {
            find_this_reference(callee).or_else(|| args.iter().find_map(find_this_reference))
        }
        NodeKind::Index { object, index } => {
            find_this_reference(object).or_else(|| find_this_reference(index))
        }
        NodeKind::List(items) => items.iter().find_map(find_this_reference),
        NodeKind::Literal(_) => None,
    }
}

// ---------------------------------------------------------------------------
// Namespaces that are never a note property
// ---------------------------------------------------------------------------

/// A bare identifier is a note property, so these must be excluded explicitly
/// or a filter would read them as frontmatter. Exported so filter inversion
/// excludes exactly the same names rather than keeping a second copy.
pub const RESERVED: [&str; 7] = ["this", "file", "note", "formula", "value", "index", "acc"];

fn resolve_identifier(name: &str, ctx: &EvalContext) -> Result<BasesValue> {
    if let Some(bindings) = &ctx.bindings {
        if let Some(v) = bindings.get(name) {
            return Ok(v.clone());
        }
    }

    // A dotted name routes to the right namespace. This happens here as well as
    // in `resolve_property_path` because a base may write `file.name` as a
    // single identifier when it is only ever used as a filter operand.
    if name.contains('.') {
        return resolve_property_path(name, ctx);
    }

    Ok(match name {
        "this" => {
            let tc = ctx.this_value.as_ref().ok_or_else(|| MissingThisContext {
                base: "this".into(),
            })?;
            BasesValue::File(Rc::new(this_as_file(tc)))
        }
        "file" => BasesValue::File(Rc::new(ctx.file.clone())),
        "note" => namespace_map(&ctx.note),
        "formula" => namespace_map(&ctx.formula),
        // Bound only inside a lambda. Outside one they are simply absent, which
        // the `isEmpty()` semantics treat as an empty value rather than an error.
        "value" | "index" | "acc" => BasesValue::Null,
        // A bare identifier is a note property (the documented default).
        _ => lookup_path(&namespace_map(&ctx.note), &[name]),
    })
}

/// A namespace presented as a value, so `note.x` and `formula.y` work.
///
/// A non-empty map becomes a List of single-key maps is the wrong shape; instead
/// member access on it goes through `read_member`, which is what `lookup_path`
/// does. An empty map reads as `Null` so `note.nothing.length` is null rather
/// than a type error.
fn namespace_map(map: &BTreeMap<String, BasesValue>) -> BasesValue {
    BasesValue::Namespace(Rc::new(map.clone()))
}

fn resolve_property_path(path: &str, ctx: &EvalContext) -> Result<BasesValue> {
    let (head, rest) = path.split_once('.').unwrap_or((path, ""));
    let parts: Vec<&str> = if rest.is_empty() {
        Vec::new()
    } else {
        rest.split('.').collect()
    };

    Ok(match head {
        "this" => {
            let tc = ctx.this_value.as_ref().ok_or_else(|| MissingThisContext {
                base: path.to_string(),
            })?;
            // `this.note.X` is note-scoped; `this.file.X` is file-scoped.
            if parts.first() == Some(&"note") {
                lookup_path(&namespace_map(&tc.note), &parts[1..])
            } else if parts.first() == Some(&"file") {
                lookup_path(&BasesValue::File(Rc::new(this_as_file(tc))), &parts[1..])
            } else {
                // Otherwise the spelling is ambiguous and real vaults use both
                // senses: `this.path` is a File member while `this.projects` is
                // frontmatter. Prefer frontmatter, then fall back to the File.
                let as_note = lookup_path(&namespace_map(&tc.note), &parts);
                if !matches!(as_note, BasesValue::Null) {
                    as_note
                } else {
                    lookup_path(&BasesValue::File(Rc::new(this_as_file(tc))), &parts)
                }
            }
        }
        "file" => lookup_path(&BasesValue::File(Rc::new(ctx.file.clone())), &parts),
        "note" => lookup_path(&namespace_map(&ctx.note), &parts),
        "formula" => lookup_path(&namespace_map(&ctx.formula), &parts),
        _ => {
            let mut all = vec![head];
            all.extend(parts);
            lookup_path(&namespace_map(&ctx.note), &all)
        }
    })
}

/// Present `this` as a `FileValue` that also answers to the host note's
/// frontmatter keys.
///
/// Real vaults use `this.path`, `this.file.name`, `this.asLink()` and
/// `this.projects` interchangeably, so one value has to satisfy all four. The
/// extra keys ride along in `overrides` rather than through a proxy, because
/// Rust has no equivalent proxy and a wrapper struct would have to be threaded
/// through every file method.
fn this_as_file(tc: &ThisContext) -> FileValue {
    let mut f = tc.file.clone();
    f.overrides = Some(Rc::new(tc.note.clone()));
    f
}

/// Walk a dotted path, returning null for any missing link in the chain.
fn lookup_path(root: &BasesValue, parts: &[&str]) -> BasesValue {
    let mut cur = root.clone();
    for part in parts {
        match &cur {
            BasesValue::List(items) => {
                // `file.tags.contains(...)` style access on a list maps.
                cur = BasesValue::List(items.iter().map(|i| read_member(i, part)).collect());
            }
            _ => cur = read_member(&cur, part),
        }
    }
    cur
}

/// Read a single named member from any value, returning null when absent.
pub fn read_member(target: &BasesValue, name: &str) -> BasesValue {
    match target {
        BasesValue::Null => BasesValue::Null,
        BasesValue::File(f) => read_file_member(f, name),
        BasesValue::Link {
            target: t,
            display,
            resolved,
        } => match name {
            "target" => BasesValue::String(t.clone()),
            "display" => match display {
                Some(d) => BasesValue::String(d.clone()),
                None => BasesValue::Null,
            },
            "path" => match resolved {
                Some(p) => BasesValue::String(p.clone()),
                None => BasesValue::Null,
            },
            _ => BasesValue::Null,
        },
        BasesValue::Date(d) => match name {
            "year" => BasesValue::Number(d.year() as f64),
            "month" => BasesValue::Number(d.month() as f64),
            "day" => BasesValue::Number(d.day() as f64),
            "hour" => BasesValue::Number(d.hour() as f64),
            "minute" => BasesValue::Number(d.minute() as f64),
            "second" => BasesValue::Number(d.second() as f64),
            "millisecond" => BasesValue::Number(d.millisecond() as f64),
            _ => BasesValue::Null,
        },
        BasesValue::Duration(d) => match name {
            "days" => BasesValue::Number(d.days() as f64),
            "hours" => BasesValue::Number(d.hours() as f64),
            "minutes" => BasesValue::Number(d.minutes() as f64),
            "seconds" => BasesValue::Number(d.seconds() as f64),
            "milliseconds" => BasesValue::Number(d.millis as f64),
            "months" => BasesValue::Number(d.months as f64),
            "years" => BasesValue::Number(d.years as f64),
            _ => BasesValue::Null,
        },
        BasesValue::List(items) => {
            if name == "length" {
                BasesValue::Number(items.len() as f64)
            } else {
                BasesValue::Null
            }
        }
        BasesValue::String(s) => {
            if name == "length" {
                // Obsidian counts UTF-16 code units here, so an emoji counts as
                // two. Probed against 1.13.7.
                BasesValue::Number(s.encode_utf16().count() as f64)
            } else {
                BasesValue::Null
            }
        }
        BasesValue::Namespace(map) => match map.get(name) {
            Some(v) => v.clone(),
            None => BasesValue::Null,
        },
        BasesValue::Bool(_) | BasesValue::Number(_) => BasesValue::Null,
    }
}

fn read_file_member(file: &FileValue, name: &str) -> BasesValue {
    // `this.<frontmatter key>` wins over the file member, which is what makes
    // `this.projects` read the host note's frontmatter.
    if let Some(overrides) = &file.overrides {
        if let Some(v) = overrides.get(name) {
            return v.clone();
        }
        if name == "note" {
            return namespace_map(overrides);
        }
    }
    let a = &file.accessors;
    match name {
        // Probed against a live Obsidian 1.13.7: the `file.name` column renders
        // WITHOUT the extension, matching `basename`. Recorded in
        // docs/divergences.md; the docs claim the opposite.
        "name" | "basename" => BasesValue::String(file.basename.clone()),
        "path" => BasesValue::String(file.path.clone()),
        "folder" => BasesValue::String(file.folder.clone()),
        "ext" => BasesValue::String(file.ext.clone()),
        "size" => BasesValue::Number((a.size)() as f64),
        "ctime" => BasesValue::Date((a.ctime)()),
        "mtime" => BasesValue::Date((a.mtime)()),
        "tags" => BasesValue::List((a.tags)()),
        "links" => BasesValue::List((a.links)()),
        "embeds" => BasesValue::List((a.embeds)()),
        "backlinks" => BasesValue::List((a.backlinks)()),
        "properties" => namespace_map(&(a.properties)()),
        "file" => BasesValue::File(Rc::new(file.clone())),
        // Not in the official `file.*` table; Obsidian returns null. We ship it
        // as a documented extension. See docs/divergences.md D3.
        "tasks" => BasesValue::List((a.tasks)()),
        _ => BasesValue::Null,
    }
}

// ---------------------------------------------------------------------------
// Operators
// ---------------------------------------------------------------------------

fn evaluate_unary(op: UnaryOp, operand: &Node, ctx: &EvalContext) -> Result<BasesValue> {
    let v = evaluate(operand, ctx)?;
    Ok(match op {
        UnaryOp::Not => BasesValue::Bool(!v.is_truthy()),
        UnaryOp::Negate => BasesValue::Number(-to_number(&v, "unary -")?),
    })
}

fn evaluate_binary(op: BinOp, left: &Node, right: &Node, ctx: &EvalContext) -> Result<BasesValue> {
    // Short-circuit before evaluating the right side.
    if op == BinOp::And {
        let l = evaluate(left, ctx)?;
        if !l.is_truthy() {
            return Ok(BasesValue::Bool(false));
        }
        return Ok(BasesValue::Bool(evaluate(right, ctx)?.is_truthy()));
    }
    if op == BinOp::Or {
        let l = evaluate(left, ctx)?;
        if l.is_truthy() {
            return Ok(l);
        }
        return evaluate(right, ctx);
    }

    let l = evaluate(left, ctx)?;
    let r = evaluate(right, ctx)?;

    use std::cmp::Ordering;
    Ok(match op {
        BinOp::Eq => BasesValue::Bool(values_equal(&l, &r)),
        BinOp::NotEq => BasesValue::Bool(!values_equal(&l, &r)),
        BinOp::Gt | BinOp::Lt | BinOp::GtEq | BinOp::LtEq => {
            // An incomparable pair is falsy rather than an error, matching
            // Obsidian's behaviour of treating a mismatched type as "not
            // greater than".
            //
            // A null operand is falsy too, and `compare` would rank Null below
            // everything rather than decline: it has to, because a sort needs
            // Null to go somewhere. Obsidian's ComparisonExpr returns Null for
            // any null side, and null is not truthy, so `due < today()` simply
            // drops a note with no `due` instead of including it.
            if matches!(l, BasesValue::Null) || matches!(r, BasesValue::Null) {
                return Ok(BasesValue::Bool(false));
            }
            let ord = compare(&l, &r);
            let b = match ord {
                None => false,
                Some(o) => match op {
                    BinOp::Gt => o == Ordering::Greater,
                    BinOp::Lt => o == Ordering::Less,
                    BinOp::GtEq => o != Ordering::Less,
                    BinOp::LtEq => o != Ordering::Greater,
                    _ => unreachable!("checked above"),
                },
            };
            BasesValue::Bool(b)
        }
        BinOp::Add => add(&l, &r)?,
        BinOp::Sub => subtract(&l, &r)?,
        BinOp::Mul => multiply(&l, &r)?,
        BinOp::Div => divide(&l, &r)?,
        BinOp::Rem => modulo(&l, &r)?,
        _ => {
            return Err(
                BasesError::new(format!("Unsupported operator \"{}\"", op.as_str()))
                    .with_construct(op.as_str()),
            )
        }
    })
}

fn add(a: &BasesValue, b: &BasesValue) -> Result<BasesValue> {
    // Date arithmetic wins over string concatenation, so `date + "1d"` is a date
    // rather than the literal text of a date followed by "1d".
    if matches!(a, BasesValue::Date(_)) || matches!(b, BasesValue::Date(_)) {
        return apply_duration_or_number(a, b, |d, n| {
            BasesValue::Date(BasesDate::from_millis(d.millis() + n))
        });
    }
    if matches!(a, BasesValue::String(_)) || matches!(b, BasesValue::String(_)) {
        return Ok(BasesValue::String(format!(
            "{}{}",
            a.to_display_string(),
            b.to_display_string()
        )));
    }
    if matches!(a, BasesValue::Duration(_)) || matches!(b, BasesValue::Duration(_)) {
        return Ok(combine_durations(a, b, |x, y| x + y));
    }
    if matches!(a, BasesValue::List(_)) || matches!(b, BasesValue::List(_)) {
        let mut out = a.to_list();
        out.extend(b.to_list());
        return Ok(BasesValue::List(out));
    }
    Ok(BasesValue::Number(to_number(a, "+")? + to_number(b, "+")?))
}

fn subtract(a: &BasesValue, b: &BasesValue) -> Result<BasesValue> {
    if let (BasesValue::Date(x), BasesValue::Date(y)) = (a, b) {
        // Obsidian's runtime returns a Duration here even though its docs claim
        // milliseconds. We return a Duration AND make `number()` work on it, so
        // both the documented `((a-b)/86400000).round()` and the real-world
        // `(a-b).days.round(0)` idioms work. See docs/divergences.md.
        return Ok(BasesValue::Duration(Duration::from_millis(
            x.millis() - y.millis(),
        )));
    }
    if matches!(a, BasesValue::Date(_)) {
        return apply_duration_or_number(a, b, |d, n| {
            BasesValue::Date(BasesDate::from_millis(d.millis() - n))
        });
    }
    if matches!(a, BasesValue::Duration(_)) {
        return Ok(combine_durations(a, b, |x, y| x - y));
    }
    Ok(BasesValue::Number(to_number(a, "-")? - to_number(b, "-")?))
}

fn multiply(a: &BasesValue, b: &BasesValue) -> Result<BasesValue> {
    // The documented rule: duration must be on the LEFT for duration x scalar.
    if let BasesValue::Duration(d) = a {
        return Ok(BasesValue::Duration(scale_duration(*d, to_number(b, "*")?)));
    }
    if matches!(b, BasesValue::Duration(_)) {
        return Err(BasesError::new(
            "Multiplying a scalar by a duration is not supported; put the duration on the left \
(e.g. duration(\"1d\") * 2, not 2 * duration(\"1d\"))",
        )
        .with_construct("*"));
    }
    Ok(BasesValue::Number(to_number(a, "*")? * to_number(b, "*")?))
}

fn divide(a: &BasesValue, b: &BasesValue) -> Result<BasesValue> {
    let divisor = to_number(b, "/")?;
    if divisor == 0.0 {
        // Obsidian does not guard this; real vaults guard it themselves
        // (`if(attempted > 0, attempted / attempted, 0)`). Returning 0 keeps a
        // table renderable instead of failing the whole view.
        return Ok(BasesValue::Number(0.0));
    }
    if let BasesValue::Duration(d) = a {
        return Ok(BasesValue::Duration(Duration {
            millis: (d.millis as f64 / divisor) as i64,
            months: (d.months as f64 / divisor) as i64,
            years: (d.years as f64 / divisor) as i64,
        }));
    }
    Ok(BasesValue::Number(to_number(a, "/")? / divisor))
}

fn modulo(a: &BasesValue, b: &BasesValue) -> Result<BasesValue> {
    let divisor = to_number(b, "%")?;
    if divisor == 0.0 {
        return Ok(BasesValue::Number(0.0));
    }
    Ok(BasesValue::Number(to_number(a, "%")? % divisor))
}

fn scale_duration(d: Duration, n: f64) -> Duration {
    Duration {
        millis: (d.millis as f64 * n) as i64,
        months: (d.months as f64 * n) as i64,
        years: (d.years as f64 * n) as i64,
    }
}

fn as_duration(v: &BasesValue) -> Result<Duration> {
    Ok(match v {
        BasesValue::Duration(d) => *d,
        BasesValue::Date(_) => Duration::ZERO,
        BasesValue::String(s) => crate::stdlib::parse_duration_literal(s)?,
        BasesValue::Number(n) => Duration::from_millis(*n as i64),
        _ => Duration::ZERO,
    })
}

fn combine_durations(a: &BasesValue, b: &BasesValue, f: fn(i64, i64) -> i64) -> BasesValue {
    let da = as_duration(a).unwrap_or(Duration::ZERO);
    let db = as_duration(b).unwrap_or(Duration::ZERO);
    BasesValue::Duration(Duration {
        millis: f(da.millis, db.millis),
        months: f(da.months, db.months),
        years: f(da.years, db.years),
    })
}

fn apply_duration_or_number(
    date: &BasesValue,
    other: &BasesValue,
    f: impl Fn(BasesDate, i64) -> BasesValue,
) -> Result<BasesValue> {
    let Some(d) = coerce_date(date) else {
        return Err(BasesError::new(
            "Expected a date on the left of an arithmetic operator",
        ));
    };
    match other {
        BasesValue::Duration(dur) => {
            // Calendar months/years are added by calendar, not by millisecond span.
            if dur.years != 0 || dur.months != 0 {
                let shifted = shift_months(&d, dur.months + dur.years * 12)?;
                return Ok(BasesValue::Date(shifted));
            }
            Ok(f(d, dur.millis))
        }
        BasesValue::String(s) => {
            let dur = crate::stdlib::parse_duration_literal(s)?;
            apply_duration_or_number(&BasesValue::Date(d), &BasesValue::Duration(dur), f)
        }
        other => Ok(f(d, to_number_loose(other).unwrap_or(0.0) as i64)),
    }
}

// ---------------------------------------------------------------------------
// Calls and members
// ---------------------------------------------------------------------------

/// The lambda body and the context it runs in, passed together.
///
/// The body is a borrowed `&Node` rather than something captured, because a
/// `Fn` boxed as `'static` would have to own the subtree it walks. Passing the
/// subtree per call keeps the AST borrowed for exactly as long as the query.
pub struct LambdaCall<'a> {
    pub body: &'a Node,
    pub value: &'a BasesValue,
    pub index: usize,
    pub acc: Option<&'a BasesValue>,
    pub ctx: &'a EvalContext,
}

pub type LambdaRunner = Rc<dyn Fn(LambdaCall<'_>) -> Result<BasesValue>>;

fn evaluate_call(callee: &Node, args: &[Node], ctx: &EvalContext) -> Result<BasesValue> {
    // A method call: `value.foo(...)`
    if let NodeKind::Member { object, property } = &callee.kind {
        let target = evaluate(object, ctx)?;
        let Some(method) = get_method(&target, property) else {
            return Err(BasesError::new(format!(
                "Type error: \"{property}\" is not a method on {}",
                describe_type(&target)
            ))
            .with_construct(property));
        };

        // A higher-order method receives its argument as an AST, because the
        // body must be evaluated once per element with `value`/`index`/`acc`
        // bound. `filter`, `map`, `some`, `every`, `find`, `sort`, `reduce`.
        if let Some(arity) = method.lambda_arity {
            let body = args.first().ok_or_else(|| {
                BasesError::new(format!("\"{property}()\" requires an expression argument"))
                    .with_construct(property.clone())
            })?;
            // The seed for `reduce` is the second argument, evaluated once.
            let seed = if property == "reduce" {
                match args.get(1) {
                    Some(node) => Some(evaluate(node, ctx)?),
                    None => None,
                }
            } else {
                None
            };
            let runner: LambdaRunner = Rc::new(|call: LambdaCall<'_>| {
                let mut bindings = call.ctx.bindings.clone().unwrap_or_default();
                bindings.insert("value".into(), call.value.clone());
                bindings.insert("index".into(), BasesValue::Number(call.index as f64));
                bindings.insert("acc".into(), call.acc.cloned().unwrap_or(BasesValue::Null));
                let inner = EvalContext {
                    bindings: Some(bindings),
                    ..call.ctx.clone()
                };
                evaluate(call.body, &inner)
            });
            return crate::stdlib::run_higher_order(
                &target, property, arity, runner, body, seed, ctx,
            );
        }

        let mut evaluated = Vec::with_capacity(args.len());
        for arg in args {
            evaluated.push(evaluate(arg, ctx)?);
        }
        let plain: MethodFn = method.fn_body;
        return plain(&target, &evaluated, ctx);
    }

    if let NodeKind::Identifier(name) = &callee.kind {
        if let Some(global) = get_global(name) {
            let mut evaluated = Vec::with_capacity(args.len());
            for arg in args {
                evaluated.push(evaluate(arg, ctx)?);
            }
            return (global.body)(&evaluated, ctx);
        }
        // Not a global. A value in scope could still be callable, but Bases has
        // no first-class functions, so this is an error rather than a call.
        return Err(
            BasesError::new(format!("Unknown function \"{name}()\"")).with_construct(name.clone())
        );
    }

    Err(BasesError::new(
        "Attempted to call a value that is not a function",
    ))
}

fn evaluate_member(object: &Node, property: &str, ctx: &EvalContext) -> Result<BasesValue> {
    // `this.<prop>` routes through the property-path resolver so frontmatter
    // keys on the host note resolve (`this.projects`), not just file members.
    if let NodeKind::Identifier(name) = &object.kind {
        if name == "this" {
            return resolve_property_path(&format!("this.{property}"), ctx);
        }
    }
    let target = evaluate(object, ctx)?;
    Ok(read_member(&target, property))
}

fn evaluate_index(object: &Node, index: &Node, ctx: &EvalContext) -> Result<BasesValue> {
    let target = evaluate(object, ctx)?;
    let idx = evaluate(index, ctx)?;
    Ok(index_into(&target, &idx))
}

fn index_into(target: &BasesValue, index: &BasesValue) -> BasesValue {
    match target {
        BasesValue::List(items) => {
            let Some(i) = to_number_loose(index) else {
                return BasesValue::Null;
            };
            if i < 0.0 || i.fract() != 0.0 {
                return BasesValue::Null;
            }
            items.get(i as usize).cloned().unwrap_or(BasesValue::Null)
        }
        BasesValue::String(s) => {
            let Some(i) = to_number_loose(index) else {
                return BasesValue::Null;
            };
            // A string indexes by character, not by byte, so an emoji is one index.
            match s.chars().nth(i.max(0.0) as usize) {
                Some(c) => BasesValue::String(c.to_string()),
                None => BasesValue::Null,
            }
        }
        // `file["name"]` is a documented spelling of `file.name`.
        BasesValue::File(_) | BasesValue::Link { .. } | BasesValue::Namespace(_) => match index {
            BasesValue::String(name) => read_member(target, name),
            _ => BasesValue::Null,
        },
        _ => BasesValue::Null,
    }
}

// ---------------------------------------------------------------------------
// Coercion
// ---------------------------------------------------------------------------

pub fn to_number_loose(v: &BasesValue) -> Option<f64> {
    match v {
        BasesValue::Number(n) => Some(*n),
        BasesValue::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        BasesValue::Date(d) => Some(d.millis() as f64),
        BasesValue::Duration(d) => Some(d.millis as f64),
        BasesValue::String(s) => {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                None
            } else {
                trimmed.parse::<f64>().ok().filter(|n| n.is_finite())
            }
        }
        _ => None,
    }
}

fn to_number(v: &BasesValue, context: &str) -> Result<f64> {
    to_number_loose(v).ok_or_else(|| {
        BasesError::new(format!(
            "Expected a number but got {} in \"{context}\"",
            describe_type(v)
        ))
    })
}

/// Coerce to a date. A number is epoch milliseconds; a string must be ISO.
pub fn coerce_date(v: &BasesValue) -> Option<BasesDate> {
    match v {
        BasesValue::Date(d) => Some(d.clone()),
        BasesValue::String(s) => crate::stdlib::parse_date_loose(s),
        BasesValue::Number(n) => Some(BasesDate::from_millis(*n as i64)),
        _ => None,
    }
}

pub fn describe_type(v: &BasesValue) -> &'static str {
    crate::stdlib::describe(v)
}

/// Add calendar months, clamping the day to the target month's length.
///
/// `2024-01-31 + 1 month` is `2024-02-29`, not `2024-03-02`, and a filter on a
/// due date depends on that.
fn shift_months(d: &BasesDate, months: i64) -> Result<BasesDate> {
    crate::stdlib::add_months(d, months)
}
