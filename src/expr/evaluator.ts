/**
 * Tree-walking evaluator.
 *
 * The semantics here are the part of this project most likely to drift from
 * Obsidian, so each non-obvious rule is commented with the reason. Several
 * encode documented behaviour that Obsidian's own runtime gets wrong
 * (obsidian-help #1095); those are flagged in docs/divergences.md.
 */

import { BasesError, MissingThisContextError } from "./errors";
import type {
  BinaryNode,
  CallNode,
  IndexNode,
  ListNode,
  MemberNode,
  Node,
  UnaryNode,
} from "./parser";
import { parse } from "./parser";
import { getGlobal, getMethod, lookupHigherOrder, receiverType, receiverTypeName } from "./stdlib";
import {
  type BasesValue,
  DateValue,
  DurationValue,
  FileValue,
  isDate,
  isDuration,
  isLink,
  isList,
  parseDurationLiteral,
  stripExtension,
} from "./values";

/**
 * Everything an expression can see. Constructed per note: `note` is that note's
 * frontmatter, `file` its metadata, `formula` the computed columns, and `this`
 * the host note's context (see `thisHost` below).
 */
export interface EvalContext {
  note: Record<string, BasesValue>;
  file: FileValue;
  formula: Record<string, BasesValue>;
  /**
   * The `this` object. A FileValue carrying the host note's frontmatter, so
   * that all three real-world spellings work: `this.path`, `this.file.name`
   * and `this.projects` / `this.note.blockedBy`.
   */
  thisValue?: ThisContext;
  /** Ambient bindings for lambda parameters (`value`, `index`, `acc`). */
  bindings?: Record<string, BasesValue>;
}

export interface ThisContext {
  file: FileValue;
  note: Record<string, BasesValue>;
}

export function evaluate(node: Node, ctx: EvalContext): BasesValue {
  switch (node.type) {
    case "Literal":
      return node.value;

    case "List":
      return (node as ListNode).elements.map((el) => evaluate(el, ctx));

    case "Identifier":
      return resolveIdentifier(node.name, ctx);

    case "Unary":
      return evaluateUnary(node, ctx);

    case "Binary":
      return evaluateBinary(node, ctx);

    case "Call":
      return evaluateCall(node, ctx);

    case "Member":
      return evaluateMember(node, ctx);

    case "Index":
      return evaluateIndex(node, ctx);
  }
}

export function evaluateExpression(source: string, ctx: EvalContext): BasesValue {
  const ast = parse(source);
  // Pre-scan for `this`. Without this, a filter like
  // `project.contains(link(this.file.name))` would short-circuit on a missing
  // `project` and quietly evaluate to null -- exactly the silent empty result
  // we exist to avoid.
  if (ctx.thisValue === undefined) {
    const ref = findThisReference(ast);
    if (ref !== null) {
      throw new MissingThisContextError(ref);
    }
  }
  return evaluate(ast, ctx);
}

/** First `this` reference in the tree, or null when there is none. */
function findThisReference(node: Node): string | null {
  switch (node.type) {
    case "Identifier":
      return node.name === "this" ? "this" : null;
    case "Member":
      if (node.object.type === "Identifier" && node.object.name === "this") {
        return `this.${node.property}`;
      }
      return findThisReference(node.object);
    case "Binary":
      return findThisReference(node.left) ?? findThisReference(node.right);
    case "Unary":
      return findThisReference(node.operand);
    case "Call":
      return (
        findThisReference(node.callee) ??
        node.args.map((a) => findThisReference(a)).find((r) => r !== null) ??
        null
      );
    case "Index":
      return findThisReference(node.object) ?? findThisReference(node.index);
    case "List":
      return node.elements.map((a) => findThisReference(a)).find((r) => r !== null) ?? null;
    case "Literal":
      return null;
  }
}

// -- identifiers -------------------------------------------------------------

/**
 * Namespaces that are never a note property.
 *
 * A bare identifier is a note property, so `file`, `formula` and `this` have to
 * be excluded explicitly or a filter would read them as frontmatter. Exported so
 * filter inversion in `mcp/drafts.ts` excludes exactly the same names rather
 * than keeping a second copy that can drift.
 */
export const RESERVED = new Set(["this", "file", "note", "formula", "value", "index", "acc"]);

function resolveIdentifier(name: string, ctx: EvalContext): BasesValue {
  const bindings = ctx.bindings;
  if (bindings !== undefined && Object.hasOwn(bindings, name)) {
    return bindings[name]!;
  }

  // Dotted names route to the right namespace: `file.name`, `note.age`,
  // `formula.ppu`, `this.file.name`.
  if (name.includes(".")) {
    return resolvePropertyPath(name, ctx);
  }

  switch (name) {
    case "this": {
      if (ctx.thisValue === undefined) {
        throw new MissingThisContextError("this");
      }
      return thisAsFile(ctx);
    }
    case "file":
      return ctx.file;
    case "note":
      return ctx.note as unknown as BasesValue;
    case "formula":
      return ctx.formula as unknown as BasesValue;
    case "value":
    case "index":
    case "acc":
      // Bound only inside a lambda. Outside one they are simply absent, which
      // the `isEmpty()` semantics treat as an empty value rather than an error.
      return null;
    default:
      // A bare identifier is a note property (the documented default).
      return lookupPath(ctx.note, [name]);
  }
}

function resolvePropertyPath(path: string, ctx: EvalContext): BasesValue {
  const dot = path.indexOf(".");
  const head = path.slice(0, dot);
  const rest = path.slice(dot + 1);
  const parts = rest.split(".");

  switch (head) {
    case "this": {
      if (ctx.thisValue === undefined) {
        throw new MissingThisContextError(path);
      }
      const tc = ctx.thisValue;
      // `this.note.X` is note-scoped; `this.file.X` is file-scoped.
      if (parts[0] === "note") return lookupPath(tc.note, parts.slice(1));
      if (parts[0] === "file") return lookupPath(thisAsFileObject(tc), parts.slice(1));
      // Otherwise the spelling is ambiguous and real vaults use both senses:
      // `this.path` is a File member while `this.projects` is frontmatter.
      // Prefer the host note's frontmatter, then fall back to the File.
      const asNote = lookupPath(tc.note, parts);
      if (asNote !== null && asNote !== undefined) return asNote;
      return lookupPath(thisAsFileObject(tc), parts);
    }
    case "file":
      return lookupPath(ctx.file as unknown as Record<string, BasesValue>, parts);
    case "note":
      return lookupPath(ctx.note, parts);
    case "formula":
      return lookupPath(ctx.formula, parts);
    default:
      return lookupPath(ctx.note, path.split("."));
  }
}

/**
 * Present `this` as a FileValue that also answers to the host note's
 * frontmatter keys. Real vaults use `this.path`, `this.file.name`,
 * `this.asLink()` and `this.projects` interchangeably, so one object has to
 * satisfy all four.
 */
function thisAsFile(ctx: EvalContext): FileValue {
  return makeThisProxy(ctx.thisValue!);
}

function thisAsFileObject(tc: ThisContext): FileValue {
  return makeThisProxy(tc);
}

function makeThisProxy(tc: ThisContext): FileValue {
  const f = tc.file;
  const extra = tc.note as unknown as Record<string, BasesValue>;
  return new Proxy(f, {
    get(target, prop, receiver) {
      if (typeof prop !== "string") return Reflect.get(target, prop, receiver);
      if (Object.hasOwn(extra, prop)) return extra[prop];
      if (prop === "note") return tc.note as unknown as BasesValue;
      const own = Reflect.get(target, prop, target);
      if (typeof own === "function") return own.bind(target);
      return own;
    },
  });
}

/** Walk a dotted path, returning null for any missing link in the chain. */
function lookupPath(root: unknown, parts: string[]): BasesValue {
  let cur: unknown = root;
  for (const part of parts) {
    if (cur === null || cur === undefined) return null;
    if (isList(cur)) {
      // `file.tags.contains(...)` style access on a list yields a mapped list.
      cur = cur.map((item) => readMember(item, part));
      continue;
    }
    if (cur instanceof FileValue) {
      cur = readMember(cur, part);
      continue;
    }
    if (typeof cur === "object" || cur instanceof RegExp) {
      cur = readMember(cur, part);
      continue;
    }
    // Primitives have no named members; `.length` on a string still works.
    cur = readMember(cur, part);
  }
  return normaliseMember(cur);
}

/** Read a single named member from any value, returning null when absent. */
export function readMember(target: unknown, name: string): BasesValue {
  if (target === null || target === undefined) return null;

  if (target instanceof FileValue) {
    return readFileMember(target, name);
  }
  if (isLink(target)) {
    if (name === "target") return target.target;
    if (name === "display") return target.display ?? null;
    if (name === "path") return target.resolvedPath ?? null;
    return null;
  }
  if (isDate(target)) {
    switch (name) {
      case "year":
        return target.year;
      case "month":
        return target.month;
      case "day":
        return target.day;
      case "hour":
        return target.hour;
      case "minute":
        return target.minute;
      case "second":
        return target.second;
      case "millisecond":
        return target.millisecond;
      default:
        return null;
    }
  }
  if (isDuration(target)) {
    switch (name) {
      case "days":
        return target.days;
      case "hours":
        return target.hours;
      case "minutes":
        return target.minutes;
      case "seconds":
        return target.seconds;
      case "milliseconds":
        return target.milliseconds;
      case "months":
        return target.months;
      case "years":
        return target.years;
      default:
        return null;
    }
  }
  if (isList(target)) {
    // `list.length`
    if (name === "length") return target.length;
    return null;
  }
  if (typeof target === "string") {
    if (name === "length") return target.length;
    return null;
  }
  if (typeof target === "object") {
    const rec = target as Record<string, unknown>;
    if (Object.hasOwn(rec, name)) return rec[name] as BasesValue;
    return null;
  }
  return null;
}

function readFileMember(file: FileValue, name: string): BasesValue {
  const a = file.accessors;
  switch (name) {
    case "name":
      // Probed against a live Obsidian 1.13.7: the `file.name` column renders
      // WITHOUT the extension, matching `basename`. Recorded in
      // docs/divergences.md; the docs claim the opposite.
      return file.basename;
    case "basename":
      return file.basename;
    case "path":
      return file.path;
    case "folder":
      return file.folder;
    case "ext":
      return file.ext;
    case "size":
      return a.size();
    case "ctime":
      return a.ctime();
    case "mtime":
      return a.mtime();
    case "tags":
      return a.tags();
    case "links":
      return a.links();
    case "embeds":
      return a.embeds();
    case "backlinks":
      return a.backlinks();
    case "properties":
      return a.properties();
    case "file":
      return file;
    case "tasks":
      // Not in the official `file.*` table; Obsidian's co-founder states bases
      // does not read file contents. We ship it as a documented extension.
      return a.tasks();
    default:
      return null;
  }
}

function normaliseMember(v: unknown): BasesValue {
  if (v === undefined) return null;
  return v as BasesValue;
}

// -- operators ---------------------------------------------------------------

function evaluateUnary(node: UnaryNode, ctx: EvalContext): BasesValue {
  if (node.op === "!") {
    return !isTruthy(evaluate(node.operand, ctx));
  }
  const v = evaluate(node.operand, ctx);
  return -toNumber(v, ctx);
}

function evaluateBinary(node: BinaryNode, ctx: EvalContext): BasesValue {
  // Short-circuit before evaluating the right side.
  if (node.op === "&&") {
    const left = evaluate(node.left, ctx);
    if (!isTruthy(left)) return false;
    return isTruthy(evaluate(node.right, ctx));
  }
  if (node.op === "||") {
    const left = evaluate(node.left, ctx);
    if (isTruthy(left)) return left;
    return evaluate(node.right, ctx);
  }

  const l = evaluate(node.left, ctx);
  const r = evaluate(node.right, ctx);

  switch (node.op) {
    case "==":
      return valuesEqual(l, r);
    case "!=":
      return !valuesEqual(l, r);
    case ">":
      return compare(l, r) > 0;
    case "<":
      return compare(l, r) < 0;
    case ">=":
      return compare(l, r) >= 0;
    case "<=":
      return compare(l, r) <= 0;
    case "+":
      return add(l, r, ctx);
    case "-":
      return subtract(l, r, ctx);
    case "*":
      return multiply(l, r, ctx);
    case "/":
      return divide(l, r, ctx);
    case "%":
      return modulo(l, r, ctx);
    default:
      throw new BasesError(`Unsupported operator "${node.op}"`, { construct: node.op });
  }
}

/**
 * Truthiness. An EMPTY LIST is falsy, which is what makes `if(projects,
 * projects.length, 0)` work as a null-guard in real vaults.
 */
export function isTruthy(v: BasesValue): boolean {
  if (v === null) return false;
  if (typeof v === "boolean") return v;
  if (typeof v === "number") return v !== 0;
  if (typeof v === "string") return v.length > 0;
  if (isList(v)) return v.length > 0;
  if (isLink(v)) return true;
  if (isDuration(v)) return v.ms !== 0 || v.months !== 0 || v.years !== 0;
  if (v instanceof DateValue) return true;
  if (v instanceof FileValue) return true;
  if (v instanceof RegExp) return true;
  if (typeof v === "object" && v !== null) return Object.keys(v).length > 0;
  return Boolean(v);
}

/**
 * Equality with link awareness: two links are equal when they resolve to the
 * same file, or -- when unresolved -- when their text matches exactly.
 */
export function valuesEqual(a: BasesValue, b: BasesValue): boolean {
  if (isLink(a) || isLink(b)) return linkEqual(a, b);
  if (a instanceof FileValue || b instanceof FileValue) {
    const fa = a instanceof FileValue ? a : isLink(b) ? b.resolvedPath : undefined;
    const fb = b instanceof FileValue ? b : isLink(a) ? a.resolvedPath : undefined;
    if (fa !== undefined && fb !== undefined) return fa === fb;
    // A File compared against a plain string compares by its path.
    if (a instanceof FileValue && typeof b === "string") return a.path === b || a.basename === b;
    if (b instanceof FileValue && typeof a === "string") return b.path === a || b.basename === a;
  }
  if (isList(a) && !isList(b)) return valuesEqual([a], [b]);
  if (!isList(a) && isList(b)) return valuesEqual([a], [b]);
  if (isList(a) && isList(b)) {
    if (a.length !== b.length) return false;
    return a.every((x, i) => valuesEqual(x, b[i]!));
  }
  if (isDate(a) || isDate(b)) {
    if (isDate(a) && isDate(b)) return a.ms === b.ms;
    const dateSide = isDate(a) ? a : (b as DateValue);
    if (typeof b === "string") {
      try {
        return dateSide.ms === parseDateLoose(b).ms;
      } catch {
        return false;
      }
    }
    if (typeof a === "string") return valuesEqual(a, b);
  }
  if (isDuration(a) || isDuration(b)) {
    if (isDuration(a) && isDuration(b)) {
      return a.ms === b.ms && a.months === b.months && a.years === b.years;
    }
  }
  return a === b;
}

function linkEqual(a: BasesValue, b: BasesValue): boolean {
  const asFile = (v: BasesValue): string | undefined =>
    v instanceof FileValue ? v.path : isLink(v) ? v.resolvedPath : undefined;
  const pa = asFile(a);
  const pb = asFile(b);
  if (pa !== undefined && pb !== undefined) return pa === pb;
  // One side resolved and the other did not: fall back to text comparison.
  const ta = linkText(a);
  const tb = linkText(b);
  if (ta === null || tb === null) return false;
  if (pa !== undefined || pb !== undefined) {
    return normaliseLinkText(ta) === normaliseLinkText(tb);
  }
  return ta === tb;
}

function linkText(v: BasesValue): string | null {
  if (isLink(v)) return v.target;
  if (typeof v === "string") return v;
  if (v instanceof FileValue) return v.path;
  return null;
}

function normaliseLinkText(text: string): string {
  return stripExtension(text.trim());
}

export function compare(a: BasesValue, b: BasesValue): number {
  if (a === null || a === undefined) return b === null || b === undefined ? 0 : -1;
  if (b === null || b === undefined) return 1;

  if (isDuration(a) && isDuration(b)) {
    return a.ms - b.ms;
  }
  if (isDate(a) || isDate(b)) {
    const da = coerceDate(a);
    const db = coerceDate(b);
    if (da !== null && db !== null) return da.ms - db.ms;
  }
  if (isList(a) && isList(b)) {
    // Lists compare by their first differing element, so sort keys are stable.
    for (let i = 0; i < Math.min(a.length, b.length); i++) {
      const c = compare(a[i]!, b[i]!);
      if (c !== 0) return c;
    }
    return a.length - b.length;
  }
  if (typeof a === "string" && typeof b === "string") {
    // Dates written as ISO strings must order chronologically, not lexically
    // once formats differ.
    const da = tryParseDateLoose(a);
    const db = tryParseDateLoose(b);
    if (da !== null && db !== null) return da.ms - db.ms;
    return a < b ? -1 : a > b ? 1 : 0;
  }
  if (typeof a === "boolean" || typeof b === "boolean") {
    const na = a ? 1 : 0;
    const nb = b ? 1 : 0;
    return na - nb;
  }
  const na = toNumberLoose(a);
  const nb = toNumberLoose(b);
  if (na !== null && nb !== null) return na - nb;
  return String(a) < String(b) ? -1 : String(a) > String(b) ? 1 : 0;
}

function add(a: BasesValue, b: BasesValue, ctx: EvalContext): BasesValue {
  // Date arithmetic wins over string concatenation, so `date + "1d"` is a date
  // rather than the literal text of a date followed by "1d".
  if (isDate(a) || isDate(b)) {
    return applyDurationOrNumber(a, b, (da, n) => new DateValue(da.ms + n));
  }
  if (typeof a === "string" || typeof b === "string") {
    return toDisplayString(a) + toDisplayString(b);
  }
  if (isDuration(a) || isDuration(b)) {
    return combineDurations(a, b, (x, y) => x + y);
  }
  if (isList(a) || isList(b)) {
    const la = isList(a) ? a : [a];
    const lb = isList(b) ? b : [b];
    return [...la, ...lb];
  }
  return toNumber(a, ctx) + toNumber(b, ctx);
}

function subtract(a: BasesValue, b: BasesValue, ctx: EvalContext): BasesValue {
  if (isDate(a) && isDate(b)) {
    // Obsidian's runtime returns a Duration here even though its docs claim
    // milliseconds. We return a Duration AND make `number()` work on it, so
    // both the documented `((a-b)/86400000).round()` and the real-world
    // `(a-b).days.round(0)` idioms work. See docs/divergences.md.
    return new DurationValue(a.ms - b.ms);
  }
  if (isDate(a)) return applyDurationOrNumber(a, b, (da, n) => new DateValue(da.ms - n));
  if (isDuration(a)) return combineDurations(a, b, (x, y) => x - y);
  return toNumber(a, ctx) - toNumber(b, ctx);
}

function multiply(a: BasesValue, b: BasesValue, ctx: EvalContext): BasesValue {
  // The documented rule: duration must be on the LEFT for duration x scalar.
  if (isDuration(a)) return scaleDuration(a, toNumber(b, ctx));
  if (isDuration(b)) {
    throw new BasesError(
      "Multiplying a scalar by a duration is not supported; put the duration on the left " +
        '(e.g. duration("1d") * 2, not 2 * duration("1d"))',
      { construct: "*" },
    );
  }
  return toNumber(a, ctx) * toNumber(b, ctx);
}

function divide(a: BasesValue, b: BasesValue, ctx: EvalContext): BasesValue {
  const divisor = toNumber(b, ctx);
  if (divisor === 0) {
    // Obsidian does not guard this; real vaults guard it themselves
    // (`if(attempted > 0, attempted / attempted, 0)`). Returning 0 keeps a
    // table renderable instead of failing the whole view.
    return 0;
  }
  if (isDuration(a)) {
    return new DurationValue(a.ms / divisor, a.months / divisor, a.years / divisor);
  }
  return toNumber(a, ctx) / divisor;
}

function modulo(a: BasesValue, b: BasesValue, ctx: EvalContext): BasesValue {
  const divisor = toNumber(b, ctx);
  if (divisor === 0) return 0;
  return toNumber(a, ctx) % divisor;
}

function scaleDuration(d: DurationValue, n: number): DurationValue {
  return new DurationValue(d.ms * n, d.months * n, d.years * n);
}

function combineDurations(
  a: BasesValue,
  b: BasesValue,
  f: (x: number, y: number) => number,
): DurationValue {
  const toDur = (v: BasesValue): DurationValue => {
    if (isDuration(v)) return v;
    if (isDate(v)) return new DurationValue(0);
    if (typeof v === "string") return parseDurationLiteral(v);
    if (typeof v === "number") return new DurationValue(v);
    return new DurationValue(0);
  };
  const da = toDur(a);
  const db = toDur(b);
  return new DurationValue(f(da.ms, db.ms), f(da.months, db.months), f(da.years, db.years));
}

function applyDurationOrNumber(
  date: BasesValue,
  other: BasesValue,
  f: (d: DateValue, n: number) => DateValue,
): BasesValue {
  const d = coerceDate(date);
  if (d === null) {
    throw new BasesError(`Expected a date on the left of an arithmetic operator`);
  }
  if (isDuration(other)) {
    // Calendar months/years are added by calendar, not by millisecond span.
    if (other.years !== 0 || other.months !== 0) {
      const dt = new Date(d.ms);
      dt.setMonth(dt.getMonth() + other.months + other.years * 12);
      return new DateValue(dt.getTime(), d.dateOnly);
    }
    return f(d, other.ms);
  }
  if (typeof other === "string") {
    return applyDurationOrNumber(d, parseDurationLiteral(other), f);
  }
  return f(d, toNumberLoose(other) ?? 0);
}

// -- calls -------------------------------------------------------------------

function evaluateCall(node: CallNode, ctx: EvalContext): BasesValue {
  // A method call: `value.foo(...)`
  if (node.callee.type === "Member") {
    const member = node.callee as MemberNode;
    const target = evaluate(member.object, ctx);
    const method = getMethod(target, member.property);
    if (method === undefined) {
      throw new BasesError(
        `Type error: "${member.property}" is not a method on ${describeType(target)}`,
        { construct: member.property },
      );
    }

    // Higher-order methods receive their argument as an AST, because the body
    // must be evaluated once per element with `value` / `index` / `acc` bound.
    const lambda = method.lambda;
    if (lambda !== undefined) {
      const def = lookupHigherOrder(`${receiverType(target)}.${member.property}`);
      if (def === undefined) {
        throw new BasesError(`Internal error: no higher-order impl for ${member.property}`);
      }
      const body = node.args[0];
      if (body === undefined) {
        throw new BasesError(`"${member.property}()" requires an expression argument`, {
          construct: member.property,
        });
      }
      // The seed for `reduce` is the second argument, evaluated once.
      const seed =
        member.property === "reduce" && node.args[1] !== undefined
          ? evaluate(node.args[1], ctx)
          : undefined;
      const items = target as BasesValue[];
      return def.run(
        items,
        (value, index, acc) => {
          const bindings: Record<string, BasesValue> = { ...(ctx.bindings ?? {}) };
          bindings[def.params[0] ?? "value"] = value;
          bindings[def.params[1] ?? "index"] = index;
          bindings[def.params[2] ?? "acc"] = acc ?? null;
          return evaluate(body, { ...ctx, bindings });
        },
        seed,
      );
    }

    const args = node.args.map((a) => evaluate(a, ctx));
    return method.fn({ target, args, ctx, lambdaCtx: ctx, arity: args.length });
  }

  const name = node.callee.type === "Identifier" ? node.callee.name : null;
  const globalFn = name === null ? undefined : getGlobal(name);
  if (globalFn !== undefined) {
    const args = node.args.map((a) => evaluate(a, ctx));
    return globalFn(args, ctx);
  }
  if (name !== null) {
    // Not a global: it may still be a value in scope holding a function.
    const value = resolveIdentifier(name, ctx);
    if (typeof value === "function") {
      return (value as (...a: BasesValue[]) => BasesValue)(
        ...node.args.map((a) => evaluate(a, ctx)),
      );
    }
    throw new BasesError(`Unknown function "${name}()"`, { construct: name });
  }

  const callee = evaluate(node.callee, ctx);
  if (typeof callee === "function") {
    return (callee as (...a: BasesValue[]) => BasesValue)(
      ...node.args.map((a) => evaluate(a, ctx)),
    );
  }
  throw new BasesError("Attempted to call a value that is not a function");
}

function evaluateMember(node: MemberNode, ctx: EvalContext): BasesValue {
  // `this.<prop>` must go through the property-path router so that frontmatter
  // keys on the host note resolve (`this.projects`), not just file members.
  if (node.object.type === "Identifier" && node.object.name === "this") {
    return resolvePropertyPath(`this.${node.property}`, ctx);
  }
  const target = evaluate(node.object, ctx);
  return readMember(target, node.property);
}

function evaluateIndex(node: IndexNode, ctx: EvalContext): BasesValue {
  const target = evaluate(node.object, ctx);
  const index = evaluate(node.index, ctx);

  if (isList(target)) {
    const i = toNumberLoose(index);
    if (i === null) return null;
    return target[i] ?? null;
  }
  if (typeof target === "string") {
    const i = toNumberLoose(index);
    if (i === null) return null;
    return target[i] ?? null;
  }
  if (isLink(target) || target instanceof FileValue) {
    // `file["name"]` is a documented spelling of `file.name`.
    if (typeof index === "string") return readMember(target, index);
    return null;
  }
  if (target !== null && typeof target === "object") {
    if (typeof index === "string") return readMember(target, index);
    if (isList(index)) return index.map((k) => readMember(target, String(k)));
  }
  return null;
}

// -- coercion ----------------------------------------------------------------

/** Stringify for display. Lists join with ", ", links render as `[[target]]`. */
export function toDisplayString(v: BasesValue): string {
  if (v === null || v === undefined) return "";
  if (typeof v === "string") return v;
  if (typeof v === "number" || typeof v === "boolean") return String(v);
  if (isList(v)) return v.map(toDisplayString).join(", ");
  if (isLink(v)) return v.toWikilink();
  if (v instanceof DateValue) return v.toString();
  if (isDuration(v)) return v.toString();
  if (v instanceof FileValue) return v.path;
  if (v instanceof RegExp) return v.source;
  if (typeof v === "object") {
    return JSON.stringify(v);
  }
  return String(v);
}

export function toNumberLoose(v: BasesValue): number | null {
  if (typeof v === "number") return v;
  if (typeof v === "boolean") return v ? 1 : 0;
  if (isDate(v)) return v.ms;
  if (isDuration(v)) return v.ms;
  if (typeof v === "string") {
    const trimmed = v.trim();
    if (trimmed === "") return null;
    const n = Number(trimmed);
    return Number.isNaN(n) ? null : n;
  }
  if (v === null) return null;
  return null;
}

function toNumber(v: BasesValue, _ctx: EvalContext): number {
  const n = toNumberLoose(v);
  if (n === null) {
    throw new BasesError(`Expected a number but got ${describeType(v)}`);
  }
  return n;
}

export function coerceDate(v: BasesValue): DateValue | null {
  if (isDate(v)) return v;
  if (typeof v === "string") {
    const parsed = tryParseDateLoose(v);
    return parsed;
  }
  if (typeof v === "number") return new DateValue(v);
  return null;
}

function parseDateLoose(text: string): DateValue {
  return tryParseDateLoose(text) ?? new DateValue(NaN);
}

function tryParseDateLoose(text: string): DateValue | null {
  const trimmed = text.trim();
  const iso = /^(\d{4})-(\d{2})-(\d{2})(?:[T ](\d{2}):(\d{2})(?::(\d{2}))?)?$/.exec(trimmed);
  if (iso) {
    return DateValue.fromParts(
      Number(iso[1]),
      Number(iso[2]),
      Number(iso[3]),
      iso[4] !== undefined ? Number(iso[4]) : 0,
      iso[5] !== undefined ? Number(iso[5]) : 0,
      iso[6] !== undefined ? Number(iso[6]) : 0,
      0,
      iso[4] === undefined,
    );
  }
  return null;
}

export function describeType(v: BasesValue): string {
  return receiverTypeName(v);
}
