/**
 * Backend equivalence: the filesystem and a non-filesystem `VaultSource` resolve
 * one vault to the same answers.
 *
 * The claim this suite turns into an executable assertion is the one the project
 * was built on -- "resolving a note containing a base yields identical output
 * over fs and WebDAV". It used to be prose. It is now a set of full-structure
 * comparisons over the testing vault, and it runs in the ordinary unit suite
 * with no server, no network, no Docker and no environment variable, because the
 * backend under test is an in-memory `VaultSource` rather than an HTTP client.
 *
 * How the assertions are written is the design:
 *
 *   - FULL structures, never a subset. A comparison that only checked row paths
 *     would pass while every cell value disagreed, and cell values are what an
 *     agent acts on.
 *   - ORDER is part of the value. `toEqual` on arrays is positional, so row
 *     order, group order and backlink order are all pinned without a separate
 *     "did it sort?" test pretending to cover them.
 *   - No conditional. Nothing here reads an environment variable or asks whether
 *     a server is reachable, so there is no assertion that can silently skip
 *     itself and report a green suite that compared nothing.
 *
 * Nothing here writes to `test/vault`. It is the Obsidian parity oracle and
 * stays at exactly `CORPUS_SIZE` files. The write comparisons happen in two
 * throwaway sandboxes -- a temp directory and a `Map` -- seeded identically.
 *
 * mtime is the one field the two backends cannot agree on, and the reason the
 * memory source has a fixed clock at all. `Vault` feeds `stat().mtime` to
 * `file.ctime` and `file.mtime`, so a base reading either puts a real timestamp
 * into query results and rendered markdown. The corpus deliberately reads
 * neither, and a test below PROVES that, so this suite can compare every other
 * field byte for byte instead of excluding mtime everywhere it appears.
 */

import { afterAll, describe, expect, test } from "bun:test";
import { mkdir, mkdtemp, readdir, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";

import type { QueryOptions } from "../../src/bases/query";
import { renderMarkdown } from "../../src/render/markdown";
import { Resolver } from "../../src/service";
import { contentHash, FsVaultSource } from "../../src/vault/fs";
import { isIndexable } from "../../src/vault/source";
import { CORPUS_SIZE, countFiles, loadCorpus, VAULT_DIR } from "./corpus";
import { MEMORY_MTIME_MS, type MemoryFault, MemoryVaultSource } from "./memory";

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

const corpus = await loadCorpus();

/** The reference backend, pointed at the oracle vault itself. Read-only. */
const fsSource = new FsVaultSource(VAULT_DIR);
const fsResolver = await Resolver.open(fsSource);

/** The fake WebDAV, holding the same bytes in a `Map`. */
const memory = new MemoryVaultSource(corpus);
const memResolver = await Resolver.open(memory);

/** The corpus notes, by path. Seven notes and two bases. */
const notes = corpus.filter((f) => f.path.endsWith(".md")).map((f) => f.path);

/** Every oracle view: the default one plus each named view. */
const ORACLE_VIEWS: Array<{ base: string; view?: string }> = [
  { base: "AllNotes.base" },
  { base: "AllNotes.base", view: "All" },
  { base: "AllNotes.base", view: "ByPriority" },
  { base: "AllNotes.base", view: "AsList" },
];

/** Host notes that bind `this` for `Tickets.base`, one at the vault root. */
const HOSTS = ["Projects/SomeProject.md", "Projects/OtherProject.md", "Root Project.md"];

/** A note carrying a `![[Tickets.base]]` embed AND a ```base fence. */
const NOTE_WITH_BASE = "Root Project.md";
/** A note carrying neither. Without it, the projection assertions could pass empty. */
const NOTE_WITHOUT_BASE = "Root Ticket.md";

/** The `structured` render, which `Resolver.render` deliberately does not expose. */
async function structured(resolver: Resolver, base: string, options: QueryOptions = {}) {
  const parsed = await resolver.loadBase(base);
  const result = await resolver.query(base, options);
  return renderMarkdown(parsed, result, "structured");
}

/** The rejection, whatever class it carries. An un-awaited `.rejects` proves nothing. */
async function rejection(run: () => Promise<unknown>): Promise<Error> {
  try {
    await run();
  } catch (err) {
    return err as Error;
  }
  throw new Error("Expected a rejection, but the call resolved");
}

/** A status carried structurally by a refusal, for a caller that would retry on it. */
function statusOf(err: Error): number | undefined {
  return (err as { status?: number }).status;
}

/**
 * Budget for tests that build a sandbox on disk.
 *
 * Every sandbox test writes the nine-file corpus to a temp directory and opens a
 * `Resolver` over it, which is milliseconds of real work. Under load it can be
 * seconds, and bun's 5s default would then fail a test that is not broken --
 * exactly the flake this project cannot afford, because a red suite gets retried
 * until it goes green and a real regression goes with it. The parity suite sets
 * its own budget for the same reason.
 */
const SANDBOX_TIMEOUT_MS = 30_000;

/** Temp directories, removed when the suite finishes. */
const sandboxes: string[] = [];

async function sandbox(): Promise<string> {
  const dir = await mkdtemp(join(tmpdir(), "bases-mcp-webdav-"));
  sandboxes.push(dir);
  return dir;
}

/** Write the corpus to a directory, so a temp vault matches the `Map` exactly. */
async function seedDir(dir: string): Promise<void> {
  for (const file of corpus) {
    const abs = join(dir, file.path);
    await mkdir(dirname(abs), { recursive: true });
    await writeFile(abs, file.content, "utf8");
  }
}

afterAll(async () => {
  for (const dir of sandboxes) await rm(dir, { recursive: true, force: true });
});

// ---------------------------------------------------------------------------
// The corpus, and the preconditions that make the comparisons meaningful
// ---------------------------------------------------------------------------

describe("the corpus", () => {
  test("is the testing vault, and the testing vault is untouched", async () => {
    // The oracle's size underpins every comparison below: a tenth file would
    // change what Obsidian returns for the same query, so the parity and
    // equivalence suites would then be describing different vaults.
    expect(corpus).toHaveLength(CORPUS_SIZE);
    expect(await countFiles(VAULT_DIR)).toBe(CORPUS_SIZE);
  });

  test("holds both bases and seven notes", () => {
    const bases = corpus.filter((f) => f.path.endsWith(".base")).map((f) => f.path);
    expect(bases).toEqual(["AllNotes.base", "Tickets.base"]);
    expect(notes).toHaveLength(7);
    expect(notes).toContain(NOTE_WITH_BASE);
    expect(notes).toContain(NOTE_WITHOUT_BASE);
  });

  test("the named invariant is present: a note that embeds a base", async () => {
    // Asserted through the projection, not by matching text, because "embeds a
    // base" is a claim about what `Vault` sees. A string search would still pass
    // on a note whose embed the parser had stopped recognising.
    const view = await memResolver.readNote(NOTE_WITH_BASE);
    expect(view.regions.length).toBeGreaterThan(0);
  });

  test("and a note that does not, so a projection assertion cannot pass empty", async () => {
    const view = await fsResolver.readNote(NOTE_WITHOUT_BASE);
    expect(view.regions).toEqual([]);
    expect(view.content).toBe(view.raw);
  });

  test("no corpus base reads file.mtime or file.ctime", () => {
    // The precondition that lets every comparison below be exact. Both accessors
    // read `stat().mtime`, which is the real clock on disk and a fixed instant in
    // memory, so a base touching either would make this suite fail on a Tuesday
    // and pass on a Wednesday. If one is added, this test fails first and names
    // the reason instead of leaving a mystery mismatch in a cell value.
    const offenders = corpus
      .filter((f) => /file\.(mtime|ctime)/.test(f.content))
      .map((f) => f.path);
    expect(offenders).toEqual([]);
  });
});

// ---------------------------------------------------------------------------
// The source contract itself
// ---------------------------------------------------------------------------

describe("list()", () => {
  test("returns identical vault-relative POSIX paths from both backends", async () => {
    expect(await memory.list()).toEqual(await fsSource.list());
  });

  test("is sorted, so Vault's insertion order -- and every unsorted view -- agree", async () => {
    const paths = await memory.list();
    expect(paths).toEqual([...paths].sort());
    expect(paths.length).toBeGreaterThan(1);
  });

  test("includes the bases alongside the notes", async () => {
    const paths = await memory.list();
    expect(paths).toContain("Tickets.base");
    expect(paths).toContain("AllNotes.base");
  });

  test("filters dotfiles, dot-directories and other extensions exactly as fs does", async () => {
    // The testing vault cannot carry this case: it must stay at nine files. So
    // the rule is pinned against a tree built in a temp directory, compared
    // three ways -- fs, memory, and `isIndexable` itself -- so a backend cannot
    // pass by agreeing with a wrong rule that two implementations happen to share.
    const tree: Record<string, string> = {
      ".obsidian/app.json": "{}",
      ".hidden/Note.md": "# hidden",
      "Notes/Alpha.md": "# alpha",
      "Notes/Deep/Beta.md": "# beta",
      "Notes/Deep/Notes.base": "views: []",
      "Notes/scratch.txt": "not a note",
      "Templates/template.md": "# template",
    };
    const dir = await sandbox();
    for (const [path, content] of Object.entries(tree)) {
      await mkdir(dirname(join(dir, path)), { recursive: true });
      await writeFile(join(dir, path), content, "utf8");
    }
    const expected = Object.keys(tree).filter(isIndexable).sort();

    expect(expected).toEqual([
      "Notes/Alpha.md",
      "Notes/Deep/Beta.md",
      "Notes/Deep/Notes.base",
      "Templates/template.md",
    ]);
    expect(await new MemoryVaultSource(toFiles(tree)).list()).toEqual(expected);
    expect(await new FsVaultSource(dir).list()).toEqual(expected);
  });
});

describe("hash()", () => {
  test("agrees on every corpus file", async () => {
    for (const file of corpus) {
      expect(await memory.hash(file.path)).toBe(await fsSource.hash(file.path));
    }
  });

  test("is a 32-character hex digest, so a truncation change cannot pass quietly", async () => {
    for (const file of corpus) {
      expect(await memory.hash(file.path)).toMatch(/^[0-9a-f]{32}$/);
    }
  });

  test("changes when the content changes, and comes back afterwards", async () => {
    const original = corpus.find((f) => f.path === "Root Ticket.md")?.content ?? "";
    const before = await memory.hash("Root Ticket.md");
    await memory.writeText("Root Ticket.md", "# different\n");
    expect(await memory.hash("Root Ticket.md")).not.toBe(before);

    // Restored rather than left dirty: a later test that asserted against the
    // oracle would otherwise fail for a reason that has nothing to do with it.
    await memory.writeText("Root Ticket.md", original);
    expect(await memory.hash("Root Ticket.md")).toBe(before);
  });
});

describe("stat()", () => {
  test("reports the identical byte size for every corpus file", async () => {
    for (const file of corpus) {
      expect((await memory.stat(file.path)).size).toBe((await fsSource.stat(file.path)).size);
    }
  });

  test("counts bytes, not UTF-16 code units", async () => {
    // The project notes carry non-ASCII frontmatter, so a code-unit count would
    // disagree with the file on disk.
    const path = "Projects/SomeProject.md";
    const content = corpus.find((f) => f.path === path)?.content ?? "";
    expect(content).toContain("ö");
    const size = (await memory.stat(path)).size;
    expect(size).toBe((await fsSource.stat(path)).size);
    expect(size).toBeGreaterThan(content.length);
  });

  test("mtime is the fixed instant, never the wall clock", async () => {
    // The one field the two backends cannot agree on, asserted as a fact rather
    // than assumed. See the file header for why the corpus stays clear of it.
    expect((await memory.stat("Root Ticket.md")).mtime.getTime()).toBe(MEMORY_MTIME_MS);
    expect((await fsSource.stat("Root Ticket.md")).mtime.getTime()).not.toBe(MEMORY_MTIME_MS);
  });

  test("hands out a fresh Date, so one caller cannot move every other comparison", async () => {
    const first = await memory.stat("Root Ticket.md");
    first.mtime.setUTCFullYear(1999);
    expect((await memory.stat("Root Ticket.md")).mtime.getUTCFullYear()).toBe(2024);
  });
});

describe("path normalisation", () => {
  test("resolves a redundant path to the same note on both backends", async () => {
    for (const path of ["Tickets/../Root Ticket.md", "./Tickets/Fix login redirect.md"]) {
      expect(await memory.readText(path)).toBe(await fsSource.readText(path));
    }
  });

  test("refuses a path that escapes the vault root, on both backends", async () => {
    for (const escapee of ["../outside.md", "/etc/passwd", "Tickets/../../outside.md"]) {
      const fromFs = await rejection(() => fsSource.readText(escapee));
      const fromMemory = await rejection(() => memory.readText(escapee));
      expect(fromFs.message).toMatch(/escapes the vault root/);
      expect(fromMemory.message).toMatch(/escapes the vault root/);
      expect(statusOf(fromMemory)).toBe(403);
    }
  });

  test("refuses to read a file that is not there, on both backends", async () => {
    const fromFs = await rejection(() => fsSource.readText("Nope.md"));
    const fromMemory = await rejection(() => memory.readText("Nope.md"));
    expect(fromFs).toBeInstanceOf(Error);
    // The fake carries the status as a field, not only as prose: a real backend
    // deciding whether to retry needs it structurally, and a test that matched on
    // strings would let one through.
    expect(statusOf(fromMemory)).toBe(404);
  });
});

// ---------------------------------------------------------------------------
// The Resolver surface, compared in full
// ---------------------------------------------------------------------------

describe("listBases()", () => {
  test("lists the same bases with the same view names and types", async () => {
    expect(await memResolver.listBases()).toEqual(await fsResolver.listBases());
  });

  test("is not vacuously empty", async () => {
    const bases = await memResolver.listBases();
    expect(bases).toHaveLength(2);
    expect(bases.map((b) => b.path)).toEqual(["AllNotes.base", "Tickets.base"]);
  });
});

describe("query()", () => {
  for (const { base, view } of ORACLE_VIEWS) {
    const label = view === undefined ? "the default view" : `view "${view}"`;

    test(`${base}, ${label}: rows, order, groups and every cell value agree`, async () => {
      // The FULL QueryResult: `total`, `context`, `warnings`, `groups`, and the
      // per-row `formula` record alongside `values`. A partial comparison misses
      // exactly the drift this suite exists to catch -- a formula evaluated per
      // column rather than per note shows up nowhere else.
      expect(await memResolver.query(base, { view })).toEqual(
        await fsResolver.query(base, { view }),
      );
    });

    test(`${base}, ${label}: the JSON surface agrees`, async () => {
      const [memBase, fsBase] = await Promise.all([
        memResolver.loadBase(base),
        fsResolver.loadBase(base),
      ]);
      expect(memResolver.toJson(await memResolver.query(base, { view }), memBase)).toEqual(
        fsResolver.toJson(await fsResolver.query(base, { view }), fsBase),
      );
    });
  }

  for (const host of HOSTS) {
    test(`Tickets.base hosted in ${host}: identical rows`, async () => {
      const options = { context: host };
      expect(await memResolver.query("Tickets.base", options)).toEqual(
        await fsResolver.query("Tickets.base", options),
      );
    });
  }

  test("row ORDER is asserted, not merely the row set", async () => {
    const options = { view: "AsList" };
    const ours = (await memResolver.query("AllNotes.base", options)).rows.map((r) => r.path);
    const theirs = (await fsResolver.query("AllNotes.base", options)).rows.map((r) => r.path);
    expect(ours).toEqual(theirs);
    expect(ours.length).toBeGreaterThan(1);
  });

  test("the same base under different Host notes yields different rows, identically", async () => {
    const rowsFor = async (host: string) =>
      (await memResolver.query("Tickets.base", { context: host })).rows.map((r) => r.path);
    const some = await rowsFor("Projects/SomeProject.md");
    const other = await rowsFor("Projects/OtherProject.md");

    expect(some).not.toEqual(other);
    expect(some).toEqual(await rowsFor("Projects/SomeProject.md"));
    expect(some.length).toBeGreaterThan(0);
    expect(other.length).toBeGreaterThan(0);
  });

  test("a base needing `this` with no Host note is a hard error on BOTH backends", async () => {
    // The divergence from the CLI is backend-independent. Refusing is our rule,
    // and it has to be our rule on every backend or `this` means different
    // things depending on where the vault lives.
    const fromMemory = await rejection(() => memResolver.query("Tickets.base"));
    const fromFs = await rejection(() => fsResolver.query("Tickets.base"));
    expect(fromMemory.message).toBe(fromFs.message);
    expect(fromMemory.message).toMatch(/this/i);
  });

  test("an unknown view name is refused identically", async () => {
    const options = { view: "NoSuchView" };
    const fromMemory = await rejection(() => memResolver.query("AllNotes.base", options));
    const fromFs = await rejection(() => fsResolver.query("AllNotes.base", options));
    expect(fromMemory.message).toBe(fromFs.message);
  });
});

describe("render(), both styles", () => {
  for (const { base, view } of ORACLE_VIEWS) {
    test(`${base}, view=${view ?? "(default)"}: flat markdown agrees`, async () => {
      expect(await memResolver.render(base, { view })).toBe(
        await fsResolver.render(base, { view }),
      );
    });

    test(`${base}, view=${view ?? "(default)"}: structured markdown agrees`, async () => {
      expect(await structured(memResolver, base, { view })).toBe(
        await structured(fsResolver, base, { view }),
      );
    });
  }

  for (const host of HOSTS) {
    test(`Tickets.base hosted in ${host}: both styles agree`, async () => {
      const options = { context: host };
      expect(await memResolver.render("Tickets.base", options)).toBe(
        await fsResolver.render("Tickets.base", options),
      );
      expect(await structured(memResolver, "Tickets.base", options)).toBe(
        await structured(fsResolver, "Tickets.base", options),
      );
    });
  }

  test("the two styles really differ, so comparing one is not comparing both", async () => {
    // Without this, `structured` could quietly become an alias of `flat` and the
    // assertions above would stay green while covering one surface twice.
    // `AsList` is a `list` view, which is where the two diverge.
    const flat = await memResolver.render("AllNotes.base", { view: "AsList" });
    const rich = await structured(memResolver, "AllNotes.base", { view: "AsList" });
    expect(flat).not.toBe(rich);
    expect(flat.startsWith("|")).toBe(true);
    expect((rich.split("\n")[0] ?? "").startsWith("- ")).toBe(true);
  });

  test("structured keeps the group headers that flat drops", async () => {
    const options = { view: "ByPriority" };
    expect(await memResolver.render("AllNotes.base", options)).not.toContain("**");
    expect(await structured(memResolver, "AllNotes.base", options)).toContain("**");
  });

  test("structured keeps the summaries footer that flat drops", async () => {
    const options = { view: "All" };
    const flat = await memResolver.render("AllNotes.base", options);
    const rich = await structured(memResolver, "AllNotes.base", options);
    expect(flat).not.toBe(rich);
    // The footer repeats the column labels, so it is one table further down.
    expect(rich.split("\n").filter((l) => l.startsWith("| ")).length).toBeGreaterThan(
      flat.split("\n").filter((l) => l.startsWith("| ")).length,
    );
  });
});

describe("readNote()", () => {
  test("projects every note in the vault identically, regions and provenance included", async () => {
    // The full `NoteView`: path, raw text, projected content, and one provenance
    // record per Base region. Comparing only `content` would hide a provenance
    // regression, and provenance is what an agent uses to decide whether the rows
    // it is looking at came from a file it can go and edit.
    expect(notes.length).toBeGreaterThan(0);
    for (const note of notes) {
      expect(await memResolver.readNote(note)).toEqual(await fsResolver.readNote(note));
    }
  });

  test("raw: true returns the stored bytes identically", async () => {
    for (const note of notes) {
      expect(await memResolver.readNote(note, { raw: true })).toEqual(
        await fsResolver.readNote(note, { raw: true }),
      );
    }
  });

  test("a Host note's projection renders its embedded base into a provenance fence", async () => {
    // `Root Project.md` carries a `![[Tickets.base]]` embed AND a ```base fence,
    // so it exercises both kinds of Base region. The count is pinned: a parser
    // that stopped seeing the inline fence would still agree with fs if fs also
    // stopped seeing it, and that is precisely the regression this catches.
    const view = await memResolver.readNote(NOTE_WITH_BASE);
    expect(view.regions).toHaveLength(2);
    expect(view.content).toContain('```base-rendered path="Tickets.base"');
    // `context` is UNDEFINED on purpose: `readNote` records the context the
    // CALLER asked for, not the Host note it bound `this` to. The Host note is
    // re-derived from the note being written, so the fence does not need it --
    // but an agent reading a Projection cannot see which note scoped the rows,
    // and that is worth a test rather than a surprise.
    expect(view.regions.map((r) => r.provenance)).toEqual([
      { path: "Tickets.base", view: undefined, context: undefined },
      { view: undefined, context: undefined },
    ]);
  });

  test("the inline fence scopes itself to the same Host note as the embed", async () => {
    // Both regions of `Root Project.md` bind `this` to that note, and exactly one
    // ticket links it. A region that lost its Host note would raise instead of
    // rendering a row, so reaching the prose is itself the assertion.
    const view = await memResolver.readNote(NOTE_WITH_BASE);
    expect(view.content).toContain("Root Ticket");
  });

  test("the projection really replaced the region, rather than echoing the note", async () => {
    const view = await memResolver.readNote(NOTE_WITH_BASE);
    expect(view.content).not.toBe(view.raw);
    expect(view.raw).toContain("![[Tickets.base]]");
    expect(view.content).not.toContain("![[Tickets.base]]");
  });

  test("a note with no Base region projects to itself", async () => {
    const view = await memResolver.readNote(NOTE_WITHOUT_BASE);
    expect(view.content).toBe(view.raw);
    expect(view.regions).toEqual([]);
  });

  test("an unknown note is refused identically", async () => {
    const fromMemory = await rejection(() => memResolver.readNote("Nope.md"));
    const fromFs = await rejection(() => fsResolver.readNote("Nope.md"));
    expect(fromMemory.message).toBe(fromFs.message);
  });
});

describe("backlinks()", () => {
  test("resolves the same backlinks for every note, in the same order", async () => {
    for (const note of notes) {
      expect(memResolver.backlinks(note)).toEqual(fsResolver.backlinks(note));
    }
  });

  test("is not vacuously empty on both sides", async () => {
    // Every ticket links the project note hosting its embed, and `Root Ticket.md`
    // links its project, so the vault has backlinks in both directions.
    const total = [...HOSTS, NOTE_WITHOUT_BASE].reduce(
      (n, target) => n + memResolver.backlinks(target).length,
      0,
    );
    expect(total).toBeGreaterThan(0);
    expect(memResolver.backlinks(NOTE_WITHOUT_BASE)).toEqual(
      fsResolver.backlinks(NOTE_WITHOUT_BASE),
    );
  });

  test("an embed is not a backlink, so a Base region never links its own Host note", async () => {
    // `file.links` skips embeds and `file.embeds` carries them. A backend that
    // moved between the two would give a host note a self-link from its own
    // embed, and the shortest-path rule would then resolve it elsewhere.
    expect(memResolver.backlinks(NOTE_WITH_BASE).map((b) => b.path)).not.toContain(NOTE_WITH_BASE);
  });
});

// ---------------------------------------------------------------------------
// Writes
// ---------------------------------------------------------------------------

interface Pair {
  dir: string;
  fs: Resolver;
  memory: MemoryVaultSource;
  mem: Resolver;
}

/** Two vaults holding identical bytes: a temp directory and a `Map`. */
async function pair(): Promise<Pair> {
  const dir = await sandbox();
  await seedDir(dir);
  const memory = new MemoryVaultSource(corpus);
  return {
    dir,
    fs: await Resolver.open(new FsVaultSource(dir)),
    memory,
    mem: await Resolver.open(memory),
  };
}

describe("writeText, ensureDir and createNote", () => {
  test(
    "createNote makes its parents, and the two vaults agree afterwards",
    async () => {
      const { dir, fs, memory, mem } = await pair();
      const path = "Sandbox/Deep/Nested/New Note.md";
      const content = "---\ntags:\n  - ticket\nstatus: active\n---\n\n# New\n";

      await fs.createNote(path, content);
      await mem.createNote(path, content);

      expect(await memory.readText(path)).toBe(content);
      expect(await memory.list()).toEqual(await fs.vaultSource.backend.list());
      expect(await memory.list()).toContain(path);
      // fs created a real collection; the fake recorded that it was asked to. A
      // `Map` has no directories, so without this the parent-creating half of
      // `createNote` would go untested on the fake forever.
      expect(memory.dirs.has("Sandbox/Deep/Nested")).toBe(true);
      expect((await readdir(join(dir, "Sandbox/Deep/Nested"))).length).toBe(1);
    },
    SANDBOX_TIMEOUT_MS,
  );

  test(
    "a root-level note creates no collection",
    async () => {
      // `createNote` only has a parent when the path has a separator. Slicing
      // unconditionally would make the parent `Note.m`, and both backends would
      // dutifully create a directory beside the note.
      const { dir, memory, mem } = await pair();
      await mem.createNote("Note.md", "# root\n");
      expect(memory.dirs.size).toBe(0);
      expect((await readdir(dir)).filter((n) => !n.startsWith("."))).not.toContain("Note.m");
    },
    SANDBOX_TIMEOUT_MS,
  );

  test(
    "overwriting is silent, and the two vaults stay identical",
    async () => {
      const { fs, memory, mem } = await pair();
      await fs.createNote("Note.md", "first\n");
      await mem.createNote("Note.md", "first\n");
      await fs.createNote("Note.md", "second\n");
      await mem.createNote("Note.md", "second\n");

      expect(await memory.readText("Note.md")).toBe(
        await fs.vaultSource.backend.readText("Note.md"),
      );
      expect(await memory.hash("Note.md")).toBe(await fs.vaultSource.backend.hash("Note.md"));
      expect((await memory.stat("Note.md")).size).toBe(
        (await fs.vaultSource.backend.stat("Note.md")).size,
      );
      expect(await memory.list()).toEqual(await fs.vaultSource.backend.list());
    },
    SANDBOX_TIMEOUT_MS,
  );

  test(
    "a written note becomes a queryable row, identically",
    async () => {
      const { fs, mem } = await pair();
      const content =
        '---\ntags:\n  - ticket\nstatus: active\npriority: high\ntype: task\nproject:\n  - "[[SomeProject]]"\n---\n';
      await fs.createNote("Tickets/Zebra.md", content);
      await mem.createNote("Tickets/Zebra.md", content);

      const options = { context: "Projects/SomeProject.md" };
      expect(await mem.query("Tickets.base", options)).toEqual(
        await fs.query("Tickets.base", options),
      );
    },
    SANDBOX_TIMEOUT_MS,
  );

  test(
    "writeNote reconciles against the stored bytes and agrees across backends",
    async () => {
      const { fs, mem } = await pair();
      const original = await fs.readNote(NOTE_WITH_BASE, { raw: true });
      const edited = original.raw.replace("\n## Tickets", "\nA new line.\n\n## Tickets");

      const fromMemory = await mem.writeNote(NOTE_WITH_BASE, edited);
      const fromFs = await fs.writeNote(NOTE_WITH_BASE, edited);

      expect(edited).not.toBe(original.raw);
      expect(fromMemory.text).toBe(fromFs.text);
      expect(fromMemory.refused).toEqual(fromFs.refused);
      expect(fromMemory.removedRegion).toBe(fromFs.removedRegion);
      // Both still project the same Base regions after the edit.
      expect(await mem.readNote(NOTE_WITH_BASE)).toEqual(await fs.readNote(NOTE_WITH_BASE));
    },
    SANDBOX_TIMEOUT_MS,
  );

  test(
    "a Base region survives the write, so the embed is not replaced by a fence",
    async () => {
      // Round-tripping a Projection is the designed flow: read the note, edit the
      // prose, write it back. The rendered fence must never reach disk.
      //
      // `Projects/SomeProject.md` carries a single `![[Tickets.base]]` embed, which
      // is the shape the reconciler pairs silently. The note with an inline
      // ```base fence is pinned separately, below, because it is not that shape.
      const { mem } = await pair();
      const original = (await fsResolver.readNote("Projects/SomeProject.md", { raw: true })).raw;
      const projected = (await mem.readNote("Projects/SomeProject.md")).content;
      const result = await mem.writeNote("Projects/SomeProject.md", projected);

      expect(result.text).toContain("![[Tickets.base]]");
      expect(result.text).not.toContain("base-rendered");
      expect(result.refused).toEqual([]);
      expect(result.removedRegion).toBe(false);
      // Compared modulo trailing whitespace on purpose. A Projection puts a
      // newline after each rendered region, and both notes that carry nothing but
      // an embed end with the embed and no final newline, so round-tripping either
      // one appends exactly one. That is the Projection being lossy about
      // whitespace, not a backend difference, and it is pinned here so a later
      // change to it has to be deliberate.
      expect(result.text.trimEnd()).toBe(original.trimEnd());
      expect(result.text).not.toBe(original);
    },
    SANDBOX_TIMEOUT_MS,
  );

  test(
    "an inline base fence is restored but reported, identically on both backends",
    async () => {
      // A rendered fence for an INLINE ```base region carries no `path=`, so there
      // is nothing to pair it to and the reconciler reports a removal and an
      // insertion -- while still putting the live fence back. That is the current
      // behaviour, and it is a property of the projection, not of the backend, so
      // the assertion is that the two backends agree on it exactly.
      const { fs, mem } = await pair();
      const projected = (await mem.readNote(NOTE_WITH_BASE)).content;

      const fromMemory = await mem.writeNote(NOTE_WITH_BASE, projected);
      const fromFs = await fs.writeNote(NOTE_WITH_BASE, projected);

      expect(fromMemory.refused).toEqual(fromFs.refused);
      expect(fromMemory.text).toBe(fromFs.text);
      expect(fromMemory.removedRegion).toBe(true);
      // Removal FIRST. A restored region is put back at the point it belongs, so
      // the refusals come out in the order the regions land in the rebuilt note
      // rather than with every insertion reported ahead of every restoration.
      expect(fromMemory.refused.map((r) => r.reason)).toEqual([
        "The base region was removed.",
        "A new base region was inserted.",
      ]);
      // Restored, so the note is still renderable.
      expect(fromMemory.text).toContain("```base\nfilters:");
      expect(fromMemory.text).not.toContain("base-rendered");
    },
    SANDBOX_TIMEOUT_MS,
  );
});

// ---------------------------------------------------------------------------
// The fault seam
// ---------------------------------------------------------------------------

describe("failure injection", () => {
  /** A source over the corpus with one fault armed. */
  function armed(fault: MemoryFault): MemoryVaultSource {
    const source = new MemoryVaultSource(corpus);
    source.inject(fault);
    return source;
  }

  test("a write that lands different bytes than requested", async () => {
    // The one failure a client cannot detect from the response: the PUT succeeds
    // and the server stored something else. It is why `hash()` exists, so the
    // stored bytes must both differ from the request AND hash differently.
    const requested = "# requested\n";
    const swapped = "# swapped by the server\n";
    const source = armed({ path: "Root Ticket.md", op: "write", stored: swapped });

    await source.writeText("Root Ticket.md", requested);

    expect(await source.readText("Root Ticket.md")).toBe(swapped);
    expect(await source.hash("Root Ticket.md")).not.toBe(contentHash(requested));
    expect(await source.hash("Root Ticket.md")).toBe(contentHash(swapped));
  });

  test("a simulated 500 surfaces as a structured refusal", async () => {
    const err = await rejection(() =>
      armed({ path: "Tickets.base", op: "read", status: 500 }).readText("Tickets.base"),
    );
    expect(statusOf(err)).toBe(500);
    expect(err.message).toContain("500");
  });

  test("a fault fires only as many times as it is given", async () => {
    const source = armed({ path: "Root Ticket.md", op: "read", status: 503, times: 1 });
    expect((await rejection(() => source.readText("Root Ticket.md"))).message).toContain("503");
    expect(await source.readText("Root Ticket.md")).toContain("Root ticket");
  });

  test("a fault on one path leaves the rest of the vault readable", async () => {
    const source = armed({ path: "Tickets.base", op: "read", status: 500 });
    expect(await rejection(() => source.readText("Tickets.base"))).toBeInstanceOf(Error);
    expect(await source.list()).toHaveLength(CORPUS_SIZE);
    expect(await source.readText(NOTE_WITH_BASE)).toContain("business-idea");
  });

  test("clearFaults disarms, so one test cannot inherit another's fault", async () => {
    const source = armed({ path: "Root Ticket.md", op: "read", status: 500 });
    expect(await rejection(() => source.readText("Root Ticket.md"))).toBeInstanceOf(Error);
    source.clearFaults();
    expect(await source.readText("Root Ticket.md")).toContain("Root ticket");
  });

  test("a Resolver over a faulty source reports the failure, not an empty vault", async () => {
    // The dangerous shape is a backend that swallows an error and answers with
    // an empty vault: indistinguishable from a vault that genuinely matches
    // nothing, which is the one confusion this server exists to avoid.
    const source = new MemoryVaultSource(corpus);
    source.inject({ path: "**", status: 500 });
    expect(statusOf(await rejection(() => Resolver.open(source)))).toBe(500);
  });

  test("a fault that says neither a status nor stored bytes is refused at the seam", () => {
    // Parsed, not validated: the fake is the boundary between a test and the
    // behaviour it is asking for, so a fault that means nothing dies here rather
    // than silently never firing.
    expect(() => new MemoryVaultSource().inject({ path: "x" })).toThrow(/status|stored/i);
  });
});

// ---------------------------------------------------------------------------

/** `{ path, content }` pairs from an inline tree, for the `list()` comparison. */
function toFiles(tree: Record<string, string>): Array<{ path: string; content: string }> {
  return Object.entries(tree).map(([path, content]) => ({ path, content }));
}
