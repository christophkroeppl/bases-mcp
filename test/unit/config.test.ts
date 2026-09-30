/**
 * Startup configuration.
 *
 * The one thing this must get right is refusing to start against a vault the
 * caller did not name. A server that quietly falls back to some other vault
 * answers questions from it, and the client has no way to tell -- which is the
 * single confusion this project exists to prevent.
 */

import { describe, expect, test } from "bun:test";

import { type Env, parseConfig } from "../../src/index";

function env(overrides: Env): Env {
  return overrides;
}

const FS = { BASES_MCP_VAULT: "/tmp/vault" };
const DAV = {
  BASES_MCP_WEBDAV_URL: "http://localhost:5000/vault",
  BASES_MCP_WEBDAV_USER: "u",
  BASES_MCP_WEBDAV_PASSWORD: "p",
};

describe("a filesystem vault", () => {
  test("an absolute path is taken and resolved", () => {
    const r = parseConfig(env(FS));
    expect(r.ok).toBe(true);
    if (!r.ok) return;
    expect(r.config.kind).toBe("fs");
    if (r.config.kind !== "fs") return;
    expect(r.config.dir).toBe("/tmp/vault");
  });

  test("a relative path is resolved to an absolute one", () => {
    const r = parseConfig(env({ BASES_MCP_VAULT: "test/vault" }));
    expect(r.ok).toBe(true);
    if (!r.ok || r.config.kind !== "fs") return;
    expect(r.config.dir.startsWith("/")).toBe(true);
  });

  test("a blank path is a named mistake, not an absent vault", () => {
    const r = parseConfig(env({ BASES_MCP_VAULT: "   " }));
    expect(r.ok).toBe(false);
    if (r.ok) return;
    expect(r.message).toContain("BASES_MCP_VAULT");
  });
});

describe("a WebDAV vault", () => {
  test("url, user and password together are accepted", () => {
    const r = parseConfig(env(DAV));
    expect(r.ok).toBe(true);
    if (!r.ok || r.config.kind !== "webdav") return;
    expect(r.config.user).toBe("u");
    expect(r.config.password).toBe("p");
  });

  test("the base URL gets a trailing slash, so a child name cannot fuse onto it", () => {
    const r = parseConfig(env({ ...DAV, BASES_MCP_WEBDAV_URL: "http://h:5000/vault" }));
    expect(r.ok).toBe(true);
    if (!r.ok || r.config.kind !== "webdav") return;
    expect(r.config.url).toBe("http://h:5000/vault/");
  });

  test("an already-slashed URL is left alone", () => {
    const r = parseConfig(env({ ...DAV, BASES_MCP_WEBDAV_URL: "http://h:5000/vault/" }));
    expect(r.ok).toBe(true);
    if (!r.ok || r.config.kind !== "webdav") return;
    expect(r.config.url).toBe("http://h:5000/vault/");
  });

  test("a URL without a password is refused, naming what is missing", () => {
    const r = parseConfig(
      env({ BASES_MCP_WEBDAV_URL: DAV.BASES_MCP_WEBDAV_URL, BASES_MCP_WEBDAV_USER: "u" }),
    );
    expect(r.ok).toBe(false);
    if (r.ok) return;
    expect(r.message).toContain("BASES_MCP_WEBDAV_PASSWORD");
    expect(r.message).not.toContain("BASES_MCP_WEBDAV_USER and");
  });

  test("credentials inside the URL are refused, and the message never echoes them", () => {
    const r = parseConfig(
      env({ ...DAV, BASES_MCP_WEBDAV_URL: "http://sekret:alsosecret@localhost:5000/vault" }),
    );
    expect(r.ok).toBe(false);
    if (r.ok) return;
    // The password must not appear even in the refusal explaining why.
    expect(r.message).not.toContain("alsosecret");
    expect(r.message).toContain("BASES_MCP_WEBDAV_USER");
  });

  test("a non-http scheme is refused", () => {
    for (const url of ["ftp://h/vault", "file:///tmp/vault"]) {
      const r = parseConfig(env({ ...DAV, BASES_MCP_WEBDAV_URL: url }));
      expect(r.ok).toBe(false);
    }
  });

  test("an unparseable URL is refused", () => {
    const r = parseConfig(env({ ...DAV, BASES_MCP_WEBDAV_URL: "not a url" }));
    expect(r.ok).toBe(false);
  });

  test("user or password set without a URL is refused, and says so", () => {
    const r = parseConfig(env({ BASES_MCP_WEBDAV_USER: "u", BASES_MCP_WEBDAV_PASSWORD: "p" }));
    expect(r.ok).toBe(false);
    if (r.ok) return;
    expect(r.message).toContain("without BASES_MCP_WEBDAV_URL");
  });
});

describe("precedence", () => {
  test("BASES_MCP_VAULT wins, and the ignored variables are named", () => {
    const r = parseConfig(env({ ...FS, ...DAV }));
    expect(r.ok).toBe(true);
    if (!r.ok || r.config.kind !== "fs") return;
    expect(r.warnings).toHaveLength(1);
    expect(r.warnings[0]).toContain("BASES_MCP_WEBDAV_URL");
    expect(r.warnings[0]).toContain("ignored");
  });

  test("no backend at all is a usage message, never a default vault", () => {
    const r = parseConfig(env({}));
    expect(r.ok).toBe(false);
    if (r.ok) return;
    expect(r.message).toContain("BASES_MCP_VAULT");
    expect(r.message).toContain("BASES_MCP_WEBDAV_URL");
    // Critically: it must not name a concrete path to fall back to.
    expect(r.message).not.toContain("test/vault");
  });

  test("an empty environment is not treated as a filesystem vault", () => {
    expect(parseConfig(env({})).ok).toBe(false);
  });
});
