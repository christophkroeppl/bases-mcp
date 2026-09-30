/**
 * Local filesystem vault source.
 *
 * Reads are served from a snapshot so that a single query sees a consistent
 * view: `file.backlinks` and `file.links` would otherwise observe the vault
 * mid-write. `refresh()` is explicit rather than watching, because the
 * filesystem backend is a development and test path and deterministic
 * behaviour matters more than freshness here.
 */

import { createHash } from "node:crypto";
import { promises as fs } from "node:fs";
import * as path from "node:path";

import { type FileStat, isIndexable, type VaultSource } from "./source";

export class FsVaultSource implements VaultSource {
  readonly kind = "fs" as const;
  private readonly root: string;
  private files: string[] | null = null;
  private readonly textCache = new Map<string, string>();
  private readonly hashCache = new Map<string, string>();

  constructor(root: string) {
    this.root = path.resolve(root);
  }

  /** Drop the snapshot so the next read observes the current vault. */
  async refresh(): Promise<void> {
    this.files = null;
    this.textCache.clear();
    this.hashCache.clear();
  }

  async list(): Promise<string[]> {
    if (this.files !== null) return this.files;
    const out: string[] = [];
    await this.walk("", out);
    out.sort();
    this.files = out;
    return out;
  }

  private async walk(rel: string, out: string[]): Promise<void> {
    const abs = rel === "" ? this.root : path.join(this.root, rel);
    let entries: import("node:fs").Dirent[];
    try {
      entries = await fs.readdir(abs, { withFileTypes: true });
    } catch {
      return;
    }
    for (const entry of entries) {
      const name: string = entry.name;
      const childRel = rel === "" ? name : `${rel}/${name}`;
      if (entry.isDirectory()) {
        if (name.startsWith(".")) continue;
        await this.walk(childRel, out);
      } else if (entry.isFile()) {
        if (isIndexable(childRel)) out.push(childRel);
      }
    }
  }

  async readText(rel: string): Promise<string> {
    const cached = this.textCache.get(rel);
    if (cached !== undefined) return cached;
    const text = await fs.readFile(this.abs(rel), "utf8");
    this.textCache.set(rel, text);
    return text;
  }

  async writeText(rel: string, data: string): Promise<void> {
    const abs = this.abs(rel);
    await fs.mkdir(path.dirname(abs), { recursive: true });
    await fs.writeFile(abs, data, "utf8");
    this.textCache.set(rel, data);
    this.hashCache.delete(rel);
    if (this.files !== null) this.files = null;
  }

  async stat(rel: string): Promise<FileStat> {
    const s = await fs.stat(this.abs(rel));
    return { size: s.size, mtime: s.mtime };
  }

  async hash(rel: string): Promise<string> {
    const cached = this.hashCache.get(rel);
    if (cached !== undefined) return cached;
    const text = await this.readText(rel);
    const h = contentHash(text);
    this.hashCache.set(rel, h);
    return h;
  }

  async ensureDir(rel: string): Promise<void> {
    await fs.mkdir(this.abs(rel), { recursive: true });
  }

  async delete(rel: string): Promise<void> {
    await fs.rm(this.abs(rel), { force: true });
    this.textCache.delete(rel);
    this.hashCache.delete(rel);
    if (this.files !== null) this.files = null;
  }

  /** Guard against a path escaping the vault root. */
  private abs(rel: string): string {
    const resolved = path.resolve(this.root, rel);
    if (resolved !== this.root && !resolved.startsWith(this.root + path.sep)) {
      throw new Error(`Path escapes the vault root: ${rel}`);
    }
    return resolved;
  }
}

/**
 * The content hash every backend must compute identically.
 *
 * Exported rather than left inside `hash()` because read-verify-write only ever
 * compares two hashes from ONE backend, so nothing upstream would notice a
 * second backend hashing differently -- until two clients edited the same note
 * through different backends and the conflict check silently stopped firing.
 * `test/webdav/equivalence.test.ts` compares hashes across backends, and that
 * comparison is only meaningful while there is one definition of the function.
 *
 * Truncated to 32 hex characters (128 bits): long enough that a collision is
 * not a thing a vault will ever meet, and short enough to read in a log line.
 * Deliberately NOT a server ETag, which a WebDAV server is free to invent.
 */
export function contentHash(text: string): string {
  return createHash("sha256").update(text).digest("hex").slice(0, 32);
}
