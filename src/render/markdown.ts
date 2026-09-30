/**
 * Markdown rendering.
 *
 * Two surfaces. `flat` reproduces what `obsidian base:query format=md` emits,
 * probed on Obsidian 1.13.7: one GitHub-flavoured markdown table for every view
 * type. `structured` is the agent-facing Projection, which keeps structure, so
 * it renders `list` and `map` as markdown lists and everything else as a table.
 *
 * Cards and kanban have no faithful markdown equivalent, so they are flattened
 * into a table. That is an intentional simplification, documented in
 * docs/divergences.md, not an accident.
 *
 * Observed details we reproduce exactly:
 *   - Column headers are DISPLAY NAMES, not property IDs.
 *   - A null or absent cell renders as an EMPTY cell, not `-` or `null`.
 *   - List values are joined with ", " and tags keep their leading `#`.
 *   - Cells are centred with padding, which `flat` must keep for parity.
 *     Structured tables left-align instead, since centring is hard to scan.
 */

import { displayNameFor as labelFor } from "../bases/labels";
import type { BaseFile, BaseView } from "../bases/parse";
import { canonical, type QueryResult, type ResolvedRow } from "../bases/query";
import { toDisplayString } from "../expr/evaluator";
import type { BasesValue } from "../expr/values";
import { DateValue } from "../expr/values";

export interface RenderColumn {
  /** Canonical Property ID. */
  id: string;
  /** Header text: the display name when one is configured. */
  header: string;
}

/** View types `structured` renders as a markdown list. Everything else is a table. */
const LIST_TYPES = new Set(["list", "map"]);

export function renderColumns(base: BaseFile, view: BaseView): RenderColumn[] {
  const ids =
    view.order !== undefined && view.order.length > 0
      ? view.order
      : ["file.name", ...Object.keys(base.properties)];

  return ids.map((id) => ({
    id: canonical(id),
    header: displayNameFor(base, id),
  }));
}

export function displayNameFor(base: BaseFile, id: string): string {
  return labelFor(base, id);
}

/**
 * How much structure a rendered view keeps.
 *
 * `flat` is CLI-exact: `obsidian base:query format=md` renders EVERY view type
 * as the same centred flat markdown table, ignoring `groupBy` and `summaries`.
 * It is a lossy export format, not a view.
 *
 * `structured` renders `list` and `map` as markdown lists and every other view
 * type as a left-aligned markdown table, keeping group headers and a summaries
 * footer. This is the agent-facing Projection, which is never written back to
 * disk and so carries no parity obligation. See docs/divergences.md.
 */
export type RenderStyle = "flat" | "structured";

/** Render a resolved query as markdown. */
export function renderMarkdown(
  base: BaseFile,
  result: QueryResult,
  style: RenderStyle = "flat",
): string {
  const view = result.view;

  if (style === "flat") {
    return tableFor(renderColumns(base, view), result.rows);
  }
  if (LIST_TYPES.has(view.type)) {
    return renderAsList(base, view, result);
  }
  return renderAsTable(base, view, result);
}

function cellValue(row: ResolvedRow, id: string): BasesValue {
  return row.values[id];
}

function renderAsTable(base: BaseFile, view: BaseView, result: QueryResult): string {
  const columns = renderColumns(base, view);
  const lines: string[] = [];

  if (result.groups !== null) {
    for (const group of result.groups) {
      lines.push(`**${group.key}**`);
      lines.push("");
      lines.push(tableFor(columns, group.rows, "left"));
      lines.push("");
    }
    return lines.join("\n").replace(/\n+$/, "\n");
  }

  // Measure the summaries footer together with the rows, so the footer lines
  // up under the table instead of being squeezed to its own narrower widths.
  const cells = view.summaries === undefined ? null : summaryCells(columns, view, result.rows);
  const widths = columnWidths(columns, result.rows, cells ?? []);

  lines.push(tableFor(columns, result.rows, "left", widths));
  if (cells !== null) {
    lines.push("");
    lines.push(summaryTable(columns, cells, widths));
  }
  return lines.join("\n");
}

/** How a table pads its cells to a common width. */
type TableAlign = "center" | "left";

/**
 * Column widths, shared by a table and the summaries footer beneath it.
 *
 * The footer has to be measured together with the rows, or it comes out
 * narrower than the table it is summarising.
 */
function columnWidths(
  columns: RenderColumn[],
  rows: ResolvedRow[],
  extraCells: Array<string | null> = [],
): number[] {
  return columns.map((col, i) => {
    let width = Math.max(displayWidth(col.header), displayWidth(extraCells[i] ?? ""));
    for (const row of rows) {
      width = Math.max(width, displayWidth(toCellText(cellValue(row, col.id))));
    }
    return Math.max(width, 3);
  });
}

function tableFor(
  columns: RenderColumn[],
  rows: ResolvedRow[],
  align: TableAlign = "center",
  widths: number[] = columnWidths(columns, rows),
): string {
  const header = columns.map((c) => c.header);
  const body = rows.map((r) => columns.map((c) => toCellText(cellValue(r, c.id))));

  const out: string[] = [];
  out.push(`| ${padCells(header, widths, align)} |`);
  out.push(`| ${widths.map((w) => "-".repeat(w)).join(" | ")} |`);
  for (const row of body) {
    out.push(`| ${padCells(row, widths, align)} |`);
  }
  return out.join("\n");
}

/**
 * A null or absent value renders as an empty cell, matching Obsidian. Note this
 * is NOT the same as an empty string property, but both render blank.
 */
function toCellText(v: BasesValue | undefined): string {
  if (v === null || v === undefined) return "";
  return toDisplayString(v);
}

function padCells(cells: string[], widths: number[], align: TableAlign = "center"): string {
  return cells
    .map((c, i) => {
      const width = widths[i] ?? displayWidth(c);
      return align === "center" ? centre(c, width) : padEnd(c, width);
    })
    .join(" | ");
}

/** Obsidian centres its table cells, so we do too. */
function centre(text: string, width: number): string {
  const pad = width - displayWidth(text);
  if (pad <= 0) return text;
  const left = Math.floor(pad / 2);
  return " ".repeat(left) + text + " ".repeat(pad - left);
}

/** Left-alignment, so a column's text lines up down the table. */
function padEnd(text: string, width: number): string {
  const pad = width - displayWidth(text);
  if (pad <= 0) return text;
  return text + " ".repeat(pad);
}

/**
 * Character count, not UTF-16 code units, so emoji in a cell do not skew the
 * alignment. Combining marks are counted as zero width.
 */
export function displayWidth(text: string): number {
  let width = 0;
  for (const ch of text) {
    const cp = ch.codePointAt(0)!;
    if (cp === 0x200d) continue;
    // Zero-width joiner, variation selectors and combining marks.
    if (cp >= 0x0300 && cp <= 0x036f) continue;
    if (cp === 0xfe0f || cp === 0xfe0e) continue;
    const isWide =
      (cp >= 0x1100 && cp <= 0x115f) ||
      (cp >= 0x2e80 && cp <= 0xa4cf) ||
      (cp >= 0xac00 && cp <= 0xd7a3) ||
      (cp >= 0xf900 && cp <= 0xfaff) ||
      (cp >= 0xfe30 && cp <= 0xfe6f) ||
      (cp >= 0xff00 && cp <= 0xff60) ||
      (cp >= 0xffe0 && cp <= 0xffe6) ||
      (cp >= 0x1f300 && cp <= 0x1f9ff) ||
      (cp >= 0x20000 && cp <= 0x3fffd);
    width += isWide ? 2 : 1;
  }
  return width;
}

// ---------------------------------------------------------------------------
// List rendering
// ---------------------------------------------------------------------------

/**
 * A list view's primary column becomes the bullet text and the rest are
 * appended as `key: value` pairs.
 */
function renderAsList(base: BaseFile, view: BaseView, result: QueryResult): string {
  const columns = renderColumns(base, view);
  if (columns.length === 0) return "";

  const emit = (rows: ResolvedRow[], indent = ""): string[] => {
    const out: string[] = [];
    for (const row of rows) {
      const primary = columns[0]!;
      const title = toCellText(cellValue(row, primary.id));
      out.push(`${indent}- ${title === "" ? "(untitled)" : title}`);
      for (const col of columns.slice(1)) {
        const value = toCellText(cellValue(row, col.id));
        if (value === "") continue;
        out.push(`${indent}  - ${col.header}: ${value}`);
      }
    }
    return out;
  };

  const lines: string[] = [];
  if (result.groups !== null) {
    for (const group of result.groups) {
      lines.push(`**${group.key}**`);
      lines.push("");
      lines.push(...emit(group.rows));
      lines.push("");
    }
    return lines.join("\n").replace(/\n+$/, "\n");
  }
  lines.push(...emit(result.rows));
  return lines.join("\n");
}

// ---------------------------------------------------------------------------
// Summaries
// ---------------------------------------------------------------------------

/**
 * The summarised value for each column, or `""` for a column the view does not
 * summarise. Returned as cells rather than a rendered table so the caller can
 * measure them into the column widths first.
 */
function summaryCells(columns: RenderColumn[], view: BaseView, rows: ResolvedRow[]): string[] {
  const entries = Object.entries(view.summaries ?? {});
  return columns.map((col) => {
    const spec = entries.find(([id]) => canonical(id) === col.id)?.[1];
    if (spec === undefined) return "";
    const values = rows
      .map((r) => cellValue(r, col.id))
      .filter((v) => v !== null && v !== undefined);
    return summarise(spec, values);
  });
}

/** The footer: a one-row table reusing the main table's column widths. */
function summaryTable(columns: RenderColumn[], cells: string[], widths: number[]): string {
  const labels = columns.map((c) => c.header);
  return [
    `| ${padCells(labels, widths, "left")} |`,
    `| ${widths.map((w) => "-".repeat(w)).join(" | ")} |`,
    `| ${padCells(cells, widths, "left")} |`,
  ].join("\n");
}

function summarise(spec: string, values: BasesValue[]): string {
  const nums: number[] = [];
  const strings: string[] = [];
  let filled = 0;
  let checked = 0;
  let unchecked = 0;
  const unique = new Set<string>();

  for (const v of values) {
    if (v === null || v === undefined || v === "") continue;
    filled++;
    if (typeof v === "boolean") {
      if (v) checked++;
      else unchecked++;
    }
    const n = typeof v === "number" ? v : Number(toDisplayString(v));
    if (!Number.isNaN(n)) nums.push(n);
    strings.push(toDisplayString(v));
    unique.add(toDisplayString(v));
  }

  switch (spec) {
    case "Average":
      return nums.length === 0 ? "" : String(round3(sum(nums) / nums.length));
    case "Sum":
      return nums.length === 0 ? "" : String(round3(sum(nums)));
    case "Min":
      return nums.length === 0 ? "" : String(minOf(nums));
    case "Max":
      return nums.length === 0 ? "" : String(maxOf(nums));
    case "Range": {
      if (nums.length === 0) return "";
      // Range over dates is a span; over numbers it is Max-Min.
      if (values.every((v) => v instanceof Object && "ms" in v)) {
        return `${maxOf(nums) - minOf(nums)} ms`;
      }
      return String(round3(maxOf(nums) - minOf(nums)));
    }
    case "Median":
      return nums.length === 0 ? "" : String(round3(median(nums)));
    case "Stddev":
      return nums.length < 2 ? "" : String(round3(stddev(nums)));
    case "Unique":
      return String(unique.size);
    case "Filled":
      return String(filled);
    case "Empty":
      return String(values.length - filled);
    case "Checked":
      return String(checked);
    case "Unchecked":
      return String(unchecked);
    case "Count":
      return String(values.length);
    case "Earliest":
    case "Latest": {
      // Both are documented as date summaries, and both are overloaded onto
      // whatever orderable values are present.
      const times = values
        .filter((v) => typeof v === "object" && v !== null && "ms" in v)
        .map((v) => (v as { ms: number }).ms)
        .filter((ms) => !Number.isNaN(ms));
      if (times.length === 0) return "";
      const picked = spec === "Earliest" ? minOf(times) : maxOf(times);
      return toDisplayString(new DateValue(picked, true));
    }
    default:
      // A custom summary expression over `values` is not supported yet.
      throw new Error(
        `Custom summary "${spec}" is not implemented; use a built-in summary name ` +
          `(Average, Min, Max, Sum, Range, Median, Stddev, Unique, Filled, Empty, ` +
          `Checked, Unchecked, Count, Earliest, Latest).`,
      );
  }
}

function sum(xs: number[]): number {
  return xs.reduce((a, b) => a + b, 0);
}
function minOf(xs: number[]): number {
  return xs.reduce((a, b) => (b < a ? b : a), Infinity);
}
function maxOf(xs: number[]): number {
  return xs.reduce((a, b) => (b > a ? b : a), -Infinity);
}
function median(xs: number[]): number {
  const s = [...xs].sort((a, b) => a - b);
  const mid = Math.floor(s.length / 2);
  return s.length % 2 === 1 ? s[mid]! : (s[mid - 1]! + s[mid]!) / 2;
}
function stddev(xs: number[]): number {
  const mean = sum(xs) / xs.length;
  return Math.sqrt(sum(xs.map((x) => (x - mean) ** 2)) / xs.length);
}
function round3(x: number): number {
  return Math.round(x * 1000) / 1000;
}
