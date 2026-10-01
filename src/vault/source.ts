/**
 * The single I/O boundary for vault content.
 *
 * Everything above this interface is pure, so the filesystem and WebDAV
 * backends are interchangeable by construction. That equivalence is not
 * assumed: `test/webdav/` asserts the same vault resolves byte-identically
 * through both.
 */

export interface FileStat {
  size: number;
  mtime: Date;
}

export interface VaultSource {
  /** A stable identifier for logging and error messages. */
  readonly kind: "fs" | "webdav";

  /** Vault-relative POSIX paths of every note (`.md`) and base (`.base`). */
  list(): Promise<string[]>;

  readText(path: string): Promise<string>;

  /**
   * Read a note as it is RIGHT NOW, bypassing any cache.
   *
   * This is what the write path uses, and the distinction from `readText` is
   * the whole point: `readText` may answer from a snapshot taken earlier in the
   * process's life, which is right for queries and catastrophic for a write.
   * Reconciling an edit against a stale copy and then writing the result over
   * the top of whatever the user has since typed destroys their work while
   * reporting success.
   *
   * Obsidian autosaves continuously, so "the user edited this note in the last
   * thirty seconds" is the normal case rather than an edge case.
   */
  readFresh(path: string): Promise<string>;

  /**
   * Is there a file at this path, RIGHT NOW?
   *
   * `list()` is a snapshot, so it cannot answer this for a file created since
   * the snapshot was taken -- which is exactly the question
   * `add_note_to_base`'s commit has to ask before replacing a note verbatim.
   * Asking the index instead is a check that passes precisely when the note it
   * was supposed to protect was written behind the server's back.
   */
  exists(path: string): Promise<boolean>;

  writeText(path: string, data: string): Promise<void>;

  stat(path: string): Promise<FileStat>;

  /**
   * Content hash, used for read-verify-write.
   *
   * Deliberately NOT the server ETag: dufs does not emit `getetag` in its
   * WebDAV test suite, so a server ETag cannot be trusted for conflict
   * detection. Hashing what we ourselves read is both backend-agnostic and
   * sufficient to detect a concurrent write.
   */
  hash(path: string): Promise<string>;

  /** Create intermediate collections. A no-op on backends that need none. */
  ensureDir?(path: string): Promise<void>;

  delete?(path: string): Promise<void>;
}

/** Paths Obsidian considers notes. Bases are queried alongside them. */
export const NOTE_EXT = ".md";
export const BASE_EXT = ".base";

export function isNotePath(path: string): boolean {
  return path.endsWith(NOTE_EXT);
}

export function isBasePath(path: string): boolean {
  return path.endsWith(BASE_EXT);
}

/** Ignore `.obsidian/`, dotfiles and templates' scratch space. */
export function isIndexable(path: string): boolean {
  if (path.startsWith(".")) return false;
  const segments = path.split("/");
  if (segments.some((s) => s.startsWith("."))) return false;
  return isNotePath(path) || isBasePath(path);
}
