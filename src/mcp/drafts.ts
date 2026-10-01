/**
 * The draft / verify / commit handshake behind `add_note_to_base`.
 *
 * A row in a Base IS a note, so "add a row" means "author a note the Base's
 * filter will actually match". The filter is the ground truth: the draft we
 * return is a best-effort INVERSION of it, and the verify step -- which runs
 * that real filter against the draft's own frontmatter -- is the only thing
 * that makes the result trustworthy.
 *
 * Two calls, never one. The first returns a draft; the agent edits it and calls
 * again with `draft_id` plus the edited content. Only then do we touch the
 * vault. Splitting it that way is the point: inversion cannot be complete
 * (arbitrary expressions are not invertible), so the agent has to see what we
 * guessed. And a verify failure names the expressions that did not match and
 * hands back the base's raw YAML, so the correction is made against the filter
 * rather than against our guess at it.
 *
 * Verification runs against an overlay vault that splices the unsaved note into
 * the real index. That is deliberate: link resolution, tag collection and
 * frontmatter coercion are exactly the machinery a filter like
 * `project.contains(link(this.file.name))` depends on, and a hand-built
 * stand-in would be a second, drifting implementation of all three.
 */

import { randomUUID } from "node:crypto";

import { stringify as stringifyYaml } from "yaml";

import { type BaseFile, type BaseView, type FilterNode, selectView } from "../bases/parse";
import { orderFormulas, resolveHostNote } from "../bases/query";
import { BasesError } from "../expr/errors";
import { type EvalContext, evaluate, isTruthy, RESERVED, toDisplayString } from "../expr/evaluator";
import {
  type CallNode,
  type LiteralNode,
  type MemberNode,
  type Node,
  parse,
  tryParse,
} from "../expr/parser";
import { type BasesValue, basename, isList, stripExtension } from "../expr/values";
import { parseNoteWithEmbeds } from "../note/parse";
import { contentHash } from "../vault/fs";
import type { FileStat, VaultSource } from "../vault/source";
import { Vault } from "../vault/vault";

// ---------------------------------------------------------------------------
// Surface
// ---------------------------------------------------------------------------

export interface AddNoteOptions {
  /**
   * Path to the `.base` the new note has to match. First call only -- the
   * second call reads the base back out of the stored draft.
   */
  base?: string;
  /**
   * Vault-relative path of the note to create. First call only, for the same
   * reason. The agent sends back `draft_id` and `content`, nothing else.
   */
  path?: string;
  /** Host note, which binds `this` for a scoped base. First call only. */
  context?: string;
  /** View whose filters the note has to satisfy. Defaults to the first view. */
  view?: string;
  /** Second call only: the id returned by the first. */
  draft_id?: string;
  /** Second call only: the agent's edited note. */
  content?: string;
}

export interface DraftProposal {
  draft_id: string;
  path: string;
  /** The proposed note. A suggestion, never a guarantee of a match. */
  content: string;
  /** Absolute expiry as epoch milliseconds. */
  expires_at: number;
  /** The resolved `.base` path the draft was inverted from. */
  base: string;
  view: string;
  context: string | null;
}

export interface DraftCommit {
  written: string;
  draft_id: string;
  verified: true;
}

export type AddNoteToBaseResult = DraftProposal | DraftCommit;

/**
 * The slice of the Resolver this module needs.
 *
 * Declared here rather than imported so the dependency runs one way: the
 * Resolver calls into this module, never the reverse.
 */
export interface DraftHost {
  readonly vaultSource: Vault;
  loadBase(path: string): Promise<BaseFile>;
  resolveBasePath(path: string): string;
  createNote(path: string, content: string): Promise<string>;
}

// ---------------------------------------------------------------------------
// The draft store
// ---------------------------------------------------------------------------

/**
 * Thirty minutes: long enough to write a note, short enough that a stale
 * `draft_id` cannot surface in a later session and author a row against a
 * filter nobody is looking at any more.
 */
export const DRAFT_TTL_MS = 30 * 60 * 1000;

export interface StoredDraft {
  id: string;
  /** Resolved `.base` path -- the filter this draft was inverted from. */
  base: string;
  /** View name, so the commit verifies against the filters we actually read. */
  view: string;
  path: string;
  context: string | null;
  /** The draft exactly as proposed, kept so a later call can diff the edits. */
  original: string;
  /** Absolute expiry, epoch milliseconds. */
  expiresAt: number;
}

export class DraftStore {
  private readonly drafts = new Map<string, StoredDraft>();

  constructor(private readonly ttlMs: number = DRAFT_TTL_MS) {}

  put(draft: Omit<StoredDraft, "id" | "expiresAt">): StoredDraft {
    this.prune();
    const record: StoredDraft = { ...draft, id: randomUUID(), expiresAt: Date.now() + this.ttlMs };
    this.drafts.set(record.id, record);
    return record;
  }

  /** Look up a live draft. Expiry is a miss, not a special case to handle. */
  get(id: string): StoredDraft {
    const draft = this.drafts.get(id);
    if (draft === undefined) throw expiredDraft(id, this.ttlMs);
    if (draft.expiresAt <= Date.now()) {
      this.drafts.delete(id);
      throw expiredDraft(id, this.ttlMs);
    }
    return draft;
  }

  /** Drop a draft. One draft commits one note, so a commit consumes it. */
  release(id: string): void {
    this.drafts.delete(id);
  }

  clear(): void {
    this.drafts.clear();
  }

  get size(): number {
    return this.drafts.size;
  }

  /** Bounded by TTL rather than by a timer: nothing lives long enough to pile up. */
  private prune(): void {
    const now = Date.now();
    for (const [id, draft] of this.drafts) {
      if (draft.expiresAt <= now) this.drafts.delete(id);
    }
  }
}

/**
 * The process-wide store. A server is one vault in one process, so the drafts
 * belong to the process rather than to a Resolver -- and keeping them here is
 * what lets `addNoteToBase` be a single delegating method.
 */
export const drafts = new DraftStore();

function expiredDraft(id: string, ttlMs: number): BasesError {
  return new BasesError(
    `Draft ${id} is unknown or has expired (drafts live ${Math.round(ttlMs / 60000)} minutes). ` +
      `Call add_note_to_base again WITHOUT draft_id to get a fresh draft.`,
    { construct: "draft_id" },
  );
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/**
 * Draft a note, or commit one against a draft already handed out.
 *
 * The branch is on `draft_id` alone: that is the only field that says which
 * half of the handshake this is, and it is the field the agent has to send back.
 * The path guard runs first, so a `.base` path is refused before the vault is
 * consulted at all -- on either call.
 */
export async function addNoteToBase(
  host: DraftHost,
  options: AddNoteOptions,
): Promise<AddNoteToBaseResult> {
  // `draft_id` alone decides which half of the handshake this is, because it
  // is the only field the agent is required to send back. Everything else on
  // the commit path -- base, path, view, context -- is read back from the
  // stored draft, so nothing here may assume it is present in `options`.
  if (options.draft_id !== undefined) return commitDraft(host, options);
  return proposeDraft(host, options);
}

// ---------------------------------------------------------------------------
// First call: invert the filter into a draft
// ---------------------------------------------------------------------------

async function proposeDraft(host: DraftHost, options: AddNoteOptions): Promise<DraftProposal> {
  // The path is refused before the vault is consulted, so a `.base` never even
  // gets as far as a lookup.
  const path = requireField(options.path, "path");
  assertNotePath(path);
  await assertNoteAbsent(host, path);

  const basePath = host.resolveBasePath(requireField(options.base, "base"));
  const base = await host.loadBase(basePath);
  const view = selectView(base, options.view);
  const context = options.context ?? null;
  const content = renderDraft(path, invertEffective(base, view, context), view);

  const stored = drafts.put({
    base: basePath,
    view: view.name,
    path,
    context,
    original: content,
  });

  return {
    draft_id: stored.id,
    path,
    content,
    expires_at: stored.expiresAt,
    base: basePath,
    view: view.name,
    context,
  };
}

/**
 * A field the first call cannot do without.
 *
 * The options type keeps `base` and `path` optional because the second call
 * omits them, so the first call has to say plainly which one is missing rather
 * than letting `undefined` reach the vault as a path.
 */
function requireField(value: string | undefined, name: string): string {
  if (value === undefined) {
    throw new BasesError(
      `\`${name}\` is required when proposing a draft. On the second call, send only ` +
        `\`draft_id\` and \`content\` -- the rest is read back from the stored draft.`,
    );
  }
  return value;
}

/**
 * Refuse to draft a row for a path that is already taken.
 *
 * Asks the BACKEND, not the index. The index is a snapshot from whenever this
 * process last listed the vault, so a note written since then is invisible to it
 * -- and a note written since then is exactly what this check exists to catch.
 */
async function assertNoteAbsent(host: DraftHost, path: string): Promise<void> {
  if (await host.vaultSource.backend.exists(path)) {
    throw new BasesError(
      `${path} already exists. add_note_to_base creates a NEW row; use write_note to change a note ` +
        `that is already there, or pick another path.`,
      { note: path },
    );
  }
}

/**
 * A row is a note, and a note is a `.md` file.
 *
 * `.base` is refused outright: no note operation ever writes a Base, and a
 * Base region is the one thing an agent must not be able to overwrite by
 * accident. Anything that is not `.md` is refused too, because the vault index
 * only recognises `.md` -- such a file would be written and then never appear
 * in the Base, which is the exact silent failure this tool exists to prevent.
 */
function assertNotePath(path: string): void {
  if (path.trim() === "") {
    throw new BasesError("A note path is required.");
  }
  if (path.endsWith(".base")) {
    throw new BasesError(
      `Refusing to write ${path}: a Base is never written by a note operation. ` +
        `Rows come from notes -- create a note with add_note_to_base, and edit the .base file ` +
        `itself for view, filter and formula changes.`,
      { note: path },
    );
  }
  if (!path.endsWith(".md")) {
    throw new BasesError(
      `Refusing to write ${path}: a row in a Base has to be a .md note, because a .md note is ` +
        `the only thing the vault indexes.`,
      { note: path },
    );
  }
}

// ---------------------------------------------------------------------------
// Second call: verify, then commit
// ---------------------------------------------------------------------------

async function commitDraft(host: DraftHost, options: AddNoteOptions): Promise<DraftCommit> {
  const stored = drafts.get(options.draft_id!);
  // The stored path is authoritative and is re-checked here, so a draft that
  // somehow names a `.base` is refused on the commit path too rather than only
  // on the propose path.
  assertNotePath(stored.path);

  // `base` and `path` are optional in the options because this call omits them.
  // If a caller DOES send one, it has to agree with the draft.
  if (options.base !== undefined) {
    const basePath = host.resolveBasePath(options.base);
    if (basePath !== stored.base) {
      throw new BasesError(
        `Draft ${stored.id} was drafted against ${stored.base}, not ${basePath}. Re-send it without ` +
          `draft_id to get a draft for the base you actually mean.`,
        { note: stored.path },
      );
    }
  }
  if (options.path !== undefined) {
    // A `.base` is refused before it is even compared with the draft, so the
    // agent gets the "a Base is never written" reason rather than a confusing
    // mismatch message.
    assertNotePath(options.path);
    if (options.path !== stored.path) {
      throw new BasesError(
        `Draft ${stored.id} was drafted for ${stored.path}, not ${options.path}. The verify step is ` +
          `only sound for the path the draft was built for.`,
        { note: options.path },
      );
    }
  }

  const content = options.content;
  if (content === undefined) {
    throw new BasesError(
      `Draft ${stored.id} needs the edited note as "content". Send the content you want written, ` +
        `or call add_note_to_base without draft_id to get a fresh draft.`,
      { note: stored.path },
    );
  }

  const base = await host.loadBase(stored.base);
  const view = selectView(base, stored.view);
  if (stored.context === null && effectiveFilters(base, view).some((f) => mentionsThis(f.node))) {
    throw new BasesError(
      `${stored.base} is scoped to a host note: its filter references "this", and none was ` +
        `supplied. Nothing was written. Call add_note_to_base again with "context" set to the note ` +
        `the base is embedded in, so "this" binds to a real note.`,
      { view: view.name, construct: "this" },
    );
  }

  // Structural validity before semantic validity: malformed frontmatter would
  // otherwise reach the filter as "no properties" and be reported as a mismatch
  // the agent cannot act on.
  const parsed = parseNoteWithEmbeds(stored.path, content);
  if (parsed.malformedFrontmatter) {
    throw new BasesError(
      `The frontmatter in ${stored.path} is not valid YAML, so nothing was written. Fix it and ` +
        `resend the same draft_id.`,
      { note: stored.path },
    );
  }

  const failures = await verificationFailures(host, base, view, stored, content);
  if (failures.length > 0) {
    const yaml = await host.vaultSource.readText(stored.base);
    throw verificationError(stored, view, failures, yaml);
  }

  // The absence check ran when the draft was PROPOSED. Between then and now a
  // human may have created a note at this path -- minutes later, while they read
  // the proposal -- and `createNote` replaces verbatim. So the commit asks the
  // BACKEND whether the path is taken, not the index: the index is a snapshot
  // taken when this process last listed the vault, and a note written since then
  // is exactly the one this check exists to catch. Asking the index is a check
  // that passes precisely when it is needed.
  if (await host.vaultSource.backend.exists(stored.path)) {
    throw new BasesError(
      `${stored.path} was created after this draft was proposed, so nothing was written. ` +
        `add_note_to_base only creates a NEW row; use write_note to change a note that already ` +
        `exists, or pick another path.`,
      { note: stored.path },
    );
  }

  await host.createNote(stored.path, content);
  drafts.release(stored.id);
  return { written: stored.path, draft_id: stored.id, verified: true };
}

/** One filter expression, and what it made of the draft. */
interface FilterProbe {
  where: string;
  /** The expression as the base wrote it. */
  expression: string;
  /**
   * What the expression produced. A `threw: ...` string means it did not
   * produce a value at all -- usually the draft is missing the property the
   * expression dereferences.
   */
  value: BasesValue | string;
}

/**
 * Every filter expression the draft fails, or an empty list when it matches.
 *
 * The verdict comes from the base's real filter tree -- base-level `filters`
 * ANDed with the view's own -- run through the same `parse`, `evaluate` and
 * `isTruthy` the query pipeline uses, against a context assembled the same way
 * `queryBase` assembles one. Only the reporting is ours.
 *
 * `queryBase` keeps `compileFilter` / `runFilter` private and hands back only
 * rows, so the tree walk is mirrored rather than shared. `probe` below and
 * those two functions have to keep agreeing; `test/unit/drafts.test.ts` pins it
 * by asserting that a note this accepts is a row `resolver.query` returns.
 */
async function verificationFailures(
  host: DraftHost,
  base: BaseFile,
  view: BaseView,
  stored: StoredDraft,
  content: string,
): Promise<FilterProbe[]> {
  const vault = await vaultWithDraft(host.vaultSource.backend, stored.path, content);
  const ctx = draftEvalContext(vault, stored.path, base, stored.context);

  const failures: FilterProbe[] = [];
  for (const filter of effectiveFilters(base, view)) {
    probe(filter.node, filter.where, ctx, failures);
  }
  return failures;
}

/**
 * Build the evaluation context for a note that is not on disk.
 *
 * The formula pass mirrors `queryBase`: formulas are ordered first (a formula
 * may read another) and evaluated against a context whose `formula` bag is
 * still empty, because that is the graph the query pipeline builds.
 */
function draftEvalContext(
  vault: Vault,
  path: string,
  base: BaseFile,
  context: string | null,
): EvalContext {
  const record = vault.note(path);
  if (record === undefined) {
    throw new BasesError(`Could not index the draft for ${path}; refusing to write it.`, {
      note: path,
    });
  }
  const ctx: EvalContext = {
    note: record.frontmatter,
    file: vault.fileValue(path),
    formula: {},
    // The same resolver the query pipeline uses, so a bad host note is refused
    // in one voice here and in `resolve_base` rather than two.
    thisValue: resolveHostNote(vault, context),
  };

  const formulas: Record<string, BasesValue> = {};
  for (const name of orderFormulas(base.formulas)) {
    formulas[name] = evaluate(parse(base.formulas[name]!), ctx);
  }
  ctx.formula = formulas;
  return ctx;
}

/**
 * Run one filter node, recording every leaf that came out falsy.
 *
 * `or` and `not` are reported as a whole rather than branch by branch. Which
 * branch an author meant is a guess, and naming a branch they did not write
 * would be a worse error than naming the group.
 */
function probe(
  node: FilterNode,
  where: string,
  ctx: EvalContext,
  failures: FilterProbe[],
  collect = true,
): boolean {
  if (typeof node === "string") {
    // A draft that omits a property the filter dereferences makes the
    // expression THROW rather than evaluate to false -- `project.contains(x)`
    // on a note with no `project` is a null dereference, not a mismatch. From
    // the agent's point of view that is still just "this filter did not pass",
    // and reporting it as a raw type error would hide the filter and the base
    // YAML behind it. So an evaluation error is recorded as a failure of that
    // one leaf, and the loop carries on to the others.
    let value: BasesValue;
    try {
      value = evaluate(parse(node), ctx);
    } catch (err) {
      if (collect) {
        failures.push({ where, expression: node, value: `threw: ${errText(err)}` });
      }
      return false;
    }
    if (isTruthy(value)) return true;
    if (collect) failures.push({ where, expression: node, value });
    return false;
  }

  if (node.and !== undefined) {
    return node.and
      .map((child, i) => probe(child, `${where}.and[${i}]`, ctx, failures, collect))
      .every(Boolean);
  }

  if (node.or !== undefined) {
    const ok = node.or.some((child) => probe(child, `${where}.or`, ctx, failures, false));
    if (!ok && collect) {
      failures.push({
        where,
        expression: `any of (${node.or.map(describe).join(", ")})`,
        value: false,
      });
    }
    return ok;
  }

  // `not` is NAND, not negation: none of these may be true.
  const excluded = node.not ?? [];
  const ok = excluded.every((child) => !probe(child, `${where}.not`, ctx, failures, false));
  if (!ok && collect) {
    failures.push({
      where,
      expression: `none of (${excluded.map(describe).join(", ")})`,
      value: true,
    });
  }
  return ok;
}

/**
 * The filter tree the verdict comes from: base-level `filters` ANDed with the
 * view's own. Labelled the way the base writes them, so a failure points at
 * the key the author has to edit.
 */
function effectiveFilters(
  base: BaseFile,
  view: BaseView,
): Array<{ where: string; node: FilterNode }> {
  const out: Array<{ where: string; node: FilterNode }> = [];
  if (base.filters !== undefined) out.push({ where: "filters", node: base.filters });
  if (view.filters !== undefined) {
    out.push({ where: `views.${view.name}.filters`, node: view.filters });
  }
  return out;
}

/**
 * Does anything in the filter refer to `this`?
 *
 * Checked before anything is evaluated, because the alternative is a type error
 * from a property the draft has not set yet -- `Type error: "contains" is not a
 * method on null` says nothing about the real problem, which is an unbound host.
 */
function mentionsThis(node: FilterNode): boolean {
  if (typeof node === "string") {
    const ast = tryParse(node);
    return ast !== null && mentionsThisNode(ast);
  }
  return (node.and ?? node.or ?? node.not ?? []).some(mentionsThis);
}

function mentionsThisNode(node: Node): boolean {
  switch (node.type) {
    case "Identifier":
      return node.name === "this";
    case "Literal":
      return false;
    case "Member":
      return mentionsThisNode(node.object);
    case "Unary":
      return mentionsThisNode(node.operand);
    case "Binary":
      return mentionsThisNode(node.left) || mentionsThisNode(node.right);
    case "Index":
      return mentionsThisNode(node.object) || mentionsThisNode(node.index);
    case "List":
      return node.elements.some(mentionsThisNode);
    case "Call":
      return mentionsThisNode(node.callee) || node.args.some(mentionsThisNode);
  }
}

/**
 * The failure an agent has to act on, so it carries everything needed to act.
 *
 * The base's YAML is inlined verbatim rather than summarised: the agent is about
 * to edit frontmatter, and the only authority on what that frontmatter has to
 * satisfy is the filter itself. A paraphrase would be a second thing to get
 * wrong.
 */
function errText(err: unknown): string {
  return err instanceof Error ? err.message : String(err);
}

function verificationError(
  stored: StoredDraft,
  view: BaseView,
  failures: FilterProbe[],
  yaml: string,
): BasesError {
  const reported = failures.map(
    (f) => `  - ${f.expression}  ->  ${toDisplayString(f.value)}   [${f.where}]`,
  );
  return new BasesError(
    `${stored.path} does not match view "${view.name}" of ${stored.base}, so nothing was written.\n` +
      `These filter expressions did not match the draft's frontmatter:\n${reported.join("\n")}\n` +
      `Correct the note and call add_note_to_base again with draft_id ${stored.id} and the edited ` +
      `content; the draft is still live until ${new Date(stored.expiresAt).toISOString()}.\n` +
      `The base's own filter, verbatim (${stored.base}):\n${yaml}`,
  );
}

// ---------------------------------------------------------------------------
// Filter inversion
// ---------------------------------------------------------------------------

interface Entry {
  key: string;
  value: BasesValue;
}

interface Inversion {
  entries: Entry[];
  /**
   * Constructs no inversion covers. They become TODO comments in the draft:
   * dropping a conjunct would produce a note that looks right and matches
   * nothing, which is the failure this whole tool is built to avoid.
   */
  unresolved: string[];
}

/**
 * Invert the base-level filter ANDed with the view's own.
 *
 * Only a conjunction of invertible leaves yields frontmatter. `or` and `not`
 * are reported instead of guessed: a value we invented for a disjunction would
 * be a coin flip about which branch the author meant, and a coin flip that
 * happens to verify is still a coin flip.
 */
function invertEffective(base: BaseFile, view: BaseView, context: string | null): Inversion {
  return merge([invert(base.filters, context), invert(view.filters, context)]);
}

/** One node: recurse through a conjunction, parse an expression, or give up. */
function invert(node: FilterNode | undefined, context: string | null): Inversion {
  if (node === undefined) return { entries: [], unresolved: [] };

  if (typeof node === "string") {
    const ast = tryParse(node);
    if (ast === null) return { entries: [], unresolved: [node] };
    return invertNode(ast, node, context);
  }

  if (node.and !== undefined) {
    return merge(node.and.map((child) => invert(child, context)));
  }

  // `or` and `not` are the two shapes a single value cannot express.
  return { entries: [], unresolved: [describe(node)] };
}

/**
 * Invert one filter node, or report it as un-invertible.
 *
 * The four forms that invert are the ones real bases are built from:
 * `file.hasTag("x")`, `<prop>.contains(link(this.file.name))`,
 * `<prop>.contains(link("Literal"))`, and `<prop> == "literal"` (either way
 * round). Everything else -- `file.*` comparisons, `inFolder`, bare formula
 * predicates, `!=`, regex, lambdas -- becomes a TODO comment, because a
 * half-right guess is indistinguishable from a right one until the row silently
 * fails to appear.
 */
function invertNode(node: Node, source: string, context: string | null): Inversion {
  const nothing: Inversion = { entries: [], unresolved: [source] };

  // `a && b` is as invertible as its parts: both halves have to hold anyway.
  if (node.type === "Binary" && node.op === "&&") {
    return merge([invertNode(node.left, source, context), invertNode(node.right, source, context)]);
  }

  const hasTag = asCall(node, "hasTag", "file");
  if (hasTag !== null) {
    const tag = hasTag.args[0];
    if (tag !== undefined && tag.type === "Literal" && typeof tag.value === "string") {
      return { entries: [{ key: "tags", value: [tag.value] }], unresolved: [] };
    }
    return nothing;
  }

  const contains = asContains(node);
  if (contains !== null) {
    const key = notePropertyKey(contains.property);
    const needle = contains.needle;
    if (key === null) return nothing;

    if (isCallOf(needle, "link") && needle.args.length === 1) {
      const target = needle.args[0]!;
      if (isHostFileName(target)) {
        // `this.file.name` is the host note, so only a known host can be written.
        if (context === null || context === "") {
          return {
            entries: [],
            unresolved: [
              `${source} -- pass the host note as "context" and re-draft, or set ${key} yourself`,
            ],
          };
        }
        return {
          entries: [{ key, value: [`[[${stripExtension(basename(context))}]]`] }],
          unresolved: [],
        };
      }
      if (target.type === "Literal" && typeof target.value === "string") {
        return { entries: [{ key, value: [`[[${target.value}]]`] }], unresolved: [] };
      }
    }
    return nothing;
  }

  // Equality is symmetric, so `status == "x"` and `"x" == status` agree.
  if (node.type === "Binary" && node.op === "==") {
    const leftKey = notePropertyKey(node.left);
    if (leftKey !== null && isPlainLiteral(node.right)) {
      return { entries: [{ key: leftKey, value: node.right.value }], unresolved: [] };
    }
    const rightKey = notePropertyKey(node.right);
    if (rightKey !== null && isPlainLiteral(node.left)) {
      return { entries: [{ key: rightKey, value: node.left.value }], unresolved: [] };
    }
  }

  return nothing;
}

/** Combine inversions, refusing to let two conjuncts fight over one key. */
function merge(parts: Inversion[]): Inversion {
  const entries: Entry[] = [];
  const unresolved: string[] = [];

  for (const part of parts) {
    unresolved.push(...part.unresolved);
    for (const entry of part.entries) {
      const existing = entries.find((e) => e.key === entry.key);
      if (existing === undefined) {
        entries.push({ ...entry });
        continue;
      }
      if (isList(existing.value) && isList(entry.value)) {
        for (const value of entry.value) {
          if (!existing.value.includes(value)) existing.value.push(value);
        }
        continue;
      }
      unresolved.push(
        `filters disagree about "${entry.key}": ${toDisplayString(existing.value)} vs ` +
          `${toDisplayString(entry.value)}`,
      );
    }
  }

  return { entries, unresolved };
}

/** Human-readable form of a filter node, for the placeholders we cannot invert. */
function describe(node: FilterNode): string {
  if (typeof node === "string") return node;
  if (node.and !== undefined) return `all of (${node.and.map(describe).join(", ")})`;
  if (node.or !== undefined) return `any of (${node.or.map(describe).join(", ")})`;
  return `none of (${(node.not ?? []).map(describe).join(", ")})`;
}

// -- node shapes ------------------------------------------------------------

/**
 * The frontmatter key a node reads, or null when it is not a note property.
 *
 * `file.*`, `formula.*` and `this.*` all read from somewhere frontmatter
 * cannot write, so they are deliberately not invertible. A filter over them
 * still gets a TODO in the draft -- un-invertible, not invisible.
 */
function notePropertyKey(node: Node): string | null {
  if (node.type === "Identifier") {
    return RESERVED.has(node.name) ? null : node.name;
  }
  if (node.type === "Member" && node.object.type === "Identifier" && node.object.name === "note") {
    return node.property;
  }
  if (
    node.type === "Index" &&
    node.object.type === "Identifier" &&
    node.object.name === "note" &&
    node.index.type === "Literal" &&
    typeof node.index.value === "string"
  ) {
    return node.index.value;
  }
  return null;
}

/** `file.<method>(...)` with exactly `arity` arguments. */
function asCall(node: Node, method: string, namespace: string, arity = 1): CallNode | null {
  if (node.type !== "Call" || node.args.length !== arity) return null;
  const callee = node.callee;
  if (
    callee.type !== "Member" ||
    callee.property !== method ||
    callee.object.type !== "Identifier" ||
    callee.object.name !== namespace
  ) {
    return null;
  }
  return node;
}

function isCallOf(node: Node, global: string): node is CallNode {
  return node.type === "Call" && node.callee.type === "Identifier" && node.callee.name === global;
}

/** `<prop>.contains(<needle>)`, for any receiver. */
function asContains(node: Node): { property: Node; needle: Node } | null {
  if (node.type !== "Call" || node.args.length !== 1) return null;
  const callee = node.callee as MemberNode;
  if (callee.type !== "Member" || callee.property !== "contains") return null;
  return { property: callee.object, needle: node.args[0]! };
}

/** `this.file.name` or `this.file.basename` -- the same note either way. */
function isHostFileName(node: Node): boolean {
  if (node.type !== "Member" || (node.property !== "name" && node.property !== "basename")) {
    return false;
  }
  const file = node.object;
  return (
    file.type === "Member" &&
    file.property === "file" &&
    file.object.type === "Identifier" &&
    file.object.name === "this"
  );
}

/** A literal a frontmatter value can be written as. Links and dates are not. */
function isPlainLiteral(node: Node): node is LiteralNode {
  if (node.type !== "Literal") return false;
  const value = node.value;
  return typeof value === "string" || typeof value === "number" || typeof value === "boolean";
}

// ---------------------------------------------------------------------------
// Draft rendering
// ---------------------------------------------------------------------------

/**
 * Assemble the note: frontmatter first, because that is the part the base reads.
 *
 * The view's `order` columns are seeded with empty values so the agent can see
 * the shape a row is expected to have. Empty is deliberate -- it can never
 * satisfy a filter, so a seed can never make verification pass by accident.
 */
function renderDraft(path: string, inversion: Inversion, view: BaseView): string {
  const taken = new Set(inversion.entries.map((e) => e.key));
  const entries = [...inversion.entries, ...seedColumns(view, taken)];

  const frontmatter = frontmatterLines(entries, inversion.unresolved);
  const lines: string[] = [];
  if (frontmatter.length > 0) lines.push("---", ...frontmatter, "---", "");
  lines.push(`# ${noteTitle(path)}`, "");
  lines.push(
    "TODO: replace this paragraph with the note body. The frontmatter above was inverted from " +
      "the base's filter, so it is a suggestion -- the base's own filter is the ground truth.",
  );
  return `${lines.join("\n")}\n`;
}

/**
 * The `order` columns that are note properties, as empty values.
 *
 * `file.*` and `formula.*` are skipped: the first is not frontmatter and the
 * second is computed, so neither can be seeded.
 */
function seedColumns(view: BaseView, taken: Set<string>): Entry[] {
  const out: Entry[] = [];
  for (const id of view.order ?? []) {
    const ast = tryParse(id);
    if (ast === null) continue;
    const key = notePropertyKey(ast);
    if (key === null || taken.has(key)) continue;
    taken.add(key);
    out.push({ key, value: "" });
  }
  return out;
}

/** The frontmatter block, line by line, with the placeholders at the end. */
function frontmatterLines(entries: Entry[], unresolved: string[]): string[] {
  const lines: string[] = [];
  for (const entry of entries) {
    // `yaml` decides the quoting; a value like "[[X]]" or "a: b" would be
    // unreadable if we hand-assembled it.
    lines.push(stringifyYaml({ [entry.key]: entry.value }).replace(/\n+$/, ""));
  }
  for (const expr of unresolved) {
    lines.push(`# TODO: could not invert ${collapse(expr)}`);
  }
  return lines;
}

/** A YAML comment must occupy exactly one line. */
function collapse(text: string): string {
  return text.replace(/\s+/g, " ").trim();
}

function noteTitle(path: string): string {
  return stripExtension(basename(path));
}

// ---------------------------------------------------------------------------
// The verification vault
// ---------------------------------------------------------------------------

async function vaultWithDraft(inner: VaultSource, path: string, content: string): Promise<Vault> {
  const vault = new Vault(new DraftVaultSource(inner, path, content));
  await vault.load();
  return vault;
}

/**
 * The real vault with one unsaved note spliced in, and read-only.
 *
 * `writeText` throws rather than delegating: verification must be incapable of
 * touching the vault, so that property is structural instead of a promise.
 */
class DraftVaultSource implements VaultSource {
  readonly kind: "fs" | "webdav";

  constructor(
    private readonly inner: VaultSource,
    private readonly path: string,
    private readonly content: string,
  ) {
    this.kind = inner.kind;
  }

  async list(): Promise<string[]> {
    // The draft is added even when a note is already at that path, because a
    // commit replaces it; the Set is what stops it being indexed twice.
    const paths = new Set(await this.inner.list());
    paths.add(this.path);
    return [...paths].sort();
  }

  async readText(path: string): Promise<string> {
    return path === this.path ? this.content : this.inner.readText(path);
  }

  /**
   * The draft overlays the real vault, so a fresh read still has to consult the
   * draft for its own path before going to the backend for anything else.
   */
  async readFresh(path: string): Promise<string> {
    return path === this.path ? this.content : this.inner.readFresh(path);
  }

  async exists(path: string): Promise<boolean> {
    return path === this.path || this.inner.exists(path);
  }

  async writeText(path: string): Promise<void> {
    throw new BasesError(
      `Refusing to write ${path}: a verification vault is read-only. The note is written once, ` +
        `after its filter has matched.`,
      { note: path },
    );
  }

  async stat(path: string): Promise<FileStat> {
    if (path !== this.path) return this.inner.stat(path);
    // A draft has no mtime of its own, so the write time is the honest answer.
    return { size: new TextEncoder().encode(this.content).length, mtime: new Date() };
  }

  async hash(path: string): Promise<string> {
    if (path !== this.path) return this.inner.hash(path);
    return contentHash(this.content);
  }
}
