/**
 * Note segmentation, and the byte-exactness every write depends on.
 *
 * A note is parsed into segments that concatenate back to the original bytes
 * exactly. That invariant is what makes the write path safe: a Base region is a
 * byte span we can preserve verbatim while everything around it is editable, so a
 * span that is off by one character is an edit the agent never asked for.
 *
 * Most of this file is about line endings, because that is where the invariant
 * was nearly lost -- and it was lost on the other side of the port rather than
 * here, which makes it worth pinning in both trees.
 */

import { describe, expect, test } from "bun:test";

import {
  isBaseRegion,
  parseNoteWithEmbeds,
  type Segment,
  serialise,
  splitBaseEmbeds,
} from "../../src/note/parse";
import { reconcileNote } from "../../src/render/project";

/** The Host notes the CRLF cases below are built from. */
const CRLF_SHAPES: ReadonlyArray<readonly [string, string]> = [
  ["plain", "# Host\r\n\r\n![[T.base]]\r\n"],
  ["indented", "# Host\r\n\r\n  ![[T.base]]  \r\n"],
  ["with a view", "# Host\r\n\r\n![[T.base#View]]\r\n"],
  ["no trailing newline", "# Host\r\n\r\n![[T.base]]"],
];

/** The Base regions in `text`, as the spans they occupy. */
function regions(text: string): string[] {
  const note = parseNoteWithEmbeds("Host.md", text);
  return note.segments.filter(isBaseRegion).map((s) => text.slice(s.start, s.end));
}

describe("a CRLF Host note still has its Base region", () => {
  // Regression, and the reason it never fired here. The embed pattern ends in
  // `[ \t]*$` under the multiline flag. JavaScript's multiline `$` matches before
  // `\r`, `\n`, `\u2028` and `\u2029`, so a CRLF line still matches. Rust's `(?m)`
  // treats only `\n` as a line terminator, so the same pattern with `(?m)` matches
  // nothing at all on a CRLF note -- which made the Base region invisible, and
  // `write_note` then persisted the agent's deletion of it while reporting
  // `health: ok`. Silent vault corruption, on a note the tool had just told the
  // agent it had protected.
  for (const [label, text] of CRLF_SHAPES) {
    test(`the region is found in a CRLF note: ${label}`, () => {
      expect(regions(text)).toHaveLength(1);
    });

    test(`the region names its Base: ${label}`, () => {
      const note = parseNoteWithEmbeds("Host.md", text);
      const region = note.segments.filter(isBaseRegion)[0]!;
      expect(region).toMatchObject({ basePath: "T.base" });
    });

    test(`the CRLF note round-trips byte for byte: ${label}`, () => {
      const note = parseNoteWithEmbeds("Host.md", text);
      expect(serialise(note.segments)).toBe(text);
    });
  }

  // The property the whole thing exists for. A region the parser cannot see is a
  // region the reconciler cannot restore, so the deletion lands on disk and the
  // tool reports success. Asserted through the reconciler rather than through the
  // parser, because "the parser found it" and "the region survived" are different
  // claims and only the second one is the bug.
  test("deleting the region from a CRLF note is refused and restored", () => {
    const original = "# Host\r\n\r\nIntro.\r\n\r\n![[T.base]]\r\n\r\nTail.\r\n";
    const edited = original.replace("![[T.base]]\r\n", "");

    const result = reconcileNote("Host.md", original, edited);

    expect(result.removedRegion).toBe(true);
    expect(result.refused.map((r) => r.reason)).toEqual(["The base region was removed."]);
    expect(result.text).toContain("![[T.base]]");
    expect(result.text).toContain("Intro.");
    expect(result.text).toContain("Tail.");
  });
});

describe("the embed pattern and carriage returns", () => {
  test("the multiline `$` this relies on does match before a `\\r`", () => {
    // The guard on the guard. The reason the CRLF cases above pass is an engine
    // property, not a property of this code, and the failure mode it guards
    // against is invisible in a test run: a regex change that stopped matching
    // CRLF would fail the tests above too, but only with this as the reason.
    // Rust's `(?m)` asserts false here, which is the entire difference between
    // the two trees.
    expect(/^x$/m.test("x\r\n")).toBe(true);
  });

  test("an embed on a CRLF line is split out of the prose", () => {
    // `splitBaseEmbeds` is where the pattern lives, so this is the narrowest
    // assertion that a change to it would break.
    const segments: Segment[] = splitBaseEmbeds([
      { kind: "prose", raw: "# Host\r\n\r\n![[T.base]]\r\n\r\ntail\r\n", start: 0, end: 26 },
    ]);

    expect(segments.map((s) => s.kind)).toEqual(["prose", "baseEmbed", "prose"]);
    expect(segments[1]).toMatchObject({ basePath: "T.base" });
  });

  test("an LF note's region span is unchanged by the CRLF cases", () => {
    // The CRLF handling does not move the boundary on an LF note: the region is
    // still exactly the line, with no newline of its own.
    expect(regions("# Host\n\n  ![[T.base]]  \n")).toEqual(["  ![[T.base]]  "]);
  });
});
