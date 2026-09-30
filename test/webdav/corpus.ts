/**
 * The corpus: the testing vault, as `{ path, content }`.
 *
 * One definition, shared by every WebDAV-related suite. Two suites that each
 * globbed `test/vault` would agree today and diverge the first time someone adds
 * a file, and the symptom would be an equivalence failure pointing at a backend
 * that had not changed.
 *
 * The enumeration is delegated to `FsVaultSource.list()` rather than repeated
 * here. That is the whole point: the corpus is then exactly the set of files the
 * filesystem backend is willing to show an agent, so the fake can never be
 * seeded with something no backend could have produced. A hand-rolled recursive
 * read would be a second definition of the same rule, and would eventually
 * disagree with the one `Vault` actually consumes.
 *
 * Read-only by construction. Nothing here writes, and `test/vault` must stay at
 * exactly `CORPUS_SIZE` files: it is the parity oracle, and a file added to it
 * silently changes what `obsidian base:query` returns.
 */

import { promises as fs } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

import { FsVaultSource } from "../../src/vault/fs";
import type { VaultFile } from "./memory";

const HERE = dirname(fileURLToPath(import.meta.url));

/** The testing vault on disk. */
export const VAULT_DIR = join(HERE, "..", "vault");

/**
 * How many files the testing vault holds, `.obsidian` aside.
 *
 * Pinned as a number rather than re-derived, because a corpus that has quietly
 * grown means either an unrecorded change to the oracle or a test that stopped
 * covering what it claims to. Both are worth failing over.
 */
export const CORPUS_SIZE = 9;

/**
 * Read the testing vault into memory.
 *
 * `dir` is a parameter only so a test can point the loader at a tree it built
 * itself; nothing in the suite needs that today, and a defaulted argument is
 * cheaper than an edit when one does.
 */
export async function loadCorpus(dir: string = VAULT_DIR): Promise<VaultFile[]> {
  const source = new FsVaultSource(dir);
  const files: VaultFile[] = [];
  for (const path of await source.list()) {
    files.push({ path, content: await source.readText(path) });
  }
  return files;
}

/** Every `.md` and `.base` in the testing vault, by path. */
export async function corpusPaths(): Promise<string[]> {
  return (await loadCorpus()).map((f) => f.path);
}

/**
 * Count the files in a directory tree, ignoring dot-directories.
 *
 * Used only to prove the oracle's size has not changed. It reads the tree
 * itself rather than reusing the loader, because the loader's answer is the
 * thing under test: asking it how many files there are would be circular.
 */
export async function countFiles(dir: string, rel = ""): Promise<number> {
  const entries = await fs.readdir(rel === "" ? dir : join(dir, rel), { withFileTypes: true });
  let total = 0;
  for (const entry of entries) {
    if (entry.name.startsWith(".")) continue;
    const child = rel === "" ? entry.name : `${rel}/${entry.name}`;
    if (entry.isDirectory()) total += await countFiles(dir, child);
    else if (entry.isFile()) total += 1;
  }
  return total;
}
