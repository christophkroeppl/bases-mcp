/**
 * The Bases standard library: global functions and per-type methods.
 *
 * Methods are looked up by the *runtime type* of the receiver, which is how
 * `contains` can mean three different things: substring on a String,
 * whole-element match on a List, and a type error on anything else.
 */

import { BasesError } from "./errors";
import {
  coerceDate,
  describeType,
  type EvalContext,
  evaluate,
  isTruthy,
  toDisplayString,
  toNumberLoose,
} from "./evaluator";
import type { Node } from "./parser";
import {
  type BasesValue,
  DateValue,
  DurationValue,
  FileValue,
  isDate,
  isDuration,
  isLink,
  isList,
  LinkValue,
  parseDateValue,
  parseDurationLiteral,
  stripExtension,
} from "./values";

export interface MethodCall {
  target: BasesValue;
  args: BasesValue[];
  ctx: EvalContext;
  /** Context with lambda bindings applied, for higher-order methods. */
  lambdaCtx: EvalContext;
  /** Number of AST arguments, which may exceed the number of evaluated ones. */
  arity: number;
}

export interface Lambda {
  /** Which binding name argument `i` should be exposed as. */
  param(i: number): string;
  /** True if this method evaluates its argument as an expression per item. */
  higherOrder: true;
}

export type GlobalFn = (args: BasesValue[], ctx: EvalContext) => BasesValue;
export type MethodFn = (call: MethodCall) => BasesValue;

interface MethodDef {
  fn: MethodFn;
  lambda?: Lambda;
}

const globals = new Map<string, GlobalFn>();
const methods = new Map<string, Map<string, MethodDef>>();

export function defineGlobal(name: string, fn: GlobalFn): void {
  globals.set(name, fn);
}

export function defineMethod(type: string, name: string, fn: MethodFn): void {
  let byName = methods.get(type);
  if (byName === undefined) {
    byName = new Map();
    methods.set(type, byName);
  }
  byName.set(name, { fn });
}

/** Register on every receiver type. */
function defineAny(name: string, fn: MethodFn): void {
  for (const t of [
    "string",
    "number",
    "boolean",
    "date",
    "duration",
    "list",
    "link",
    "file",
    "object",
    "null",
  ]) {
    defineMethod(t, name, fn);
  }
}

export function getGlobal(name: string): GlobalFn | undefined {
  return globals.get(name);
}

export function getMethod(target: BasesValue, name: string): MethodDef | undefined {
  const t = receiverType(target);
  const byName = methods.get(t);
  if (byName === undefined) return undefined;
  const found = byName.get(name);
  if (found !== undefined) return found;
  // Fall back to the universal methods.
  return methods.get("null")?.get(name);
}

/** Order matters: string and list checks must precede the generic object case. */
export function receiverType(v: BasesValue): string {
  if (v === null || v === undefined) return "null";
  if (isLink(v)) return "link";
  if (v instanceof FileValue) return "file";
  if (isDate(v)) return "date";
  if (isDuration(v)) return "duration";
  if (v instanceof RegExp) return "object";
  if (isList(v)) return "list";
  if (typeof v === "string") return "string";
  if (typeof v === "number") return "number";
  if (typeof v === "boolean") return "boolean";
  if (typeof v === "object") return "object";
  return "object";
}

/** The receiver's type name as Obsidian's error messages spell it. */
export function receiverTypeName(v: BasesValue): string {
  switch (receiverType(v)) {
    case "null":
      return "null";
    case "link":
      return "Link";
    case "file":
      return "File";
    case "date":
      return "Date";
    case "duration":
      return "duration";
    case "list":
      return "List";
    case "string":
      return "String";
    case "number":
      return "Number";
    case "boolean":
      return "Boolean";
    default:
      return "Object";
  }
}

// =========================================================================
// Globals
// =========================================================================

defineGlobal("if", (args) => {
  const [cond, whenTrue, whenFalse] = args;
  return isTruthy(cond) ? (whenTrue ?? null) : (whenFalse ?? null);
});

defineGlobal("number", (args) => {
  const v = args[0];
  if (isDate(v)) return v.ms;
  if (isDuration(v)) return v.ms;
  if (typeof v === "boolean") return v ? 1 : 0;
  if (typeof v === "number") return v;
  if (typeof v === "string") {
    const n = Number(v.trim());
    if (v.trim() === "" || Number.isNaN(n)) {
      throw new BasesError(`number() could not convert "${v}"`, { construct: "number" });
    }
    return n;
  }
  if (isList(v)) {
    throw new BasesError("number() does not accept a List", { construct: "number" });
  }
  return 0;
});

defineGlobal("string", (args) => toDisplayString(args[0] ?? null));
defineGlobal("toString", (args) => toDisplayString(args[0] ?? null));

defineGlobal("list", (args) => {
  const v = args[0];
  if (v === undefined) return [];
  if (isList(v)) return v;
  if (v === null) return [];
  return [v];
});

defineGlobal("min", (args) => {
  const nums = args.map((a) => requireNumber(a, "min"));
  return nums.length === 0 ? null : Math.min(...nums);
});

defineGlobal("max", (args) => {
  const nums = args.map((a) => requireNumber(a, "max"));
  return nums.length === 0 ? null : Math.max(...nums);
});

defineGlobal("date", (args) => {
  const v = args[0];
  if (isDate(v)) return v;
  if (typeof v === "string") {
    try {
      return parseDateValue(v);
    } catch (err) {
      throw new BasesError(
        `Invalid date format for left side of function "date": ${(err as Error).message}`,
        { construct: "date" },
      );
    }
  }
  if (typeof v === "number") return new DateValue(v);
  throw new BasesError(`date() cannot convert ${describeType(v)}`, { construct: "date" });
});

defineGlobal("duration", (args) => {
  const v = args[0];
  if (isDuration(v)) return v;
  if (typeof v === "string") return parseDurationLiteral(v);
  if (typeof v === "number") return new DurationValue(v);
  throw new BasesError(`duration() cannot convert ${describeType(v)}`, { construct: "duration" });
});

defineGlobal("today", () => {
  const now = new Date();
  return DateValue.fromParts(
    now.getFullYear(),
    now.getMonth() + 1,
    now.getDate(),
    0,
    0,
    0,
    0,
    true,
  );
});

defineGlobal("now", () => new DateValue(Date.now()));

defineGlobal("random", () => Math.random());

defineGlobal("escapeHTML", (args) => escapeHtml(toDisplayString(args[0] ?? "")));

defineGlobal("html", (args) => toDisplayString(args[0] ?? ""));

defineGlobal("icon", (args) => `:${toDisplayString(args[0] ?? "")}:`);

defineGlobal("image", (args) => {
  const v = args[0];
  if (isLink(v)) return `!${v.toWikilink()}`;
  return toDisplayString(v);
});

defineGlobal("link", (args) => {
  const target = args[0];
  const display = args[1];
  if (target === null || target === undefined) {
    throw new BasesError("link() requires a target", { construct: "link" });
  }
  if (target instanceof FileValue) {
    return new LinkValue(target.path, displayString(display), target.path);
  }
  if (isLink(target)) {
    return new LinkValue(
      target.target,
      displayString(display) ?? target.display,
      target.resolvedPath,
    );
  }
  return new LinkValue(toDisplayString(target), displayString(display) ?? null);
});

defineGlobal("file", (args, ctx) => {
  const v = args[0];
  if (v instanceof FileValue) return v;
  if (isLink(v)) return ctx.file.accessors.resolve(v.resolvedPath ?? v.target) ?? null;
  if (typeof v === "string") return ctx.file.accessors.resolve(v) ?? null;
  return null;
});

function requireNumber(v: BasesValue, fn: string): number {
  const n = toNumberLoose(v);
  if (n === null) {
    throw new BasesError(
      `Type error in "${fn}", parameter expects Number, given ${describeType(v)}`,
      {
        construct: fn,
      },
    );
  }
  return n;
}

function displayString(v: BasesValue | undefined): string | undefined {
  if (v === undefined || v === null) return undefined;
  const s = toDisplayString(v);
  return s === "" ? undefined : s;
}

function escapeHtml(s: string): string {
  return s
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;");
}

// =========================================================================
// Universal methods
// =========================================================================

defineAny("toString", ({ target }) => toDisplayString(target));

defineAny("isTruthy", ({ target }) => isTruthy(target));

defineAny("isType", ({ target, args }) => {
  // Obsidian's `isType` takes a lowercase type name: "string", "number",
  // "boolean", "list", "date", "duration", "link", "file", "object", "null".
  return receiverType(target) === String(args[0]);
});

defineAny("isEmpty", ({ target }) => {
  if (target === null || target === undefined) return true;
  if (isList(target)) return target.length === 0;
  if (typeof target === "string") return target.length === 0;
  if (typeof target === "number") return Number.isNaN(target);
  if (typeof target === "boolean") return false;
  // A Date is never "empty" -- Obsidian defines date.isEmpty() as always false,
  // even when the underlying property is absent.
  if (isDate(target)) return false;
  if (isDuration(target)) return target.ms === 0 && target.months === 0 && target.years === 0;
  if (isLink(target)) return false;
  if (target instanceof FileValue) return false;
  if (typeof target === "object") return Object.keys(target).length === 0;
  return false;
});

// =========================================================================
// String methods
// =========================================================================

function str(v: BasesValue, fn: string): string {
  if (typeof v !== "string") {
    throw new BasesError(
      `Type error in "${fn}", parameter expects String, given ${describeType(v)}`,
      {
        construct: fn,
      },
    );
  }
  return v;
}

defineMethod("string", "contains", ({ target, args }) => {
  const needle = args[0];
  if (typeof needle !== "string") {
    throw new BasesError(
      `Type error in "contains", parameter "value". Expected String not, given ${describeType(needle)}.`,
      { construct: "contains" },
    );
  }
  return str(target, "contains").includes(needle);
});

defineMethod("string", "containsAll", ({ target, args }) => {
  const s = str(target, "containsAll");
  return args.every((a) => typeof a === "string" && s.includes(a));
});

defineMethod("string", "containsAny", ({ target, args }) => {
  const s = str(target, "containsAny");
  return args.some((a) => typeof a === "string" && s.includes(a));
});

defineMethod("string", "startsWith", ({ target, args }) => {
  const p = args[0];
  if (typeof p !== "string") {
    throw new BasesError(
      `Type error in "startsWith", parameter expects String, given ${describeType(p)}`,
    );
  }
  return str(target, "startsWith").startsWith(p);
});

defineMethod("string", "endsWith", ({ target, args }) => {
  const p = args[0];
  if (typeof p !== "string") {
    throw new BasesError(
      `Type error in "endsWith", parameter expects String, given ${describeType(p)}`,
    );
  }
  return str(target, "endsWith").endsWith(p);
});

defineMethod("string", "lower", ({ target }) => str(target, "lower").toLowerCase());
defineMethod("string", "title", ({ target }) => titleCase(str(target, "title")));
defineMethod("string", "trim", ({ target }) => str(target, "trim").trim());
defineMethod("string", "reverse", ({ target }) => [...str(target, "reverse")].reverse().join(""));
defineMethod("string", "repeat", ({ target, args }) => {
  const n = toNumberLoose(args[0] ?? null) ?? 0;
  return str(target, "repeat").repeat(Math.max(0, Math.floor(n)));
});

defineMethod("string", "slice", ({ target, args }) => {
  const s = str(target, "slice");
  const start = toNumberLoose(args[0] ?? null) ?? 0;
  const end = args.length > 1 ? toNumberLoose(args[1] ?? null) : null;
  return end === null ? s.slice(start) : s.slice(start, end);
});

defineMethod("string", "split", ({ target, args }) => {
  const s = str(target, "split");
  const sep = args[0];
  const limit = args.length > 1 ? toNumberLoose(args[1] ?? null) : null;
  if (sep instanceof RegExp) {
    const parts = s.split(sep);
    return limit === null ? parts : parts.slice(0, limit);
  }
  if (typeof sep !== "string") {
    throw new BasesError(
      `Type error in "split", separator expects String, given ${describeType(sep)}`,
    );
  }
  if (sep === "") return [...s];
  const parts = s.split(sep);
  return limit === null ? parts : parts.slice(0, limit);
});

/**
 * `replace` accepts a String or a RegExp. A RegExp's `g` flag decides
 * replace-first vs replace-all, and the replacement supports `$1` capture
 * references: `"John Smith".replace(/(\w+) (\w+)/, "$2, $1")` -> "Smith, John".
 */
defineMethod("string", "replace", ({ target, args }) => {
  const s = str(target, "replace");
  const pattern = args[0];
  const replacement = args[1] === undefined || args[1] === null ? "" : toDisplayString(args[1]);
  if (pattern instanceof RegExp) {
    // A fresh RegExp so the `lastIndex` of a global pattern does not leak.
    const rx = new RegExp(pattern.source, pattern.flags);
    return s.replace(rx, replacement);
  }
  if (typeof pattern === "string") {
    return s.replace(pattern, replacement);
  }
  throw new BasesError(
    `Type error in "replace", pattern expects String or RegExp, given ${describeType(pattern)}`,
  );
});

defineMethod("string", "asFile", ({ target, ctx }) => {
  const s = str(target, "asFile");
  return ctx.file.accessors.resolve(s) ?? null;
});

function titleCase(s: string): string {
  return s.replace(/\w\S*/g, (word) => word.charAt(0).toUpperCase() + word.slice(1).toLowerCase());
}

// =========================================================================
// RegExp
// =========================================================================

defineMethod("object", "matches", ({ target, args }) => {
  if (!(target instanceof RegExp)) {
    throw new BasesError(`Type error in "matches", expects RegExp, given ${describeType(target)}`);
  }
  const v = args[0];
  if (typeof v !== "string") {
    throw new BasesError(
      `Type error in "matches", parameter expects String, given ${describeType(v)}`,
    );
  }
  return new RegExp(target.source, target.flags.replace("g", "")).test(v);
});

// =========================================================================
// Number methods
// =========================================================================

/**
 * Narrow a receiver to Number. Methods are registered per receiver type, so a
 * mismatch here means the dispatch table and this file disagree.
 */
function num(v: BasesValue, fn: string): number {
  if (typeof v === "number") return v;
  // A numeric string still works: Obsidian coerces in arithmetic contexts.
  const n = toNumberLoose(v);
  if (n !== null) return n;
  throw new BasesError(
    `Type error in "${fn}", parameter expects Number, given ${describeType(v)}`,
    {
      construct: fn,
    },
  );
}

defineMethod("number", "abs", ({ target }) => Math.abs(num(target, "abs")));
defineMethod("number", "ceil", ({ target }) => Math.ceil(num(target, "ceil")));
defineMethod("number", "floor", ({ target }) => Math.floor(num(target, "floor")));
/** Half-up, not banker's rounding: `(2.5).round()` is 3, per the docs. */
defineMethod("number", "round", ({ target, args }) => {
  const digits = args.length > 0 ? (toNumberLoose(args[0] ?? null) ?? 0) : 0;
  const scaled = num(target, "round") * 10 ** digits;
  // Round half away from zero, which is what Obsidian documents.
  const rounded = scaled < 0 ? -Math.round(-scaled) : Math.round(scaled);
  return rounded / 10 ** digits;
});
defineMethod("number", "toFixed", ({ target, args }) => {
  const d = toNumberLoose(args[0] ?? null) ?? 0;
  return num(target, "toFixed").toFixed(d);
});

// =========================================================================
// List methods
// =========================================================================

function list(v: BasesValue, fn: string): BasesValue[] {
  if (!isList(v)) {
    throw new BasesError(
      `Type error in "${fn}", parameter expects List, given ${describeType(v)}`,
      {
        construct: fn,
      },
    );
  }
  return v;
}

defineMethod("list", "contains", ({ target, args }) => {
  const items = list(target, "contains");
  const needle = args[0];
  if (isLink(needle) || needle instanceof FileValue) {
    return items.some((item) => valuesEqualLoose(item, needle));
  }
  if (isDate(needle)) {
    return items.some((item) => isDate(item) && item.ms === needle.ms);
  }
  if (isDuration(needle)) {
    return items.some((item) => isDuration(item) && item.ms === needle.ms);
  }
  if (isList(needle)) {
    return items.some((item) => isList(item) && valuesEqualLoose(item, needle));
  }
  if (needle === null || needle === undefined) {
    return items.some((item) => item === null || item === undefined);
  }
  if (typeof needle !== "string" && typeof needle !== "number" && typeof needle !== "boolean") {
    throw new BasesError(
      `Type error in "contains", parameter "value". Expected String not, given ${describeType(needle)}.`,
      { construct: "contains" },
    );
  }
  return items.some((item) => valuesEqualLoose(item, needle));
});

defineMethod("list", "containsAll", ({ target, args }) =>
  args.every((needle) => listContains(target, needle)),
);
defineMethod("list", "containsAny", ({ target, args }) =>
  args.some((needle) => listContains(target, needle)),
);

function listContains(target: BasesValue, needle: BasesValue): boolean {
  const items = list(target, "contains");
  return items.some((item) => valuesEqualLoose(item, needle));
}

defineMethod("list", "join", ({ target, args }) => {
  const sep = args[0] === undefined || args[0] === null ? "" : toDisplayString(args[0]);
  return list(target, "join").map(toDisplayString).join(sep);
});

defineMethod("list", "unique", ({ target }) => {
  const items = list(target, "unique");
  const out: BasesValue[] = [];
  for (const item of items) {
    if (!out.some((existing) => valuesEqualLoose(existing, item))) out.push(item);
  }
  return out;
});

defineMethod("list", "flat", ({ target }) =>
  list(target, "flat").flatMap((v) => (isList(v) ? v : [v])),
);

defineMethod("list", "reverse", ({ target }) => [...list(target, "reverse")].reverse());

defineMethod("list", "sort", ({ target }) => {
  const items = [...list(target, "sort")];
  return items.sort((a, b) => compareLoose(a, b));
});

defineMethod("list", "slice", ({ target, args }) => {
  const s = toNumberLoose(args[0] ?? null) ?? 0;
  const e = args.length > 1 ? toNumberLoose(args[1] ?? null) : null;
  return e === null ? list(target, "slice").slice(s) : list(target, "slice").slice(s, e);
});

defineMethod("list", "sum", ({ target }) =>
  list(target, "sum").reduce<number>((acc, v) => acc + (toNumberLoose(v) ?? 0), 0),
);
defineMethod("list", "mean", ({ target }) => {
  const items = list(target, "mean");
  if (items.length === 0) return null;
  return items.reduce<number>((acc, v) => acc + (toNumberLoose(v) ?? 0), 0) / items.length;
});
defineMethod("list", "count", ({ target }) => list(target, "count").length);
defineMethod("list", "min", ({ target }) => extremum(list(target, "min"), -1));
defineMethod("list", "max", ({ target }) => extremum(list(target, "max"), 1));

function extremum(items: BasesValue[], dir: 1 | -1): BasesValue {
  if (items.length === 0) return null;
  let best: BasesValue = items[0]!;
  for (const v of items.slice(1)) {
    if (compareLoose(v, best) === dir) best = v;
  }
  return best;
}

// -- higher-order list methods ---------------------------------------------

/**
 * `filter`, `map` and `reduce` take an expression, not a value, so the argument
 * must be evaluated per element. The evaluator detects these methods and passes
 * a callback that evaluates the body with `value` / `index` / `acc` bound; this
 * module only holds the per-method iteration logic.
 */
export interface HigherOrderDef {
  params: string[];
  /** Iterate, binding params[0]=value, params[1]=index (and params[2]=acc). */
  run: (
    items: BasesValue[],
    evalItem: (value: BasesValue, index: number, acc?: BasesValue) => BasesValue,
    seed?: BasesValue,
  ) => BasesValue;
}

export const higherOrderImpls = new Map<string, HigherOrderDef>();

export function lookupHigherOrder(key: string): HigherOrderDef | undefined {
  return higherOrderImpls.get(key);
}

function defineHigherOrder(
  type: string,
  name: string,
  params: string[],
  run: HigherOrderDef["run"],
): void {
  let byName = methods.get(type);
  if (byName === undefined) {
    byName = new Map();
    methods.set(type, byName);
  }
  byName.set(name, {
    lambda: { higherOrder: true, param: (i) => params[Math.min(i, params.length - 1)] ?? "value" },
    fn: () => {
      throw new BasesError("Internal error: higher-order method reached the value path");
    },
  });
  higherOrderImpls.set(`${type}.${name}`, { params, run });
}

defineHigherOrder("list", "filter", ["value", "index"], (items, b) =>
  items.filter((v, i) => isTruthy(b(v, i))),
);

defineHigherOrder("list", "map", ["value", "index"], (items, b) => items.map((v, i) => b(v, i)));

defineHigherOrder("list", "find", ["value", "index"], (items, b) => {
  for (let i = 0; i < items.length; i++) {
    if (isTruthy(b(items[i]!, i))) return items[i]!;
  }
  return null;
});

defineHigherOrder("list", "some", ["value", "index"], (items, b) =>
  items.some((v, i) => isTruthy(b(v, i))),
);

defineHigherOrder("list", "every", ["value", "index"], (items, b) =>
  items.every((v, i) => isTruthy(b(v, i))),
);

defineHigherOrder("list", "flatMap", ["value", "index"], (items, b) =>
  items.flatMap((v, i) => {
    const r = b(v, i);
    return isList(r) ? r : [r];
  }),
);

/**
 * `reduce` binds `value`, `index` and `acc`. The accumulator starts as the
 * explicit second argument, which real vaults seed with either `0` or `null`.
 */
/**
 * `reduce` binds `value`, `index` and `acc`, and the accumulator starts as the
 * explicit seed argument. Real vaults seed with `0` for a sum or `null` for the
 * documented max idiom, so the seed must not default to the first element.
 */
defineHigherOrder("list", "reduce", ["value", "index", "acc"], (items, b, seed) => {
  let acc: BasesValue = seed ?? null;
  for (let i = 0; i < items.length; i++) {
    acc = b(items[i]!, i, acc);
  }
  return acc;
});

// =========================================================================
// Date methods
// =========================================================================

defineMethod("date", "format", ({ target, args }) => {
  const d = asDate(target, "format");
  const pattern = args[0];
  if (typeof pattern !== "string") {
    throw new BasesError(
      `Type error in "format", parameter expects String, given ${describeType(pattern)}`,
    );
  }
  return formatDate(d, pattern);
});

defineMethod("date", "date", ({ target }) => asDate(target, "date").date());
defineMethod("date", "time", ({ target }) => asDate(target, "time").time());
defineMethod("date", "relative", ({ target }) => relativeTime(asDate(target, "relative")));
defineMethod("date", "asLink", ({ target }) => new LinkValue(asDate(target, "asLink").toString()));
defineMethod("date", "asFile", () => null);

/**
 * Moment-style date formatting, limited to the tokens the docs document.
 * Unrecognised characters pass through, so literal text still renders.
 */
export function formatDate(d: DateValue, pattern: string): string {
  const pad2 = (n: number) => (n < 10 ? `0${n}` : String(n));
  const monthNames = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
  ];
  const shortMonth = monthNames.map((m) => m.slice(0, 3));

  // Tokenise longest-first so YYYY wins over YY and YYYY-[W]WW escapes work.
  const tokens: string[] = [];
  let i = 0;
  while (i < pattern.length) {
    if (pattern[i] === "[") {
      const close = pattern.indexOf("]", i);
      if (close !== -1) {
        tokens.push({ literal: true, text: pattern.slice(i + 1, close) } as unknown as string);
        i = close + 1;
        continue;
      }
    }
    const four = pattern.slice(i, i + 4);
    const three = pattern.slice(i, i + 3);
    const two = pattern.slice(i, i + 2);
    if (four === "YYYY") {
      tokens.push(String(d.year));
      i += 4;
      continue;
    }
    if (three === "MMM") {
      tokens.push(shortMonth[d.month - 1]!);
      i += 3;
      continue;
    }
    if (two === "YY") {
      tokens.push(pad2(d.year % 100));
      i += 2;
      continue;
    }
    if (two === "MM") {
      tokens.push(pad2(d.month));
      i += 2;
      continue;
    }
    if (two === "DD") {
      tokens.push(pad2(d.day));
      i += 2;
      continue;
    }
    if (two === "HH") {
      tokens.push(pad2(d.hour));
      i += 2;
      continue;
    }
    if (two === "hh") {
      tokens.push(pad2(((d.hour + 11) % 12) + 1));
      i += 2;
      continue;
    }
    if (two === "mm") {
      tokens.push(pad2(d.minute));
      i += 2;
      continue;
    }
    if (two === "ss") {
      tokens.push(pad2(d.second));
      i += 2;
      continue;
    }
    const ch = pattern[i]!;
    if (ch === "M") {
      tokens.push(String(d.month));
      i += 1;
      continue;
    }
    if (ch === "D") {
      tokens.push(String(d.day));
      i += 1;
      continue;
    }
    if (ch === "H") {
      tokens.push(String(d.hour));
      i += 1;
      continue;
    }
    if (ch === "h") {
      tokens.push(String(((d.hour + 11) % 12) + 1));
      i += 1;
      continue;
    }
    if (ch === "m") {
      tokens.push(String(d.minute));
      i += 1;
      continue;
    }
    if (ch === "s") {
      tokens.push(String(d.second));
      i += 1;
      continue;
    }
    if (ch === "A") {
      tokens.push(d.hour < 12 ? "AM" : "PM");
      i += 1;
      continue;
    }
    if (ch === "a") {
      tokens.push(d.hour < 12 ? "am" : "pm");
      i += 1;
      continue;
    }
    tokens.push(ch);
    i += 1;
  }
  return tokens.join("");
}

function relativeTime(d: DateValue): string {
  const delta = d.ms - Date.now();
  const abs = Math.abs(delta);
  const future = delta > 0;
  const units: Array<[number, string]> = [
    [1000, "second"],
    [60_000, "minute"],
    [3_600_000, "hour"],
    [86_400_000, "day"],
    [604_800_000, "week"],
    [2_592_000_000, "month"],
    [31_536_000_000, "year"],
  ];
  if (abs < 1000) return "just now";
  for (const [ms, unit] of units) {
    if (abs < ms) continue;
    const n = Math.floor(abs / ms);
    const plural = n === 1 ? unit : `${unit}s`;
    return future ? `in ${n} ${plural}` : `${n} ${plural} ago`;
  }
  return "just now";
}

// =========================================================================
// Link methods
// =========================================================================

defineMethod("link", "asFile", ({ target, ctx }) => {
  const l = target as LinkValue;
  if (l.resolvedPath !== undefined) {
    return ctx.file.accessors.resolve(l.resolvedPath) ?? null;
  }
  return ctx.file.accessors.resolve(l.target) ?? null;
});

defineMethod("link", "linksTo", ({ target, args, ctx }) => {
  const l = target as LinkValue;
  const other = args[0];
  if (other === null || other === undefined) return false;
  const to =
    other instanceof FileValue
      ? other.path
      : isLink(other)
        ? (other.resolvedPath ?? other.target)
        : typeof other === "string"
          ? other
          : null;
  if (to === null) return false;
  // "Does the file this link points at itself link onward to `to`?"
  const from = l.resolvedPath ?? l.target;
  const fromFile = ctx.file.accessors.resolve(from);
  if (fromFile === undefined) return false;
  return fromFile.accessors.links().some((x) => linkPointsAt(x, to));
});

/** Compare a link target against a path, tolerating extension and folder depth. */
export function matchesLinkText(a: string, b: string): boolean {
  if (a === b) return true;
  const na = normaliseLink(a);
  const nb = normaliseLink(b);
  if (na === nb) return true;
  return na.endsWith(`/${nb}`) || nb.endsWith(`/${na}`);
}

function normaliseLink(s: string): string {
  return stripExtension(s.trim().replace(/\.md$/, "")).toLowerCase();
}

defineMethod("link", "contains", ({ target, args }) => {
  // `subcategory.contains(link("Note#Heading", "Alias"))` is a documented
  // pattern, so compare against the link text when the receiver is a string.
  const s = toDisplayString(target);
  const needle = args[0];
  if (isLink(needle) || needle instanceof FileValue) {
    const t = needle instanceof FileValue ? needle.path : (needle as LinkValue).target;
    return matchesLinkText(s, t);
  }
  return s.includes(toDisplayString(needle));
});

defineMethod("link", "toString", ({ target }) => (target as LinkValue).toWikilink());

// =========================================================================
// File methods
// =========================================================================

defineMethod("file", "asLink", ({ target, args }) => {
  const f = target as FileValue;
  const display = args[0];
  return new LinkValue(f.path, displayString(display), f.path);
});

defineMethod("file", "hasTag", ({ target, args }) => {
  const f = target as FileValue;
  const tags = f.accessors.tags().map((t) => normaliseTag(t));
  return args.some((a) => {
    const want = normaliseTag(a);
    if (typeof want !== "string") return false;
    // Nested tags match: hasTag("tag1") is true for #tag1/a.
    return tags.some((t) => t === want || t.startsWith(`${want}/`));
  });
});

defineMethod("file", "inFolder", ({ target, args }) => {
  const f = target as FileValue;
  const want = toDisplayString(args[0] ?? "").replace(/\/+$/, "");
  if (want === "") return true;
  if (f.folder === want) return true;
  // inFolder is recursive; `file.folder ==` is not.
  return f.folder.startsWith(`${want}/`);
});

defineMethod("file", "hasProperty", ({ target, args }) => {
  const f = target as FileValue;
  const name = toDisplayString(args[0] ?? "");
  if (name === "") return false;
  if (Object.hasOwn(f.accessors.properties(), name)) return true;
  return filePropertyExists(f, name);
});

defineMethod("file", "hasLink", ({ target, args }) => {
  const f = target as FileValue;
  const other = args[0];
  if (other === null || other === undefined) return false;
  const to =
    other instanceof FileValue
      ? other.path
      : isLink(other)
        ? (other.resolvedPath ?? other.target)
        : typeof other === "string"
          ? other
          : null;
  if (to === null) return false;
  return f.accessors.links().some((l) => linkPointsAt(l, to));
});

defineMethod("file", "contains", ({ target, args }) => {
  const f = target as FileValue;
  const needle = args[0];
  if (isLink(needle) || needle instanceof FileValue) {
    const to = needle instanceof FileValue ? needle.path : (needle as LinkValue).target;
    return f.accessors.links().some((l) => linkPointsAt(l, to));
  }
  return false;
});

function linkPointsAt(l: BasesValue, path: string): boolean {
  if (isLink(l)) return matchesLinkText(l.resolvedPath ?? l.target, path);
  if (typeof l === "string") return matchesLinkText(l, path);
  return false;
}

function filePropertyExists(_f: FileValue, name: string): boolean {
  switch (name) {
    case "name":
    case "basename":
    case "path":
    case "folder":
    case "ext":
    case "size":
    case "ctime":
    case "mtime":
    case "tags":
    case "links":
    case "embeds":
    case "backlinks":
    case "properties":
    case "file":
      return true;
    default:
      return false;
  }
}

/** `hasTag` accepts the bare name; `file.tags` elements carry a leading `#`. */
function normaliseTag(v: BasesValue): string {
  if (typeof v !== "string") return "";
  return v.trim().replace(/^#+/, "");
}

// =========================================================================
// Object methods
// =========================================================================

defineMethod("object", "keys", ({ target }) => {
  if (target === null || target === undefined) return [];
  if (target instanceof FileValue) return ["name", "path", "folder", "ext", "basename"];
  if (isList(target)) return target.map((_, i) => String(i));
  if (typeof target === "object") return Object.keys(target);
  return [];
});

defineMethod("object", "values", ({ target }) => {
  if (target === null || target === undefined) return [];
  if (target instanceof FileValue) {
    return [target.name, target.path, target.folder, target.ext, target.basename];
  }
  if (isList(target)) return [...target];
  if (typeof target === "object") return Object.values(target);
  return [target];
});

// =========================================================================
// Shared helpers
// =========================================================================

function asDate(v: BasesValue, fn: string): DateValue {
  if (isDate(v)) return v;
  const c = coerceDate(v);
  if (c === null) {
    throw new BasesError(`Type error in "${fn}", parameter expects Date, given ${describeType(v)}`);
  }
  return c;
}

function valuesEqualLoose(a: BasesValue, b: BasesValue): boolean {
  if (isLink(a) || isLink(b) || a instanceof FileValue || b instanceof FileValue) {
    const pa = a instanceof FileValue ? a.path : isLink(a) ? (a.resolvedPath ?? a.target) : null;
    const pb = b instanceof FileValue ? b.path : isLink(b) ? (b.resolvedPath ?? b.target) : null;
    if (pa !== null && pb !== null) return matchesLinkText(pa, pb);
  }
  if (isDate(a) && isDate(b)) return a.ms === b.ms;
  if (isDate(a) && typeof b === "string") {
    const db = coerceDate(b);
    return db !== null && db.ms === a.ms;
  }
  if (isDuration(a) && isDuration(b)) return a.ms === b.ms;
  if (typeof a === "string" && typeof b === "string") return a === b;
  return a === b;
}

function compareLoose(a: BasesValue, b: BasesValue): number {
  if (a === null) return b === null ? 0 : -1;
  if (b === null) return 1;
  if (isDate(a) || isDate(b)) {
    const da = coerceDate(a);
    const db = coerceDate(b);
    if (da !== null && db !== null) return da.ms - db.ms;
  }
  if (isDuration(a) && isDuration(b)) return a.ms - b.ms;
  const na = toNumberLoose(a);
  const nb = toNumberLoose(b);
  if (na !== null && nb !== null) return na - nb;
  const sa = toDisplayString(a);
  const sb = toDisplayString(b);
  return sa < sb ? -1 : sa > sb ? 1 : 0;
}

export type { Node };
export { evaluate as evaluateNode };
