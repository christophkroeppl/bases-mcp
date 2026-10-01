/**
 * What a bad `context` does.
 *
 * `context` is the Host note that binds `this`, and it is the one field on
 * `resolve_base` whose bad values are all the same mistake wearing different
 * clothes: the agent named something that is not a note. A path that does not
 * exist, a `.base`, a folder, a blank string -- four spellings of "I meant to
 * pass a note". The engine has to say so in one voice, naming the field, the
 * value and what was expected, and put the offending path in `note` so a
 * client can act on it without scraping the prose.
 *
 * Every case here reads the testing vault and writes nothing, so there is
 * nothing to clean up.
 */

import { describe, expect, test } from "bun:test";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { BasesError } from "../../src/expr/errors";
import { Resolver } from "../../src/service";

const HERE = dirname(fileURLToPath(import.meta.url));
const VAULT_DIR = join(HERE, "..", "vault");

const resolver = await Resolver.openDir(VAULT_DIR);

/** The rejection, or a failure saying the call unexpectedly succeeded. */
async function rejection(context: string, base = "Tickets.base"): Promise<BasesError> {
  try {
    await resolver.query(base, { context });
  } catch (err) {
    if (err instanceof BasesError) return err;
    throw new Error(`Expected a BasesError, got ${(err as Error).name}: ${(err as Error).message}`);
  }
  throw new Error(`Expected ${base} to reject context ${JSON.stringify(context)}, but it resolved`);
}

/**
 * The advice half of a refusal: everything from "is a folder" onwards, minus
 * the value each message quotes back and the `(note: ...)` suffix.
 */
function guidance(err: BasesError): string {
  return err.message.slice(err.message.indexOf(", and ")).replace(/\s*\(note: [^)]*\)$/, "");
}

describe("a context that is not a note", () => {
  test("a path that does not exist says so, and names the field", async () => {
    const err = await rejection("Nope/Missing.md");
    expect(err.message).toContain("`context`");
    expect(err.message).toContain("Nope/Missing.md");
    expect(err.message).toContain("does not exist in this vault");
    // The structured field, which is what a client acts on.
    expect(err.note).toBe("Nope/Missing.md");
  });

  test("a .base is refused as a base, not reported as missing", async () => {
    // Tickets.base EXISTS, so "does not exist in this vault" is a lie the
    // agent cannot act on -- it goes looking for a typo in a file it can see.
    const err = await rejection("Tickets.base");
    expect(err.message).toContain("`context`");
    expect(err.message).toContain("Tickets.base");
    expect(err.message).toContain(".base file");
    expect(err.message).not.toContain("does not exist in this vault");
    expect(err.note).toBe("Tickets.base");
  });

  test("a folder is refused as a folder, not reported as missing", async () => {
    // Same lie, same fix. "Projects" is right there in the vault.
    const err = await rejection("Projects");
    expect(err.message).toContain("`context`");
    expect(err.message).toContain("Projects");
    expect(err.message).toContain("folder");
    expect(err.message).not.toContain("does not exist in this vault");
    expect(err.note).toBe("Projects");
  });

  test("a blank context says it is blank rather than quoting nothing", async () => {
    // Whitespace was previously reported as `does not exist in this vault
    // (note:    )`, which reads as a path with spaces in it.
    const err = await rejection("   ");
    expect(err.message).toContain("`context`");
    expect(err.message).toContain("empty");
    expect(err.message).not.toContain("does not exist in this vault");
  });

  test("a trailing slash names the same folder, and is reported as one", async () => {
    // `Projects` and `Projects/` are the same folder, so they must not get
    // two different verdicts from one rule -- and the message must still quote
    // the value that was actually sent, because that is what the agent has to
    // correct.
    const withSlash = await rejection("Projects/");
    const without = await rejection("Projects");
    expect(withSlash.message).toContain('"Projects/" is a folder');
    expect(without.message).toContain('"Projects" is a folder');
    // Identical but for the value each one quotes back.
    expect(guidance(withSlash)).toBe(guidance(without));
  });

  test("every bad context is refused the same way, whatever the base", async () => {
    // AllNotes.base never references `this`, so a bad context there is pure
    // input validation with no engine behaviour behind it -- the message must
    // still be the same one, or the agent learns two vocabularies.
    const scoped = await rejection("Nope/Missing.md", "Tickets.base");
    const unscoped = await rejection("Nope/Missing.md", "AllNotes.base");
    expect(unscoped.message).toBe(scoped.message);
  });
});

describe("a context that is a note is untouched", () => {
  test("a nested host still scopes the base", async () => {
    const rows = await resolver.query("Tickets.base", { context: "Projects/SomeProject.md" });
    expect(rows.rows.map((r) => r.path)).toEqual([
      "Tickets/Add offline mode.md",
      "Tickets/Fix login redirect.md",
    ]);
  });

  test("a root-level host still scopes the base", async () => {
    const rows = await resolver.query("Tickets.base", { context: "Root Project.md" });
    expect(rows.rows.map((r) => r.path)).toEqual(["Root Ticket.md"]);
  });

  test("a note name without its path or extension still resolves", async () => {
    // Obsidian accepts all three spellings of the same note, so refusing the
    // short ones would be a regression against the resolver we mirror.
    const rows = await resolver.query("Tickets.base", { context: "SomeProject" });
    expect(rows.rows.map((r) => r.path)).toContain("Tickets/Fix login redirect.md");
  });

  test("a base that does not reference `this` still resolves with no context", async () => {
    const rows = await resolver.query("AllNotes.base");
    expect(rows.rows.length).toBeGreaterThan(0);
  });
});

describe("file.links", () => {
  test("keeps every distinct link, not just the first", async () => {
    // Regression: `dedupe` keyed on `String(link)`, which is `[object Object]`
    // for every LinkValue, so any note with two links collapsed to one. That
    // also cost backlinks, since `backlinksFor` is built on `linksFor`.
    const resolver = await Resolver.openDir("test/vault");
    const links = resolver.vaultSource.linksFor("Root Project.md");
    const targets = links.map((l) => (l as { target: string }).target);
    expect(targets).toEqual(["Geschäftsidee", "Projects"]);
  });

  test("a repeated link is still one link", async () => {
    const resolver = await Resolver.openDir("test/vault");
    expect(resolver.vaultSource.linksFor("Root Ticket.md")).toHaveLength(1);
  });
});
