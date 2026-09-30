/**
 * The draft / verify / commit handshake.
 *
 * The contract these tests pin is not "we produced a nice note" -- it is the
 * one that matters: a note is written if and only if the base's REAL filter
 * matches it. Everything else (the inversion, the placeholders) is best-effort
 * by design, and the tests say so where they touch it.
 *
 * These tests write into the testing vault, so every one that creates a note
 * deletes it in a `finally`. Every path they own is asserted absent before the
 * test runs, so a run that dies mid-write fails loudly on the next run instead
 * of quietly reusing a stray note. `deleteNote` is the only thing that removes
 * a file, and it only ever takes a path one of these tests just created.
 */

import { beforeEach, describe, expect, test } from "bun:test";
import { promises as fs } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { type AddNoteOptions, type DraftProposal, DraftStore, drafts } from "../../src/mcp/drafts";
import { parseNote } from "../../src/note/parse";
import { Resolver } from "../../src/service";

const HERE = dirname(fileURLToPath(import.meta.url));
const VAULT_DIR = join(HERE, "..", "vault");
const FIXTURES = join(HERE, "..", "fixtures");

/** The scratch note most of these tests use; the rest name their own. */
const SCRATCH = "Tickets/__draft-scratch.md";
const HOST = "Projects/SomeProject.md";

const resolver = await Resolver.openDir(VAULT_DIR);

beforeEach(() => {
  expect(resolver.vaultSource.notePaths()).not.toContain(SCRATCH);
  drafts.clear();
});

/** The message of a rejection, so a test can assert on its whole text. */
async function messageOf(run: () => Promise<unknown>): Promise<string> {
  try {
    await run();
  } catch (err) {
    return (err as Error).message;
  }
  throw new Error("Expected a rejection, but the call succeeded");
}

async function deleteNote(path: string): Promise<void> {
  // Called as a method: the backend's `delete` is not a free function, and
  // detaching it would leave it with no vault root to resolve against.
  const backend = resolver.vaultSource.backend;
  if (backend.delete === undefined)
    throw new Error("The fs backend must support delete for these tests");
  await backend.delete(path);
  // The snapshot still lists the file until it is rebuilt, and a later query
  // would then read bytes that are no longer there.
  await resolver.reload();
}

function frontmatterOf(content: string): Record<string, unknown> {
  return parseNote(SCRATCH, content).frontmatter as Record<string, unknown>;
}

/**
 * The drafting half of the handshake, narrowed to what it returns.
 *
 * `addNoteToBase` is one function with two shapes, because the branch is on
 * `draft_id` and the caller knows which half it is in.
 */
async function draftFrom(source: Resolver, options: AddNoteOptions): Promise<DraftProposal> {
  const result = await source.addNoteToBase(options);
  if (!("content" in result)) throw new Error("Expected a draft, but the call committed a note");
  return result;
}

function draftFor(path: string, context?: string, view?: string): Promise<DraftProposal> {
  return draftFrom(resolver, { base: "Tickets.base", path, context, view });
}

describe("the first call: a draft, an id and an expiry", () => {
  test("returns a draft id, a non-empty draft and an absolute expiry", async () => {
    const draft = await draftFor(SCRATCH, HOST);

    expect(draft.draft_id).toMatch(/^[0-9a-f-]{36}$/);
    expect(draft.content.length).toBeGreaterThan(0);
    // An absolute timestamp, not a duration: an agent that stored the draft
    // across a restart has to be able to say when it goes stale.
    expect(draft.expires_at).toBeGreaterThan(Date.now());
    expect(draft.expires_at - Date.now()).toBeLessThanOrEqual(30 * 60 * 1000);
    expect(draft).toMatchObject({
      path: SCRATCH,
      base: "Tickets.base",
      view: "All",
      context: HOST,
    });
  });

  test("the draft is a note we could parse, not a fragment", async () => {
    const { content } = await draftFor(SCRATCH, HOST);
    const note = parseNote(SCRATCH, content);
    expect(note.malformedFrontmatter).toBe(false);
    expect(content.startsWith("---\n")).toBe(true);
    expect(content.trimEnd().endsWith("the base's own filter is the ground truth.")).toBe(true);
  });

  test("nothing is written on the first call", async () => {
    await resolver.addNoteToBase({ base: "Tickets.base", path: SCRATCH, context: HOST });
    expect(resolver.vaultSource.notePaths()).not.toContain(SCRATCH);
  });
});

describe("filter inversion", () => {
  test('file.hasTag("ticket") becomes a tag', async () => {
    const { content } = await draftFor(SCRATCH, HOST);
    expect(frontmatterOf(content)["tags"]).toEqual(["ticket"]);
    expect(content).toContain("- ticket");
  });

  test("a this-scoped contains becomes a link to the host note", async () => {
    const { content } = await draftFor(SCRATCH, HOST);
    // The link, not the host's path: `[[SomeProject]]` resolves by basename
    // and survives the note moving between folders.
    expect(frontmatterOf(content)["project"]).toEqual(["[[SomeProject]]"]);
  });

  test("a nested host binds by basename without the extension", async () => {
    const { content } = await draftFor(SCRATCH, "Root Project.md");
    expect(frontmatterOf(content)["project"]).toEqual(["[[Root Project]]"]);
  });

  test("without a host the link is a marked placeholder, never a guess", async () => {
    const { content } = await draftFor(SCRATCH);
    expect(content).toContain("# TODO: could not invert project.contains(link(this.file.name))");
    // The one thing a placeholder must never do is invent a value.
    expect(frontmatterOf(content)["project"]).toBeUndefined();
  });

  test("order columns are seeded empty; computed columns are not seeded", async () => {
    const { content } = await draftFor(SCRATCH, HOST);
    const front = frontmatterOf(content);
    // `status` and `type` are note properties the view lists, so the agent sees
    // the shape a row is expected to have.
    expect(front["status"]).toBe("");
    expect(front["type"]).toBe("");
    // `file.name` is not frontmatter and `formula.priority_display` is
    // computed, so neither can be seeded.
    expect(Object.keys(front)).not.toContain("file.name");
    expect(Object.keys(front)).not.toContain("priority_display");
  });

  test("view-level filters are inverted too, and reported when they cannot be", async () => {
    const hostile = await Resolver.openDir(join(FIXTURES, "hostile-filters"));
    const { content } = await draftFrom(hostile, {
      base: "Hostile.base",
      view: "BareFormulaFilter",
      path: "Notes/__probe.md",
    });
    // The view filter is `formula.needs_follow_up`, a computed value that no
    // amount of frontmatter can pin down. It is reported, not dropped.
    expect(content).toContain("# TODO: could not invert formula.needs_follow_up");
  });

  test("a conjunct nobody can invert becomes a TODO comment", async () => {
    const nand = await Resolver.openDir(join(FIXTURES, "not-nand"));
    const { content } = await draftFrom(nand, { base: "Core.base", path: "Notes/__probe.md" });
    // `file.ext == "md"` reads a file, and a six-sibling `not:` is NAND. Both
    // are reported verbatim so the agent can satisfy them by hand.
    expect(content).toContain('# TODO: could not invert file.ext == "md"');
    expect(content).toContain('# TODO: could not invert none of (file.inFolder("plugins")');
  });
});

describe("the second call: verify, then commit", () => {
  test("the unedited draft commits, and the base then returns the row", async () => {
    try {
      const draft = await draftFor(SCRATCH, HOST);
      const result = await resolver.addNoteToBase({
        base: "Tickets.base",
        path: SCRATCH,
        context: HOST,
        draft_id: draft.draft_id,
        content: draft.content,
      });
      expect(result).toEqual({ written: SCRATCH, draft_id: draft.draft_id, verified: true });
      expect(resolver.vaultSource.notePaths()).toContain(SCRATCH);

      // The load-bearing assertion: the note we accepted is a row the real
      // query pipeline -- not our filter walk -- actually returns.
      const rows = await resolver.query("Tickets.base", { context: HOST });
      expect(rows.rows.map((r) => r.path)).toContain(SCRATCH);
    } finally {
      await deleteNote(SCRATCH);
    }
  });

  test("the second call sends ONLY draft_id and content", async () => {
    // Regression: `base` and `path` are optional because the agent does not
    // resend them, so the commit path must never read them off `options`.
    // Resending them in every test hid that it crashed without them.
    try {
      const draft = await draftFor(SCRATCH, HOST);
      const result = await resolver.addNoteToBase({
        draft_id: draft.draft_id,
        content: draft.content,
      });
      expect(result).toEqual({ written: SCRATCH, draft_id: draft.draft_id, verified: true });
    } finally {
      await deleteNote(SCRATCH);
    }
  });

  test("proposing without a path says which field is missing", async () => {
    await expect(resolver.addNoteToBase({ base: "Tickets.base" })).rejects.toThrow(/`path`/);
  });

  test("the commit is single-use", async () => {
    try {
      const draft = await draftFor(SCRATCH, HOST);
      await resolver.addNoteToBase({
        base: "Tickets.base",
        path: SCRATCH,
        context: HOST,
        draft_id: draft.draft_id,
        content: draft.content,
      });
      const replay = await messageOf(() =>
        resolver.addNoteToBase({
          base: "Tickets.base",
          path: SCRATCH,
          context: HOST,
          draft_id: draft.draft_id,
          content: draft.content,
        }),
      );
      expect(replay).toMatch(/unknown or has expired/);
    } finally {
      await deleteNote(SCRATCH);
    }
  });

  test("a draft missing a filtered property reports a mismatch, not a type error", async () => {
    // Regression: `project.contains(link(this.file.name))` DEREFERENCES
    // `project`, so a draft with no `project` at all makes the expression throw
    // instead of evaluating to false. That raw error buried the filter and the
    // base YAML, which are the two things the agent needs to correct itself.
    const draft = await draftFor(SCRATCH, HOST);
    try {
      const message = await messageOf(() =>
        resolver.addNoteToBase({
          draft_id: draft.draft_id,
          content: "---\ntags:\n  - ticket\n---\n\n# Scratch\n",
        }),
      );
      expect(message).toContain("does not match");
      expect(message).toContain("project.contains(link(this.file.name))");
      expect(message).toContain("filters:\n  and:");
      expect(resolver.vaultSource.notePaths()).not.toContain(SCRATCH);
    } finally {
      await deleteNote(SCRATCH);
    }
  });

  test("content that cannot match throws, writes nothing, and hands back the base YAML", async () => {
    const draft = await draftFor(SCRATCH, HOST);
    try {
      const message = await messageOf(() =>
        resolver.addNoteToBase({
          base: "Tickets.base",
          path: SCRATCH,
          context: HOST,
          draft_id: draft.draft_id,
          // Right shape, wrong values: the tag is absent and the link points
          // at a different project, so neither conjunct can hold.
          content: '---\ntags:\n  - note\nproject:\n  - "[[OtherProject]]"\n---\n\n# Scratch\n',
        }),
      );
      expect(message).toContain("does not match");
      expect(message).toContain('file.hasTag("ticket")');
      expect(message).toContain("project.contains(link(this.file.name))");
      // The base's own text, not our paraphrase of it: the correction is made
      // against the filter, so the filter has to be visible.
      expect(message).toContain('filters:\n  and:\n    - file.hasTag("ticket")');
      expect(resolver.vaultSource.notePaths()).not.toContain(SCRATCH);
    } finally {
      await deleteNote(SCRATCH);
    }
  });

  test("a this-scoped base refuses to commit without a host note", async () => {
    const draft = await draftFor(SCRATCH);
    try {
      const message = await messageOf(() =>
        resolver.addNoteToBase({
          base: "Tickets.base",
          path: SCRATCH,
          draft_id: draft.draft_id,
          content: draft.content,
        }),
      );
      expect(message).toContain("scoped to a host note");
      expect(message).toContain("context");
      expect(resolver.vaultSource.notePaths()).not.toContain(SCRATCH);
    } finally {
      await deleteNote(SCRATCH);
    }
  });

  test("a second call without content says what is missing", async () => {
    const draft = await draftFor(SCRATCH, HOST);
    const message = await messageOf(() =>
      resolver.addNoteToBase({
        base: "Tickets.base",
        path: SCRATCH,
        context: HOST,
        draft_id: draft.draft_id,
      }),
    );
    expect(message).toContain("content");
  });

  test("a draft cannot be redirected to another path", async () => {
    const draft = await draftFor(SCRATCH, HOST);
    // Verification is only sound for the path the draft was built for, so a
    // re-send naming a different path is refused rather than quietly honoured.
    const message = await messageOf(() =>
      resolver.addNoteToBase({
        base: "Tickets.base",
        path: "Tickets/__draft-elsewhere.md",
        context: HOST,
        draft_id: draft.draft_id,
        content: draft.content,
      }),
    );
    expect(message).toContain(SCRATCH);
    expect(resolver.vaultSource.notePaths()).not.toContain("Tickets/__draft-elsewhere.md");
  });

  test("malformed frontmatter is refused before the filter ever runs", async () => {
    const draft = await draftFor(SCRATCH, HOST);
    const message = await messageOf(() =>
      resolver.addNoteToBase({
        base: "Tickets.base",
        path: SCRATCH,
        context: HOST,
        draft_id: draft.draft_id,
        content: "---\ntags: [unclosed\n---\n\n# Scratch\n",
      }),
    );
    expect(message).toContain("not valid YAML");
    expect(resolver.vaultSource.notePaths()).not.toContain(SCRATCH);
  });
});

describe("the draft store", () => {
  test("an unknown id is a miss that says to re-draft", async () => {
    const message = await messageOf(() =>
      resolver.addNoteToBase({
        base: "Tickets.base",
        path: SCRATCH,
        context: HOST,
        draft_id: "not-a-draft",
        content: "---\n---\n",
      }),
    );
    expect(message).toMatch(/unknown or has expired/);
    expect(message).toContain("WITHOUT draft_id");
  });

  test("an expired draft is a miss, not a special case", () => {
    const store = new DraftStore(0);
    const { id } = store.put({
      base: "B.base",
      view: "V",
      path: "N.md",
      context: null,
      original: "",
    });
    expect(() => store.get(id)).toThrow(/expired/);
    // The miss deletes the draft, so an expired one is never found again.
    expect(store.size).toBe(0);
  });

  test("a live draft keeps what the commit needs", () => {
    const store = new DraftStore();
    const stored = store.put({
      base: "Tickets.base",
      view: "All",
      path: SCRATCH,
      context: HOST,
      original: "the draft we proposed",
    });
    expect(store.get(stored.id)).toMatchObject({
      base: "Tickets.base",
      view: "All",
      path: SCRATCH,
      context: HOST,
      original: "the draft we proposed",
    });
    store.release(stored.id);
    expect(store.size).toBe(0);
  });
});

describe("a Base is never written", () => {
  test("a .base path is refused on the drafting call", async () => {
    const message = await messageOf(() =>
      resolver.addNoteToBase({ base: "Tickets.base", path: "Tickets.base" }),
    );
    expect(message).toContain("a Base is never written");
  });

  test("a .base path is refused on the commit call too", async () => {
    const draft = await draftFor(SCRATCH, HOST);
    const message = await messageOf(() =>
      resolver.addNoteToBase({
        base: "Tickets.base",
        path: "Tickets.base",
        context: HOST,
        draft_id: draft.draft_id,
        content: draft.content,
      }),
    );
    expect(message).toContain("a Base is never written");
    // The base itself is untouched, not merely refused in the abstract.
    expect(await resolver.vaultSource.readText("Tickets.base")).toContain("views:");
  });

  test("a path the vault would never index is refused", async () => {
    const message = await messageOf(() =>
      resolver.addNoteToBase({ base: "Tickets.base", path: "Tickets/scratch.txt" }),
    );
    expect(message).toContain(".md note");
  });

  test("an existing note is never clobbered", async () => {
    const before = await resolver.vaultSource.readText("Tickets/Fix login redirect.md");
    const message = await messageOf(() =>
      resolver.addNoteToBase({ base: "Tickets.base", path: "Tickets/Fix login redirect.md" }),
    );
    expect(message).toContain("already exists");
    expect(await resolver.vaultSource.readText("Tickets/Fix login redirect.md")).toBe(before);
  });
});

/**
 * `this` binds to the HOST note, and the host note's path depth is its own
 * business.
 *
 * There is a claim that a draft at a different depth from its host verifies
 * against the wrong context, which would make every root-level or nested
 * pairing a lie. It is false, and these are the four pairings that say so: each
 * one commits, and each one produces a note the REAL query pipeline returns
 * as a row for that host -- which is the only evidence that matters, since a
 * note this module accepts but the pipeline drops is precisely the failure the
 * tool exists to prevent.
 */
describe("the host note's path depth does not change the verdict", () => {
  const PAIRINGS = [
    { host: "Projects/SomeProject.md", draft: "Tickets/__depth-nested.md" },
    { host: "Root Project.md", draft: "Tickets/__depth-root-host.md" },
    { host: "Projects/SomeProject.md", draft: "__depth-root-draft.md" },
    { host: "Root Project.md", draft: "__depth-root-both.md" },
  ];

  for (const { host, draft } of PAIRINGS) {
    test(`a draft at ${draft} commits against the host ${host} and becomes a row`, async () => {
      expect(resolver.vaultSource.notePaths()).not.toContain(draft);
      try {
        const proposal = await draftFor(draft, host);
        // The link is the host's BASENAME, which is what makes the depth
        // irrelevant: `[[SomeProject]]` resolves to the same note whether the
        // note being written sits beside it or two folders away.
        expect(parseNote(draft, proposal.content).frontmatter["project"]).toEqual([
          `[[${host.slice(host.lastIndexOf("/") + 1, host.length - 3)}]]`,
        ]);

        const result = await resolver.addNoteToBase({
          draft_id: proposal.draft_id,
          content: proposal.content,
        });
        expect(result).toEqual({ written: draft, draft_id: proposal.draft_id, verified: true });

        const rows = await resolver.query("Tickets.base", { context: host });
        expect(rows.rows.map((r) => r.path)).toContain(draft);
      } finally {
        await deleteNote(draft);
      }
    });
  }
});

/**
 * A root-level draft path creates no folder.
 *
 * Regression. `createNote` derived the parent directory by slicing the path at
 * its last `/`, and for a path with NO `/` that index is -1 -- so the "parent"
 * of `Note.md` came out as `Note.m`, and a directory by that name was created
 * beside the note. It was invisible from the vault index (a directory is not a
 * note, so no query ever saw it) which is exactly why it survived: the note
 * committed, verified, and became a row, and the vault quietly gained a folder
 * nobody asked for.
 */
describe("a draft at the vault root leaves the root alone", () => {
  const ROOT_SCRATCH = "__root-scratch.md";
  /** The bogus directory the parent calculation used to produce. */
  const BOGUS_DIR = "__root-scratch.m";

  /** Remove the bogus directory if a run left one behind. */
  async function removeBogusDir(): Promise<void> {
    await fs.rm(join(VAULT_DIR, BOGUS_DIR), { recursive: true, force: true });
  }

  test("writing a root-level note creates no directory beside it", async () => {
    expect(resolver.vaultSource.notePaths()).not.toContain(ROOT_SCRATCH);
    const before = await fs.readdir(VAULT_DIR);
    try {
      const proposal = await draftFor(ROOT_SCRATCH, HOST);
      const result = await resolver.addNoteToBase({
        draft_id: proposal.draft_id,
        content: proposal.content,
      });
      expect(result).toEqual({
        written: ROOT_SCRATCH,
        draft_id: proposal.draft_id,
        verified: true,
      });
      // The note, and nothing else. A directory here would be invisible to
      // every query, so asserting on the index alone would miss it.
      const added = (await fs.readdir(VAULT_DIR)).filter((n) => !before.includes(n));
      expect(added).toEqual([ROOT_SCRATCH]);
    } finally {
      await deleteNote(ROOT_SCRATCH);
      await removeBogusDir();
    }
  });

  test("a file already named like the bogus directory does not block the write", async () => {
    // The same bug, one step further on: `X.m` is a plausible real name. When a
    // file already occupies it, `mkdir` throws a bare EEXIST that is neither a
    // BasesError nor a message an agent can act on, and the note is never
    // written -- a row the base's own filter matched, refused by a collision
    // with a directory this tool was about to create for itself.
    await fs.writeFile(join(VAULT_DIR, BOGUS_DIR), "not a note\n");
    try {
      const proposal = await draftFor(ROOT_SCRATCH, HOST);
      const result = await resolver.addNoteToBase({
        draft_id: proposal.draft_id,
        content: proposal.content,
      });
      expect(result).toEqual({
        written: ROOT_SCRATCH,
        draft_id: proposal.draft_id,
        verified: true,
      });
      expect(resolver.vaultSource.notePaths()).toContain(ROOT_SCRATCH);
    } finally {
      await deleteNote(ROOT_SCRATCH);
      await removeBogusDir();
    }
  });
});
