/**
 * Display labels.
 *
 * These assertions are transcribed from live `obsidian base:query` probes on
 * Obsidian 1.13.7. They are unit tests precisely so that label drift is caught
 * without Obsidian running: the label is the JSON key AND the markdown header,
 * so a wrong one silently corrupts every rendered result.
 */

import { describe, expect, test } from "bun:test";

import { displayNameFor, labelForId } from "../../src/bases/labels";
import type { BaseFile } from "../../src/bases/parse";

function base(properties: BaseFile["properties"] = {}): BaseFile {
  return { formulas: {}, properties, summaries: {}, views: [], extra: {} };
}

describe("property display labels", () => {
  test("file.* labels are not derivable from the ID", () => {
    expect(labelForId("file.name")).toBe("file name");
    expect(labelForId("file.basename")).toBe("file base name");
    expect(labelForId("file.ext")).toBe("file extension");
    expect(labelForId("file.ctime")).toBe("created time");
    expect(labelForId("file.mtime")).toBe("modified time");
  });

  test("folder and properties DROP the file namespace", () => {
    // The one genuinely surprising pair: every other file.* keeps its prefix.
    expect(labelForId("file.folder")).toBe("folder");
    expect(labelForId("file.properties")).toBe("properties");
    expect(labelForId("file.file")).toBe("file");
  });

  test("unknown IDs fall back to the bare segment", () => {
    expect(labelForId("file.zzz")).toBe("zzz");
    expect(labelForId("note.zzz")).toBe("zzz");
    expect(labelForId("note.some.deep")).toBe("some.deep");
  });

  test("labels are NOT title-cased and underscores are kept", () => {
    // Probed with a formula base carrying no displayName: the header read
    // `priority_display`, not `Priority Display`.
    expect(labelForId("formula.priority_display")).toBe("priority_display");
    expect(labelForId("note.my_prop")).toBe("my_prop");
  });

  test("a bare ID normalizes to note. before labelling", () => {
    expect(displayNameFor(base(), "status")).toBe("status");
    expect(displayNameFor(base(), "note.status")).toBe("status");
  });

  test("an explicit displayName wins and is used verbatim", () => {
    const b = base({ "note.priority": { displayName: "PR Priority" } });
    expect(displayNameFor(b, "priority")).toBe("PR Priority");
    // Not title-cased either -- Obsidian does not normalise a configured name.
    expect(displayNameFor(b, "note.priority")).toBe("PR Priority");
  });

  test("a displayName keyed by the prefixed ID applies to a bare column", () => {
    // Obsidian's UI always writes the prefixed form, so that is the spelling
    // a base is expected to use.
    const b = base({ "note.status": { displayName: "Status" } });
    expect(displayNameFor(b, "status")).toBe("Status");
    expect(displayNameFor(b, "note.status")).toBe("Status");
  });

  test("a displayName keyed by a BARE ID is ignored, matching Obsidian", () => {
    // Probed on Obsidian 1.13.7: `properties: {status: {displayName: ...}}`
    // labels the column `status`, i.e. the config is silently dead.
    const b = base({ status: { displayName: "BareKeyed" } });
    expect(displayNameFor(b, "status")).toBe("status");
    expect(displayNameFor(b, "note.status")).toBe("status");
  });
});
