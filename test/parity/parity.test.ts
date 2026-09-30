/**
 * Parity against the live Obsidian CLI.
 *
 * Only `AllNotes.base` is a genuine oracle. `Tickets.base` scopes itself with
 * `this`, which the CLI cannot bind at all -- it returns `[]` -- so it is
 * asserted as the KNOWN DIVERGENCE rather than compared for equality.
 *
 * These tests need the testing vault registered in Obsidian. They skip cleanly
 * when the CLI is absent or the vault is not open, so the unit suite stays
 * runnable anywhere.
 */

import { describe, expect, test } from "bun:test";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

import { Resolver } from "../../src/service";
import { ObsidianCli } from "./cli";

const HERE = dirname(fileURLToPath(import.meta.url));
const VAULT_DIR = join(HERE, "..", "vault");
/** The name Obsidian knows the testing vault by. */
const CLI_VAULT = "vault";
/**
 * Each `obsidian base:query` round-trip costs ~2s because the CLI routes
 * through the running app, which is far over bun's 5s default timeout.
 */
const CLI_TIMEOUT_MS = 60_000;

const cli = new ObsidianCli({ vault: CLI_VAULT });
const available = await cli.available();
const resolver = await Resolver.openDir(VAULT_DIR);

if (!available) {
  console.warn("[parity] Obsidian CLI unavailable -- the CLI tests below report as SKIPPED.");
}

/**
 * A test that needs the Obsidian CLI.
 *
 * Gating with `skipIf` rather than an early `return` inside the body matters:
 * an early return makes the test report PASS with zero assertions, so a missing
 * CLI reads as a green parity suite when nothing was actually compared. This is
 * the project's parity oracle, so "did not run" has to be visible.
 */
const cliTest = test.skipIf(!available);

describe("parity: AllNotes.base (a genuine oracle -- no `this`)", () => {
  for (const view of ["All", "ByPriority", "AsList"]) {
    cliTest(
      `view "${view}" matches format=json exactly`,
      async () => {
        const base = await resolver.loadBase("AllNotes.base");
        const ours = resolver.toJson(await resolver.query("AllNotes.base", { view }), base);
        const { rows: theirs } = await cli.queryJson("AllNotes.base", view);
        expect(ours).toEqual(theirs);
      },
      CLI_TIMEOUT_MS,
    );
  }

  cliTest(
    "the default view matches format=json",
    async () => {
      const base = await resolver.loadBase("AllNotes.base");
      const ours = resolver.toJson(await resolver.query("AllNotes.base"), base);
      const { rows: theirs } = await cli.queryJson("AllNotes.base");
      expect(ours).toEqual(theirs);
    },
    CLI_TIMEOUT_MS,
  );

  cliTest(
    "row ORDER is asserted, not just the row set",
    async () => {
      const ours = (await resolver.query("AllNotes.base", { view: "AsList" })).rows.map(
        (r) => r.path,
      );
      const { rows: theirs } = await cli.queryJson("AllNotes.base", "AsList");
      expect(ours).toEqual(theirs.map((r) => (r as { path: string }).path));
    },
    CLI_TIMEOUT_MS,
  );

  test("an unsorted view orders by file.name, not by path", async () => {
    // The vault deliberately contains `Root Ticket.md`, whose path order and
    // name order disagree, so this asserts the rule rather than the accident.
    const names = (await resolver.query("AllNotes.base", { view: "AsList" })).rows.map((r) => ({
      path: r.path,
      name: r.values["file.name"],
    }));
    const byName = [...names].sort((a, b) => (String(a.name) < String(b.name) ? -1 : 1));
    expect(names).toEqual(byName);
  });

  for (const view of ["All", "ByPriority", "AsList"]) {
    cliTest(
      `view "${view}" matches format=md exactly`,
      async () => {
        const ours = await resolver.render("AllNotes.base", { view });
        const theirs = await cli.queryMarkdown("AllNotes.base", view);
        expect(ours.trim()).toBe(theirs.trim());
      },
      CLI_TIMEOUT_MS,
    );
  }

  test(
    "format=md flattens groupBy and renders list as a table",
    async () => {
      // The CLI's markdown export is lossy by design. We reproduce that on the
      // `resolve_base` surface and assert it here so the loss is deliberate
      // rather than an accident someone later "fixes".
      const grouped = await resolver.render("AllNotes.base", { view: "ByPriority" });
      expect(grouped).not.toContain("**");
      expect(grouped.split("\n").filter((l) => l.startsWith("| ")).length).toBeGreaterThan(0);

      const asList = await resolver.render("AllNotes.base", { view: "AsList" });
      expect(asList.startsWith("|")).toBe(true);
    },
    CLI_TIMEOUT_MS,
  );
});

describe("parity: Tickets.base (the documented divergence)", () => {
  cliTest(
    "the CLI cannot bind `this` and returns an empty result",
    async () => {
      // Asserted as the divergence: this SHOULD be empty. If a future Obsidian
      // teaches the CLI to bind `this`, this test fails and the registry needs
      // updating -- which is the intended signal, not a flake.
      const { rows: theirs } = await cli.queryJson("Tickets.base");
      expect(theirs).toEqual([]);
    },
    CLI_TIMEOUT_MS,
  );

  test("we scope to the host note instead of returning nothing", async () => {
    const scoped = await resolver.query("Tickets.base", { context: "Projects/SomeProject.md" });
    expect(scoped.rows.map((r) => r.path)).toEqual([
      "Tickets/Add offline mode.md",
      "Tickets/Fix login redirect.md",
    ]);
  });

  test("the same base, a different host, yields different rows", async () => {
    const other = await resolver.query("Tickets.base", { context: "Projects/OtherProject.md" });
    expect(other.rows.map((r) => r.path)).toEqual(["Tickets/Invoice export.md"]);
  });

  test("a root-level host note scopes just like a nested one", async () => {
    const root = await resolver.query("Tickets.base", { context: "Root Project.md" });
    expect(root.rows.map((r) => r.path)).toEqual(["Root Ticket.md"]);
  });

  test("no host is a HARD ERROR, never a silent empty array", async () => {
    // Awaited deliberately: an un-awaited `.rejects` is a promise nobody
    // inspects, so a genuine regression here would report as a pass.
    await expect(resolver.query("Tickets.base")).rejects.toThrow(/this/i);
  });
});
