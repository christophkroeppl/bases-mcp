/**
 * An in-memory `VaultSource`: a fake WebDAV server with no network in it.
 *
 * This is the harness half of the WebDAV work. The invariant the project claims
 * -- "resolving a note containing a base yields identical output over fs and
 * WebDAV" -- was prose until this existed. A real WebDAV test needs a server, a
 * network and a clock, so it gets skipped, so it does not run on every commit,
 * so it rots. This source is that same contract with the I/O removed: any
 * `VaultSource` implementation can be pointed at the same corpus and held
 * against the filesystem one, in the ordinary unit suite, anywhere.
 *
 * Three rules make it a useful oracle rather than a second opinion:
 *
 *   1. It reuses `contentHash` rather than hashing for itself. Two hash
 *      implementations are two chances to disagree, and a disagreement there
 *      would be reported as a backend bug rather than a test bug.
 *   2. Its mtime is a FIXED instant, never `Date.now()`. The filesystem's mtime
 *      is the real clock, so any comparison touching mtime is a comparison
 *      against time, and a flaky one. See `MEMORY_MTIME_MS`.
 *   3. Its path normalisation reproduces `path.resolve`, so a path that is legal
 *      for one backend is legal for the other. `Vault` hands out vault-relative
 *      POSIX paths, and a backend that resolved them differently would differ in
 *      `list()` output alone, which is noise masquerading as a finding.
 *
 * It is a FAKE, so it is wrong in the ways a fake must be: see `delete`, which
 * mirrors the filesystem's forgiving behaviour rather than WebDAV's.
 */

import { contentHash } from "../../src/vault/fs";
import { type FileStat, isIndexable, type VaultSource } from "../../src/vault/source";

/**
 * The instant every file in a memory vault reports as its mtime, in
 * milliseconds since the epoch.
 *
 * Fixed rather than sampled, because the filesystem's mtime is the real clock
 * and any assertion comparing the two would compare two different moments. It
 * is in the past so that `(now() - file.mtime)` stays positive, which is the
 * idiom real vaults use and therefore the one worth keeping meaningful.
 */
export const MEMORY_MTIME_MS = Date.UTC(2024, 0, 1, 0, 0, 0, 0);

/**
 * The fixed instant as a `Date`.
 *
 * A fresh object per call, because `FileStat.mtime` is a mutable `Date` and a
 * shared constant would let one caller shift every other comparison in the
 * suite by calling `setMonth` on it.
 */
export function memoryMtime(): Date {
  return new Date(MEMORY_MTIME_MS);
}

/** One file the fake serves, keyed by vault-relative POSIX path. */
export interface VaultFile {
  path: string;
  content: string;
}

/** The operations a fault can be attached to. */
export type MemoryOp = "list" | "read" | "stat" | "hash" | "write" | "delete";

/** A fault on every path. */
const ANY_PATH = "**";

/**
 * An injected failure.
 *
 * The two cases that matter are the ones a correct-but-unlucky client hits:
 * a 5xx the server invented, and a write that reports success while storing
 * something else. The second is why `stored` exists: read-verify-write exists
 * precisely to catch it, and a fake that can only fail loudly cannot exercise
 * the verify step.
 */
export interface MemoryFault {
  /** Vault-relative POSIX path, or `"**"` for any path. */
  path: string;
  /** The operation that fails. Omit to fail every operation on the path. */
  op?: MemoryOp;
  /**
   * The status a WebDAV server would answer with, e.g. 404 or 500.
   *
   * Omit only when `stored` is set: the write then SUCCEEDS and this value would
   * be a lie.
   */
  status?: number;
  /**
   * Store this text instead of the requested one, simulating a server that
   * accepted the PUT and wrote something else. Only meaningful on `write`.
   */
  stored?: string;
  /** Fire at most this many times. Omit to fail every time. */
  times?: number;
}

/**
 * A refusal from the fake server.
 *
 * `status` is carried rather than left in the message because the caller that
 * has to react -- the WebDAV backend deciding whether to retry -- needs it as a
 * field. A fake that only threw prose would let a real backend pass its tests
 * by matching on strings.
 */
export class MemoryVaultError extends Error {
  readonly status: number;
  readonly path: string;

  constructor(status: number, path: string, message: string) {
    super(message);
    this.name = "MemoryVaultError";
    this.status = status;
    this.path = path;
  }
}

/** What the fake is currently holding, and what it has been asked to create. */
interface ArmedFault {
  fault: MemoryFault;
  remaining: number;
}

export class MemoryVaultSource implements VaultSource {
  readonly kind = "webdav" as const;

  private readonly files = new Map<string, string>();
  private readonly armed: ArmedFault[] = [];
  /**
   * Collections `ensureDir` was asked to create.
   *
   * A `Map` has no directories, so `writeText` has no parents to create and
   * `list()` cannot accidentally see a directory. The set exists so a test can
   * still assert that `ensureDir` was CALLED with the parent, which is the part
   * of the contract a real backend has to honour and a Map alone would hide.
   */
  readonly dirs = new Set<string>();

  constructor(seed: Iterable<VaultFile> = []) {
    for (const file of seed) this.files.set(normalise(file.path), file.content);
  }

  /**
   * Arm a fault. Faults are consulted in the order armed, so a later fault is
   * only reached when the earlier ones decline to fire.
   *
   * `ensureDir` consults the `write` table, because creating a collection is a
   * write to the server; a fault on `write` therefore also fires on `ensureDir`
   * for the same path.
   */
  inject(fault: MemoryFault): void {
    if (fault.status === undefined && fault.stored === undefined) {
      throw new TypeError(
        `A memory fault must say what happens: give a \`status\` to refuse with, or \`stored\` ` +
          `bytes to accept the write and keep. Got: ${JSON.stringify(fault)}`,
      );
    }
    this.armed.push({ fault, remaining: fault.times ?? Number.POSITIVE_INFINITY });
  }

  /** Disarm everything. Lets one test reuse a source without inheriting faults. */
  clearFaults(): void {
    this.armed.length = 0;
  }

  /**
   * Vault-relative POSIX paths of every indexable file, sorted.
   *
   * Sorted with the default comparator because that is what `FsVaultSource.list`
   * does, and `list()` order feeds `Vault`'s insertion order, which feeds
   * ambiguous-link resolution and every unsorted view. A different order here
   * would show up as a phantom backend bug.
   *
   * No snapshot cache, unlike the filesystem backend. The cache is invisible to
   * every assertion in the suite, and a fake that cached would let a backend get
   * away with forgetting to re-list after a write.
   */
  async list(): Promise<string[]> {
    this.fire({ op: "list", path: ANY_PATH });
    return [...this.files.keys()].filter(isIndexable).sort();
  }

  async readText(rel: string): Promise<string> {
    const path = this.resolve(rel);
    this.fire({ op: "read", path });
    return this.read(path, "GET");
  }

  /**
   * There is no cache here, so a fresh read is the same read.
   *
   * It still fires the `read` fault, because the faults stand for what the
   * SERVER does and a real backend bypasses its cache only to reach the server.
   */
  async readFresh(rel: string): Promise<string> {
    return this.readText(rel);
  }

  async exists(rel: string): Promise<boolean> {
    const path = this.resolve(rel);
    this.fire({ op: "stat", path });
    return this.files.has(path);
  }

  async stat(rel: string): Promise<FileStat> {
    const path = this.resolve(rel);
    this.fire({ op: "stat", path });
    return { size: byteLength(this.read(path, "PROPFIND")), mtime: memoryMtime() };
  }

  /**
   * The backend's own hash, taken over the text it read.
   *
   * `contentHash` is shared with the filesystem backend, so this cannot drift
   * from it: a change to the hash function changes both, and the equivalence
   * suite still passes.
   */
  async hash(rel: string): Promise<string> {
    const path = this.resolve(rel);
    this.fire({ op: "hash", path });
    return contentHash(this.read(path, "GET"));
  }

  /**
   * Store text at a vault-relative path, creating whatever collections that
   * implies and overwriting silently.
   *
   * Overwriting silently is what a WebDAV `PUT` does, and it is what
   * `FsVaultSource.writeText` does, so a client that relies on either behaves
   * the same over both.
   */
  async writeText(rel: string, data: string): Promise<void> {
    const path = this.resolve(rel);
    const swapped = this.fire({ op: "write", path });
    this.files.set(path, swapped ?? data);
  }

  /**
   * Record a collection.
   *
   * Creating a directory that already exists is not an error on either backend,
   * and `ensureDir` is not asked to report whether it did anything.
   */
  async ensureDir(rel: string): Promise<void> {
    const path = this.resolve(rel);
    this.fire({ op: "write", path });
    this.dirs.add(path);
  }

  /**
   * Remove a file.
   *
   * Deleting something absent is a NO-OP, which is `FsVaultSource`'s behaviour
   * and NOT WebDAV's: a real `DELETE` of a missing resource answers 404. The
   * fake follows the filesystem because the filesystem is the reference
   * implementation every backend is pinned to, and `VaultSource.delete` has no
   * caller in `src/` for the difference to bite yet. A real WebDAV backend that
   * copied this would silently skip a delete that never happened.
   */
  async delete(rel: string): Promise<void> {
    const path = this.resolve(rel);
    this.fire({ op: "delete", path });
    this.files.delete(path);
  }

  /** Normalise, refusing an escape exactly as `FsVaultSource.abs` does. */
  private resolve(rel: string): string {
    return normalise(rel);
  }

  /** The stored text, or the refusal a server would have given. */
  private read(path: string, verb: string): string {
    const text = this.files.get(path);
    if (text === undefined) {
      throw new MemoryVaultError(404, path, `${verb} ${path}: 404 Not Found`);
    }
    return text;
  }

  /**
   * Run the fault table for one operation.
   *
   * A fault carrying `stored` does not throw. It returns the replacement text, so
   * the write SUCCEEDS and the server keeps something else: the failure mode
   * read-verify-write exists to catch and the only one a client cannot detect
   * from the response alone. Handing the replacement back rather than applying it
   * here is what keeps the ordering honest -- applying it before the write would
   * be overwritten by the write and the fault would be a no-op that looked armed.
   *
   * Every other fault throws. Returning instead would let a caller carry on with
   * a value it never received.
   */
  private fire(at: { op: MemoryOp; path: string }): string | undefined {
    let replacement: string | undefined;
    for (const armed of this.armed) {
      const { fault } = armed;
      if (armed.remaining <= 0) continue;
      if (fault.op !== undefined && fault.op !== at.op) continue;
      if (fault.path !== ANY_PATH && normalise(fault.path) !== at.path) continue;

      armed.remaining -= 1;
      if (fault.stored !== undefined) {
        if (at.op === "write") replacement = fault.stored;
        continue;
      }
      if (fault.status === undefined) {
        // Unreachable: `inject` refuses a fault naming neither outcome. Named
        // rather than skipped, because a fault that silently declines to fire is
        // the exact shape of the bug this suite exists to prevent.
        throw new TypeError(`Memory fault on ${at.path} names no status: ${JSON.stringify(fault)}`);
      }
      throw new MemoryVaultError(
        fault.status,
        at.path,
        `${(fault.op ?? at.op).toUpperCase()} ${at.path}: ${fault.status} injected`,
      );
    }
    return replacement;
  }
}

/**
 * Bytes on the wire, not UTF-16 code units.
 *
 * `stat().size` is compared against the filesystem's `st_size` elsewhere, so it
 * has to be a byte count. `TextEncoder` rather than `Buffer` because that is
 * what the draft vault in `src/mcp/drafts.ts` already reaches for.
 */
function byteLength(text: string): number {
  return new TextEncoder().encode(text).length;
}

/**
 * Vault-relative POSIX normalisation, matching what `FsVaultSource.abs`
 * computes after `path.resolve`.
 *
 * The rules `path.resolve` applies, and why each one is here rather than
 * guessed: empty and `.` segments vanish, `..` pops, a `..` with nothing left
 * to pop escapes the root, and a leading `/` is an absolute path that escapes
 * it too. Every one of them decides whether a client gets a note or a refusal,
 * so getting one wrong makes the two backends disagree about paths that are
 * legal in both.
 */
function normalise(rel: string): string {
  if (rel.startsWith("/")) throw escapesRoot(rel);
  const out: string[] = [];
  for (const segment of rel.split("/")) {
    if (segment === "" || segment === ".") continue;
    if (segment === "..") {
      if (out.length === 0) throw escapesRoot(rel);
      out.pop();
      continue;
    }
    out.push(segment);
  }
  return out.join("/");
}

function escapesRoot(rel: string): MemoryVaultError {
  return new MemoryVaultError(403, rel, `Path escapes the vault root: ${rel}`);
}
