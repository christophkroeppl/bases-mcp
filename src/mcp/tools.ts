/**
 * The MCP tool surface.
 *
 * Six tools, one Resolver, and no logic of their own: every tool validates its
 * arguments, calls the Resolver, and shapes the answer. Keeping the tools thin is
 * what lets `Resolver` be the single place the engine's behaviour lives, so the CLI
 * parity suite and the MCP surface cannot drift apart -- they are the same code
 * path.
 *
 * Three decisions the shape of this file follows from:
 *
 * 1. **Two return channels, chosen by what the payload is.** Data (rows, a
 *    note, a write report) goes out as a fenced JSON block AND as
 *    `structuredContent`, so a text-only client and a machine client both get
 *    what they need from one call. Presentation -- `resolve_base`'s markdown --
 *    stays a single text block, because a table is not an object and dressing
 *    it as one would be a lie about its shape.
 *
 * 2. **`isError` means the tool could not do its job.** It is reserved for a
 *    thrown `BasesError`: a bad path, a view that does not exist, an unbound
 *    `this`. A `write_note` that restored a base region and applied everything
 *    else is a SUCCESS with `health: "partial-with-errors"`, because the note
 *    on disk is now what the agent asked for minus the part that is not a legal
 *    edit. Collapsing the two would teach the agent that a partial apply is a
 *    failure, and it would retry a write that already landed.
 *
 * 3. **Health is three-state everywhere.** Obsidian reports an error AND
 *    correct rows in the same response; we refuse to imitate that, so warnings
 *    demote a result to `partial-with-errors` instead of poisoning it.
 *
 * Descriptions carry what a model cannot infer from the argument names: which
 * surface is CLI-parity and which is not, that `add_note_to_base` is two calls,
 * and that a base region survives every write.
 */

import { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import type { CallToolResult } from "@modelcontextprotocol/sdk/types.js";
import { z } from "zod";

import { displayNameFor } from "../bases/labels";
import type { BaseFile } from "../bases/parse";
import type { QueryResult } from "../bases/query";
import { BasesError } from "../expr/errors";
import { renderMarkdown } from "../render/markdown";
import type { Resolver } from "../service";
import { stringified } from "../service";
import type { AddNoteToBaseResult } from "./drafts";

/**
 * How much of what was asked for actually happened.
 *
 * `ok` is a clean result. `partial-with-errors` is a result the caller may still
 * act on, carrying whatever the engine complained about alongside it.
 * `failed` means nothing was produced.
 */
type Health = "ok" | "partial-with-errors" | "failed";

/**
 * Build the MCP server for one vault.
 *
 * Takes a Resolver rather than opening one, because a server is one vault in
 * one process and the caller owns the vault lifetime: it decides when the
 * snapshot is reloaded, and this function never touches I/O outside the tools.
 */
export function createToolsServer(resolver: Resolver): McpServer {
  const server = new McpServer({ name: "bases-mcp", version: "0.1.0" });

  // Restated rather than inherited, and bound because `registerTool` reads its
  // registry off `this`. See `ToolRegistrar`.
  const registerTool = server.registerTool.bind(server) as ToolRegistrar;

  registerTool(
    "list_bases",
    {
      title: "List bases",
      description:
        "List every `.base` file in the vault with its views and layout types. A base queries the " +
        "WHOLE vault -- there is no `from` clause -- so this is the only way to find out what " +
        "exists before resolving one. Pass a `view` name from this list verbatim to resolve_base; " +
        "with no `view`, resolve_base uses the base's first view.",
    },
    () =>
      attempt(async () => {
        const bases = await resolver.listBases();
        return data(
          { bases, health: "ok" satisfies Health },
          `Every base queries the whole vault. ${bases.length} base(s) found.`,
        );
      }),
  );

  registerTool(
    "resolve_base",
    {
      title: "Resolve a base view",
      description:
        "Resolve one view of a base into rows. Two surfaces, and the difference matters:\n" +
        "- format=markdown is the FLAT, CLI-parity surface: byte-comparable with " +
        "`obsidian base:query format=md`, which collapses EVERY view type into one centred table " +
        "and drops group headers and summaries.\n" +
        "- Bases embedded in a note are the opposite -- get_note renders them STRUCTURED, with " +
        "group headers and a summaries footer. Use markdown here to read a whole table; use " +
        "get_note to read a base in the note it lives in.\n" +
        "`context` is the host note that binds `this`. A base whose filter references `this` " +
        "REQUIRES it: with no host note this tool fails rather than returning the empty result the " +
        "Obsidian CLI returns, because an empty result is indistinguishable from a base that " +
        "genuinely matches nothing.\n" +
        "`includeAllFormulas` adds a column for every formula the base declares, not just those in " +
        "the view's `order`. It applies to format=json only -- markdown is the parity surface and " +
        "shows exactly `order`.\n" +
        "`health` is `ok`, `partial-with-errors` (rows resolved, but the base reported warnings) " +
        "or `failed`.",
      inputSchema: {
        base: z.string().describe("Path to the `.base` file, as listed by list_bases."),
        view: z.string().optional().describe("View name. Defaults to the base's first view."),
        context: z
          .string()
          .optional()
          .describe("Host note path binding `this`. Required for a base that references `this`."),
        format: z
          .enum(["markdown", "json"])
          .default("markdown")
          .describe(
            "`markdown` for the flat CLI-parity table, `json` for rows keyed by display label.",
          ),
        includeAllFormulas: z
          .boolean()
          .default(false)
          .describe("json only: add a column for every formula the base declares."),
      },
    },
    (args) =>
      attempt(async () => {
        const path = resolver.resolveBasePath(args.base);
        const base = await resolver.loadBase(path);
        // One query for both formats. `Resolver.render` is these two lines; it is inlined so the
        // `QueryResult` survives to the health field, where its warnings decide ok vs
        // partial-with-errors. The markdown is byte-identical either way.
        const result = await resolver.query(path, { view: args.view, context: args.context });
        const health: Health = result.warnings.length > 0 ? "partial-with-errors" : "ok";

        if (args.format === "markdown") {
          return {
            content: [
              { type: "text", text: renderMarkdown(base, result, "flat") },
              { type: "text", text: healthNote(result, health) },
            ],
            structuredContent: summarise(result, health),
          };
        }

        const rows = withAllFormulas(
          resolver.toJson(result, base),
          result,
          base,
          args.includeAllFormulas,
        );
        return data({ rows, ...summarise(result, health) });
      }),
  );

  registerTool(
    "get_note",
    {
      title: "Get a note",
      description:
        "Read a note as a Projection: every base region is replaced by a `base-rendered` fence " +
        "carrying its `path`, `view` and `context`, and `regions` lists that provenance -- one " +
        "entry per base region, in document order. Each embedded base binds `this` to the note it " +
        "lives in, so no host note has to be supplied here.\n" +
        "This Projection is NEVER written back to disk. Obsidian treats a `base` fence as live YAML " +
        "and would reject rendered markdown inside one. To edit a note, read it with raw=true, " +
        "edit THAT, and send it to write_note.\n" +
        "`raw: true` returns the stored text untouched and an empty `regions`.\n" +
        "`base_hash` is the hash of `raw`. Send it back as `write_note`'s `base_hash` so the " +
        "write is conditional rather than blind.",
      inputSchema: {
        path: z.string().describe("Path to the `.md` note."),
        raw: z
          .boolean()
          .default(false)
          .describe("Return the stored text with base regions intact instead of the Projection."),
      },
    },
    (args) =>
      attempt(async () => {
        const note = await resolver.readNote(args.path, { raw: args.raw });
        const payload = {
          path: note.path,
          raw: note.raw,
          content: note.content,
          base_hash: note.baseHash,
          regions: note.regions.map((r) => r.provenance),
          health: "ok" satisfies Health,
        };
        return data(
          payload,
          args.raw === true
            ? undefined
            : "`content` is a Projection. Edit `raw`, not `content`, and send that to write_note.",
        );
      }),
  );

  registerTool(
    "write_note",
    {
      title: "Write a note",
      description:
        "Apply an edit to a note. Every non-base change is written. Base regions are restored " +
        "byte-for-byte, and any attempt to change, insert or delete one is REFUSED and reported " +
        "in `refused` -- each entry carries a reason and guidance for what to do instead.\n" +
        "A refusal means that part of your edit did NOT land: the base region was put back as it " +
        "was, while the rest of your edit was applied. `health` is `partial-with-errors` whenever " +
        "anything was refused, and `isError` stays false because the write itself succeeded.\n" +
        "Rows come from notes, not from the base file. To add a row, use add_note_to_base -- it " +
        "authors a note whose properties satisfy the base's filter.\n" +
        "Send the `base_hash` from the get_note this edit is based on. Obsidian autosaves " +
        "constantly, so a note can change under you between the read and the write; with the hash " +
        "the write is REFUSED and nothing is touched, rather than overwriting whatever arrived " +
        "since. Omit it only when writing a note wholesale rather than editing one read earlier.",
      inputSchema: {
        path: z.string().describe("Path to the `.md` note to write."),
        content: z
          .string()
          .describe(
            "The agent's edited note. Send the `raw` text from get_note, never a Projection.",
          ),
        base_hash: z
          .string()
          .optional()
          .describe(
            "`base_hash` from the get_note this edit is based on. Supplying it makes the write " +
              "conditional: if the note changed in the meantime the write is refused.",
          ),
      },
    },
    (args) =>
      attempt(async () => {
        const before = await resolver.readNote(args.path, { raw: true });
        const result = await resolver.writeNote(args.path, args.content, args.base_hash);
        const health: Health = result.refused.length > 0 ? "partial-with-errors" : "ok";
        const applied = changeSummary(before.raw, result.text);

        return data(
          {
            path: result.path,
            applied,
            refusals: result.refused.map((r) => ({
              region: r.index,
              base: r.basePath,
              reason: r.reason,
              guidance: r.guidance,
            })),
            removedRegion: result.removedRegion,
            health,
          },
          refusalNote(result.refused.length, result.removedRegion),
        );
      }),
  );

  registerTool(
    "add_note_to_base",
    {
      title: "Add a note to a base",
      description:
        "Add a row to a base by creating a note its filter actually matches. A row IS a note, so " +
        "this is a TWO-CALL HANDSHAKE, never one:\n" +
        "1. Call with `base` and `path` (plus `context` when the base references `this`). You get " +
        "back a proposed note and a `draft_id`. NOTHING has been written.\n" +
        "2. Edit the proposed note, then call again with ONLY `draft_id` and `content`.\n" +
        "The second call runs the base's real filter against your frontmatter and writes only on a " +
        "match, so a note that could never be a row is never created. On a mismatch the error names " +
        "the expressions that failed and inlines the base's own filter; nothing is written and the " +
        "draft stays live for 30 minutes, so you can correct the frontmatter and resend the same " +
        "`draft_id`.\n" +
        "Branch on the result: `written` present means the note was created; absent means you are " +
        "holding a draft to edit.",
      inputSchema: {
        base: z
          .string()
          .optional()
          .describe("Path to the `.base` the note has to match. First call only."),
        path: z
          .string()
          .optional()
          .describe("Vault-relative `.md` path to create. First call only."),
        context: z
          .string()
          .optional()
          .describe("Host note binding `this`. First call only; required for a scoped base."),
        view: z.string().optional().describe("View whose filters the note must satisfy."),
        draft_id: z
          .string()
          .optional()
          .describe("The id from the first call. Sending it selects the commit path."),
        content: z.string().optional().describe("Your edited note. Second call only."),
      },
    },
    (args) =>
      attempt(async () => {
        const result = await resolver.addNoteToBase(args);
        return data({ ...result, health: "ok" satisfies Health }, handshakeNote(result));
      }),
  );

  registerTool(
    "backlinks",
    {
      title: "Backlinks",
      description:
        "Inbound links to a note, from the same resolver that backs every base -- so a link that " +
        "appears here and a link that satisfies a base filter are the same fact, not two resolvers " +
        "disagreeing. An empty list means nothing links here; that is an answer, not an error.",
      inputSchema: {
        path: z.string().describe("Path to the `.md` note."),
      },
    },
    (args) =>
      attempt(async () => {
        const path = resolver.resolveNotePath(args.path);
        const backlinks = resolver.backlinks(path);
        return data(
          { path, backlinks, health: "ok" satisfies Health },
          `${backlinks.length} note(s) link to ${path}.`,
        );
      }),
  );

  return server;
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

/**
 * The slice of `McpServer.registerTool` this file uses, restated against zod v3.
 *
 * The SDK's own signature accepts `ZodRawShapeCompat | AnySchema`, and `AnySchema`
 * is a union with `zod/v4/core`'s `$ZodType` -- a structurally recursive type. Putting
 * a schema anywhere near it makes TypeScript exceed its instantiation depth: on
 * `@modelcontextprotocol/sdk` 1.31.0 with `zod` 3.25.76 and TypeScript 5.9, a single
 * `registerTool` with a two-key input schema is TS2589, and typechecking the file
 * takes 55 seconds instead of 2.5. That reproduces in a bare project with no tsconfig
 * of ours involved, so it is the SDK's typings rather than anything here.
 *
 * Restating the signature in v3-only terms keeps the one property this file actually
 * depends on -- the handler's argument type is inferred from the schema, per key --
 * and drops the union nothing in this file uses. It is asserted once, here, so the
 * cost is one line instead of six.
 */
type ToolRegistrar = <S extends Record<string, z.ZodTypeAny>>(
  name: string,
  config: { title?: string; description?: string; inputSchema?: S },
  handler: (args: { [K in keyof S]: z.output<S[K]> }) => Promise<CallToolResult>,
) => void;

// ---------------------------------------------------------------------------
// Results
// ---------------------------------------------------------------------------

/** The counts every `resolve_base` answer carries, whatever the format. */
function summarise(result: QueryResult, health: Health): Record<string, unknown> {
  return {
    base: result.basePath,
    view: result.view.name,
    type: result.view.type,
    context: result.context,
    rowCount: result.rows.length,
    total: result.total,
    health,
    warnings: result.warnings,
  };
}

/** A one-line read of the health field, for a reader who sees only the text. */
function healthNote(result: QueryResult, health: Health): string {
  if (health === "ok") {
    return `${result.rows.length} of ${result.total} row(s) -- view "${result.view.name}" (${result.view.type}).`;
  }
  return (
    `health: ${health}. ${result.rows.length} of ${result.total} row(s) resolved; the base reported:\n` +
    result.warnings.map((w) => `- ${w}`).join("\n")
  );
}

/**
 * Append a column per formula, for `includeAllFormulas`.
 *
 * `toJson` emits exactly the columns the view orders, because that is what the
 * Obsidian CLI emits. Formulas a base declares but does not order are the one
 * thing an agent cannot otherwise see, so the option adds them explicitly --
 * labelled through the same `displayNameFor` rule, and stringified the same way,
 * so the added columns are indistinguishable from the parity ones.
 */
function withAllFormulas(
  rows: unknown[],
  result: QueryResult,
  base: BaseFile,
  includeAllFormulas: boolean,
): unknown[] {
  if (!includeAllFormulas) return rows;
  const names = Object.keys(base.formulas);
  if (names.length === 0) return rows;

  return rows.map((row, i) => {
    const out: Record<string, unknown> = { ...(row as Record<string, unknown>) };
    const values = result.rows[i]?.formula;
    if (values === undefined) return out;
    for (const name of names) {
      out[displayNameFor(base, `formula.${name}`)] = stringified(values[name]);
    }
    return out;
  });
}

/**
 * How much of the agent's text survived reconciliation.
 *
 * A multiset count over lines, not a diff: it answers "did my edit land and how big
 * was it", which is the question an agent has after a partial apply. Anything
 * finer would mean owning a diff algorithm in a tool whose real job is reporting
 * refusals.
 */
function changeSummary(before: string, after: string): Record<string, unknown> {
  const counts = (text: string): Map<string, number> => {
    const m = new Map<string, number>();
    for (const line of text.split("\n")) m.set(line, (m.get(line) ?? 0) + 1);
    return m;
  };
  const diff = (from: Map<string, number>, against: Map<string, number>): number => {
    let n = 0;
    for (const [line, count] of from) n += Math.max(0, count - (against.get(line) ?? 0));
    return n;
  };
  const oldLines = counts(before);
  const newLines = counts(after);
  return {
    changed: before !== after,
    linesAdded: diff(newLines, oldLines),
    linesRemoved: diff(oldLines, newLines),
  };
}

/** The refusal count, stated plainly, so it cannot be skimmed past. */
function refusalNote(count: number, removed: boolean): string | undefined {
  if (count === 0) return undefined;
  const parts = [`${count} base region(s) were restored, not edited.`];
  if (removed) parts.push("One of them had been deleted outright.");
  parts.push("Rows come from notes: use add_note_to_base to create one that matches.");
  return parts.join(" ");
}

/** Which half of the handshake came back, named so the agent cannot mistake it. */
function handshakeNote(result: AddNoteToBaseResult): string | undefined {
  if ("written" in result) return `Wrote ${result.written}. The draft is consumed.`;
  return (
    `Draft ${result.draft_id} for ${result.path}. NOTHING has been written yet. Edit the ` +
    `proposed note and call again with only draft_id and content; it expires at ` +
    `${new Date(result.expires_at).toISOString()}.`
  );
}

// ---------------------------------------------------------------------------
// Envelopes
// ---------------------------------------------------------------------------

/**
 * A data result, on both channels.
 *
 * `structuredContent` is what a machine client reads; the fenced block is what a
 * model reads. Emitting both costs a second copy of the payload and saves every
 * consumer from parsing markdown to get at a row.
 */
function data(payload: Record<string, unknown>, warning?: string): CallToolResult {
  const text = ["```json", JSON.stringify(payload, null, 2), "```"];
  if (warning !== undefined) text.push(warning);
  return {
    content: [{ type: "text", text: text.join("\n") }],
    structuredContent: payload,
  };
}

/**
 * Run a handler, turning anything it throws into a structured tool result.
 *
 * A `BasesError` is the engine's own voice -- it names the construct, the note, the
 * view or the property that failed, and those fields are the only thing that makes a
 * bad base fixable. They are lifted onto the result so a client can act on them
 * without scraping the prose. Anything else is a bug here rather than in the vault,
 * and is reported as such.
 */
function failure(err: unknown): CallToolResult {
  if (err instanceof BasesError) {
    const context: Record<string, unknown> = {};
    if (err.construct !== undefined) context["construct"] = err.construct;
    if (err.property !== undefined) context["property"] = err.property;
    if (err.view !== undefined) context["view"] = err.view;
    if (err.note !== undefined) context["note"] = err.note;
    if (err.position !== undefined) context["position"] = err.position;
    const structured = {
      health: "failed" satisfies Health,
      error: { message: err.message, ...context },
    };
    return {
      content: [{ type: "text", text: err.message }],
      structuredContent: structured,
      isError: true,
    };
  }
  const message = err instanceof Error ? err.message : String(err);
  return {
    content: [{ type: "text", text: `Internal error: ${message}` }],
    structuredContent: { health: "failed" satisfies Health, error: { message } },
    isError: true,
  };
}

/**
 * The one place a tool is allowed to fail.
 *
 * Handlers describe intent, not recovery: every one of them wraps its body in this,
 * so a malformed base, a missing note or an unbound `this` comes back as a tool
 * result the agent can read rather than as a protocol error that takes the server
 * down with it.
 */
async function attempt(work: () => Promise<CallToolResult>): Promise<CallToolResult> {
  try {
    return await work();
  } catch (err) {
    return failure(err);
  }
}
