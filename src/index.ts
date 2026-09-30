/**
 * The stdio MCP server entrypoint.
 *
 * Everything here exists to protect one thing: stdout. On a stdio transport
 * stdout IS the protocol stream, so a single stray `console.log` anywhere in
 * the dependency chain -- ours, the SDK's, or a future tool's -- injects a
 * line of prose into JSON-RPC and the client drops the connection. The rule is
 * therefore absolute: startup diagnostics go to stderr, and nothing goes to
 * stdout except what the transport itself writes. Once `connect` is called this
 * file is silent forever.
 *
 * The other job is refusing to start against the wrong vault. This server is
 * one vault per process, configured by environment, and every path where that
 * configuration is absent, blank, incomplete or unimplemented is a hard failure
 * naming the variable to set. The tempting alternative -- defaulting
 * `BASES_MCP_VAULT` to `test/vault` or the home directory, or quietly falling
 * back to the filesystem when WebDAV is configured -- is worse than not
 * starting at all: a server answering confidently from the wrong vault is a
 * corruption no agent downstream can see. WebDAV is Phase 6 and does not exist
 * yet, so asking for it is an error rather than a reason to substitute
 * something else.
 *
 * `main` is exported and startup is guarded by `import.meta.main`, so importing
 * this module starts no server and holds no vault.
 */

import { stat } from "node:fs/promises";
import { resolve } from "node:path";

import { StdioServerTransport } from "@modelcontextprotocol/sdk/server/stdio.js";

import { createToolsServer } from "./mcp/tools";
import { Resolver } from "./service";
import { FsVaultSource } from "./vault/fs";
import { WebdavVaultSource } from "./vault/webdav";

/** Prefix on every line this process writes to stderr. */
const NAME = "bases-mcp";

/**
 * The environment this entrypoint reads.
 *
 * `process.env` is structurally this, and so is a plain object in a test, which
 * is what makes `main`'s configuration testable without spawning a process.
 */
export type Env = Record<string, string | undefined>;

/**
 * The vault this process will serve for its whole life.
 *
 * A union rather than one shape with optional fields, so an fs-only path cannot
 * be handed a WebDAV URL and read a wrong directory: the two have nothing in
 * common to default.
 */
export type VaultConfig =
  | {
      readonly kind: "fs";
      /** Absolute path to the vault directory. */
      readonly dir: string;
    }
  | {
      readonly kind: "webdav";
      /** Base URL of the WebDAV collection backing the vault. */
      readonly url: string;
      /** Basic-auth username. Never logged, never echoed in an error. */
      readonly user: string;
      /** Basic-auth password. Never logged, never echoed in an error. */
      readonly password: string;
    };

/**
 * A parsed environment, or the one thing wrong with it.
 *
 * The failure carries a finished, user-facing message rather than a field
 * describing what was missing, because there is exactly one consumer and it
 * only ever prints. Nothing downstream re-derives what went wrong.
 */
export type ConfigResult =
  | { readonly ok: true; readonly config: VaultConfig; readonly warnings: readonly string[] }
  | { readonly ok: false; readonly message: string };

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/** A variable with content in it: present, non-blank, trimmed. */
function filled(env: Env, name: string): string | undefined {
  const value = env[name];
  if (value === undefined) return undefined;
  const trimmed = value.trim();
  return trimmed === "" ? undefined : trimmed;
}

/**
 * Read the vault configuration out of the environment.
 *
 * `BASES_MCP_VAULT` and `BASES_MCP_WEBDAV_URL` are the two variables that
 * SELECT a backend, so a blank one is an error rather than an absence: the
 * intent is unmistakable and "no vault configured" would send whoever typo'd
 * it looking in the wrong place. A blank credential is just an absent
 * credential, and falls through to the same messages an unset one would.
 *
 * `BASES_MCP_VAULT` wins when both are configured, because it is the backend
 * that exists. That precedence is not silent -- the caller is handed a warning,
 * since a WebDAV URL that quietly does nothing is its own kind of surprise.
 */
export function parseConfig(env: Env): ConfigResult {
  for (const name of ["BASES_MCP_VAULT", "BASES_MCP_WEBDAV_URL"]) {
    if (env[name] !== undefined && filled(env, name) === undefined) {
      return { ok: false, message: `${name} is set but blank. Unset it, or give it a value.` };
    }
  }

  const dir = filled(env, "BASES_MCP_VAULT");

  if (dir !== undefined) {
    const ignored = webdavVariables(env);
    const warnings =
      ignored.length === 0
        ? []
        : [
            `${list(ignored)} ${plural(ignored, "is", "are")} also set and ignored: BASES_MCP_VAULT wins.`,
          ];
    return { ok: true, config: { kind: "fs", dir: resolve(dir) }, warnings };
  }

  const url = filled(env, "BASES_MCP_WEBDAV_URL");
  if (url !== undefined) {
    const user = filled(env, "BASES_MCP_WEBDAV_USER");
    const password = filled(env, "BASES_MCP_WEBDAV_PASSWORD");
    if (user === undefined || password === undefined) {
      const missing = [
        ...(user === undefined ? ["BASES_MCP_WEBDAV_USER"] : []),
        ...(password === undefined ? ["BASES_MCP_WEBDAV_PASSWORD"] : []),
      ];
      return {
        ok: false,
        message:
          `BASES_MCP_WEBDAV_URL is set but ${list(missing)} ${plural(missing, "is", "are")} not. ` +
          `A WebDAV vault needs both, because an unauthenticated request would read a 401 ` +
          `as an empty vault.`,
      };
    }
    const parsed = parseWebdavUrl(url);
    if (parsed === undefined) {
      return { ok: false, message: invalidWebdavUrl(url) };
    }
    return { ok: true, config: { kind: "webdav", url: parsed, user, password }, warnings: [] };
  }

  const orphans = webdavVariables(env);
  if (orphans.length > 0) {
    return {
      ok: false,
      message:
        `${list(orphans)} ${plural(orphans, "is", "are")} set without BASES_MCP_WEBDAV_URL. ` +
        `Set BASES_MCP_WEBDAV_URL to the base URL of the WebDAV collection backing the vault.`,
    };
  }

  return { ok: false, message: USAGE };
}

/** The WebDAV variables that carry a value, in the order the usage text lists them. */
function webdavVariables(env: Env): string[] {
  return ["BASES_MCP_WEBDAV_URL", "BASES_MCP_WEBDAV_USER", "BASES_MCP_WEBDAV_PASSWORD"].filter(
    (name) => filled(env, name) !== undefined,
  );
}

/** Join names the way a sentence does: "A", "A and B", "A, B and C". */
function list(names: readonly string[]): string {
  if (names.length <= 2) return names.join(" and ");
  return `${names.slice(0, -1).join(", ")} and ${names[names.length - 1]}`;
}

/** "is" for one name, "are" for more, so the sentence is never ungrammatical. */
function plural(names: readonly string[], one: string, many: string): string {
  return names.length === 1 ? one : many;
}

/**
 * What the WebDAV case has to say, whichever variable revealed it.
 *
 * The refusal to fall back is stated explicitly because the fallback is the
 * obvious "fix" and it is wrong: it would answer queries from a directory
 * nobody asked for, and the client would have no way to tell.
 */
function invalidWebdavUrl(url: string): string {
  // The URL is echoed with any userinfo REDACTED. A URL is attacker- or
  // operator-supplied, and the whole reason userinfo is refused is that it
  // would ride along in every string built from it -- which includes this
  // message, printed to stderr. Naming the rejected URL is useful; quoting its
  // password is not.
  return (
    `BASES_MCP_WEBDAV_URL is ${redactUrl(url)}, which is not a usable WebDAV base URL.\n` +
    `It must be an http(s) URL with no credentials in it: \`http://host:port/path/\`.\n` +
    `Credentials in the URL would end up in any message built from it, so put them in\n` +
    `BASES_MCP_WEBDAV_USER and BASES_MCP_WEBDAV_PASSWORD instead.`
  );
}

/** Replace any `user:password@` in a URL with `***:***@`. */
function redactUrl(url: string): string {
  return url.replace(/\/\/[^/@]*@/, "//***:***@");
}

/**
 * Normalise the WebDAV base URL, or refuse it.
 *
 * Returns undefined for anything we cannot serve from, and a href-safe form for
 * what we can: a trailing slash, because the backend appends collection names to
 * this and a missing slash would make `/vaultProjects` out of `/vault` + `Projects`.
 */
function parseWebdavUrl(url: string): string | undefined {
  let parsed: URL;
  try {
    parsed = new URL(url);
  } catch {
    return undefined;
  }
  if (parsed.protocol !== "http:" && parsed.protocol !== "https:") return undefined;
  // userinfo in the URL is refused, not honoured: it would ride along in every
  // string this file builds, including error messages.
  if (parsed.username !== "" || parsed.password !== "") return undefined;
  const path = parsed.pathname.endsWith("/") ? parsed.pathname : `${parsed.pathname}/`;
  return `${parsed.origin}${path}`;
}

const USAGE =
  `No vault configured. This server serves exactly one vault, and it must be told which.\n` +
  `\n` +
  `  BASES_MCP_VAULT             path to a vault directory on the local filesystem\n` +
  `\n` +
  `  BASES_MCP_WEBDAV_URL        WebDAV base URL, no credentials in it\n` +
  `  BASES_MCP_WEBDAV_USER       WebDAV username\n` +
  `  BASES_MCP_WEBDAV_PASSWORD   WebDAV password\n` +
  `\n` +
  `  Exactly one backend is used. BASES_MCP_VAULT wins if both are set.\n` +
  `\n` +
  `  WebDAV example:\n` +
  `  BASES_MCP_WEBDAV_URL=http://localhost:5000/vault BASES_MCP_WEBDAV_USER=u ` +
  `BASES_MCP_WEBDAV_PASSWORD=p ${NAME}\n` +
  `\n` +
  `Example:\n` +
  `  BASES_MCP_VAULT=/path/to/vault ${NAME}`;

// ---------------------------------------------------------------------------
// Startup
// ---------------------------------------------------------------------------

/**
 * Start the server on stdio, or exit non-zero explaining why not.
 *
 * Every failure below is fatal by design. A half-started MCP server is worse
 * than one that never ran: the client sees a process that accepted a
 * connection and then answered from nothing.
 */
export async function main(env: Env = process.env): Promise<void> {
  const parsed = parseConfig(env);
  if (!parsed.ok) fail(parsed.message);
  for (const warning of parsed.warnings) warn(warning);

  const resolver = await openVault(parsed.config);
  const server = createToolsServer(resolver);

  // Past this line, silence. The transport owns stdout from here on.
  await server.connect(new StdioServerTransport());
}

/**
 * Index the vault, or die naming the path that failed.
 *
 * The directory is checked before the resolver opens it because
 * `FsVaultSource` swallows a failed `readdir`: a mistyped `BASES_MCP_VAULT`
 * would otherwise index as a vault with no notes, and every base would come
 * back with zero rows. An empty vault is indistinguishable from a vault that
 * genuinely matches nothing, which is the one confusion this server exists to
 * avoid.
 */
async function openVault(config: VaultConfig): Promise<Resolver> {
  if (config.kind === "webdav") {
    try {
      return await Resolver.open(
        new WebdavVaultSource({ url: config.url, user: config.user, password: config.password }),
      );
    } catch (err) {
      // The URL is safe to name; the credentials are never in this string, and
      // the backend is built so they cannot be.
      fail(
        `cannot open WebDAV vault at ${config.url}: ${err instanceof Error ? err.message : String(err)}`,
      );
    }
  }
  try {
    const stats = await stat(config.dir).catch(() => undefined);
    if (stats === undefined) throw new Error("no such directory");
    if (!stats.isDirectory()) throw new Error("not a directory");
    return await Resolver.open(new FsVaultSource(config.dir));
  } catch (err) {
    fail(`cannot open vault at ${config.dir}: ${err instanceof Error ? err.message : String(err)}`);
  }
}

// ---------------------------------------------------------------------------
// Reporting
// ---------------------------------------------------------------------------

/** A diagnostic on stderr. Never stdout, at no point, for any reason. */
function warn(message: string): void {
  process.stderr.write(`${NAME}: ${message}\n`);
}

/** Report why we are not running, and stop. */
function fail(message: string): never {
  warn(message);
  process.exit(1);
}

if (import.meta.main) {
  await main();
}
