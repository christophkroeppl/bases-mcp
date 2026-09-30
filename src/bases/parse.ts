/**
 * `.base` file parsing.
 *
 * The view level is a documented open namespace: plugins persist arbitrary keys
 * there (`cardOrders`, `columnColors`, `swimlaneOrders`, and composite keys
 * joined by `\x1f` or `||`). Those keys are PRESERVED verbatim and never
 * interpreted -- a resolver that rejects or drops them corrupts real vaults.
 */

import { parse as parseYaml } from "yaml";

import { BasesError } from "../expr/errors";

export type Direction = "ASC" | "DESC";

export interface SortEntry {
  property: string;
  direction: Direction;
}

export interface GroupBy {
  property: string;
  direction: Direction;
}

export interface BaseView {
  type: string;
  name: string;
  limit?: number;
  filters?: FilterNode;
  order?: string[];
  groupBy?: GroupBy;
  sort?: SortEntry[];
  summaries?: Record<string, string>;
  /**
   * Every other key at the view level, preserved untouched. Includes the
   * documented core keys (`rowHeight`, `columnSize`) and every plugin key.
   */
  extra: Record<string, unknown>;
}

export type FilterNode = FilterObject | string;

export interface FilterObject {
  and?: FilterNode[];
  or?: FilterNode[];
  /** "None of the following are true" -- NAND, not logical negation. */
  not?: FilterNode[];
}

export interface BaseFile {
  /** Global filters, ANDed with each view's own filters. */
  filters?: FilterNode;
  /** Formula name -> expression. */
  formulas: Record<string, string>;
  /** Property ID -> display config. */
  properties: Record<string, PropertyConfig>;
  /** Custom summary name -> expression over `values`. */
  summaries: Record<string, string>;
  views: BaseView[];
  /** Unrecognised top-level keys, preserved for round-tripping. */
  extra: Record<string, unknown>;
}

export interface PropertyConfig {
  displayName?: string;
  [key: string]: unknown;
}

const VIEW_CORE_KEYS = new Set([
  "type",
  "name",
  "limit",
  "filters",
  "order",
  "groupBy",
  "sort",
  "summaries",
]);

/** Top-level keys Obsidian defines; anything else is preserved, not dropped. */
const BASE_CORE_KEYS = new Set(["filters", "formulas", "properties", "summaries", "views"]);

export function parseBase(path: string, text: string): BaseFile {
  let raw: unknown;
  try {
    // Obsidian's own writer escapes `|` inside filter expressions, which would
    // otherwise be read as a YAML block scalar indicator.
    raw = parseYaml(text.replace(/\\\|/g, "|"));
  } catch (err) {
    throw new BasesError(`Invalid YAML in ${path}: ${(err as Error).message}`, { note: path });
  }
  if (raw === null || typeof raw !== "object" || Array.isArray(raw)) {
    throw new BasesError(`A base file must be a YAML mapping: ${path}`, { note: path });
  }

  const obj = raw as Record<string, unknown>;
  const viewsRaw = obj["views"];
  if (!Array.isArray(viewsRaw)) {
    throw new BasesError(`A base file must define a "views" list: ${path}`, { note: path });
  }

  const views: BaseView[] = viewsRaw.map((v, i) => parseView(v, i, path));
  if (views.length === 0) {
    throw new BasesError(`A base file must define at least one view: ${path}`, { note: path });
  }

  const extra: Record<string, unknown> = {};
  for (const [k, v] of Object.entries(obj)) {
    if (!BASE_CORE_KEYS.has(k)) extra[k] = v;
  }

  return {
    filters: normaliseFilters(obj["filters"], path, "filters"),
    formulas: asStringMap(obj["formulas"], "formulas", path),
    properties: parseProperties(obj["properties"], path),
    summaries: asStringMap(obj["summaries"], "summaries", path),
    views,
    extra,
  };
}

function parseView(v: unknown, index: number, path: string): BaseView {
  if (v === null || typeof v !== "object" || Array.isArray(v)) {
    throw new BasesError(`views[${index}] must be a mapping in ${path}`, { note: path });
  }
  const o = v as Record<string, unknown>;
  const type = o["type"];
  if (typeof type !== "string" || type === "") {
    // A view with no type is unusable, and guessing one would silently
    // render the wrong layout.
    throw new BasesError(`views[${index}] is missing a "type" in ${path}`, { note: path });
  }

  const extra: Record<string, unknown> = {};
  for (const [k, val] of Object.entries(o)) {
    if (!VIEW_CORE_KEYS.has(k)) extra[k] = val;
  }

  const view: BaseView = {
    type,
    name: typeof o["name"] === "string" ? o["name"] : `View ${index + 1}`,
    extra,
  };

  if (typeof o["limit"] === "number") view.limit = o["limit"];
  const filters = normaliseFilters(o["filters"], path, `views[${index}].filters`);
  if (filters !== undefined) view.filters = filters;

  if (Array.isArray(o["order"])) {
    view.order = (o["order"] as unknown[]).filter((x): x is string => typeof x === "string");
  }

  if (o["groupBy"] !== undefined && o["groupBy"] !== null) {
    const g = o["groupBy"] as Record<string, unknown>;
    if (typeof g["property"] === "string") {
      view.groupBy = { property: g["property"], direction: readDirection(g["direction"]) };
    }
  }

  if (Array.isArray(o["sort"])) {
    view.sort = (o["sort"] as unknown[]).flatMap((entry) => {
      if (entry === null || typeof entry !== "object") return [];
      const e = entry as Record<string, unknown>;
      // Obsidian 1.9 wrote `column:`; it is `property:` now. Both occur in the
      // wild -- TaskNotes still emits `column:` -- so accept either.
      const prop = e["property"] ?? e["column"];
      if (typeof prop !== "string") return [];
      return [{ property: prop, direction: readDirection(e["direction"]) }];
    });
  }

  if (o["summaries"] !== undefined && o["summaries"] !== null) {
    view.summaries = asStringMap(o["summaries"], `views[${index}].summaries`, path);
  }

  return view;
}

function readDirection(v: unknown): Direction {
  return typeof v === "string" && v.toUpperCase() === "DESC" ? "DESC" : "ASC";
}

function asStringMap(v: unknown, what: string, path: string): Record<string, string> {
  if (v === undefined || v === null) return {};
  if (typeof v !== "object" || Array.isArray(v)) {
    throw new BasesError(`${what} must be a mapping in ${path}`, { note: path });
  }
  const out: Record<string, string> = {};
  for (const [k, val] of Object.entries(v as Record<string, unknown>)) {
    if (typeof val === "string") out[k] = val;
  }
  return out;
}

function parseProperties(v: unknown, path: string): Record<string, PropertyConfig> {
  if (v === undefined || v === null) return {};
  if (typeof v !== "object" || Array.isArray(v)) {
    throw new BasesError(`properties must be a mapping in ${path}`, { note: path });
  }
  const out: Record<string, PropertyConfig> = {};
  for (const [k, val] of Object.entries(v as Record<string, unknown>)) {
    if (val !== null && typeof val === "object" && !Array.isArray(val)) {
      out[k] = val as PropertyConfig;
    }
  }
  return out;
}

/**
 * Validate a filter node. A filter object may contain exactly ONE of
 * `and` / `or` / `not`; siblings are an error, not a silent merge. Obsidian's
 * own message is `"filters" may only have one of an "and", "or", or "not" keys.`
 */
export function normaliseFilters(v: unknown, path: string, where: string): FilterNode | undefined {
  if (v === undefined || v === null) return undefined;

  if (typeof v === "string") return v;

  if (Array.isArray(v)) {
    // A bare YAML list under `filters:` is invalid in Obsidian.
    throw new BasesError(
      `"filters" must be a string or a filter object, not a bare list (at ${where} in ${path})`,
      { note: path },
    );
  }

  if (typeof v !== "object") {
    throw new BasesError(`Invalid filter at ${where} in ${path}`, { note: path });
  }

  const o = v as Record<string, unknown>;
  const keys = ["and", "or", "not"].filter((k) => Object.hasOwn(o, k));
  if (keys.length === 0) {
    throw new BasesError(
      `A filter object must contain one of "and", "or", or "not" (at ${where} in ${path})`,
      { note: path },
    );
  }
  if (keys.length > 1) {
    throw new BasesError(`"filters" may only have one of an "and", "or", or "not" keys.`, {
      note: path,
    });
  }

  const key = keys[0]!;
  const list = normaliseList(o[key], path, `${where}.${key}`);
  return { [key]: list } as FilterObject;
}

function normaliseList(v: unknown, path: string, where: string): FilterNode[] {
  // `not:` accepts a single scalar as well as a list.
  if (typeof v === "string") return [v];
  if (!Array.isArray(v)) {
    throw new BasesError(`Filter group "${where}" in ${path} must be a list`, { note: path });
  }
  return v.map((item) => normaliseFilters(item, path, where)!).filter((x) => x !== undefined);
}

/** Select a view by name; falls back to the first view, as Obsidian does. */
export function selectView(base: BaseFile, viewName?: string): BaseView {
  if (viewName === undefined || viewName === "") {
    return base.views[0]!;
  }
  const found = base.views.find((v) => v.name === viewName);
  if (found === undefined) {
    throw new BasesError(
      `No view named "${viewName}" in this base. Available views: ${base.views
        .map((v) => v.name)
        .join(", ")}`,
      { view: viewName },
    );
  }
  return found;
}
