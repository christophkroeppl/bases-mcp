/**
 * Rendering.
 *
 * The two surfaces are a deliberate, documented choice: `flat` mirrors
 * `obsidian base:query format=md` for parity, while `structured` keeps the
 * group headers and list layout that the flat export throws away. These tests
 * exist so neither surface drifts into the other.
 */

import { describe, expect, test } from "bun:test";
import type { BaseFile } from "../../src/bases/parse";
import type { QueryGroup, QueryResult, ResolvedRow } from "../../src/bases/query";
import { renderMarkdown } from "../../src/render/markdown";

function base(): BaseFile {
  return { formulas: {}, properties: {}, summaries: {}, views: [], extra: {} };
}

function row(path: string, name: string, status: string): ResolvedRow {
  return {
    path,
    formula: {},
    values: { "file.name": name, "note.status": status },
  };
}

const ROWS: ResolvedRow[] = [
  row("Tickets/B.md", "Beta", "active"),
  row("Root A.md", "Alpha", "done"),
];

const GROUPS: QueryGroup[] = [
  { key: "1 – high", rows: [ROWS[0]] },
  { key: "2 – normal", rows: [ROWS[1]] },
];

function result(type: string, groups: QueryGroup[] | null = null): QueryResult {
  return {
    basePath: "T.base",
    view: { type, name: "V", order: ["file.name", "status"] },
    rows: groups === null ? ROWS : groups.flatMap((g) => g.rows),
    groups,
    total: ROWS.length,
    limited: false,
  } as unknown as QueryResult;
}

describe("flat rendering (the format=md parity surface)", () => {
  test("renders a table for every view type the CLI knows", () => {
    for (const type of ["table", "cards", "kanban", "list", "map"]) {
      const md = renderMarkdown(base(), result(type), "flat");
      expect(md.startsWith("|")).toBe(true);
      // `list` is a table here, NOT a markdown list.
      expect(md).not.toContain("- Alpha");
      expect(md).toContain("Alpha");
    }
  });

  test("drops group headers", () => {
    const md = renderMarkdown(base(), result("table", GROUPS), "flat");
    expect(md).not.toContain("**1 – high**");
    expect(md).not.toContain("**");
  });

  test("an unknown view type falls back to a table", () => {
    // Plugin view types are a documented open namespace, so an unrecognised
    // type degrades to a table rather than failing the whole render.
    const md = renderMarkdown(base(), result("gallery"), "flat");
    expect(md.startsWith("|")).toBe(true);
    expect(md).toContain("Alpha");
  });

  test("centres cells, matching the CLI byte for byte", () => {
    const md = renderMarkdown(base(), result("table"), "flat");
    // The CLI centres; `flat` is compared against it byte for byte. Centring
    // puts a pad BEFORE the first cell's text, so two or more spaces separate
    // the pipe from "Alpha" -- left alignment would leave exactly one.
    expect(md).toMatch(/\|\s{2,}Alpha\s{2,}\|/);
  });
});

describe("structured rendering (the Projection surface)", () => {
  test("keeps group headers", () => {
    const md = renderMarkdown(base(), result("table", GROUPS), "structured");
    expect(md).toContain("**1 – high**");
    expect(md).toContain("**2 – normal**");
  });

  test("renders a list view as a list", () => {
    const md = renderMarkdown(base(), result("list"), "structured");
    expect(md).toContain("- Alpha");
    // The nested line uses the same display label as the table header would.
    expect(md).toContain("status: done");
  });

  test("renders a table view as a table", () => {
    const md = renderMarkdown(base(), result("table"), "structured");
    expect(md.startsWith("|")).toBe(true);
  });

  test("cards and kanban render as tables", () => {
    for (const type of ["cards", "kanban", "board"]) {
      const md = renderMarkdown(base(), result(type), "structured");
      expect(md.startsWith("|")).toBe(true);
    }
  });

  test("map renders as a list", () => {
    const md = renderMarkdown(base(), result("map"), "structured");
    expect(md).toContain("- Alpha");
  });

  test("an unknown view type falls back to a table", () => {
    const md = renderMarkdown(base(), result("gallery"), "structured");
    expect(md.startsWith("|")).toBe(true);
    expect(md).toContain("Alpha");
  });

  test("left-aligns cells instead of centring them", () => {
    const md = renderMarkdown(base(), result("table"), "structured");
    expect(md).toContain("| Alpha");
    // Left-aligned means no leading pad before the first cell's text. Line 3
    // is the Alpha row: the header, the rule, then Beta, then Alpha.
    expect(md.split("\n")[3]).toMatch(/^\| Alpha\s+\|/);
  });
});

describe("summaries", () => {
  test("the footer aligns with the table it summarises", () => {
    const r = result("table");
    r.view.summaries = { "note.status": "Unique" };
    const md = renderMarkdown(base(), r, "structured");
    const lines = md.split("\n");
    // The footer is the last table, introduced by a blank line.
    const blank = lines.indexOf("");
    expect(blank).toBeGreaterThan(0);
    const header = lines[0]!;
    const footer = lines[blank + 1]!;
    expect(footer.startsWith("| file name")).toBe(true);
    // Every rendered line is the same width, so the footer lines up under the
    // table instead of shrinking to fit its own short numbers.
    for (const l of lines) {
      if (l !== "") expect(l.length).toBe(header.length);
    }
  });

  test("Earliest and Latest are implemented, not merely advertised", () => {
    // Both are named in the unsupported-summary error message, so they must
    // actually resolve.
    for (const spec of ["Earliest", "Latest"]) {
      const r = result("table");
      r.view.summaries = { "file.name": spec };
      expect(() => renderMarkdown(base(), r, "structured")).not.toThrow();
    }
  });

  test("an unknown summary name hard-errors", () => {
    const r = result("table");
    r.view.summaries = { "note.status": "NotASummaryName" };
    expect(() => renderMarkdown(base(), r, "structured")).toThrow(/not implemented/);
  });
});

describe("column headers", () => {
  test("use Obsidian's display labels, not title-cased IDs", () => {
    const b: BaseFile = {
      ...base(),
      properties: { "note.status": { displayName: "Status" } },
    };
    const md = renderMarkdown(b, result("table"), "flat");
    expect(md).toContain("file name");
    expect(md).toContain("Status");
    // Not `File Name`, and not the Property ID.
    expect(md).not.toContain("File Name");
    expect(md).not.toContain("note.status");
  });
});
