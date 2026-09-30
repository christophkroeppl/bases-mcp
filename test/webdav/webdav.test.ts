/**
 * The WebDAV backend, in two tiers.
 *
 * Tier 1 needs no server and runs in the ordinary unit suite: a fake WebDAV
 * transport stands in for the network, and the two hard constraints of this
 * backend are asserted as properties of the requests it makes. That is the only
 * way to pin them. "Recurse with repeated `Depth: 1`, never `Depth: infinity`" and
 * "read a listing rather than a file per note" are claims about headers and about
 * which requests happen at all, so the test records both and reads them back.
 *
 * Tier 2 runs against a live server and is gated on `BASES_MCP_WEBDAV_URL` with
 * `test.skipIf`, never with an early `return` in the body. An early return reports
 * PASS with zero assertions, so a missing server reads as a green suite that
 * compared nothing -- the exact bug this project already had to fix once, in the
 * parity suite.
 *
 * What the equivalence suite already covers is not repeated here: that a
 * `VaultSource` produces the same answers as the filesystem one. What it could not
 * cover, because the backend under test there is an in-memory `Map` with no HTTP in
 * it, is whether THIS file's href mapping, XML parsing and request discipline are
 * right. That is tier 1's job, and it is compared against the same oracle the
 * equivalence suite uses: `test/vault`, read by `FsVaultSource`.
 */

import { describe, expect, test } from "bun:test";

import { Resolver } from "../../src/service";
import { contentHash, FsVaultSource } from "../../src/vault/fs";
import { isIndexable } from "../../src/vault/source";
import {
  davRefusal,
  parseDavDate,
  parseMultistatus,
  vaultPathFromHref,
  vaultRelativePath,
  WebdavError,
  type WebdavFetch,
  WebdavVaultSource,
} from "../../src/vault/webdav";
import { CORPUS_SIZE, loadCorpus, VAULT_DIR } from "./corpus";
import type { VaultFile } from "./memory";

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

const BASE_URL = "https://dav.example/dav/vault";
/** The server-side path `BASE_URL` points at. Hrefs are rooted here. */
const BASE_PATH = "/dav/vault";
/** Every mtime the fake server hands out, so no assertion is against a clock. */
const SERVER_MTIME = "Tue, 01 Oct 2024 10:11:12 GMT";
const SERVER_MTIME_MS = Date.UTC(2024, 9, 1, 10, 11, 12, 0);

const USER = "agent";
const PASSWORD = "correct horse battery staple";

/**
 * A `207 Multi-Status` body as a server actually sends one.
 *
 * Written out rather than generated, because the point of the parsing tests is that
 * the shapes a real server emits are understood: a namespace prefix, a collection
 * self-entry, a percent-encoded href, an empty `<resourcetype/>` for a file, a
 * `404` propstat sitting beside a `200` one, and properties split across two
 * propstats. Every value here is asserted by name below, so a change in what is
 * extracted cannot pass as a change in a fixture.
 */
const CANNED = `<?xml version="1.0" encoding="utf-8"?>
<D:multistatus xmlns:D="DAV:">
  <D:response>
    <D:href>/dav/vault/</D:href>
    <D:propstat>
      <D:prop>
        <D:resourcetype><D:collection/></D:resourcetype>
        <D:getlastmodified>Tue, 01 Oct 2024 09:00:00 GMT</D:getlastmodified>
      </D:prop>
      <D:status>HTTP/1.1 200 OK</D:status>
    </D:propstat>
  </D:response>
  <D:response>
    <D:href>/dav/vault/Root%20Project.md</D:href>
    <D:propstat>
      <D:prop>
        <D:resourcetype/>
        <D:getcontentlength>512</D:getcontentlength>
        <D:getlastmodified>Tue, 01 Oct 2024 10:11:12 GMT</D:getlastmodified>
      </D:prop>
      <D:status>HTTP/1.1 200 OK</D:status>
    </D:propstat>
  </D:response>
  <D:response>
    <D:href>/dav/vault/Projects/</D:href>
    <D:propstat>
      <D:prop>
        <D:resourcetype><D:collection/></D:resourcetype>
        <D:getlastmodified>Tue, 01 Oct 2024 12:00:00 +0200</D:getlastmodified>
      </D:prop>
      <D:status>HTTP/1.1 200 OK</D:status>
    </D:propstat>
  </D:response>
  <D:response>
    <D:href>/dav/vault/Projects/SomeProject.md</D:href>
    <D:propstat>
      <D:prop>
        <D:resourcetype/>
        <D:getcontentlength>8</D:getcontentlength>
        <D:getlastmodified>Tue, 01 Oct 2024 12:00:00 +0200</D:getlastmodified>
      </D:prop>
      <D:status>HTTP/1.1 200 OK</D:status>
    </D:propstat>
    <D:propstat>
      <D:prop><D:getetag>"unproven"</D:getetag></D:prop>
      <D:status>HTTP/1.1 404 Not Found</D:status>
    </D:propstat>
  </D:response>
  <D:response>
    <D:href>/dav/vault/Tickets/Fix%20login%20redirect.md</D:href>
    <D:propstat>
      <D:prop>
        <D:resourcetype/>
        <D:getcontentlength>0</D:getcontentlength>
        <D:getlastmodified>Tue, 01 Oct 2024 09:30:00 GMT</D:getlastmodified>
      </D:prop>
      <D:status>HTTP/1.1 200 OK</D:status>
    </D:propstat>
  </D:response>
  <D:response>
    <D:href>/dav/vault/Caf%C3%A9/50%25.md</D:href>
    <D:propstat>
      <D:prop>
        <D:resourcetype/>
        <D:getcontentlength>17</D:getcontentlength>
        <D:getlastmodified>Tue, 01 Oct 2024 09:45:00 GMT</D:getlastmodified>
      </D:prop>
      <D:status>HTTP/1.1 200 OK</D:status>
    </D:propstat>
  </D:response>
  <D:response>
    <D:href>/dav/vault/Deleted%20While%20Listing.md</D:href>
    <D:propstat>
      <D:prop><D:getcontentlength>10</D:getcontentlength></D:prop>
      <D:status>HTTP/1.1 404 Not Found</D:status>
    </D:propstat>
  </D:response>
</D:multistatus>`;

/** The canned listing, keyed by the vault path it maps to. */
function cannedByPath(): Map<string, ReturnType<typeof parseMultistatus>[number]> {
  return new Map(parseMultistatus(CANNED, BASE_PATH).map((r) => [r.path, r]));
}

// ---------------------------------------------------------------------------
// A fake WebDAV server
// ---------------------------------------------------------------------------

/** One request the source made, as the server saw it. */
interface Recorded {
  readonly method: string;
  readonly url: string;
  readonly depth: string | undefined;
  readonly authorization: string | undefined;
  readonly body: string | undefined;
}

type Status = { readonly status: number; readonly statusText: string };

/**
 * A WebDAV server, in enough of it to be a real client.
 *
 * It serves `PROPFIND` at `Depth: 0` and `Depth: 1`, `GET`, `PUT`, `MKCOL` and
 * `DELETE`, generates hrefs the way a server does (percent-encoded, trailing slash
 * on every collection) and refuses what a server refuses: `405` for a `MKCOL` onto
 * an existing collection, `409` for one whose parent is missing, `404` for a
 * `DELETE` of nothing.
 *
 * The hrefs it generates are the only thing it shares with the backend's mapping,
 * and it keeps them ENCODED, so a request arrives as the raw string the backend
 * built. A decoding mistake in the backend therefore shows up as a file the fake
 * does not recognise, rather than being cancelled out by a decoder on both sides.
 */
class FakeDav {
  readonly requests: Recorded[] = [];
  private readonly files = new Map<string, string>();
  private readonly dirs = new Set<string>([""]);
  /** Per-collection refusals, keyed by vault path, for the failure tests. */
  private readonly refuse = new Map<string, Status>();
  private mtime = SERVER_MTIME;

  constructor(seed: Iterable<VaultFile> = []) {
    for (const file of seed) this.store(file.path, file.content);
  }

  /** A server that answers every request on a vault path with this status. */
  refusing(path: string, status: Status): this {
    this.refuse.set(path, status);
    return this;
  }

  /** The mtime every `getlastmodified` reports, so no assertion is against a clock. */
  withMtime(value: string): this {
    this.mtime = value;
    return this;
  }

  /** The bytes at a vault path, as the server holds them. */
  stored(path: string): string | undefined {
    return this.files.get(path);
  }

  hasDir(path: string): boolean {
    return this.dirs.has(path);
  }

  private store(path: string, content: string): void {
    const segments = path.split("/");
    for (let i = 1; i < segments.length; i++) {
      this.dirs.add(segments.slice(0, i).join("/"));
    }
    this.files.set(path, content);
  }

  /** The href for a vault path, encoded the way a server encodes one. */
  private href(path: string): string {
    if (path === "") return `${BASE_URL}/`;
    return `${BASE_URL}/${path.split("/").map(encodeURIComponent).join("/")}`;
  }

  /**
   * The vault path a request URL names, as this server stores it.
   *
   * Decoding here is the server's own business and shares no code with the
   * backend's href mapping: the two meet only at the wire, so a mistake in the
   * backend's outbound encoding reaches the fake as a path it does not hold rather
   * than being cancelled out by a shared decoder.
   */
  private pathOf(url: URL): string | undefined {
    const base = pathnameOf(BASE_URL);
    if (!url.pathname.startsWith(base)) return undefined;
    return url.pathname
      .slice(base.length)
      .split("/")
      .filter((segment) => segment !== "")
      .map((segment) => decodeURIComponent(segment))
      .join("/");
  }

  /** Direct children of a vault path, plus the path itself, in sorted order. */
  private membersOf(path: string): string[] {
    const prefix = path === "" ? "" : `${path}/`;
    const all = new Set<string>();
    for (const file of this.files.keys()) {
      if (file.startsWith(prefix)) all.add(prefix + firstSegment(file, prefix));
    }
    for (const dir of this.dirs) {
      if (dir !== path && dir.startsWith(prefix)) all.add(prefix + firstSegment(dir, prefix));
    }
    return [path, ...[...all].sort()];
  }

  readonly fetch: WebdavFetch = async (input, init) => {
    const url = new URL(String(input));
    const headers = new Headers(init?.headers);
    const method = (init?.method ?? "GET").toUpperCase();
    const path = this.pathOf(url);

    this.requests.push({
      method,
      url: url.toString(),
      depth: headers.get("Depth") ?? undefined,
      authorization: headers.get("Authorization") ?? undefined,
      body: typeof init?.body === "string" ? init.body : undefined,
    });

    if (path === undefined) return refusal(404, "Not Found");
    const blocked = this.refuse.get(path);
    if (blocked !== undefined) return refusal(blocked.status, blocked.statusText);

    // A collection URL ends in `/` and a file URL does not, and a server that is
    // handed the wrong one answers 404. Stated as a rule here so a backend that
    // dropped the slash is refused the way a real server refuses it, rather than
    // quietly served -- this is the one part of the fake a backend can be wrong about
    // without the wire noticing.
    const isCollection = this.dirs.has(path);
    const trailing = url.pathname.endsWith("/");
    // `Depth: 0` is exempt: a properties request for a collection is answered by
    // plenty of servers whichever way the URL is written, and refusing it here
    // would mean the backend never got to say "that is a collection, not a note".
    const properties = method === "PROPFIND" && headers.get("Depth") === "0";
    if (method !== "PUT" && method !== "MKCOL" && !properties && trailing !== isCollection) {
      return refusal(404, "Not Found");
    }

    switch (method) {
      case "PROPFIND":
        return this.propfind(path, headers.get("Depth"));
      case "GET":
        return this.files.has(path)
          ? new Response(this.files.get(path), { status: 200, statusText: "OK" })
          : refusal(404, "Not Found");
      case "PUT": {
        if (!this.dirs.has(parentOf(path))) return refusal(409, "Conflict");
        this.store(path, String(init?.body ?? ""));
        return refusal(201, "Created");
      }
      case "MKCOL":
        return this.mkcol(path);
      case "DELETE":
        if (!this.files.delete(path)) return refusal(404, "Not Found");
        return refusal(204, "No Content");
      default:
        return refusal(405, "Method Not Allowed");
    }
  };

  private propfind(path: string, depth: string | null): Response {
    if (this.dirs.has(path) || this.files.has(path)) {
      if (this.dirs.has(path) && depth === "1") {
        return multistatus(this.membersOf(path).map((member) => this.resource(member)));
      }
      return multistatus([this.resource(path)]);
    }
    return refusal(404, "Not Found");
  }

  private resource(path: string): string {
    const collection = this.dirs.has(path);
    return [
      `  <D:response><D:href>${escapeXml(this.href(path))}</D:href><D:propstat><D:prop>`,
      collection
        ? `<D:resourcetype><D:collection/></D:resourcetype>`
        : `<D:resourcetype/><D:getcontentlength>${byteLength(this.files.get(path) ?? "")}</D:getcontentlength>`,
      `<D:getlastmodified>${this.mtime}</D:getlastmodified>`,
      `</D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>`,
    ].join("");
  }

  private mkcol(path: string): Response {
    if (this.dirs.has(path) || this.files.has(path)) return refusal(405, "Method Not Allowed");
    if (!this.dirs.has(parentOf(path))) return refusal(409, "Conflict");
    this.dirs.add(path);
    return refusal(201, "Created");
  }
}

function pathnameOf(url: string): string {
  return new URL(url).pathname;
}

/** The vault path of a path's parent collection. A root-level file's is the root. */
function parentOf(path: string): string {
  const cut = path.lastIndexOf("/");
  return cut < 0 ? "" : path.slice(0, cut);
}

function firstSegment(path: string, prefix: string): string {
  return path.slice(prefix.length).split("/")[0];
}

function byteLength(text: string): number {
  return new TextEncoder().encode(text).length;
}

function escapeXml(text: string): string {
  return text.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
}

function refusal(status: number, statusText: string): Response {
  return new Response(null, { status, statusText });
}

/** A `207 Multi-Status` body for the given `response` blocks. */
function multistatus(responses: readonly string[]): Response {
  return new Response(
    `<?xml version="1.0" encoding="utf-8"?><D:multistatus xmlns:D="DAV:">` +
      `${responses.join("")}</D:multistatus>`,
    { status: 207, statusText: "Multi-Status" },
  );
}

/** A `207 Multi-Status` body listing nothing. */
const EMPTY_LISTING = '<?xml version="1.0"?><D:multistatus xmlns:D="DAV:"/>';

/** A `PROPFIND` answer carrying a size but no `getlastmodified`. */
const NO_MTIME_PROPFIND = `<?xml version="1.0"?><D:multistatus xmlns:D="DAV:"><D:response>
  <D:href>/dav/vault/Note.md</D:href><D:propstat><D:prop><D:resourcetype/>
  <D:getcontentlength>4</D:getcontentlength></D:prop>
  <D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response></D:multistatus>`;

/** A `PROPFIND` answer carrying an mtime but no `getcontentlength`. */
const NO_SIZE_PROPFIND = `<?xml version="1.0"?><D:multistatus xmlns:D="DAV:"><D:response>
  <D:href>/dav/vault/Note.md</D:href><D:propstat><D:prop><D:resourcetype/>
  <D:getlastmodified>Tue, 01 Oct 2024 10:11:12 GMT</D:getlastmodified></D:prop>
  <D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response></D:multistatus>`;

/** A source over a fake server, with Basic auth, at the fake's base URL. */
function sourceOver(server: FakeDav): WebdavVaultSource {
  return new WebdavVaultSource({
    url: BASE_URL,
    user: USER,
    password: PASSWORD,
    fetch: server.fetch,
  });
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

// ---------------------------------------------------------------------------
// Tier 1: hrefs, paths, XML
// ---------------------------------------------------------------------------

describe("href to vault-relative path", () => {
  test("strips the server base path and percent-decodes what is left", () => {
    expect(vaultPathFromHref("/dav/vault/Root%20Project.md", BASE_PATH)).toBe("Root Project.md");
    expect(vaultPathFromHref("/dav/vault/Tickets/Fix%20login%20redirect.md", BASE_PATH)).toBe(
      "Tickets/Fix login redirect.md",
    );
  });

  test("a collection href loses its trailing slash, so it is a path and not a name", () => {
    // `FsVaultSource.walk` passes `Projects`, never `Projects/`, and a listing
    // compared against it would be off by a slash on every directory.
    expect(vaultPathFromHref("/dav/vault/Projects/", BASE_PATH)).toBe("Projects");
    expect(vaultPathFromHref("/dav/vault/", BASE_PATH)).toBe("");
  });

  test("the collection self-reference maps to the collection's own vault path", () => {
    for (const [href, path] of [
      ["/dav/vault/", ""],
      ["/dav/vault/Projects/", "Projects"],
    ] as const) {
      expect(vaultPathFromHref(href, BASE_PATH)).toBe(path);
    }
  });

  test("decodes a percent escape, not the escape character", () => {
    // The two traps in one name: a non-ASCII byte, and a literal `%` that a naive
    // decode would treat as the start of an escape.
    expect(vaultPathFromHref("/dav/vault/Caf%C3%A9/50%25.md", BASE_PATH)).toBe("Café/50%.md");
  });

  test("leaves a + alone, because it is not a space in a path", () => {
    expect(vaultPathFromHref("/dav/vault/A+B.md", BASE_PATH)).toBe("A+B.md");
  });

  test("accepts a full URL, which RFC 4918 allows and servers do send", () => {
    expect(vaultPathFromHref("https://dav.example/dav/vault/Root%20Ticket.md", BASE_PATH)).toBe(
      "Root Ticket.md",
    );
  });

  test("a base path with an escape of its own is compared decoded, so it still matches", () => {
    expect(vaultPathFromHref("/dav/My%20Vault/Note.md", "/dav/My%20Vault")).toBe("Note.md");
  });

  test("refuses an href outside the base, rather than indexing another tree", () => {
    expect(() => vaultPathFromHref("/elsewhere/Secret.md", BASE_PATH)).toThrow(
      /outside the configured vault base/,
    );
  });

  test("refuses a path that escapes the vault root through ..", () => {
    expect(() => vaultPathFromHref("/dav/vault/../etc/passwd", BASE_PATH)).toThrow(
      /escapes the vault root/,
    );
  });

  test("resolves a redundant . and .. rather than keeping them in a note's path", () => {
    expect(vaultPathFromHref("/dav/vault/Tickets/../Root%20Ticket.md", BASE_PATH)).toBe(
      "Root Ticket.md",
    );
  });

  test("refuses malformed percent-encoding instead of decoding it to nonsense", () => {
    expect(() => vaultPathFromHref("/dav/vault/100%.md", BASE_PATH)).toThrow(
      /not valid percent-encoding/,
    );
  });
});

describe("caller-supplied paths", () => {
  test("resolves a redundant path to the note it names, as the filesystem backend does", () => {
    expect(vaultRelativePath("Tickets/../Root Ticket.md")).toBe("Root Ticket.md");
    expect(vaultRelativePath("./Tickets/Fix login redirect.md")).toBe(
      "Tickets/Fix login redirect.md",
    );
  });

  test("refuses every escape, including a leading slash", () => {
    for (const escapee of ["../outside.md", "/etc/passwd", "Tickets/../../outside.md"]) {
      expect(() => vaultRelativePath(escapee)).toThrow(/escapes the vault root/);
    }
  });

  test("an escape is refused before any request is made", async () => {
    // Structural rather than promised: the guard runs on the argument, so a path
    // from an agent cannot become a URL outside the vault even if the guard were
    // later moved. The empty request log is the assertion.
    const server = new FakeDav();
    const source = sourceOver(server);
    await rejection(() => source.readText("../../etc/passwd"));
    expect(server.requests).toEqual([]);
  });
});

describe("a 207 Multi-Status body", () => {
  test("extracts every resource, by the vault path it maps to", () => {
    expect(parseMultistatus(CANNED, BASE_PATH).map((r) => r.path)).toEqual([
      "",
      "Root Project.md",
      "Projects",
      "Projects/SomeProject.md",
      "Tickets/Fix login redirect.md",
      "Café/50%.md",
    ]);
  });

  test("tells a collection from a file, on the resourcetype element alone", () => {
    const byPath = cannedByPath();
    expect(byPath.get("")?.isCollection).toBe(true);
    expect(byPath.get("Projects")?.isCollection).toBe(true);
    expect(byPath.get("Root Project.md")?.isCollection).toBe(false);
    expect(byPath.get("Café/50%.md")?.isCollection).toBe(false);
  });

  test("extracts the byte size, verbatim and unrounded", () => {
    const byPath = cannedByPath();
    expect(byPath.get("Root Project.md")?.size).toBe(512);
    expect(byPath.get("Projects/SomeProject.md")?.size).toBe(8);
    expect(byPath.get("Café/50%.md")?.size).toBe(17);
    expect(byPath.get("Projects")?.size).toBeUndefined();
  });

  test("extracts getlastmodified as the string the server sent", () => {
    const byPath = cannedByPath();
    expect(byPath.get("Root Project.md")?.lastModified).toBe(SERVER_MTIME);
    expect(byPath.get("Projects")?.lastModified).toBe("Tue, 01 Oct 2024 12:00:00 +0200");
  });

  test("drops a resource whose only propstat is a 404", () => {
    // A listing can name a file that was deleted between the request and the
    // response. Indexing it would put a path in `list()` that a later `readText`
    // cannot serve.
    expect(parseMultistatus(CANNED, BASE_PATH).map((r) => r.path)).not.toContain(
      "Deleted While Listing.md",
    );
  });

  test("keeps the properties from a 200 propstat when a sibling propstat is a 404", () => {
    // The 404 block is for a property the server declined to report, not for the
    // resource. Reading only the first block, or merging blindly, loses the size.
    const byPath = cannedByPath();
    expect(byPath.get("Projects/SomeProject.md")?.size).toBe(8);
    expect(byPath.get("Projects/SomeProject.md")?.lastModified).toBe(
      "Tue, 01 Oct 2024 12:00:00 +0200",
    );
  });

  test("an empty collection is an empty listing, not a malformed document", () => {
    expect(
      parseMultistatus('<?xml version="1.0"?><D:multistatus xmlns:D="DAV:"/>', BASE_PATH),
    ).toEqual([]);
  });

  test("refuses a body that is not a multistatus at all", () => {
    // A login page or an HTML error body parsed as a vault is a vault with no
    // notes, which is the one confusion this server exists to avoid.
    expect(() => parseMultistatus("<html><body>404</body></html>", BASE_PATH)).toThrow(
      /did not return a multistatus/,
    );
    expect(() =>
      parseMultistatus(
        '<?xml version="1.0"?><D:error xmlns:D="DAV:"><D:status>403 Forbidden</D:status></D:error>',
        BASE_PATH,
      ),
    ).toThrow(/did not return a multistatus/);
  });
});

describe("getlastmodified", () => {
  test("reads the instant, in the server's own timezone", () => {
    expect(parseDavDate(SERVER_MTIME, "Note.md").getTime()).toBe(SERVER_MTIME_MS);
    expect(parseDavDate("Tue, 01 Oct 2024 12:00:00 +0200", "Note.md").getTime()).toBe(
      Date.UTC(2024, 9, 1, 10, 0, 0, 0),
    );
  });

  test("reads the two obsolete formats RFC 9110 still allows", () => {
    expect(parseDavDate("Tuesday, 01-Oct-24 10:11:12 GMT", "Note.md").getTime()).toBe(
      SERVER_MTIME_MS,
    );
    expect(parseDavDate("Tue Oct  1 10:11:12 2024", "Note.md").getTime()).toBe(SERVER_MTIME_MS);
  });

  test("refuses a date it cannot read, rather than yielding an Invalid Date", () => {
    // An Invalid Date reaches `file.mtime` and then a rendered cell as NaN, which
    // looks like data.
    const err = rejectionOf(() => parseDavDate("yesterday-ish", "Note.md"));
    expect(err).toBeInstanceOf(Error);
    expect(err.message).toMatch(/not a date this client can read/);
    expect(err.message).toContain("yesterday-ish");
  });
});

describe("the response classifier", () => {
  test("a 2xx passes, and 207 with it", () => {
    for (const status of [200, 201, 204, 207, 299]) {
      expect(davRefusal("read", "GET", "Note.md", status, "OK")).toBeUndefined();
    }
  });

  test("MKCOL tolerates 405, because the collection is already there", () => {
    expect(davRefusal("ensureDir", "MKCOL", "Projects", 405, "Method Not Allowed")).toBeUndefined();
  });

  test("DELETE tolerates 404, because the resource is already not there", () => {
    expect(davRefusal("delete", "DELETE", "Note.md", 404, "Not Found")).toBeUndefined();
  });

  test("the tolerance is the operation's, not a blanket one", () => {
    // 405 on a read is a server that does not do PROPFIND, and 404 on a read is a
    // note that is not there. Neither is the MKCOL or DELETE answer.
    expect(davRefusal("read", "GET", "Note.md", 404, "Not Found")).toBeInstanceOf(WebdavError);
    expect(davRefusal("list", "PROPFIND", "", 405, "Method Not Allowed")).toBeInstanceOf(
      WebdavError,
    );
    expect(davRefusal("write", "PUT", "Note.md", 404, "Not Found")).toBeInstanceOf(WebdavError);
  });

  test("every other status refuses, a redirect included", () => {
    for (const status of [301, 302, 400, 401, 403, 500, 503]) {
      expect(davRefusal("list", "PROPFIND", "", status, "")).toBeInstanceOf(WebdavError);
    }
  });

  test("the refusal names the operation, the method, the status and the path", () => {
    const err = davRefusal("read", "GET", "Tickets/Fix login redirect.md", 404, "Not Found");
    expect(err?.message).toBe(
      'WebDAV read: GET "Tickets/Fix login redirect.md": 404 Not Found (construct: webdav)',
    );
    expect(err?.status).toBe(404);
    expect(err?.operation).toBe("read");
    expect(err?.method).toBe("GET");
    expect(err?.path).toBe("Tickets/Fix login redirect.md");
  });

  test("a refusal is a BasesError, so the MCP layer reports it as a tool failure", () => {
    // Not an internal error: a server refusing a request is the server's answer,
    // and it must reach the agent as a structured result naming the operation.
    const err = davRefusal("stat", "PROPFIND", "Note.md", 500, "Internal Server Error");
    expect(err).toBeInstanceOf(WebdavError);
    expect(err?.name).toBe("WebdavError");
  });
});

// ---------------------------------------------------------------------------
// Tier 1: the requests the backend makes
// ---------------------------------------------------------------------------

describe("recursion", () => {
  test("asks for one collection at a time, and never for infinity", async () => {
    const server = new FakeDav(await loadCorpus());
    await sourceOver(server).list();

    expect(server.requests.map((r) => [r.method, r.depth])).toEqual([
      ["PROPFIND", "1"],
      ["PROPFIND", "1"],
      ["PROPFIND", "1"],
    ]);
    // One request per collection, and nothing else. `Depth: infinity` would make
    // this a single request, and no mainstream server would answer it.
    expect(server.requests.map((r) => r.url)).toEqual([
      `${BASE_URL}/`,
      `${BASE_URL}/Projects/`,
      `${BASE_URL}/Tickets/`,
    ]);
  });

  test("asks for the three properties it reads, and never for an ETag", async () => {
    // The request states what the backend will use. A `getetag` in here would be a
    // plan to trust one, and the plan says a server ETag cannot be trusted: dufs
    // does not emit `getetag` in its WebDAV test suite at all.
    const server = new FakeDav(await loadCorpus());
    await sourceOver(server).list();

    const body = server.requests[0]?.body ?? "";
    expect(body).toContain("<d:resourcetype/>");
    expect(body).toContain("<d:getcontentlength/>");
    expect(body).toContain("<d:getlastmodified/>");
    expect(body).not.toContain("getetag");
    expect(body).not.toContain("allprop");
  });

  test("walks depth-first, so a grandchild collection is listed before its uncle", async () => {
    // Breadth-first would answer `B/` before `A/A2/`. The order of the requests IS
    // the order of the walk, so this is the only place it is observable.
    const server = new FakeDav([
      { path: "A/A2/Deep.md", content: "# deep\n" },
      { path: "A/Alpha.md", content: "# alpha\n" },
      { path: "B/Beta.md", content: "# beta\n" },
      { path: "C.md", content: "# c\n" },
    ]);
    await sourceOver(server).list();

    expect(server.requests.map((r) => r.url)).toEqual([
      `${BASE_URL}/`,
      `${BASE_URL}/A/`,
      `${BASE_URL}/A/A2/`,
      `${BASE_URL}/B/`,
    ]);
  });

  test("asks for a listing, not a file per note", async () => {
    // The alternative -- PROPFIND nothing and GET every note -- is the same number
    // of round trips for a flat vault and far worse for a deep one, and it cannot
    // see a collection it has no permission to enter.
    const server = new FakeDav(await loadCorpus());
    const paths = await sourceOver(server).list();

    expect(paths).toHaveLength(CORPUS_SIZE);
    expect(server.requests.every((r) => r.method === "PROPFIND")).toBe(true);
  });

  test("refuses a listing that names a collection inside itself", async () => {
    // The alternative to noticing the cycle is walking it: the same URLs, forever,
    // with a tool call hanging and nothing to report. `childrenOf` already drops a
    // listing's self-entry, so a cycle needs a collection to be its own DESCENDANT --
    // which is why this fake is three collections deep.
    const listings: Record<string, string[]> = {
      "": ["/dav/vault/", "/dav/vault/Loop/"],
      Loop: ["/dav/vault/Loop/", "/dav/vault/Loop/Inner/"],
      "Loop/Inner": ["/dav/vault/Loop/Inner/", "/dav/vault/Loop/"],
    };
    const source = new WebdavVaultSource({
      url: BASE_URL,
      fetch: async (input) => {
        const asked = new URL(input).pathname.slice(BASE_PATH.length).replace(/^\/+|\/+$/g, "");
        const hrefs = listings[asked];
        if (hrefs === undefined) return refusal(404, "Not Found");
        const responses = hrefs
          .map(
            (href) =>
              `<D:response><D:href>${href}</D:href><D:propstat><D:prop>` +
              `<D:resourcetype><D:collection/></D:resourcetype></D:prop>` +
              `<D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>`,
          )
          .join("");
        return new Response(
          `<?xml version="1.0"?><D:multistatus xmlns:D="DAV:">${responses}</D:multistatus>`,
          { status: 207 },
        );
      },
    });

    const err = await rejection(() => source.list());
    expect(err.message).toMatch(/no finite tree/);
    expect(err.message).toContain("Loop");
  });

  test("refuses rather than indexing a smaller vault when a listing is refused", async () => {
    // The deliberate divergence from `FsVaultSource`, which swallows a failed
    // `readdir` and indexes whatever it could still reach. Over HTTP there is no
    // directory to stat first, and the result of swallowing is a vault that answers
    // every query with zero rows and no reason.
    const server = new FakeDav(await loadCorpus()).refusing("Projects", {
      status: 403,
      statusText: "Forbidden",
    });
    const err = await rejection(() => sourceOver(server).list());

    expect(err).toBeInstanceOf(WebdavError);
    expect(err.message).toContain("PROPFIND");
    expect(err.message).toContain("403");
    expect((err as WebdavError).status).toBe(403);
  });

  test("names the collection it could not read, not the whole vault", async () => {
    const server = new FakeDav([{ path: "Projects/Note.md", content: "# n\n" }]).refusing(
      "Projects",
      {
        status: 403,
        statusText: "Forbidden",
      },
    );
    const err = await rejection(() => sourceOver(server).list());
    expect(err.message).toContain("Projects");
  });
});

describe("list()", () => {
  test("matches the filesystem backend over the testing vault, path for path", async () => {
    // The oracle. `test/vault` is what `obsidian base:query` answers from, and the
    // corpus loader only ever hands the fake what `FsVaultSource.list` will show.
    const server = new FakeDav(await loadCorpus());
    expect(await sourceOver(server).list()).toEqual(await new FsVaultSource(VAULT_DIR).list());
  });

  test("filters exactly as isIndexable does, dot-directories and extensions included", async () => {
    // The same tree the equivalence suite pins for fs and the in-memory source,
    // because "the same" is only meaningful across all three.
    const tree: Record<string, string> = {
      ".obsidian/app.json": "{}",
      ".hidden/Note.md": "# hidden",
      "Notes/Alpha.md": "# alpha",
      "Notes/Deep/Beta.md": "# beta",
      "Notes/Deep/Notes.base": "views: []",
      "Notes/scratch.txt": "not a note",
      "Templates/template.md": "# template",
    };
    const files = Object.entries(tree).map(([path, content]) => ({ path, content }));
    const expected = files
      .map((f) => f.path)
      .filter(isIndexable)
      .sort();

    expect(expected).toEqual([
      "Notes/Alpha.md",
      "Notes/Deep/Beta.md",
      "Notes/Deep/Notes.base",
      "Templates/template.md",
    ]);
    expect(await sourceOver(new FakeDav(files)).list()).toEqual(expected);
  });

  test("does not descend into a dot-directory", async () => {
    // `FsVaultSource.walk` skips one; a backend that listed it would find
    // `.obsidian/workspace.md` in `list()` and index a file Obsidian does not.
    const server = new FakeDav([
      { path: ".obsidian/workspace.md", content: "# not a note\n" },
      { path: "Note.md", content: "# note\n" },
    ]);
    expect(await sourceOver(server).list()).toEqual(["Note.md"]);
    expect(server.requests.map((r) => r.url)).toEqual([`${BASE_URL}/`]);
  });

  test("is sorted, because order feeds link resolution and every unsorted view", async () => {
    const paths = await sourceOver(new FakeDav(await loadCorpus())).list();
    expect(paths).toEqual([...paths].sort());
  });

  test("is a snapshot until a write, and refresh() re-reads it", async () => {
    const server = new FakeDav(await loadCorpus());
    const source = sourceOver(server);
    const before = await source.list();

    await source.writeText("Brand New.md", "# new\n");
    expect(await source.list()).toEqual([...before, "Brand New.md"].sort());

    await source.refresh();
    expect(await source.list()).toEqual([...before, "Brand New.md"].sort());
  });
});

describe("readText()", () => {
  test("serves the stored bytes, encoding a path rather than guessing at it", async () => {
    const content = "---\ntitle: Caf\u00e9\n---\n\n# Caf\u00e9\n";
    const server = new FakeDav([{ path: "Caf\u00e9/50%.md", content }]);
    const source = sourceOver(server);

    expect(await source.readText("Caf\u00e9/50%.md")).toBe(content);
    expect(server.requests[0]?.url).toBe(`${BASE_URL}/Caf%C3%A9/50%25.md`);
    expect(server.requests[0]?.method).toBe("GET");
    expect(server.requests[0]?.depth).toBeUndefined();
  });

  test("a note's name cannot truncate the request with a # or a ?", async () => {
    const server = new FakeDav([{ path: "Q1 #2.md", content: "# q\n" }]);
    await sourceOver(server).readText("Q1 #2.md");
    expect(server.requests[0]?.url).toBe(`${BASE_URL}/Q1%20%232.md`);
  });

  test("caches under the normalised path, so two spellings are one note", async () => {
    const server = new FakeDav([{ path: "Root Ticket.md", content: "# ticket\n" }]);
    const source = sourceOver(server);

    expect(await source.readText("Tickets/../Root Ticket.md")).toBe("# ticket\n");
    expect(server.requests.length).toBe(1);
    expect(await source.readText("Root Ticket.md")).toBe("# ticket\n");
    expect(server.requests.length).toBe(1);
  });

  test("a missing note is a structured 404", async () => {
    const err = await rejection(() => sourceOver(new FakeDav()).readText("Nope.md"));
    expect(err).toBeInstanceOf(WebdavError);
    expect((err as WebdavError).status).toBe(404);
  });
});

describe("stat()", () => {
  test("reports the size and mtime the server sent, at the server's own resolution", async () => {
    const content = "---\ntitle: \u00f6\n---\n";
    const server = new FakeDav([{ path: "Note.md", content }]);
    const source = sourceOver(server);

    expect(await source.stat("Note.md")).toEqual({
      size: new TextEncoder().encode(content).length,
      mtime: new Date(SERVER_MTIME_MS),
    });
  });

  test("asks for the resource itself, not its neighbours", async () => {
    // Depth 0: a `Depth: 1` stat of a file is harmless but a `Depth: 1` stat of a
    // collection would silently return the wrong resource.
    const server = new FakeDav([{ path: "Note.md", content: "# n\n" }]);
    await sourceOver(server).stat("Note.md");
    expect(server.requests[0]?.depth).toBe("0");
  });

  test("reports the server's mtime verbatim, never the local clock", async () => {
    // mtime is the one field the two backends cannot agree on: fs reads the local
    // clock, a server reports its own. Massaging the value towards the local clock
    // would make them agree on an instant that never happened.
    const server = new FakeDav([{ path: "Note.md", content: "# n\n" }]).withMtime(
      "Tue, 01 Oct 2024 12:00:00 +0200",
    );
    const stat = await sourceOver(server).stat("Note.md");
    expect(stat.mtime.toISOString()).toBe("2024-10-01T10:00:00.000Z");
  });

  test("refuses a collection, a missing size and a missing mtime, each by name", async () => {
    // Each of the three is a server declining to say, and each is refused BY NAME
    // rather than filled in: a `FileStat` with a plausible zero or epoch in it is a
    // wrong answer that looks like data.
    const withCollection = sourceOver(
      new FakeDav([{ path: "Projects/Note.md", content: "# n\n" }]),
    );
    const collection = await rejection(() => withCollection.stat("Projects"));
    expect(collection.message).toMatch(/reported a collection/);

    const noProperty = new WebdavVaultSource({
      url: BASE_URL,
      fetch: async () => new Response(NO_SIZE_PROPFIND, { status: 207 }),
    });
    const err = await rejection(() => noProperty.stat("Note.md"));
    expect(err.message).toMatch(/did not report getcontentlength/);

    const noMtime = new WebdavVaultSource({
      url: BASE_URL,
      fetch: async () => new Response(NO_MTIME_PROPFIND, { status: 207 }),
    });
    expect((await rejection(() => noMtime.stat("Note.md"))).message).toMatch(
      /did not report getlastmodified/,
    );
  });

  test("refuses a listing that names no resource at all", async () => {
    const empty = new WebdavVaultSource({
      url: BASE_URL,
      fetch: async () =>
        new Response('<?xml version="1.0"?><D:multistatus xmlns:D="DAV:"/>', { status: 207 }),
    });
    expect((await rejection(() => empty.stat("Note.md"))).message).toMatch(/returned no resource/);
  });

  test("refuses an unparsable mtime rather than yielding an Invalid Date", async () => {
    const server = new FakeDav([{ path: "Note.md", content: "# n\n" }]).withMtime("whenever");
    const err = await rejection(() => sourceOver(server).stat("Note.md"));
    expect(err.message).toMatch(/not a date this client can read/);
  });
});

describe("hash()", () => {
  test("is the shared contentHash, so a hash means the same on both backends", async () => {
    const content = "# Root ticket\n";
    const server = new FakeDav([{ path: "Root Ticket.md", content }]);
    const source = sourceOver(server);

    expect(await source.hash("Root Ticket.md")).toBe(contentHash(content));
    expect(await source.hash("Root Ticket.md")).toMatch(/^[0-9a-f]{32}$/);
  });

  test("agrees with the filesystem backend on every file of the testing vault", async () => {
    const corpus = await loadCorpus();
    const server = new FakeDav(corpus);
    const source = sourceOver(server);
    const fs = new FsVaultSource(VAULT_DIR);

    for (const file of corpus) {
      expect(await source.hash(file.path)).toBe(await fs.hash(file.path));
    }
  });

  test("reads once, and never an ETag", async () => {
    const server = new FakeDav([{ path: "Root Ticket.md", content: "# Root ticket\n" }]);
    const source = sourceOver(server);

    await source.hash("Root Ticket.md");
    await source.hash("Root Ticket.md");
    expect(server.requests.length).toBe(1);
    expect(server.requests[0]?.body).toBeUndefined();
  });

  test("follows the content, so a changed note hashes differently", async () => {
    const server = new FakeDav([{ path: "Note.md", content: "# one\n" }]);
    const source = sourceOver(server);
    const before = await source.hash("Note.md");

    await source.writeText("Note.md", "# two\n");
    expect(await source.hash("Note.md")).not.toBe(before);
    expect(await source.hash("Note.md")).toBe(contentHash("# two\n"));
  });
});

describe("writeText()", () => {
  test("PUTs the bytes, then reads them back before reporting success", async () => {
    const server = new FakeDav();
    const source = sourceOver(server);
    await source.writeText("Note.md", "# note\n");

    expect(server.requests.map((r) => [r.method, r.url])).toEqual([
      ["PUT", `${BASE_URL}/Note.md`],
      ["GET", `${BASE_URL}/Note.md`],
    ]);
    expect(server.requests[0]?.body).toBe("# note\n");
    expect(server.stored("Note.md")).toBe("# note\n");
  });

  test("refuses a write the server accepted and did not perform", async () => {
    // The one failure a client cannot see in the response: `201 Created` for bytes
    // that are not there. This is why the read-back exists and why `hash()` is on
    // the interface at all.
    const data = "# requested\n";
    const swapped = "# swapped by the server\n";
    const lying: WebdavFetch = async (_input, init) => {
      const method = (init?.method ?? "GET").toUpperCase();
      if (method === "PUT") return refusal(201, "Created");
      return new Response(swapped, { status: 200, statusText: "OK" });
    };
    const source = new WebdavVaultSource({ url: BASE_URL, fetch: lying });

    const err = await rejection(() => source.writeText("Root Ticket.md", data));
    expect(err.message).toContain("accepted but the bytes read back are not");
    expect(err.message).toContain(contentHash(data));
    expect(err.message).toContain(contentHash(swapped));
    expect(err.message).not.toContain(data);
  });

  test("a refused write does not enter the caches, so the vault is not left lying", async () => {
    const swapped = "# swapped by the server\n";
    const source = new WebdavVaultSource({
      url: BASE_URL,
      fetch: async (_input, init) => {
        switch ((init?.method ?? "GET").toUpperCase()) {
          case "PUT":
            return refusal(201, "Created");
          case "PROPFIND":
            return new Response(EMPTY_LISTING, { status: 207 });
          default:
            return new Response(swapped, { status: 200, statusText: "OK" });
        }
      },
    });

    await rejection(() => source.writeText("Note.md", "# requested\n"));
    expect(await source.readText("Note.md")).toBe(swapped);
    expect(await source.hash("Note.md")).toBe(contentHash(swapped));
    expect(await source.list()).toEqual([]);
  });

  test("overwrites silently, as a PUT does and as the filesystem backend does", async () => {
    const server = new FakeDav([{ path: "Note.md", content: "first\n" }]);
    const source = sourceOver(server);

    await source.writeText("Note.md", "second\n");
    expect(await source.readText("Note.md")).toBe("second\n");
    expect(await source.list()).toEqual(["Note.md"]);
  });

  test("refuses a path that names the collection rather than a note", async () => {
    // `PUT` and `DELETE` with no name address the collection, which is a request
    // with no honest reading. Refused before any request goes out, so a mistyped
    // path cannot put a body where a directory is.
    const server = new FakeDav();
    const source = sourceOver(server);
    expect((await rejection(() => source.writeText("", "# n\n"))).message).toMatch(
      /collection, not a file/,
    );
    expect((await rejection(() => source.delete(""))).message).toMatch(/collection, not a file/);
    expect(server.requests).toEqual([]);
  });

  test("a refused PUT surfaces the server's own status", async () => {
    const server = new FakeDav().refusing("Note.md", {
      status: 507,
      statusText: "Insufficient Storage",
    });
    const err = await rejection(() => sourceOver(server).writeText("Note.md", "# n\n"));
    expect((err as WebdavError).status).toBe(507);
    expect(err.message).toContain("507");
  });

  test("creates a note in a collection that does not exist yet, and says so", async () => {
    // A PUT into a missing collection is a 409, so the note is not written -- and
    // the failure is reported rather than swallowed into an empty vault.
    const server = new FakeDav();
    const err = await rejection(() => sourceOver(server).writeText("Sandbox/Deep/N.md", "# n\n"));
    expect((err as WebdavError).status).toBe(409);
    expect(server.stored("Sandbox/Deep/N.md")).toBeUndefined();
  });
});

describe("ensureDir()", () => {
  test("creates every level, because MKCOL has no recursive form", async () => {
    const server = new FakeDav();
    await sourceOver(server).ensureDir("Sandbox/Deep/Nested");

    expect(server.requests.map((r) => [r.method, r.url])).toEqual([
      ["MKCOL", `${BASE_URL}/Sandbox`],
      ["MKCOL", `${BASE_URL}/Sandbox/Deep`],
      ["MKCOL", `${BASE_URL}/Sandbox/Deep/Nested`],
    ]);
    expect(server.hasDir("Sandbox/Deep/Nested")).toBe(true);
  });

  test("a collection that is already there is a success, not a failure", async () => {
    // MKCOL answers 405 for that, and `Resolver.createNote` re-asks for a
    // collection that already exists on every note it writes into it.
    const server = new FakeDav();
    const source = sourceOver(server);

    await source.ensureDir("Sandbox/Deep");
    await source.ensureDir("Sandbox/Deep");
    expect(server.requests.every((r) => r.method === "MKCOL")).toBe(true);
  });

  test("a root-level path creates no collection, as createNote expects", async () => {
    // `createNote` only calls `ensureDir` for a path with a separator. Slicing
    // unconditionally would give a root note the "parent" `Note.m`.
    const server = new FakeDav();
    await sourceOver(server).ensureDir("");
    expect(server.requests).toEqual([]);
  });

  test("any other refusal still fails, so a 409 is not mistaken for success", async () => {
    const server = new FakeDav().refusing("Sandbox/Deep", {
      status: 403,
      statusText: "Forbidden",
    });
    const err = await rejection(() => sourceOver(server).ensureDir("Sandbox/Deep/Nested"));
    expect((err as WebdavError).status).toBe(403);
  });
});

describe("delete()", () => {
  test("DELETEs the file, and forgets it", async () => {
    const server = new FakeDav([{ path: "Note.md", content: "# n\n" }]);
    const source = sourceOver(server);
    expect(await source.list()).toEqual(["Note.md"]);

    await source.delete("Note.md");
    expect(server.requests.at(-1)?.method).toBe("DELETE");
    expect(server.stored("Note.md")).toBeUndefined();
    expect(await source.list()).toEqual([]);
  });

  test("deleting nothing succeeds, because the desired state already holds", async () => {
    // A deliberate divergence from `FsVaultSource`, which runs `rm --force`. A
    // WebDAV DELETE answers 404 here; tolerating it reports the state the caller
    // asked for. `VaultSource.delete` has no caller in `src/` yet, so nothing
    // depends on the choice, and the note on the method says so.
    const source = sourceOver(new FakeDav());
    await source.delete("Never Existed.md");
  });

  test("a refusal is still a refusal", async () => {
    const server = new FakeDav([{ path: "Note.md", content: "# n\n" }]).refusing("Note.md", {
      status: 403,
      statusText: "Forbidden",
    });
    const err = await rejection(() => sourceOver(server).delete("Note.md"));
    expect((err as WebdavError).status).toBe(403);
  });
});

describe("credentials", () => {
  test("sends HTTP Basic auth on every request", async () => {
    const server = new FakeDav([{ path: "Note.md", content: "# n\n" }]);
    const source = sourceOver(server);
    await source.list();
    await source.readText("Note.md");

    const expected = `Basic ${Buffer.from(`${USER}:${PASSWORD}`, "utf8").toString("base64")}`;
    expect(server.requests.length).toBeGreaterThan(1);
    for (const request of server.requests) {
      expect(request.authorization).toBe(expected);
    }
  });

  test("never appears in an error message, whatever failed", async () => {
    // A credential in a tool result is a credential in an agent's transcript, and
    // from there in a log somewhere. Every refusal path is checked, not just one.
    const server = new FakeDav([{ path: "Note.md", content: "# n\n" }])
      .refusing("", { status: 403, statusText: "Forbidden" })
      .refusing("Note.md", { status: 403, statusText: "Forbidden" })
      .refusing("Projects", { status: 403, statusText: "Forbidden" });
    const source = sourceOver(server);

    const failures = [
      () => source.list(),
      () => source.readText("Note.md"),
      () => source.stat("Note.md"),
      () => source.hash("Note.md"),
      () => source.writeText("Note.md", "# n\n"),
      () => source.ensureDir("Projects"),
      () => source.delete("Note.md"),
    ];
    for (const failure of failures) {
      const err = await rejection(failure);
      expect(err.message).not.toContain(PASSWORD);
      expect(err.message).not.toContain(USER);
      expect(err.message).toContain("WebDAV");
    }
  });

  test("never appears in a URL either", async () => {
    const server = new FakeDav([{ path: "Note.md", content: "# n\n" }]);
    await sourceOver(server).readText("Note.md");
    for (const request of server.requests) {
      expect(request.url).not.toContain(PASSWORD);
      expect(request.url).toBe(`${BASE_URL}/Note.md`);
    }
  });

  test("a URL carrying credentials is refused rather than honoured", async () => {
    // Honouring it would put the password inside `url.toString()`, and this file
    // builds messages from strings. Keeping the credential in one place is what
    // makes "never log it" a property of the code rather than of care.
    expect(
      () => new WebdavVaultSource({ url: "https://agent:secret@dav.example/dav/vault" }),
    ).toThrow(/carries credentials/);
  });

  test("a username without a password is a misconfiguration, not anonymous access", async () => {
    expect(() => new WebdavVaultSource({ url: BASE_URL, user: USER })).toThrow(/or neither/);
    expect(() => new WebdavVaultSource({ url: BASE_URL, password: PASSWORD })).toThrow(
      /or neither/,
    );
    expect(() => new WebdavVaultSource({ url: BASE_URL })).not.toThrow();
  });

  test("an unauthenticated source sends no Authorization header at all", async () => {
    const server = new FakeDav([{ path: "Note.md", content: "# n\n" }]);
    await new WebdavVaultSource({ url: BASE_URL, fetch: server.fetch }).readText("Note.md");
    expect(server.requests[0]?.authorization).toBeUndefined();
  });
});

describe("configuration", () => {
  test("refuses a URL that is not http or https", async () => {
    // A `file:` URL would resolve against the local disk through a backend that
    // promises to be a network one, which is exactly the substitution the entrypoint
    // refuses to make silently.
    expect(() => new WebdavVaultSource({ url: "file:///tmp/vault" })).toThrow(/must be http/);
  });

  test("tolerates a trailing slash on the base URL, which changes nothing", async () => {
    const server = new FakeDav([{ path: "Note.md", content: "# n\n" }]);
    const source = new WebdavVaultSource({ url: `${BASE_URL}/`, fetch: server.fetch });
    await source.readText("Note.md");
    expect(server.requests[0]?.url).toBe(`${BASE_URL}/Note.md`);
  });

  test("abandons a request that outlives its budget, and says so", async () => {
    // A wedged PROPFIND would otherwise hang a tool call forever, with nothing to
    // report and no way to tell the agent to retry.
    const source = new WebdavVaultSource({
      url: BASE_URL,
      timeoutMs: 20,
      fetch: (_input, init) =>
        new Promise((_resolve, reject) => {
          init?.signal?.addEventListener("abort", () =>
            reject(new Error("The operation timed out")),
          );
        }),
    });
    const err = await rejection(() => source.list());
    expect(err.message).toContain("got no response");
  });
});

describe("the WebDAV backend behind the Resolver", () => {
  test("resolves the testing vault to the same answers as the filesystem backend", async () => {
    // The equivalence suite proves this for a `VaultSource` with no HTTP in it. Here
    // the whole stack is present -- hrefs, XML, `Depth: 1` recursion, `getcontentlength`
    // -- and the oracle is the same `test/vault` Obsidian itself answers from.
    const source = sourceOver(new FakeDav(await loadCorpus()));
    const ours = await Resolver.open(source);
    const theirs = await Resolver.openDir(VAULT_DIR);

    for (const view of [undefined, "All", "ByPriority", "AsList"]) {
      expect(await ours.query("AllNotes.base", { view })).toEqual(
        await theirs.query("AllNotes.base", { view }),
      );
      expect(await ours.render("AllNotes.base", { view })).toBe(
        await theirs.render("AllNotes.base", { view }),
      );
    }
    for (const host of ["Projects/SomeProject.md", "Projects/OtherProject.md", "Root Project.md"]) {
      expect(await ours.query("Tickets.base", { context: host })).toEqual(
        await theirs.query("Tickets.base", { context: host }),
      );
    }
  });

  test("projects a note carrying a Base region the same way the filesystem backend does", async () => {
    const source = sourceOver(new FakeDav(await loadCorpus()));
    const ours = await Resolver.open(source);
    const theirs = await Resolver.openDir(VAULT_DIR);

    expect(await ours.readNote("Root Project.md")).toEqual(
      await theirs.readNote("Root Project.md"),
    );
    expect(await ours.readNote("Root Project.md")).not.toEqual(
      await ours.readNote("Root Project.md", { raw: true }),
    );
  });

  test("createNote over WebDAV creates the collections the note needs", async () => {
    // The write path a real vault takes: a collection, then a note, then a re-read
    // that has to see both.
    const source = sourceOver(new FakeDav(await loadCorpus()));
    const resolver = await Resolver.open(source);
    const path = "Sandbox/Deep/Nested/New Note.md";

    await resolver.createNote(path, "---\ntags:\n  - ticket\n---\n\n# New\n");

    expect(await source.readText(path)).toContain("# New");
    expect(await source.list()).toContain(path);
    // The new note is a real row in the oracle base, not just a path in `list()`.
    expect((await resolver.query("AllNotes.base")).rows.map((r) => r.path)).toContain(path);
  });
});

// ---------------------------------------------------------------------------
// Tier 2: a live server
// ---------------------------------------------------------------------------

const liveUrl = process.env["BASES_MCP_WEBDAV_URL"];
const liveUser = process.env["BASES_MCP_WEBDAV_USER"];
const livePassword = process.env["BASES_MCP_WEBDAV_PASSWORD"];

/**
 * A test that needs a real WebDAV server.
 *
 * Gated with `skipIf`, never with an early `return` in the body. An early return
 * makes the test report PASS having asserted nothing, so an absent server would
 * read as a green suite that compared nothing -- the bug this project already fixed
 * once, in the parity suite.
 */
const liveTest = test.skipIf(!liveUrl);

/** Over the live server, or skipped. Never a `throw`: a skip is not a failure. */
function liveSource(): WebdavVaultSource {
  return new WebdavVaultSource({ url: liveUrl ?? "", user: liveUser, password: livePassword });
}

describe("against a live WebDAV server", () => {
  liveTest("list() matches the filesystem backend over the testing vault", async () => {
    expect(await liveSource().list()).toEqual(await new FsVaultSource(VAULT_DIR).list());
  });

  liveTest("every oracle view resolves identically", async () => {
    const ours = await Resolver.open(liveSource());
    const theirs = await Resolver.openDir(VAULT_DIR);

    for (const view of [undefined, "All", "ByPriority", "AsList"]) {
      expect(await ours.query("AllNotes.base", { view })).toEqual(
        await theirs.query("AllNotes.base", { view }),
      );
    }
    for (const host of ["Projects/SomeProject.md", "Projects/OtherProject.md"]) {
      expect(await ours.query("Tickets.base", { context: host })).toEqual(
        await theirs.query("Tickets.base", { context: host }),
      );
    }
  });

  liveTest("stat() reports a size and a date the server will vouch for", async () => {
    const stat = await liveSource().stat("Root Ticket.md");
    expect(stat.size).toBeGreaterThan(0);
    expect(Number.isNaN(stat.mtime.getTime())).toBe(false);
  });

  liveTest("a note written is a note read back, and gone once deleted", async () => {
    // The write path with a server that can disagree: the read-back is what makes
    // this a test rather than a hope.
    const source = liveSource();
    const path = "Sandbox/Deep/Webdav Scratch.md";
    const content = "---\ntags:\n  - ticket\n---\n\n# Scratch\n";

    await source.ensureDir("Sandbox/Deep");
    await source.writeText(path, content);
    try {
      expect(await source.readText(path)).toBe(content);
      expect(await source.hash(path)).toBe(contentHash(content));
      expect((await source.stat(path)).size).toBe(new TextEncoder().encode(content).length);
      expect(await source.list()).toContain(path);
    } finally {
      await source.delete(path);
    }
    expect(await source.list()).not.toContain(path);
  });

  liveTest("a missing note is a 404, and an escape is refused before any request", async () => {
    const source = liveSource();
    expect((await rejection(() => source.readText("Nope.md"))).message).toMatch(/404/);
    expect((await rejection(() => source.readText("../outside.md"))).message).toMatch(
      /escapes the vault root/,
    );
  });
});

/** `rejection` for a synchronous call, which `async` above cannot express. */
function rejectionOf(run: () => unknown): Error {
  try {
    run();
  } catch (err) {
    return err as Error;
  }
  throw new Error("Expected a throw, but the call returned");
}
