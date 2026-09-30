/**
 * WebDAV vault source: the second implementation of `VaultSource`.
 *
 * Three constraints from the project plan shape everything below, and each is a
 * constraint rather than a preference:
 *
 *   - RECURSION IS `Depth: 1`, ALWAYS. `Depth: infinity` is what a WebDAV client
 *     would naturally send and what no mainstream server accepts, so `list()`
 *     PROPFINDs one collection at a time and recurses into each child collection
 *     itself. The cost is one round trip per directory; the alternative is a
 *     backend that works against no server at all.
 *   - WRITES ARE VERIFIED WITH OUR OWN HASH, NOT AN ETAG. dufs does not emit
 *     `getetag` in its WebDAV test suite, so a server ETag is not merely
 *     untrustworthy here, it is absent. After every `PUT` this backend re-`GET`s
 *     the resource and compares `contentHash`, the one hash both backends share.
 *     A `PUT` that succeeds while storing something else is the one failure a
 *     client cannot detect from the response, and that read-back is what catches
 *     it.
 *   - A CREDENTIAL NEVER APPEARS IN A STRING. No log line, no error message, no
 *     URL. A URL carrying userinfo is refused outright, so there is exactly one
 *     place a credential can live and nothing this file builds can echo it.
 *
 * Snapshot semantics match `FsVaultSource` exactly -- `list()` is a snapshot until
 * `refresh()` or a write, and text and hashes are cached beneath it -- because the
 * two backends are meant to be interchangeable and the equivalence suite treats a
 * difference as a finding. Exactly two behaviours deliberately differ, both
 * commented where they happen: a refused `PROPFIND` is an error here rather than a
 * silently smaller vault (`childrenOf`), and a `delete` of something absent is
 * tolerated here rather than reported as a success it was not (`delete`).
 */

import { XMLParser } from "fast-xml-parser";

import { BasesError } from "../expr/errors";
import { contentHash } from "./fs";
import { type FileStat, isIndexable, type VaultSource } from "./source";

/**
 * The `VaultSource` method a request was issued for.
 *
 * `hash` is absent because it issues no request of its own: it hashes the text a
 * `GET` returned for `read`, so a hash and a read of the same path are one
 * round trip rather than two.
 */
export type DavOperation = "list" | "read" | "stat" | "write" | "ensureDir" | "delete";

/** The HTTP methods a vault needs, WebDAV's own two included. */
export type WebdavMethod = "PROPFIND" | "GET" | "PUT" | "MKCOL" | "DELETE";

/** `Depth` values this client is allowed to send. Never `infinity`. */
export type DavDepth = "0" | "1";

/**
 * The HTTP call this backend makes, and the only one it may depend on.
 *
 * Deliberately narrower than `typeof fetch`: Bun's `fetch` carries a static
 * `preconnect`, so a type of `typeof fetch` would make every test double in the
 * suite fake a static method to satisfy a signature nothing here ever calls. The
 * shape is also the whole vocabulary of the backend -- a method, headers, an
 * optional string body and an abort signal -- so a change to it is a change to what
 * this client is able to do.
 */
export type WebdavFetch = (
  input: string,
  init: {
    readonly method: string;
    readonly headers: Record<string, string>;
    readonly body?: string;
    readonly signal?: AbortSignal;
  },
) => Promise<Response>;

/** How long one request may take before it is abandoned, in milliseconds. */
export const DEFAULT_TIMEOUT_MS = 30_000;

/** The media type a `PROPFIND` request body is sent as. */
const XML_CONTENT_TYPE = 'application/xml; charset="utf-8"';

/** The media type a note is written as. */
const TEXT_CONTENT_TYPE = "text/plain; charset=utf-8";

/**
 * The properties a `PROPFIND` asks for.
 *
 * Asked for by name rather than with `<allprop/>` so the request states exactly
 * what the backend will read, and so a `propstat` carrying a `404` for a property
 * the server declines to invent is visible instead of quietly absent. None of the
 * three needs `getetag`, which is precisely why nothing here reads one.
 */
export const DAV_PROPFIND_BODY =
  `<?xml version="1.0" encoding="utf-8"?>` +
  `<d:propfind xmlns:d="DAV:"><d:prop>` +
  `<d:resourcetype/><d:getcontentlength/><d:getlastmodified/>` +
  `</d:prop></d:propfind>`;

// ---------------------------------------------------------------------------
// Refusals
// ---------------------------------------------------------------------------

/**
 * Statuses an operation treats as success anyway.
 *
 * Every entry is a status RFC 4918 requires, not one found inconvenient: `MKCOL`
 * answers `405` for a collection that is already there, and `DELETE` answers `404`
 * for a resource that is not. Both are the server confirming that the state the
 * caller was trying to reach already holds, which is the whole intent of the call.
 * `list`, `read`, `stat` and `write` tolerate nothing, so a `PROPFIND` that is
 * refused is a refusal rather than an empty directory.
 */
const TOLERATED: Readonly<Record<DavOperation, readonly number[]>> = {
  list: [],
  read: [],
  stat: [],
  write: [],
  ensureDir: [405],
  delete: [404],
};

/**
 * A refusal from the WebDAV server, carrying its status structurally.
 *
 * A `BasesError`, so the MCP layer reports it as a structured tool failure naming
 * the operation rather than as an internal bug in this server. The status is a
 * field and not only prose because the two reactions differ: a `503` is worth a
 * retry and a `404` is not, and a caller forced to scrape strings gets one of
 * those right by luck.
 *
 * Carries the vault-relative path rather than a URL. The configured base URL may
 * be reachable from outside, may be a hostname worth not broadcasting, and is in
 * any case not what the caller asked for.
 */
export class WebdavError extends BasesError {
  readonly status: number;
  readonly operation: DavOperation;
  readonly method: WebdavMethod;
  readonly path: string;

  constructor(
    operation: DavOperation,
    method: WebdavMethod,
    path: string,
    status: number,
    statusText: string,
  ) {
    super(`WebDAV ${operation}: ${method} "${atRoot(path)}": ${status} ${statusText}`.trimEnd(), {
      construct: "webdav",
    });
    this.name = "WebdavError";
    this.status = status;
    this.operation = operation;
    this.method = method;
    this.path = path;
  }
}

/**
 * The refusal a status implies, or `undefined` when the call did what it asked.
 *
 * `undefined` rather than a boolean so the call site reads as what it is -- throw
 * this or do not -- and so a tolerated status cannot be confused with a successful
 * one by a caller that only wants to know whether to continue. Any `2xx` passes,
 * including the `207 Multi-Status` a `PROPFIND` answers with, and the tolerated
 * statuses for the operation pass. Everything else refuses, `3xx` included: a
 * redirect this client did not follow is a server pointing somewhere else, and
 * reading a vault from there without being told would be worse than refusing.
 */
export function davRefusal(
  operation: DavOperation,
  method: WebdavMethod,
  path: string,
  status: number,
  statusText: string,
): WebdavError | undefined {
  if (status >= 200 && status < 300) return undefined;
  if (TOLERATED[operation].includes(status)) return undefined;
  return new WebdavError(operation, method, path, status, statusText);
}

// ---------------------------------------------------------------------------
// Multistatus
// ---------------------------------------------------------------------------

/**
 * One resource as a `207 Multi-Status` body describes it.
 *
 * The properties are optional because the server decides that: `propstat` carries
 * a status per group, and a server may decline to report `getcontentlength` for a
 * resource it has no length for. The mapping is total for that reason -- it
 * extracts what the XML says and passes no judgement -- while `statOf` is where a
 * missing property becomes a refusal.
 */
export interface DavResource {
  /** Vault-relative POSIX path. `""` is the vault root. */
  readonly path: string;
  readonly isCollection: boolean;
  readonly size: number | undefined;
  /** The `getlastmodified` string verbatim, never parsed here. */
  readonly lastModified: string | undefined;
}

/**
 * Parser for `DAV:` documents.
 *
 * Module-level because it holds no per-document state, and configured to strip
 * namespace prefixes so `D:href`, `d:href` and an unprefixed `href` all arrive as
 * `href` -- servers differ on the prefix and none of them differ on the meaning.
 * Values stay strings (`parseTagValue: false`) so `getcontentlength` cannot become
 * a rounded number and `getlastmodified` cannot become a `Date` the server never
 * sent.
 */
const parser = new XMLParser({
  ignoreAttributes: false,
  removeNSPrefix: true,
  parseTagValue: false,
  trimValues: true,
});

/**
 * Every resource in a `207 Multi-Status` body, in the order the server listed them.
 *
 * `basePath` is the server-side path the configured URL points at, e.g.
 * `/dav/vault` for `https://host/dav/vault/`. It is stripped from every href,
 * because an href is server-root-absolute and a vault-relative path is not: the
 * same note is `/dav/vault/Root Ticket.md` on the wire and `Root Ticket.md` in
 * every `VaultSource` contract in this project.
 *
 * A href outside `basePath` throws rather than resolving to something. It means the
 * server is rooted somewhere other than where it was addressed, and quietly
 * indexing that tree would answer queries from a vault nobody named.
 */
export function parseMultistatus(xml: string, basePath: string): DavResource[] {
  const document = asRecord(parser.parse(xml));
  const raw = document?.["multistatus"];

  if (raw === undefined) {
    throw new BasesError(
      `WebDAV PROPFIND did not return a multistatus document. The server answered with ` +
        `something else, which means it is not serving WebDAV at this URL.`,
      { construct: "webdav" },
    );
  }
  if (typeof raw === "string") {
    // `<multistatus/>` with no members parses to an empty string, and an empty
    // collection is a legitimate answer rather than a malformed document.
    if (raw.trim() === "") return [];
    throw new BasesError(
      `WebDAV PROPFIND returned a multistatus document that is not a collection listing.`,
      { construct: "webdav" },
    );
  }

  const multistatus = asRecord(raw);
  if (multistatus === undefined) {
    throw new BasesError(`WebDAV PROPFIND returned a multistatus document of unreadable shape.`, {
      construct: "webdav",
    });
  }

  const out: DavResource[] = [];
  for (const entry of asArray(multistatus["response"])) {
    const response = asRecord(entry);
    if (response === undefined) continue;
    const href = asString(response["href"]);
    if (href === undefined) continue;
    // Skipped when every propstat is a non-2xx, which is how a server says this
    // resource is gone: a listing can name a file deleted between the request and
    // the response, and indexing it would put a path in `list()` that a later
    // `readText` cannot serve. A href outside the base does NOT land here -- that
    // throws, in `vaultPathFromHref`.
    const properties = foundProperties(response);
    if (properties === undefined) continue;
    out.push({
      path: vaultPathFromHref(href, basePath),
      isCollection: isCollection(properties["resourcetype"]),
      size: contentLength(properties["getcontentlength"]),
      lastModified: trimmed(properties["getlastmodified"]),
    });
  }
  return out;
}

/**
 * The properties the server actually has for one resource.
 *
 * A `response` may carry several `propstat` blocks, each with its own status: one
 * for the properties that exist and one for those that do not. Only the `2xx` ones
 * describe the resource, and merging them is the point -- reading only the first
 * would drop the size whenever a server puts `getetag` in a second block, and
 * reading all of them would let a `404` blank a value the server did report.
 */
function foundProperties(response: Record<string, unknown>): Record<string, unknown> | undefined {
  const merged: Record<string, unknown> = {};
  let sawSuccess = false;
  for (const block of asArray(response["propstat"])) {
    const propstat = asRecord(block);
    if (propstat === undefined) continue;
    if (!isSuccess(asString(propstat["status"]))) continue;
    sawSuccess = true;
    const properties = asRecord(propstat["prop"]);
    if (properties === undefined) continue;
    for (const [name, value] of Object.entries(properties)) merged[name] = value;
  }
  return sawSuccess ? merged : undefined;
}

/** `"HTTP/1.1 200 OK"` is a success; a missing or unparsable status line is not. */
function isSuccess(status: string | undefined): boolean {
  const code = status?.match(/\s(\d{3})(?:\s|$)/)?.[1];
  return code !== undefined && Number(code) >= 200 && Number(code) < 300;
}

/**
 * Whether a `resourcetype` element names a collection.
 *
 * A collection is `<d:resourcetype><d:collection/></d:resourcetype>` and a file is
 * an empty one, so the answer is the presence of the child element rather than any
 * value inside it. A server that writes `<d:collection></d:collection>` parses
 * identically.
 */
function isCollection(resourcetype: unknown): boolean {
  const element = asRecord(resourcetype);
  return element !== undefined && "collection" in element;
}

/** A `getcontentlength` as a byte count, or `undefined` if it is not one. */
function contentLength(value: unknown): number | undefined {
  const raw = trimmed(value);
  if (raw === undefined || raw === "") return undefined;
  const size = Number(raw);
  return Number.isFinite(size) ? size : undefined;
}

// ---------------------------------------------------------------------------
// Paths
// ---------------------------------------------------------------------------

/**
 * A vault-relative POSIX path as it appears in an error message.
 *
 * The vault root is `""` everywhere in this project, because that is what
 * `FsVaultSource.walk` and `MemoryVaultSource` both call it, and an empty string in
 * prose reads as a missing value rather than as `/`.
 */
function atRoot(path: string): string {
  return path === "" ? "/" : path;
}

/**
 * The vault-relative path a `DAV:href` names.
 *
 * Hrefs are URL-encoded, percent-escaped and server-root-absolute, and every path
 * in this project is decoded, literal and vault-relative, so the mapping is three
 * steps in this order and the order is the whole trick:
 *
 *   1. Reduce to a pathname, so an href sent as a full URL and one sent as a path
 *      -- both allowed by RFC 4918, and servers do both -- differ only before the
 *      first segment.
 *   2. Split on `/` and decode each segment SEPARATELY, rather than decoding the
 *      whole path and then splitting. A note called `Root Project.md` arrives as
 *      `Root%20Project.md` and must come back with its space, while a note called
 *      `50%.md` arrives as `50%25.md` and must not be mangled by a decode that ran
 *      over the separators too.
 *   3. Strip `basePath`, compared segment by segment for the same reason. Only then
 *      is what left a vault-relative path, and a trailing slash -- which every
 *      collection href carries and no vault-relative path ever does -- is already
 *      gone with the empty final segment.
 */
export function vaultPathFromHref(href: string, basePath: string): string {
  const segments = decodeSegments(hrefPathname(href), href);
  const base = decodeSegments(basePath, basePath);
  if (!startsWith(segments, base)) {
    throw new BasesError(
      `WebDAV href is outside the configured vault base "${basePath || "/"}": ` +
        `the server rooted its listing somewhere else, so its files cannot be mapped ` +
        `to vault-relative paths.`,
      { construct: "webdav" },
    );
  }
  return foldSegments(segments.slice(base.length), "WebDAV href", href).join("/");
}

/**
 * A vault-relative path from whatever the caller passed.
 *
 * Folds `.` and `..` exactly as `path.resolve` does, so a path that is legal for
 * `FsVaultSource` is legal here and one that is not is refused here too. A leading
 * `/` is a refusal rather than something to strip: `/etc/passwd` is not a note in
 * this vault, it is a different file, and reading it because the leading slash
 * could be dropped would be the worst bug this backend could have.
 */
export function vaultRelativePath(rel: string): string {
  if (rel.startsWith("/")) throw escapesVault("Path", rel);
  return foldSegments(rel.split("/"), "Path", rel).join("/");
}

/**
 * Resolve a vault-relative path against the configured base and encode the result.
 *
 * Each segment is encoded on its own so a note called `Q1 #2.md` produces a URL
 * whose `#` cannot be read as a fragment, and whose space is `%20` rather than the
 * server having to tolerate a literal one.
 *
 * A collection keeps its trailing slash, because that is how WebDAV says a URL
 * names one. A server that receives `/vault/Projects` may answer 404, because as far
 * as it is concerned that names a resource called `Projects`; only `/vault/Projects/`
 * is the collection. The vault-relative path has no trailing slash, so the slash has
 * to be put back here, and only here, where the caller has said which kind of
 * resource it is asking about.
 */
function resourceUrl(baseUrl: string, path: string, collection: boolean): string {
  if (path === "") return `${baseUrl}/`;
  const encoded = path.split("/").map(encodeURIComponent).join("/");
  return collection ? `${baseUrl}/${encoded}/` : `${baseUrl}/${encoded}`;
}

/** The pathname of an href that may or may not be a full URL. */
function hrefPathname(href: string): string {
  try {
    return new URL(href).pathname;
  } catch {
    // A path with no scheme makes `URL` throw, which is the common case.
    return href;
  }
}

/** Path segments, percent-decoded one at a time, with no empty or `.` segments. */
function decodeSegments(pathname: string, origin: string): string[] {
  const out: string[] = [];
  for (const segment of pathname.split("/")) {
    if (segment === "") continue;
    try {
      out.push(decodeURIComponent(segment));
    } catch {
      throw new BasesError(
        `WebDAV path is not valid percent-encoding, so it cannot be read as a path: ${origin}`,
        { construct: "webdav" },
      );
    }
  }
  return out;
}

/** Whether `segments` begins with `prefix`, compared element by element. */
function startsWith(segments: readonly string[], prefix: readonly string[]): boolean {
  if (prefix.length > segments.length) return false;
  return prefix.every((segment, i) => segments[i] === segment);
}

/**
 * Fold `.` and `..` out of path segments.
 *
 * `what` names the origin in the refusal, because the two callers are different
 * trust boundaries -- a path an agent supplied and a path a server supplied -- and
 * a message that cannot say which one was wrong is a message that gets guessed at.
 */
function foldSegments(segments: readonly string[], what: string, origin: string): string[] {
  const out: string[] = [];
  for (const segment of segments) {
    if (segment === "" || segment === ".") continue;
    if (segment !== "..") {
      out.push(segment);
      continue;
    }
    if (out.length === 0) throw escapesVault(what, origin);
    out.pop();
  }
  return out;
}

/**
 * A path that must name a file.
 *
 * `PUT` and `DELETE` with no name address the collection itself, which is a request
 * with no honest reading: the server either refuses it or, worse, treats the
 * collection as a resource. The filesystem backend fails on the same call with a
 * raw `EISDIR`, so this makes the refusal say what happened.
 */
function namedFile(path: string): string {
  if (path !== "") return path;
  throw new BasesError(
    `WebDAV: the vault root is a collection, not a file, so it cannot be written or deleted.`,
    {
      construct: "webdav",
    },
  );
}

function escapesVault(what: string, origin: string): BasesError {
  return new BasesError(`${what} escapes the vault root: ${origin}`, { construct: "webdav" });
}

// ---------------------------------------------------------------------------
// Narrowing
// ---------------------------------------------------------------------------

/**
 * The parsed XML as a plain object, or `undefined` if it is not one.
 *
 * `XMLParser.parse` is typed `any` because it has to be, and a WebDAV document is
 * untrusted input. Every narrowing below is therefore a refusal rather than a
 * coercion: a server that answers PROPFIND with something unrecognisable should
 * fail here, loudly, instead of being read as a vault with no notes -- which is
 * indistinguishable from a vault that genuinely matches nothing.
 */
function asRecord(value: unknown): Record<string, unknown> | undefined {
  if (typeof value !== "object" || value === null || Array.isArray(value)) return undefined;
  return value as Record<string, unknown>;
}

/** A value as a list, wrapping the single case the parser collapses it to. */
function asArray(value: unknown): readonly unknown[] {
  if (Array.isArray(value)) return value;
  return value === undefined ? [] : [value];
}

function asString(value: unknown): string | undefined {
  return typeof value === "string" ? value : undefined;
}

function trimmed(value: unknown): string | undefined {
  const text = asString(value)?.trim();
  return text === undefined || text === "" ? undefined : text;
}

// ---------------------------------------------------------------------------
// The source
// ---------------------------------------------------------------------------

export interface WebdavVaultOptions {
  /** The WebDAV collection that IS the vault, e.g. `https://host/dav/vault/`. */
  readonly url: string;
  readonly user?: string;
  readonly password?: string;
  /** Per-request budget in milliseconds. Defaults to {@link DEFAULT_TIMEOUT_MS}. */
  readonly timeoutMs?: number;
  /**
   * The HTTP seam, defaulting to the global `fetch`.
   *
   * Present because the two hard constraints of this backend are otherwise
   * untestable without a server: that recursion sends `Depth: 1` and never
   * `infinity`, and that it reads a listing rather than a file per note. Both are
   * claims about the requests this class makes, so the test asserts on the
   * requests.
   */
  readonly fetch?: WebdavFetch;
}

export class WebdavVaultSource implements VaultSource {
  readonly kind = "webdav" as const;

  /** The base URL with no trailing slash, so paths can be appended verbatim. */
  private readonly baseUrl: string;
  /** The same base as a server-side path, for stripping from hrefs. */
  private readonly basePath: string;
  private readonly authorization: string | undefined;
  private readonly timeoutMs: number;
  private readonly transport: WebdavFetch;

  private files: string[] | null = null;
  private readonly textCache = new Map<string, string>();
  private readonly hashCache = new Map<string, string>();

  constructor(options: WebdavVaultOptions) {
    const url = new URL(options.url);
    if (url.protocol !== "http:" && url.protocol !== "https:") {
      throw new BasesError(
        `WebDAV vault URL must be http or https, not "${url.protocol}". ` +
          `Set BASES_MCP_WEBDAV_URL to the WebDAV collection that is the vault.`,
        { construct: "webdav" },
      );
    }
    if (url.username !== "" || url.password !== "") {
      // Refused rather than honoured. Accepting them would put a password in
      // `url.toString()`, and this file builds error messages from strings; keeping
      // the credential in exactly one place is what makes "never log it" a property
      // of the code rather than of everyone's care.
      throw new BasesError(
        `WebDAV vault URL carries credentials, which this client will not embed in a URL. ` +
          `Set BASES_MCP_WEBDAV_USER and BASES_MCP_WEBDAV_PASSWORD instead.`,
        { construct: "webdav" },
      );
    }
    if ((options.user === undefined) !== (options.password === undefined)) {
      throw new BasesError(
        `WebDAV needs both BASES_MCP_WEBDAV_USER and BASES_MCP_WEBDAV_PASSWORD, or neither. ` +
          `One without the other is a misconfiguration, not anonymous access.`,
        { construct: "webdav" },
      );
    }

    this.basePath = url.pathname;
    this.baseUrl = `${url.origin}${url.pathname.replace(/\/+$/, "")}`;
    this.authorization =
      options.user === undefined
        ? undefined
        : `Basic ${Buffer.from(`${options.user}:${options.password ?? ""}`, "utf8").toString("base64")}`;
    this.timeoutMs = options.timeoutMs ?? DEFAULT_TIMEOUT_MS;
    this.transport = options.fetch ?? globalThis.fetch;
  }

  /**
   * Drop the snapshot so the next read observes the current vault.
   *
   * The same escape hatch `FsVaultSource` offers, for the same reason: this server
   * holds one vault for its whole life, and re-reading it is an explicit decision
   * rather than a side effect of asking.
   */
  async refresh(): Promise<void> {
    this.files = null;
    this.textCache.clear();
    this.hashCache.clear();
  }

  async list(): Promise<string[]> {
    if (this.files !== null) return this.files;
    const out: string[] = [];
    await this.walk("", out, []);
    out.sort();
    this.files = out;
    return out;
  }

  /**
   * Depth-first, one `PROPFIND` per collection.
   *
   * Depth-first because a `PROPFIND` at `Depth: 1` answers with a collection's own
   * members and nothing below them, so a child collection is only reachable by
   * asking about it. `isIndexable` is applied to files and never to collections,
   * exactly as `FsVaultSource.walk` does: it is the definition of what a note is,
   * and a second rule for the same question is a second rule to disagree.
   *
   * `ancestors` exists because the alternative to noticing a cycle is walking it.
   * `childrenOf` already drops a listing's self-entry, so a loop needs a server
   * that names a collection as its own descendant -- and one that does would send
   * this process round the same URL until it died, with a tool call hanging and
   * nothing to report. Refusing the cycle costs one membership test and turns the
   * worst outcome into a message naming the path.
   */
  private async walk(rel: string, out: string[], ancestors: readonly string[]): Promise<void> {
    for (const resource of await this.childrenOf(rel)) {
      if (resource.isCollection) {
        if (isHiddenCollection(resource.path)) continue;
        if (ancestors.includes(resource.path)) throw cyclic(resource.path);
        await this.walk(resource.path, out, [...ancestors, resource.path]);
        continue;
      }
      if (isIndexable(resource.path)) out.push(resource.path);
    }
  }

  /**
   * The members of one collection, excluding the collection itself.
   *
   * The self-entry is dropped because a `Depth: 1` listing includes the collection
   * that was asked about, and recursing into it would walk the vault forever.
   *
   * A REFUSED `PROPFIND` THROWS here, and that is the one place this backend
   * deliberately disagrees with `FsVaultSource`, which swallows a failed `readdir`
   * and indexes whatever else it could reach. fs has a reason -- it is a
   * development and test path, and `src/index.ts` compensates by stat-ing the
   * directory before opening it -- but over HTTP there is no such check, and the
   * result of swallowing is the failure this server exists to avoid: a vault that
   * answers every query with zero rows and no reason. Surfacing the `403` is more
   * correct than a smaller vault, and the equivalence suite reports the difference
   * rather than hiding it.
   */
  private async childrenOf(rel: string): Promise<DavResource[]> {
    const response = await this.send("list", "PROPFIND", rel, {
      body: DAV_PROPFIND_BODY,
      depth: "1",
      collection: true,
    });
    const resources = parseMultistatus(await response.text(), this.basePath);
    return resources.filter((resource) => resource.path !== rel);
  }

  async readText(rel: string): Promise<string> {
    const path = vaultRelativePath(rel);
    const cached = this.textCache.get(path);
    if (cached !== undefined) return cached;
    const text = await this.fetchText("read", path);
    this.textCache.set(path, text);
    return text;
  }

  /**
   * Size and mtime as the SERVER reports them.
   *
   * `getlastmodified` is passed through untouched. It is the only mtime that
   * exists over WebDAV, and it is a real divergence: the filesystem reports the
   * local clock at the filesystem's resolution, a server reports its own clock at
   * its own resolution and in its own timezone. Massaging it towards the local
   * clock would make the two backends agree on a value that never existed, so the
   * instant is reported as sent and the equivalence suite avoids `file.mtime` and
   * `file.ctime` because of it.
   */
  async stat(rel: string): Promise<FileStat> {
    const path = vaultRelativePath(rel);
    const response = await this.send("stat", "PROPFIND", path, {
      body: DAV_PROPFIND_BODY,
      depth: "0",
    });
    const [resource] = parseMultistatus(await response.text(), this.basePath);
    return statOf(resource, path);
  }

  /**
   * The shared content hash, over the bytes this backend read.
   *
   * `contentHash` is the same function the filesystem backend uses, so a hash
   * taken here means what a hash taken there means. Nothing here reads an ETag: a
   * server is free to invent one, and the one server this targets does not emit
   * `getetag` at all, so an ETag comparison would be a check that either always
   * passes or always fails.
   */
  async hash(rel: string): Promise<string> {
    const path = vaultRelativePath(rel);
    const cached = this.hashCache.get(path);
    if (cached !== undefined) return cached;
    const hash = contentHash(await this.readText(path));
    this.hashCache.set(path, hash);
    return hash;
  }

  /**
   * Store text, then read it back and refuse anything but the bytes that were sent.
   *
   * The read-back is the whole point of this method over HTTP. A `PUT` answered
   * `201 Created` is a claim about what the server will do, not a report of what it
   * did, and a server that accepts a write and stores something else -- a
   * transcoding filter, a `Content-Encoding` it disagreed with, a bug -- is exactly
   * the failure a client cannot detect from the response. `contentHash` decides it,
   * and no ETag is consulted: a server is free to invent one, and dufs does not emit
   * `getetag` at all.
   *
   * The collection has to exist already. `MKCOL` is a separate call for a reason --
   * a `PUT` into a missing collection answers `409`, and the caller that wanted the
   * collection made said so by calling {@link ensureDir}.
   */
  async writeText(rel: string, data: string): Promise<void> {
    const path = namedFile(vaultRelativePath(rel));
    await this.send("write", "PUT", path, { body: data, contentType: TEXT_CONTENT_TYPE });

    const stored = contentHash(await this.fetchText("write", path));
    if (stored !== contentHash(data)) {
      throw new BasesError(
        `WebDAV write: PUT "${path}" was accepted but the bytes read back are not the bytes ` +
          `sent (wrote ${contentHash(data)}, read back ${stored}). The server stored something ` +
          `else, so the write is refused rather than reported as done.`,
        { construct: "webdav" },
      );
    }
    // Only now, after the read-back agreed: a cache updated from an unverified write
    // would report the requested bytes for a resource holding others.
    this.textCache.set(path, data);
    this.hashCache.delete(path);
    this.files = null;
  }

  /**
   * Create intermediate collections, tolerating the ones already there.
   *
   * `MKCOL` answers `405 Method Not Allowed` for a collection that exists, which is
   * the one success the protocol defines as a non-2xx, so `ensureDir` reports
   * nothing about whether it did anything -- exactly as the filesystem's `mkdir -p`
   * reports nothing. The listing snapshot is left alone for the same reason
   * `FsVaultSource.ensureDir` leaves its own: an empty collection adds no indexable
   * file, and the `writeText` that follows invalidates it.
   */
  async ensureDir(rel: string): Promise<void> {
    const path = vaultRelativePath(rel);
    if (path === "") return;
    const segments = path.split("/");
    for (let i = 0; i < segments.length; i++) {
      const prefix = segments.slice(0, i + 1).join("/");
      await this.send("ensureDir", "MKCOL", prefix);
    }
  }

  /**
   * Remove a file, tolerating one that is not there.
   *
   * This is the second deliberate divergence from `FsVaultSource`, which runs
   * `rm --force` and so reports success for a file that never existed. A WebDAV
   * `DELETE` answers `404` for a missing resource, and the honest answer is the
   * honest one: a delete that did not happen must not be reported as having
   * happened, or a caller that checks a resource is gone is told a lie about the one
   * thing it asked about. `404` is tolerated rather than thrown for a different
   * reason -- the desired state, absence, already holds -- while any other refusal
   * is a real failure. `VaultSource.delete` has no caller in `src/` yet, so nothing
   * depends on which of the two behaviours is chosen -- but the choice is recorded
   * here, because the first caller needs to know it was one.
   */
  async delete(rel: string): Promise<void> {
    const path = namedFile(vaultRelativePath(rel));
    await this.send("delete", "DELETE", path);
    this.textCache.delete(path);
    this.hashCache.delete(path);
    this.files = null;
  }

  // -- transport ------------------------------------------------------------

  /**
   * The bytes at `path`, read without consulting or filling the cache.
   *
   * `Buffer.toString("utf8")` rather than `Response.text()`: the latter removes a
   * leading byte-order mark, and `FsVaultSource` reading with `"utf8"` does not, so
   * a note that starts with a BOM would read differently on the two backends.
   */
  private async fetchText(operation: DavOperation, path: string): Promise<string> {
    const response = await this.send(operation, "GET", path);
    return Buffer.from(await response.arrayBuffer()).toString("utf8");
  }

  /**
   * Send one request and refuse the statuses this operation refuses.
   *
   * `Depth` is set only for `PROPFIND` and only to `"0"` or `"1"`. The type makes
   * `infinity` unrepresentable here, which is the point: it is a string on the
   * wire, so nothing but this signature stands between a client and a server that
   * will not answer it.
   */
  private async send(
    operation: DavOperation,
    method: WebdavMethod,
    path: string,
    options: { body?: string; depth?: DavDepth; contentType?: string; collection?: boolean } = {},
  ): Promise<Response> {
    const headers: Record<string, string> = {};
    if (this.authorization !== undefined) headers["Authorization"] = this.authorization;
    if (options.depth !== undefined) headers["Depth"] = options.depth;
    // A body with no declared type is a PROPFIND request, and the only body that
    // arrives without one.
    const contentType =
      options.contentType ?? (options.body === undefined ? undefined : XML_CONTENT_TYPE);
    if (contentType !== undefined) headers["Content-Type"] = contentType;

    const url = resourceUrl(this.baseUrl, path, options.collection === true);
    let response: Response;
    try {
      response = await this.transport(url, {
        method,
        headers,
        body: options.body,
        signal: AbortSignal.timeout(this.timeoutMs),
      });
    } catch (err) {
      // No status, so not a `WebdavError`: nothing answered. Reported as a refusal
      // rather than left to escape as a `TypeError`, which the MCP layer would call
      // an internal bug in this server rather than an unreachable one.
      throw new BasesError(
        `WebDAV ${operation}: ${method} "${atRoot(path)}" got no response: ` +
          `${err instanceof Error ? err.message : String(err)}`,
        { construct: "webdav" },
      );
    }

    const refusal = davRefusal(operation, method, path, response.status, response.statusText);
    if (refusal !== undefined) throw refusal;
    return response;
  }
}

/**
 * `FsVaultSource.walk` skips a dot-directory rather than descending into it.
 *
 * The basename is the only segment that can start with a dot here, because the walk
 * never descends into one -- so a per-segment check would be a second rule for a
 * question the recursion has already answered.
 */
function isHiddenCollection(path: string): boolean {
  return path.slice(path.lastIndexOf("/") + 1).startsWith(".");
}

function cyclic(path: string): BasesError {
  return new BasesError(
    `WebDAV list: a listing named "${atRoot(path)}" as a collection inside itself, so the ` +
      `vault has no finite tree. Refusing rather than following the cycle.`,
    { construct: "webdav" },
  );
}

/**
 * The `FileStat` one resource describes, or a refusal saying which part is missing.
 *
 * Fail-fast on a missing `getlastmodified` because there is no honest default.
 * `FileStat.mtime` feeds `file.ctime` and `file.mtime`, so substituting the epoch
 * for a value the server declined to send would put a real-looking date in a query
 * result, and this server's whole reason to exist is that a wrong answer must be
 * visibly wrong rather than plausible.
 */
function statOf(resource: DavResource | undefined, path: string): FileStat {
  if (resource === undefined) {
    throw new BasesError(`WebDAV stat: PROPFIND "${atRoot(path)}" returned no resource for it.`, {
      construct: "webdav",
    });
  }
  if (resource.isCollection) {
    throw new BasesError(`WebDAV stat: PROPFIND "${atRoot(path)}" reported a collection.`, {
      construct: "webdav",
    });
  }
  if (resource.size === undefined) {
    throw new BasesError(
      `WebDAV stat: the server did not report getcontentlength for "${atRoot(path)}", so its ` +
        `size is unknown rather than zero.`,
      { construct: "webdav" },
    );
  }
  if (resource.lastModified === undefined) {
    throw new BasesError(
      `WebDAV stat: the server did not report getlastmodified for "${atRoot(path)}", so its ` +
        `mtime is unknown rather than the epoch.`,
      { construct: "webdav" },
    );
  }
  return { size: resource.size, mtime: parseDavDate(resource.lastModified, path) };
}

/**
 * The instant a `getlastmodified` names.
 *
 * RFC 9110 allows three date formats in this header and a server may send any of
 * them; `Date` reads all three, and a value it cannot read is refused rather than
 * turned into an `Invalid Date` that would then flow into a rendered table as `NaN`.
 * The value is reported exactly as received, timezone included, because converting
 * it would be inventing a precision the server did not claim.
 */
export function parseDavDate(value: string, path: string): Date {
  const parsed = new Date(value);
  if (Number.isNaN(parsed.getTime())) {
    throw new BasesError(
      `WebDAV stat: getlastmodified for "${atRoot(path)}" is not a date this client can ` +
        `read: ${JSON.stringify(value)}.`,
      { construct: "webdav" },
    );
  }
  return parsed;
}
