/**
 * Seed a local WebDAV server with the testing vault.
 *
 * Reads `BASES_MCP_WEBDAV_URL` (+ user/password) and PUTs every file from
 * `test/vault` into it, so the WebDAV backend can be exercised against the same
 * corpus the filesystem backend uses. Existing files are left alone unless
 * `--force` is passed, because seeding is not a sync and must not clobber a
 * real vault.
 *
 *   BASES_MCP_WEBDAV_URL=http://localhost:5000/vault \
 *   BASES_MCP_WEBDAV_USER=u BASES_MCP_WEBDAV_PASSWORD=p \
 *   bun run scripts/seed-webdav.ts
 */

import { readdir, readFile, stat } from "node:fs/promises";
import { dirname, join, posix, relative, sep } from "node:path";

const VAULT_DIR = join(import.meta.dir, "..", "test", "vault");
const TIMEOUT_MS = 30_000;

interface Credentials {
  url: string;
  user?: string;
  password?: string;
}

function readCredentials(): Credentials {
  const url = process.env["BASES_MCP_WEBDAV_URL"]?.trim();
  if (url === undefined || url === "") {
    die("BASES_MCP_WEBDAV_URL is required. See the usage in src/index.ts.");
  }
  const base = url.endsWith("/") ? url : `${url}/`;
  return {
    url: base,
    user: process.env["BASES_MCP_WEBDAV_USER"]?.trim(),
    password: process.env["BASES_MCP_WEBDAV_PASSWORD"],
  };
}

function die(message: string): never {
  process.stderr.write(`seed-webdav: ${message}\n`);
  process.exit(1);
}

function authHeader(creds: Credentials): Record<string, string> {
  if (creds.user === undefined || creds.password === undefined) return {};
  const encoded = Buffer.from(`${creds.user}:${creds.password}`).toString("base64");
  return { Authorization: `Basic ${encoded}` };
}

/** Every segment percent-encoded, so a `#` or a space in a name survives. */
function encodePath(vaultRelative: string): string {
  return vaultRelative
    .split("/")
    .map((segment) => encodeURIComponent(segment))
    .join("/");
}

/** The vault's files, as vault-relative POSIX paths. */
async function walk(dir: string): Promise<string[]> {
  const out: string[] = [];
  for (const entry of await readdir(dir, { withFileTypes: true })) {
    if (entry.isDirectory()) {
      // Obsidian's own state, like the backend's own walk, is not vault content.
      if (entry.name.startsWith(".")) continue;
      out.push(...(await walk(join(dir, entry.name))));
      continue;
    }
    out.push(relative(VAULT_DIR, join(dir, entry.name)).split(sep).join(posix.sep));
  }
  return out.sort();
}

async function ensureCollection(creds: Credentials, vaultRelative: string): Promise<void> {
  const segments = vaultRelative === "" ? [] : vaultRelative.split("/");
  let current = "";
  for (const segment of segments) {
    current = current === "" ? segment : `${current}/${segment}`;
    const url = `${creds.url}${encodePath(current)}`;
    const res = await fetch(url, { method: "MKCOL", headers: authHeader(creds) });
    // 201 created, 405 already there. Anything else is a real failure.
    if (res.status === 201 || res.status === 405) continue;
    throw new Error(`MKCOL ${current} got ${res.status} ${res.statusText}`);
  }
}

async function alreadyThere(creds: Credentials, vaultRelative: string): Promise<boolean> {
  const res = await fetch(`${creds.url}${encodePath(vaultRelative)}`, {
    method: "HEAD",
    headers: authHeader(creds),
    signal: AbortSignal.timeout(TIMEOUT_MS),
  });
  if (res.status === 200) return true;
  if (res.status === 404) return false;
  throw new Error(`HEAD ${vaultRelative} got ${res.status} ${res.statusText}`);
}

async function put(creds: Credentials, vaultRelative: string, body: Uint8Array): Promise<void> {
  const parent = dirname(vaultRelative).split(sep).join(posix.sep);
  if (parent !== ".") await ensureCollection(creds, parent);

  const res = await fetch(`${creds.url}${encodePath(vaultRelative)}`, {
    method: "PUT",
    headers: { ...authHeader(creds), "Content-Type": "application/octet-stream" },
    body,
    signal: AbortSignal.timeout(TIMEOUT_MS),
  });
  if (res.status === 201 || res.status === 204 || res.status === 200) return;
  throw new Error(`PUT ${vaultRelative} got ${res.status} ${res.statusText}`);
}

async function main(): Promise<void> {
  const creds = readCredentials();
  const force = process.argv.includes("--force");
  const dryRun = process.argv.includes("--dry-run");

  const files = await walk(VAULT_DIR);
  if (files.length === 0) die(`${VAULT_DIR} contains no files to seed.`);
  process.stderr.write(`seed-webdav: ${files.length} files from ${VAULT_DIR}\n`);

  // A dry run is a plan, and a plan should not need the server to exist --
  // otherwise `--dry-run` cannot be used to check what would be sent before
  // committing to it.
  if (dryRun) {
    for (const file of files) process.stderr.write(`  would PUT ${file}\n`);
    process.stderr.write(`seed-webdav: ${files.length} would be written, nothing sent\n`);
    return;
  }

  let written = 0;
  let skipped = 0;
  for (const file of files) {
    if (!force && (await alreadyThere(creds, file))) {
      skipped++;
      process.stderr.write(`  skip   ${file} (present; --force to overwrite)\n`);
      continue;
    }
    const body = await readFile(join(VAULT_DIR, file));
    // Belt and braces: a zero-byte file is a legitimate vault file, but stat it
    // so an unreadable one fails here rather than as an empty PUT.
    const info = await stat(join(VAULT_DIR, file));
    if (info.size !== body.byteLength)
      die(`${file}: read ${body.byteLength} of ${info.size} bytes.`);
    await put(creds, file, body);
    written++;
    process.stderr.write(`  PUT    ${file}\n`);
  }

  process.stderr.write(`seed-webdav: ${written} written, ${skipped} skipped\n`);
}

await main();
