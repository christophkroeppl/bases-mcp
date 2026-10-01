/**
 * The write path, and what it must refuse to lose.
 *
 * `write_note` and the `add_note_to_base` commit are the only two operations
 * that overwrite a file a human owns. Every test here is about one of them
 * destroying something it never saw, and every test that can reproduces the real
 * sequence DOES: the agent reads, the vault changes underneath it, the agent
 * writes. A unit assertion on the reconciler would pass against an implementation
 * whose I/O path was broken, which is exactly what happened once already.
 *
 * Everything runs in a temp directory seeded from the testing vault rather than
 * in the vault itself: these tests write notes, and `test/vault` is the Obsidian
 * parity oracle and stays at exactly `CORPUS_SIZE` files. The corpus is LOADED
 * rather than re-enumerated, because a second definition of "what is a note" is
 * how the two backends' equivalence suites drift apart from each other.
 */

import { afterAll, describe, expect, test } from "bun:test";
import { mkdir, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";

import { type DraftProposal, drafts } from "../../src/mcp/drafts";
import { Resolver } from "../../src/service";
// The one hash both backends share. Imported rather than reimplemented for the
// same reason the memory vault imports it: a second hash function is a second
// chance to disagree, and a disagreement here reads as a backend bug.
import { contentHash } from "../../src/vault/fs";
import { loadCorpus } from "../webdav/corpus";

/** The note every write test here edits. A host for `Tickets.base`, like `test/vault`. */
const NOTE = "Tickets/Invoice export.md";
/** The host note that binds `this` for `Tickets.base`. */
const HOST = "Projects/SomeProject.md";
/** The path the draft handshake proposes a note at. Never written by these tests. */
const SCRATCH = "Tickets/__service-scratch.md";

const corpus = await loadCorpus();

/** Temp directories, removed when the suite finishes. */
const sandboxes: string[] = [];

/**
 * Budget for tests that build a vault on disk.
 *
 * Each of these writes the corpus to a temp directory and indexes it, which is
 * milliseconds of real work -- and milliseconds become seconds under load, at
 * which point bun's 5s default fails a test that is not broken. A red suite gets
 * retried until it is green, and a real regression goes with it.
 */
const SANDBOX_TIMEOUT_MS = 30_000;

/** A Resolver over a throwaway copy of the testing vault. */
async function sandbox(): Promise<{ dir: string; resolver: Resolver }> {
  const dir = await mkdtemp(join(tmpdir(), "bases-mcp-write-"));
  sandboxes.push(dir);
  for (const file of corpus) {
    const abs = join(dir, file.path);
    await mkdir(dirname(abs), { recursive: true });
    await writeFile(abs, file.content, "utf8");
  }
  drafts.clear();
  return { dir, resolver: await Resolver.openDir(dir) };
}

/** What the user's editor left on disk, not what the server believes it wrote. */
function onDisk(dir: string, path: string): Promise<string> {
  return readFile(join(dir, path), "utf8");
}

/** The message of a rejection, so a test can assert on its whole text. */
async function messageOf(run: () => Promise<unknown>): Promise<string> {
  try {
    await run();
  } catch (err) {
    return (err as Error).message;
  }
  throw new Error("Expected a rejection, but the call succeeded");
}

afterAll(async () => {
  for (const dir of sandboxes) await rm(dir, { recursive: true, force: true });
});

// ---------------------------------------------------------------------------
// write_note: a concurrent external edit
// ---------------------------------------------------------------------------

describe("an external edit survives write_note", () => {
  // Regression. `writeNote` read the note through the cached backend, so it
  // reconciled the agent's edit against a snapshot from whenever the note was
  // last read and wrote the result over the top. Obsidian autosaves continuously,
  // so one second of typing was enough to lose the user's paragraph -- and the
  // response said `health: ok`.
  test(
    "a note the user rewrote after the read is not clobbered",
    async () => {
      const { dir, resolver } = await sandbox();

      // The agent reads the note, exactly as the designed flow intends.
      const asRead = await resolver.readNote(NOTE, { raw: true });

      // The user edits the same note in Obsidian, which autosaves it.
      const userText = `${asRead.raw}\nTWO MINUTES LATER: the user rewrote this whole paragraph in \
Obsidian, adding several new lines of real prose that must not be lost.\n`;
      await writeFile(join(dir, NOTE), userText, "utf8");

      // The agent writes back its edit of the copy it read.
      const agentEdit = asRead.raw.replace("# Invoice export", "# Invoice export, revised");
      const message = await messageOf(() => resolver.writeNote(NOTE, agentEdit, asRead.baseHash));

      expect(message).toContain("changed since you read it");
      expect(await onDisk(dir, NOTE)).toBe(userText);
      expect(await onDisk(dir, NOTE)).toContain("the user rewrote this whole paragraph");
    },
    SANDBOX_TIMEOUT_MS,
  );

  test(
    "a ticked task the user added is not lost",
    async () => {
      // Reconciling against a stale copy loses whichever change was made outside
      // the agent's view. Here the user appends and the agent rewrites a heading,
      // so the correct result contains both.
      const { dir, resolver } = await sandbox();

      const view = await resolver.readNote(NOTE, { raw: true });
      const agentEdit = view.raw.replace("# Invoice export", "# Invoice export v2");

      await writeFile(join(dir, NOTE), `${view.raw}\n\n- [x] a task the user ticked\n`, "utf8");

      const message = await messageOf(() => resolver.writeNote(NOTE, agentEdit, view.baseHash));

      expect(message).toContain("changed since you read it");
      expect(await onDisk(dir, NOTE)).toContain("a task the user ticked");
    },
    SANDBOX_TIMEOUT_MS,
  );

  test(
    "re-reading and reapplying succeeds, which is the whole point of refusing",
    async () => {
      const { dir, resolver } = await sandbox();

      const view = await resolver.readNote(NOTE, { raw: true });
      const userText = `${view.raw}\nA paragraph the user added in Obsidian.\n`;
      await writeFile(join(dir, NOTE), userText, "utf8");

      await expect(resolver.writeNote(NOTE, view.raw, view.baseHash)).rejects.toThrow(
        /changed since you read it/,
      );

      // The agent obeys the instruction: read it again, reapply, write back. This
      // only works because `get_note` reads the note as it is NOW. Handing back a
      // second stale copy would produce the same hash, the same refusal, and an
      // agent stuck retrying a write it can never win -- a silent data loss traded
      // for a loud deadlock.
      const reread = await resolver.readNote(NOTE, { raw: true });
      expect(reread.raw).toContain("A paragraph the user added in Obsidian.");
      expect(reread.baseHash).not.toBe(view.baseHash);

      const edited = reread.raw.replace("# Invoice export", "# Invoice export, revised");
      await resolver.writeNote(NOTE, edited, reread.baseHash);

      const stored = await onDisk(dir, NOTE);
      expect(stored).toContain("A paragraph the user added in Obsidian.");
      expect(stored).toContain("# Invoice export, revised");
    },
    SANDBOX_TIMEOUT_MS,
  );

  test(
    "a conditional write succeeds when nothing changed",
    async () => {
      const { dir, resolver } = await sandbox();

      const view = await resolver.readNote(NOTE, { raw: true });
      const edited = view.raw.replace("# Invoice export", "# Invoice export, revised");

      const result = await resolver.writeNote(NOTE, edited, view.baseHash);

      expect(result.refused).toEqual([]);
      expect(result.removedRegion).toBe(false);
      expect(await onDisk(dir, NOTE)).toBe(edited);
    },
    SANDBOX_TIMEOUT_MS,
  );

  test(
    "a mismatched hash is refused rather than applied, including the hash of nothing",
    async () => {
      const { dir, resolver } = await sandbox();
      const before = await onDisk(dir, NOTE);

      for (const hash of ["0000000000000000", contentHash("")]) {
        const message = await messageOf(() => resolver.writeNote(NOTE, "# clobbered\n", hash));
        expect(message).toContain("changed since you read it");
      }

      expect(await onDisk(dir, NOTE)).toBe(before);
    },
    SANDBOX_TIMEOUT_MS,
  );

  test(
    "omitting the hash is an explicitly blind write, not an accident",
    async () => {
      // Right for an agent constructing a note wholesale, wrong for one editing
      // prose it read earlier -- so it stays allowed, and what it costs is pinned
      // here rather than argued about in a comment.
      const { dir, resolver } = await sandbox();

      const view = await resolver.readNote(NOTE, { raw: true });
      await writeFile(join(dir, NOTE), `${view.raw}\nthe user's new paragraph.\n`, "utf8");

      await resolver.writeNote(NOTE, view.raw.replace("# Invoice", "# Edited"));

      // The user's paragraph is gone, and reported as applied. That is the deal.
      expect(await onDisk(dir, NOTE)).toContain("# Edited");
      expect(await onDisk(dir, NOTE)).not.toContain("the user's new paragraph");
    },
    SANDBOX_TIMEOUT_MS,
  );
});

describe("get_note and write_note agree about what was read", () => {
  test(
    "the hash get_note returns is the hash write_note checks against",
    async () => {
      const { resolver } = await sandbox();

      // Both surfaces of `get_note`: a Projection an agent might read, and the raw
      // text it would edit. The hash describes `raw` either way, so an agent that
      // read one and wrote the other is still protected.
      const projected = await resolver.readNote(HOST);
      const raw = await resolver.readNote(HOST, { raw: true });

      expect(projected.baseHash).toBe(raw.baseHash);
      expect(projected.baseHash).toBe(contentHash(raw.raw));
    },
    SANDBOX_TIMEOUT_MS,
  );
});

// ---------------------------------------------------------------------------
// add_note_to_base: a note created in the window
// ---------------------------------------------------------------------------

describe("the add_note_to_base commit re-checks that the path is free", () => {
  // Regression. The absence check ran when the draft was PROPOSED, and
  // `createNote` replaces verbatim, so a note a human created in the window
  // between the two calls -- minutes later, while they read the proposal -- was
  // destroyed unreported. The commit re-checks, against the BACKEND rather than
  // the index: the index is a snapshot from whenever this process last listed the
  // vault, so asking it is a check that passes precisely when it is needed.
  test(
    "a note created after the draft was proposed is not overwritten",
    async () => {
      const { dir, resolver } = await sandbox();

      const proposal = (await resolver.addNoteToBase({
        base: "Tickets.base",
        path: SCRATCH,
        context: HOST,
      })) as DraftProposal;

      // A human creates the note while the agent is still editing the draft.
      const humanNote = "---\ntags:\n  - ticket\n---\n\n# Written by a human, at length.\n";
      await writeFile(join(dir, SCRATCH), humanNote, "utf8");

      const message = await messageOf(() =>
        resolver.addNoteToBase({ draft_id: proposal.draft_id, content: proposal.content }),
      );

      expect(message).toContain("was created after this draft was proposed");
      expect(await onDisk(dir, SCRATCH)).toBe(humanNote);
    },
    SANDBOX_TIMEOUT_MS,
  );

  test(
    "a commit into a path that is still free writes the note",
    async () => {
      // The counterpart, so the re-check cannot be satisfied by refusing everything.
      const { dir, resolver } = await sandbox();

      const proposal = (await resolver.addNoteToBase({
        base: "Tickets.base",
        path: SCRATCH,
        context: HOST,
      })) as DraftProposal;

      const result = await resolver.addNoteToBase({
        draft_id: proposal.draft_id,
        content: proposal.content,
      });

      expect(result).toEqual({ written: SCRATCH, draft_id: proposal.draft_id, verified: true });
      expect(await onDisk(dir, SCRATCH)).toBe(proposal.content);
    },
    SANDBOX_TIMEOUT_MS,
  );

  test(
    "a path a human filled is refused at PROPOSE time as well as at commit",
    async () => {
      // Both halves ask the backend rather than the index, so a note written
      // behind the server's back is caught whichever call happens next.
      const { dir, resolver } = await sandbox();
      const humanNote = "# Written by a human.\n";
      await writeFile(join(dir, SCRATCH), humanNote, "utf8");

      const message = await messageOf(() =>
        resolver.addNoteToBase({ base: "Tickets.base", path: SCRATCH, context: HOST }),
      );

      expect(message).toContain("already exists");
      expect(await onDisk(dir, SCRATCH)).toBe(humanNote);
    },
    SANDBOX_TIMEOUT_MS,
  );
});
