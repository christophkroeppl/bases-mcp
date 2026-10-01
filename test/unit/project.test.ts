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

describe("restoring a deleted region must not corrupt the agent's prose", () => {
  // Regression. The insertion point for a deleted region was a character offset
  // taken from the ORIGINAL text and then indexed into the EDITED text. Once the
  // agent had added or removed anything above the region, that offset pointed at
  // an arbitrary character -- routinely the middle of a sentence it had just
  // written, with the embed spliced in between the halves.
  //
  // Anchoring on the other regions' positions rather than on an offset is what
  // fixes it, and the assertion has to be about the prose rather than merely that
  // the embed is present, which is all the test above checks.
  const ORIGINAL = "# Host\n\nIntro line.\n\n![[T.base]]\n\nTrailing prose.\n";
  // The agent deletes the region and writes a long new paragraph above it, so
  // every character after the deletion point shifts.
  const NEW_PARA = "A brand new paragraph inserted by the agent, long enough to shift every byte";
  const EDITED = `# Host\n\nIntro line.\n\n${NEW_PARA}\n\nTrailing prose.\n`;

  test("the agent's prose survives, intact", () => {
    const result = reconcileNote("Host.md", ORIGINAL, EDITED);

    expect(result.removedRegion).toBe(true);
    expect(result.refused.map((r) => r.reason)).toEqual(["The base region was removed."]);
    for (const phrase of [NEW_PARA, "Intro line.", "Trailing prose."]) {
      expect(result.text).toContain(phrase);
    }
  });

  test("the embed lands on a line of its own, not against the agent's sentence", () => {
    // The exact shape of the old defect: the offset pointed at the newline that
    // ended the agent's new paragraph, so the embed was spliced straight onto the
    // end of its last line with no blank line between.
    const result = reconcileNote("Host.md", ORIGINAL, EDITED);

    expect(result.text).not.toContain(`${NEW_PARA}\n![[T.base]]`);
    const lines = result.text.split("\n");
    expect(lines.filter((l) => l === "![[T.base]]")).toHaveLength(1);
  });

  test("the embed does not land above the prose the agent wrote", () => {
    const result = reconcileNote("Host.md", ORIGINAL, EDITED);

    const lines = result.text.split("\n");
    const embed = lines.indexOf("![[T.base]]");
    const para = lines.findIndex((l) => l.startsWith("A brand new paragraph"));
    expect(para).toBeGreaterThanOrEqual(0);
    expect(embed).toBeGreaterThan(para);
  });

  test("two deleted regions come back in their original order", () => {
    const original =
      "# Host\n\nA\n\n![[One.base]]\n\nMiddle prose.\n\n![[Two.base]]\n\nEnd prose.\n";
    const edited = "# Host\n\nA\n\nOne short new line.\n\nMiddle prose.\n\nEnd prose.\n";

    const result = reconcileNote("Host.md", original, edited);

    expect(result.removedRegion).toBe(true);
    expect(result.refused).toHaveLength(2);
    expect(result.text.indexOf("![[One.base]]")).toBeLessThan(result.text.indexOf("![[Two.base]]"));
    expect(result.text).toContain("One short new line.");
  });

  test("the first of two deleted regions does not derail the note", () => {
    // Two regions naming DIFFERENT Bases, so the surviving one cannot be paired
    // and the reconciler has nothing but its original index to order by. The old
    // offset landed the restore against "Mid" and left three blank lines behind.
    const original = "# Host\n\nA\n\n![[One.base]]\n\nMid\n\n![[Two.base]]\n\nEnd\n";
    const edited = "# Host\n\nA\n\nMid\n\n![[Two.base]]\n\nEnd\n";

    const result = reconcileNote("Host.md", original, edited);

    expect(result.text).toContain("![[One.base]]");
    expect(result.text).toContain("![[Two.base]]");
    expect(result.text).not.toContain("Mid\n![[One.base]]");
    expect(result.text).not.toContain("\n\n\n\n");
    expect(result.text.indexOf("![[One.base]]")).toBeLessThan(result.text.indexOf("![[Two.base]]"));
  });

  test("a note that is nothing but a deleted region still round-trips", () => {
    const result = reconcileNote("Host.md", "![[Only.base]]\n", "");

    expect(result.removedRegion).toBe(true);
    expect(result.text).toBe("![[Only.base]]\n");
  });

  test("a restore into a CRLF note uses CRLF", () => {
    const original = "# Host\r\n\r\nIntro.\r\n\r\n![[T.base]]\r\n\r\nTail.\r\n";
    const edited = original.replace("![[T.base]]\r\n", "");

    const result = reconcileNote("Host.md", original, edited);

    expect(result.text).toContain("![[T.base]]\r\n");
    expect(result.text).not.toContain("![[T.base]]\n");
    expect(result.text).toContain("Tail.");
  });
});
