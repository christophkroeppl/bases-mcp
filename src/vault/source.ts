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
