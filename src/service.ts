/**
 * The resolver: the single entry point the MCP tools call.
 *
 * Everything here is pure with respect to the vault contents -- it reads
 * through the `VaultSource` and never writes, except in `writeNote`.
 */

import { displayNameFor as labelFor } from "./bases/labels";
import { type BaseFile, type BaseView, parseBase, selectView } from "./bases/parse";
import { type QueryOptions, type QueryResult, queryBase } from "./bases/query";
import { BasesError } from "./expr/errors";
import { toDisplayString } from "./expr/evaluator";
import { type AddNoteOptions, type AddNoteToBaseResult, addNoteToBase } from "./mcp/drafts";
import { parseNoteWithEmbeds } from "./note/parse";
import { renderMarkdown } from "./render/markdown";
import {
  type BaseRegionRef,
  project,
  type ReconcileResult,
  reconcileNote,
  wrapInFence,
} from "./render/project";
import { contentHash, FsVaultSource } from "./vault/fs";
import type { VaultSource } from "./vault/source";
import { Vault } from "./vault/vault";

export interface BaseSummary {
  path: string;
  views: Array<{ name: string; type: string }>;
}

export interface NoteView {
  path: string;
  /** The note as stored. */
  raw: string;
  /** The agent-facing projection, with base regions rendered. */
  content: string;
  /** One entry per base region, describing what was rendered. */
  regions: Array<{ provenance: Record<string, unknown> }>;
  /**
   * Content hash of `raw`, for passing back as `write_note`'s `base_hash`.
   *
   * This is what makes a write conditional rather than blind. Without it the
   * server has no way to tell "the agent edited the text it read" from "the
   * agent edited a copy that has since been overwritten", and the second case
   * silently discards whatever arrived in between.
   */
  baseHash: string;
}

export class Resolver {
  private vault: Vault;

  private constructor(vault: Vault) {
    this.vault = vault;
  }

  static async open(source: VaultSource): Promise<Resolver> {
    const vault = new Vault(source);
    await vault.load();
    return new Resolver(vault);
  }

  /** Open a local vault directory. */
  static async openDir(dir: string): Promise<Resolver> {
    return Resolver.open(new FsVaultSource(dir));
  }

  get vaultSource(): Vault {
    return this.vault;
  }

  /** Re-read the vault after an external change. */
  async reload(): Promise<void> {
    await this.vault.reload();
  }

  // -- reads ---------------------------------------------------------------

  async listBases(): Promise<BaseSummary[]> {
    const out: BaseSummary[] = [];
    for (const path of this.vault.basePaths()) {
      const text = await this.vault.readText(path);
      const base = parseBase(path, text);
      out.push({
        path,
        views: base.views.map((v) => ({ name: v.name, type: v.type })),
      });
    }
    return out;
  }

  async loadBase(path: string): Promise<BaseFile> {
    const resolved = this.resolveBasePath(path);
    const text = await this.vault.readText(resolved);
    return parseBase(resolved, text);
  }

  resolveBasePath(path: string): string {
    const exact = this.vault.basePaths().find((p) => p === path);
    if (exact !== undefined) return exact;
    const resolved = this.vault.resolve(path);
    if (resolved?.endsWith(".base")) return resolved;
    throw new BasesError(`Base file not found: ${path}`);
  }

  async viewNames(path: string): Promise<string[]> {
    const base = await this.loadBase(path);
    return base.views.map((v) => v.name);
  }

  async query(path: string, options: QueryOptions = {}): Promise<QueryResult> {
    const basePath = this.resolveBasePath(path);
    const base = await this.loadBase(basePath);
    return queryBase(this.vault, basePath, base, options);
  }

  /**
   * Render a base view as markdown.
   *
   * This is the `resolve_base --format markdown` surface, so it is `flat` and
   * byte-comparable with `obsidian base:query format=md`.
   */
  async render(path: string, options: QueryOptions = {}): Promise<string> {
    const base = await this.loadBase(path);
    const result = await this.query(path, options);
    return renderMarkdown(base, result, "flat");
  }

  /**
   * Resolve a note, rendering each base region into a provenance fence.
   *
   * `raw: true` returns the stored text untouched, which is the escape hatch
   * for an agent that wants to edit the note itself.
   *
   * Both surfaces are built from a FRESH read rather than from the indexed note.
   * `base_hash` is this text's hash and `writeNote` compares it against a fresh
   * read, so serving a stale `raw` here would make every conditional write refuse
   * -- and refuse identically on the retry, because nothing between the two would
   * re-read the note.
   */
  async readNote(path: string, options: QueryOptions & { raw?: boolean } = {}): Promise<NoteView> {
    const resolved = this.resolveNotePath(path);
    const raw = await this.vault.readFresh(resolved);
    const parsed = parseNoteWithEmbeds(resolved, raw);

    if (options.raw === true) {
      return { path: resolved, raw, content: raw, regions: [], baseHash: contentHash(raw) };
    }

    const regions: Array<{ provenance: Record<string, unknown> }> = [];

    const content = await project(parsed, async (ref: BaseRegionRef) => {
      const body = await this.renderRegion(resolved, ref, options);
      const wrapped = wrapInFence(
        body,
        ref.basePath !== undefined
          ? {
              path: ref.basePath,
              view: ref.viewName ?? undefined,
              context: options.context ?? undefined,
            }
          : {
              view: ref.viewName ?? undefined,
              context: options.context ?? undefined,
            },
      );
      regions.push({ provenance: wrapped.provenance as unknown as Record<string, unknown> });
      return wrapped.text;
    });

    return { path: resolved, raw, content, regions, baseHash: contentHash(raw) };
  }

  /** Render one base region: an embed resolves the target `.base` file. */
  private async renderRegion(
    hostPath: string,
    ref: BaseRegionRef,
    options: QueryOptions,
  ): Promise<string> {
    // An inline ```base fence carries its own YAML; `this` is the host note.
    if (ref.basePath === undefined && ref.yaml !== undefined) {
      const base = parseBase(`${hostPath}#inline`, ref.yaml);
      const result = queryBase(this.vault, `${hostPath}#inline`, base, {
        ...options,
        context: options.context ?? hostPath,
      });
      return renderMarkdown(base, result, "structured");
    }

    // A file embed resolves its own target, bound to THIS note as host.
    const target = this.resolveBasePath(ref.basePath!);
    const base = await this.loadBase(target);
    const result = queryBase(this.vault, target, base, {
      ...options,
      // An embedded base always binds `this` to the note containing it, unless
      // the caller overrode it.
      context: options.context ?? hostPath,
      view: ref.viewName ?? options.view,
    });
    return renderMarkdown(base, result, "structured");
  }

  resolveNotePath(path: string): string {
    const exact = this.vault.notePaths().find((p) => p === path);
    if (exact !== undefined) return exact;
    const resolved = this.vault.resolve(path);
    if (resolved?.endsWith(".md")) return resolved;
    throw new BasesError(`Note not found: ${path}`);
  }

  backlinks(path: string): Array<{ path: string; title: string }> {
    const resolved = this.resolveNotePath(path);
    const out: Array<{ path: string; title: string }> = [];
    for (const p of this.vault.backlinksFor(resolved)) {
      if (typeof p !== "string") continue;
      out.push({ path: p, title: titleOf(p) });
    }
    return out;
  }

  // -- writes --------------------------------------------------------------

  /**
   * Apply an agent's edit to a note.
   *
   * Base regions are restored verbatim and any attempt to change them is
   * reported. The rest of the note is written as the agent left it.
   *
   * `baseHash` makes the write conditional. When it is supplied and no longer
   * matches the note on disk, the write is REFUSED and nothing is written,
   * because the agent edited a copy that has since been replaced. Obsidian
   * autosaves continuously, so that is the normal case rather than a rare one,
   * and without the check the alternative is overwriting the user's prose and
   * reporting `health: ok`.
   *
   * Omitting it is allowed, and means "I accept that this is a blind write" --
   * which is right for an agent constructing a note wholesale and wrong for one
   * editing prose it read earlier.
   */
  async writeNote(
    path: string,
    content: string,
    baseHash?: string,
  ): Promise<ReconcileResult & { path: string }> {
    const resolved = this.resolveNotePath(path);
    // Read the note as it is on disk RIGHT NOW, not as it was when this process
    // last looked. The backend caches, and the cache is populated by queries, so
    // a note the agent read a minute ago comes back stale -- while Obsidian has
    // been autosaving over the top of it. Reconciling against the stale copy and
    // writing the result destroys the user's edits and reports success.
    const original = await this.vault.readFresh(resolved);

    if (baseHash !== undefined && contentHash(original) !== baseHash) {
      throw new BasesError(
        `${resolved} changed since you read it, so nothing was written. You edited text that ` +
          `is no longer current. Read it again with get_note, reapply your edit to the new text, ` +
          `and write it back.`,
        { note: resolved, construct: "concurrent-edit" },
      );
    }

    const result = reconcileNote(resolved, original, content);

    // Verify the result still parses, so we never write a broken note.
    parseNoteWithEmbeds(resolved, result.text);

    if (result.text !== original) {
      await this.vaultSource.backend.writeText(resolved, result.text);
      await this.vault.reload();
    }
    return { ...result, path: resolved };
  }

  /** Create or replace a note verbatim. Used only by `add_note_to_base`. */
  async createNote(path: string, content: string): Promise<string> {
    // Only a path WITH a separator has a parent to create. Slicing at
    // `lastIndexOf("/")` unconditionally gives a root-level `Note.md` the
    // "parent" `Note.m`, and mkdir then creates that directory beside the note:
    // invisible to every query, because a directory is not a note.
    const slash = path.lastIndexOf("/");
    if (slash > 0 && this.vaultSource.backend.ensureDir !== undefined) {
      await this.vaultSource.backend.ensureDir(path.slice(0, slash));
    }
    await this.vaultSource.backend.writeText(path, content);
    await this.vault.reload();
    return path;
  }

  /**
   * Add a row to a Base by authoring a note its filter actually matches.
   *
   * A row IS a note, so this is a two-call handshake. The first call inverts the
   * base's filter into a proposed note and returns a `draft_id`; the agent edits
   * it and calls again with `draft_id` plus the edited content. The second call
   * runs the base's REAL filter against the draft and writes only on a match,
   * so a note that cannot be a row is never created.
   */
  async addNoteToBase(options: AddNoteOptions): Promise<AddNoteToBaseResult> {
    return addNoteToBase(this, options);
  }

  /** Serialise rows for `resolve_base --format json`. */
  toJson(result: QueryResult, base: BaseFile): unknown[] {
    const view = result.view;
    const columns = view.order ?? ["file.name", ...Object.keys(base.properties)];
    return result.rows.map((row) => {
      const out: Record<string, unknown> = { path: row.path };
      for (const id of columns) {
        const c =
          id.startsWith("file.") || id.startsWith("note.") || id.startsWith("formula.")
            ? id
            : `note.${id}`;
        const value = row.values[c] ?? row.values[`note.${id}`];
        // The CLI stringifies every value, and joins lists with ", ".
        out[displayNameOf(base, c, id)] = stringified(value);
      }
      return out;
    });
  }
}

/**
 * The JSON object key for a column.
 *
 * Obsidian keys `format=json` rows by display label, not by Property ID, so
 * `file.name` becomes `file name` and a configured `displayName` wins outright.
 * `displayNameFor` already consults both the canonical and the as-written
 * spelling of the ID, which is all the extra lookup this needed.
 */
function displayNameOf(base: BaseFile, canonicalId: string, _rawId: string): string {
  return labelFor(base, canonicalId);
}

/**
 * The string form a cell takes in `format=json`.
 *
 * Exported because two surfaces emit JSON rows -- `toJson` and the
 * `includeAllFormulas` branch of `resolve_base` -- and Obsidian stringifies every
 * value the same way. Two copies would be free to drift, and the drift would only
 * show up as an `includeAllFormulas` response disagreeing with the columns beside
 * it in the same row.
 */
export function stringified(v: unknown): string | null {
  if (v === null || v === undefined) return null;
  if (Array.isArray(v)) return v.map((x) => stringified(x) ?? "").join(", ");
  if (typeof v === "object") return JSON.stringify(v);
  return toDisplayString(v as never);
}

function titleOf(path: string): string {
  const base = path.slice(path.lastIndexOf("/") + 1);
  const dot = base.lastIndexOf(".");
  return dot <= 0 ? base : base.slice(0, dot);
}

export { type BaseView, selectView };
