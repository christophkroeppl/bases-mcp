/**
 * The base query pipeline.
 *
 * Stages run in the order Obsidian documents: formulas, then global filters
 * AND view filters, then sort, group, limit and summaries. Formulas are
 * evaluated once per note rather than once per (note, column) pair, and are
 * topologically ordered so a formula may reference another formula.
 */

import { BasesError } from "../expr/errors";
import {
  compare,
  type EvalContext,
  evaluate,
  evaluateExpression,
  isTruthy,
  type ThisContext,
} from "../expr/evaluator";
import { type Node, parse, tryParse } from "../expr/parser";
import type { BasesValue } from "../expr/values";
import { BASE_EXT } from "../vault/source";
import type { Vault } from "../vault/vault";
import { type BaseFile, type BaseView, type FilterNode, selectView } from "./parse";

export interface ResolvedRow {
  path: string;
  /** Computed formula values, keyed by formula name. */
  formula: Record<string, BasesValue>;
  /** Cached property values, keyed by canonical Property ID. */
  values: Record<string, BasesValue>;
}

export interface QueryGroup {
  /** The group key, as a display string. */
  key: string;
  rows: ResolvedRow[];
}

export interface QueryResult {
  basePath: string;
  view: BaseView;
  rows: ResolvedRow[];
  /** Present only when the view has a `groupBy`. */
  groups: QueryGroup[] | null;
  /** Pre-limit match count, which is what the UI's "N of M" reports. */
  total: number;
  /** The host note bound to `this`, when one was supplied. */
  context: string | null;
  /** Problems that did not prevent a result, e.g. one bad filter. */
  warnings: string[];
}

export interface QueryOptions {
  /** The host note path; binds `this`. */
  context?: string | null;
  view?: string;
}

/**
 * Resolve a base's rows.
 *
 * Throws rather than returning an empty result when a base references `this`
 * and no host was supplied: the Obsidian CLI returns `[]` in that case, and
 * silently mirroring that is exactly what makes the behaviour undebuggable.
 */
export function queryBase(
  vault: Vault,
  basePath: string,
  base: BaseFile,
  options: QueryOptions = {},
): QueryResult {
  const view = selectView(base, options.view);
  const warnings: string[] = [];

  const thisValue = resolveHostNote(vault, options.context ?? null);

  // Formulas first: they may reference each other, so order them.
  const formulaOrder = orderFormulas(base.formulas);
  const compiledFormulas = new Map<string, ReturnType<typeof parse>>();
  for (const name of formulaOrder) {
    try {
      compiledFormulas.set(name, parse(base.formulas[name]!));
    } catch (err) {
      throw new BasesError(`Formula "${name}" failed to parse: ${(err as Error).message}`, {
        construct: name,
        note: basePath,
      });
    }
  }

  // Pre-parse the filters once rather than per note.
  const globalFilter = compileFilter(base.filters, "filters", basePath);
  const viewFilter = compileFilter(view.filters, `views.${view.name}.filters`, basePath);

  const rows: ResolvedRow[] = [];
  for (const notePath of vault.notePaths()) {
    const file = vault.fileValue(notePath);
    const record = vault.note(notePath);
    const note = record?.frontmatter ?? {};

    const ctx: EvalContext = {
      note,
      file,
      formula: {},
      thisValue,
    };

    // Evaluate every formula for this note, writing each result into `ctx`
    // as it goes so a formula that references another formula sees it.
    //
    // This used to accumulate into a separate object and assign it to
    // `ctx.formula` afterwards, which meant `formula.b` in
    // `{b: "formula.a + 1", a: "41"}` read `null` -- the topological sort above
    // ordered the names correctly and the values were still not visible.
    for (const name of formulaOrder) {
      ctx.formula[name] = evaluate(compiledFormulas.get(name)!, ctx);
    }

    if (globalFilter !== null && !runFilter(globalFilter, ctx)) continue;
    if (viewFilter !== null && !runFilter(viewFilter, ctx)) continue;

    rows.push({ path: notePath, formula: { ...ctx.formula }, values: {} });
  }

  const total = rows.length;

  // Cache the property values each row needs, after filtering.
  const needed = collectProperties(base, view);
  for (const row of rows) {
    const file = vault.fileValue(row.path);
    const record = vault.note(row.path);
    const ctx: EvalContext = {
      note: record?.frontmatter ?? {},
      file,
      formula: row.formula,
      thisValue,
    };
    const values: Record<string, BasesValue> = {};
    for (const id of needed) {
      values[canonical(id)] = resolveProperty(id, ctx);
    }
    row.values = values;
  }

  // Sort. `sort` is authoritative; `groupBy` then applies within groups.
  if (view.sort !== undefined && view.sort.length > 0) {
    sortRows(rows, view.sort);
  } else {
    // A view with no `sort` is still ordered, and it is NOT path order.
    // Probed on Obsidian 1.13.7: an unsorted view over notes whose names and
    // paths sort differently returns them by `file.name` ascending, regardless
    // of what `order` lists first.
    sortRows(rows, [{ property: "file.name", direction: "ASC" }]);
  }

  // Group.
  let groups: QueryGroup[] | null = null;
  if (view.groupBy !== undefined) {
    groups = groupRows(rows, view.groupBy.property, view.groupBy.direction);
  }

  // Limit: total across all groups, matching the UI.
  let limited = rows;
  if (view.limit !== undefined && view.limit >= 0) {
    if (groups !== null) {
      const budget = view.limit;
      const kept: QueryGroup[] = [];
      let used = 0;
      for (const g of groups) {
        if (used >= budget) break;
        const take = g.rows.slice(0, budget - used);
        used += take.length;
        if (take.length > 0) kept.push({ key: g.key, rows: take });
      }
      groups = kept;
      limited = kept.flatMap((g) => g.rows);
    } else {
      limited = rows.slice(0, view.limit);
    }
  }

  return {
    basePath,
    view,
    rows: limited,
    groups,
    total,
    context: options.context ?? null,
    warnings,
  };
}

// ---------------------------------------------------------------------------

/**
 * Bind `this` to the Host note, or refuse the value in the one voice every
 * caller uses.
 *
 * Shared with the draft verifier on purpose. A bad `context` is the same
 * mistake whichever tool it arrived through, and an agent that met two
 * vocabularies for it would have to learn the message as well as the rule; one
 * function is the only way to keep them in step.
 *
 * Absent (`null`) is the one value that is not an error, because it is how a
 * caller says "this base does not need a host note". Every other value must
 * name a note, and each way of failing to is named for what it actually is:
 * a `.base` and a folder both EXIST in the vault, so reporting either as
 * missing sends the agent hunting for a typo in a file it can already see.
 */
export function resolveHostNote(vault: Vault, context: string | null): ThisContext | undefined {
  if (context === null) return undefined;
  if (context.trim() === "") throw badContext(context, "is empty");

  // `resolve` indexes notes only, so a value that lands here resolved to
  // nothing: it is a path, but not to a note.
  const resolved = vault.resolve(context);
  const record = resolved === undefined ? undefined : vault.note(resolved);
  if (record === undefined) throw badContext(context, describeNonNote(vault, context));
  return { file: vault.fileValue(resolved!), note: record.frontmatter };
}

/** What the value turned out to be, for the message that refuses it. */
function describeNonNote(vault: Vault, context: string): string {
  // A trailing slash names the same folder as its absence, and an agent that
  // got `Projects/` deserves to be told it is a folder rather than that it does
  // not exist -- the same file it just named without the slash.
  const wanted = context.endsWith("/") ? context.slice(0, -1) : context;

  // A folder is checked before a base, because `Tickets` is both the folder
  // and the base's stem; the folder is what an agent naming it almost always
  // means, and it is the reading that is true either way.
  const isFolder = [...vault.notePaths(), ...vault.basePaths()].some((p) =>
    p.startsWith(`${wanted}/`),
  );
  if (isFolder) return `"${context}" is a folder, and a folder is never a Host note`;

  const isBase = vault.basePaths().some((p) => p === wanted || p === `${wanted}${BASE_EXT}`);
  if (isBase) return `"${context}" is a .base file, and a Base is never a Host note`;

  return `"${context}" does not exist in this vault`;
}

/**
 * The one refusal, naming the field, the value and the expectation.
 *
 * `note` carries the offending path so a client can act on it without parsing
 * prose. The tail is the same for every cause because the fix is the same:
 * pass the Host note, or drop the field when the base does not need one.
 */
function badContext(context: string, what: string): BasesError {
  return new BasesError(
    `\`context\` ${what}. It must be the path of the Host note that binds \`this\` -- the note a ` +
      `Base is embedded in, e.g. "Projects/SomeProject.md" -- or omitted for a Base that does not ` +
      `reference \`this\`.`,
    { note: context },
  );
}

type CompiledFilter =
  | { kind: "expr"; node: ReturnType<typeof parse> }
  | { kind: "and" | "or" | "not"; children: CompiledFilter[] };

function compileFilter(
  node: FilterNode | undefined,
  where: string,
  basePath: string,
): CompiledFilter | null {
  if (node === undefined) return null;

  if (typeof node === "string") {
    try {
      return { kind: "expr", node: parse(node) };
    } catch (err) {
      throw new BasesError(`Filter at ${where} failed to parse: ${(err as Error).message}`, {
        note: basePath,
      });
    }
  }

  if ("and" in node && node.and !== undefined) {
    return { kind: "and", children: node.and.map((c) => compileFilter(c, where, basePath)!) };
  }
  if ("or" in node && node.or !== undefined) {
    return { kind: "or", children: node.or.map((c) => compileFilter(c, where, basePath)!) };
  }
  if ("not" in node && node.not !== undefined) {
    return { kind: "not", children: node.not.map((c) => compileFilter(c, where, basePath)!) };
  }
  throw new BasesError(`Unrecognised filter at ${where} in ${basePath}`, { note: basePath });
}

/**
 * Evaluate a compiled filter. `not` is NAND -- "none of these are true" --
 * which is what the docs specify and what makes a six-sibling `not:` exclude a
 * note matching any one of them.
 */
function runFilter(f: CompiledFilter, ctx: EvalContext): boolean {
  switch (f.kind) {
    case "expr":
      return isTruthy(evaluate(f.node, ctx));
    case "and":
      return f.children.every((c) => runFilter(c, ctx));
    case "or":
      return f.children.some((c) => runFilter(c, ctx));
    case "not":
      return !f.children.some((c) => runFilter(c, ctx));
  }
}

/** Topologically order formulas so a formula may reference another. */
export function orderFormulas(formulas: Record<string, string>): string[] {
  const names = Object.keys(formulas);
  const deps = new Map<string, Set<string>>();
  for (const name of names) {
    const found = new Set<string>();
    for (const other of names) {
      if (other === name) continue;
      if (referencesFormula(formulas[name]!, other)) found.add(other);
    }
    deps.set(name, found);
  }

  const out: string[] = [];
  const state = new Map<string, "visiting" | "done">();

  const visit = (name: string, stack: string[]): void => {
    const s = state.get(name);
    if (s === "done") return;
    if (s === "visiting") {
      throw new BasesError(`Formulas form a circular reference: ${[...stack, name].join(" -> ")}`, {
        construct: name,
      });
    }
    state.set(name, "visiting");
    for (const dep of deps.get(name) ?? []) visit(dep, [...stack, name]);
    state.set(name, "done");
    out.push(name);
  };

  for (const name of names) visit(name, []);
  return out;
}

/** Does `expr` reference `formula.<name>` (or a bare `name` that is a formula)? */
function referencesFormula(expr: string, name: string): boolean {
  if (expr.includes(`formula.${name}`)) return true;
  const ast = tryParse(expr);
  return ast === null ? false : nodeMentionsFormula(ast, name);
}

/**
 * True when the AST contains a reference to `formula.<name>`, or a bare
 * identifier equal to `name` that is not itself a function call. Real vaults
 * use both spellings -- `formula.ppu` and bare `needs_follow_up`.
 */
function nodeMentionsFormula(node: Node, name: string): boolean {
  switch (node.type) {
    case "Identifier":
      return node.name === name;
    case "Literal":
      return false;
    case "Member":
      return nodeMentionsFormula(node.object, name);
    case "Unary":
      return nodeMentionsFormula(node.operand, name);
    case "Binary":
      return nodeMentionsFormula(node.left, name) || nodeMentionsFormula(node.right, name);
    case "Index":
      return nodeMentionsFormula(node.object, name) || nodeMentionsFormula(node.index, name);
    case "List":
      return node.elements.some((e) => nodeMentionsFormula(e, name));
    case "Call": {
      // A call's callee is a function name, not a formula reference.
      const calleeMentions =
        node.callee.type === "Identifier"
          ? node.callee.name === name
          : nodeMentionsFormula(node.callee, name);
      return calleeMentions || node.args.some((a) => nodeMentionsFormula(a, name));
    }
  }
}

/** Every property ID the pipeline must resolve for the row cache. */
function collectProperties(base: BaseFile, view: BaseView): string[] {
  const needed = new Set<string>();
  for (const id of Object.keys(base.properties)) needed.add(canonical(id));
  for (const id of view.order ?? []) needed.add(canonical(id));
  if (view.groupBy !== undefined) needed.add(canonical(view.groupBy.property));
  for (const id of Object.keys(view.summaries ?? {})) needed.add(canonical(id));
  for (const s of view.sort ?? []) needed.add(canonical(s.property));
  needed.add("file.name");
  needed.add("file.path");
  return [...needed];
}

/**
 * Normalise a Property ID: a bare identifier is a note property. Obsidian's
 * own UI rewrites bare keys to the prefixed form, so real vaults contain both.
 */
export function canonical(id: string): string {
  const trimmed = id.trim();
  if (
    trimmed.startsWith("note.") ||
    trimmed.startsWith("file.") ||
    trimmed.startsWith("formula.")
  ) {
    return trimmed;
  }
  return `note.${trimmed}`;
}

/** Resolve one Property ID against a row's context. */
export function resolveProperty(id: string, ctx: EvalContext): BasesValue {
  const prop = canonical(id);
  if (prop.startsWith("formula.")) {
    const name = prop.slice("formula.".length);
    return ctx.formula[name] ?? null;
  }
  // `file.*` members are handled natively by the FileValue; expression syntax
  // covers them too (e.g. `file.name`), so reuse the evaluator.
  try {
    return evaluateExpression(prop, ctx);
  } catch (err) {
    if (err instanceof BasesError) throw err;
    throw new BasesError(`Could not resolve property "${prop}": ${(err as Error).message}`, {
      property: prop,
    });
  }
}

function sortRows(rows: ResolvedRow[], sort: NonNullable<BaseView["sort"]>): void {
  rows.sort((a, b) => {
    for (const entry of sort) {
      const id = canonical(entry.property);
      const av = a.values[id];
      const bv = b.values[id];
      if (av === undefined || bv === undefined) continue;
      const cmp = compare(av, bv);
      if (cmp !== 0) return entry.direction === "DESC" ? -cmp : cmp;
    }
    // Stable tie-break on path so output is deterministic.
    return a.path < b.path ? -1 : a.path > b.path ? 1 : 0;
  });
}

function groupRows(rows: ResolvedRow[], property: string, direction: "ASC" | "DESC"): QueryGroup[] {
  const id = canonical(property);
  const map = new Map<string, QueryGroup>();

  for (const row of rows) {
    const key = groupKey(row.values[id]);
    let g = map.get(key);
    if (g === undefined) {
      g = { key, rows: [] };
      map.set(key, g);
    }
    g.rows.push(row);
  }

  const groups = [...map.values()];
  groups.sort((a, b) => (a.key < b.key ? -1 : a.key > b.key ? 1 : 0));
  if (direction === "DESC") groups.reverse();

  // Within each group, order by the same deterministic path rule.
  for (const g of groups) {
    g.rows.sort((a, b) => (a.path < b.path ? -1 : a.path > b.path ? 1 : 0));
  }
  return groups;
}

function groupKey(v: BasesValue | undefined): string {
  if (v === null || v === undefined) return "(empty)";
  if (Array.isArray(v)) {
    if (v.length === 0) return "(empty)";
    return v
      .map((x) => groupKey(x))
      .sort()
      .join(", ");
  }
  if (typeof v === "object") {
    const rec = v as unknown as Record<string, unknown>;
    if (typeof rec["resolvedPath"] === "string") return String(rec["resolvedPath"]);
    if (typeof rec["target"] === "string") return String(rec["target"]);
  }
  if (v === "") return "(empty)";
  if (typeof v === "boolean" || typeof v === "number") return String(v);
  return String(v);
}
