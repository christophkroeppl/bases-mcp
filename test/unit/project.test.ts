/**
 * Projection round-tripping.
 *
 * `get_note` hands the agent a Projection: the note with each Base region
 * replaced by a ```base-rendered fence. The agent edits the prose and writes it
 * back. Two things must hold, and both were broken before this suite existed:
 *
 *   1. The rendered fence must NEVER reach disk. Obsidian would keep it as an
 *      inert block, leaving a dead copy of the rendered rows in the note.
 *   2. Writing an untouched Projection back must be `health: ok`, not a refusal.
 *      Round-tripping is the designed flow; marking it `partial-with-errors`
 *      would train an agent to ignore refusals.
 */

import { describe, expect, test } from "bun:test";

import { parseNoteWithEmbeds } from "../../src/note/parse";
import { reconcileNote } from "../../src/render/project";

/** A host note with one embedded base, as `test/vault` has. */
const HOST = "---\nstatus: active\n---\n\n## Tickets\n\n![[Tickets.base]]\n";

/** What `get_note` returns for HOST: the embed rendered into a fence. */
const PROJECTION = [
  "---",
  "status: active",
  "---",
  "",
  "## Tickets",
  "",
  '```base-rendered path="Tickets.base"',
  "| file name |",
  "| --- |",
  "| Fix login redirect |",
  "```",
  "",
].join("\n");

describe("parsing a rendered fence", () => {
  test("a base-rendered fence is a Base region, not prose", () => {
    const parsed = parseNoteWithEmbeds("Host.md", PROJECTION);
    const fences = parsed.segments.filter((s) => s.kind === "baseFence");
    expect(fences).toHaveLength(1);
    expect(parsed.segments.filter((s) => s.kind === "prose").join("")).not.toContain(
      "base-rendered",
    );
  });

  test("the fence keeps the Base path and view from its info string", () => {
    const parsed = parseNoteWithEmbeds("Host.md", PROJECTION);
    const fence = parsed.segments.find((s) => s.kind === "baseFence");
    expect(fence).toMatchObject({ basePath: "Tickets.base", rendered: true });
  });

  test("a rendered fence carries no YAML, so it is never mistaken for live", () => {
    const parsed = parseNoteWithEmbeds("Host.md", PROJECTION);
    const fence = parsed.segments.find((s) => s.kind === "baseFence");
    expect((fence as { yaml?: string }).yaml).toBeUndefined();
  });

  test("an ordinary code fence is still prose", () => {
    const parsed = parseNoteWithEmbeds("Host.md", "---\n---\n\n```ts\nconst x = 1;\n```\n");
    expect(parsed.segments.some((s) => s.kind === "baseFence")).toBe(false);
  });

  test("a live ```base fence with attributes is still a live fence", () => {
    const parsed = parseNoteWithEmbeds("Host.md", '```base extra="x"\nviews: []\n```\n');
    const fence = parsed.segments.find((s) => s.kind === "baseFence") as {
      yaml?: string;
      rendered?: boolean;
    };
    expect(fence.rendered).toBeUndefined();
    expect(fence.yaml).toBe("views: []\n");
  });
});

describe("reconciling a Projection", () => {
  test("the rendered fence is replaced by the live region, not written to disk", () => {
    const result = reconcileNote("Host.md", HOST, PROJECTION);
    expect(result.text).not.toContain("base-rendered");
    expect(result.text).not.toContain("Fix login redirect");
    expect(result.text).toContain("![[Tickets.base]]");
  });

  test("an untouched round-trip reports no refusal", () => {
    const result = reconcileNote("Host.md", HOST, PROJECTION);
    expect(result.refused).toEqual([]);
    expect(result.removedRegion).toBe(false);
  });

  test("the host note is byte-identical after a clean round-trip", () => {
    // The strongest form of the invariant: read, write back, nothing moved.
    const result = reconcileNote("Host.md", HOST, PROJECTION);
    expect(result.text).toBe(HOST);
  });

  test("prose edits around the region still apply", () => {
    const edited = PROJECTION.replace("## Tickets", "## Tickets (edited)");
    const result = reconcileNote("Host.md", HOST, edited);
    expect(result.text).toContain("## Tickets (edited)");
    expect(result.text).toContain("![[Tickets.base]]");
    expect(result.text).not.toContain("base-rendered");
  });

  test("an agent editing the rendered rows cannot change the region", () => {
    const edited = PROJECTION.replace(
      "| Fix login redirect |",
      "| Fix login redirect |\n| Sneaky row |",
    );
    const result = reconcileNote("Host.md", HOST, edited);
    expect(result.text).toBe(HOST);
  });

  test("a rendered fence is matched by Base path, not by position", () => {
    // The note has the region first; the Projection reorders it behind some
    // prose. Position matching would pair it with nothing and restore wrongly.
    const reordered = [
      "---",
      "status: active",
      "---",
      "",
      "## Tickets",
      "",
      "intro prose",
      "",
      '```base-rendered path="Tickets.base"',
      "| file name |",
      "```",
      "",
    ].join("\n");
    const result = reconcileNote("Host.md", HOST, reordered);
    expect(result.text).toContain("intro prose");
    expect(result.text).toContain("![[Tickets.base]]");
    expect(result.text).not.toContain("base-rendered");
    expect(result.refused).toEqual([]);
  });
});

describe("deleting a region is still refused", () => {
  test("removing the embed restores it and says so", () => {
    const result = reconcileNote("Host.md", HOST, HOST.replace("![[Tickets.base]]\n", ""));
    expect(result.removedRegion).toBe(true);
    expect(result.text).toContain("![[Tickets.base]]");
  });
});
