/**
 * The vault index: note metadata, link resolution and `file.*` accessors.
 *
 * Link resolution follows Obsidian rather than intuition, because two real
 * behaviours are load-bearing and neither is what you would guess:
 *
 *  - A bare `[[Alias]]` does NOT resolve. `aliases` feeds the link *suggester*,
 *    which emits the piped form `[[Real Name|Alias]]`. Verified on 1.13.7.
 *  - An ambiguous `[[Readme]]` resolves to the SHORTEST path, not to a note in
 *    the same folder as the referrer.
 */

import { toDisplayString } from "../expr/evaluator";
import {
  type BasesValue,
  DateValue,
  type FileAccessors,
  FileValue,
  LinkValue,
  stripExtension,
} from "../expr/values";
import { type ParsedNote, parseNoteWithEmbeds, type TaskItem, type WikiLink } from "../note/parse";
import { matchPath } from "./resolve";
import { isBasePath, isNotePath, type VaultSource } from "./source";

export interface NoteRecord {
  path: string;
  parsed: ParsedNote;
  frontmatter: Record<string, BasesValue>;
  size: number;
  mtime: Date;
}

export class Vault {
  private notes = new Map<string, NoteRecord>();
  private bases: string[] = [];
  private byPath = new Map<string, string>();
  private ready = false;

  constructor(private readonly source: VaultSource) {}

  get sourceKind(): "fs" | "webdav" {
    return this.source.kind;
  }

  /** The underlying backend, for writes that must bypass the snapshot. */
  get backend(): VaultSource {
    return this.source;
  }

  /**
   * Build the index. Safe to call repeatedly; use `reload()` to force a
   * rebuild after writes.
   */
  async load(): Promise<void> {
    if (this.ready) return;
    await this.rebuild();
  }

  async reload(): Promise<void> {
    await this.rebuild();
  }

  private async rebuild(): Promise<void> {
    const paths = await this.source.list();
    this.notes = new Map();
    this.bases = [];
    this.byPath = new Map();

    for (const p of paths) {
      if (isBasePath(p)) {
        this.bases.push(p);
        continue;
      }
      if (!isNotePath(p)) continue;
      const text = await this.source.readText(p);
      const stat = await this.source.stat(p);
      const parsed = parseNoteWithEmbeds(p, text);
      this.notes.set(p, {
        path: p,
        parsed,
        // Frontmatter link values become LinkValue so link identity comparison
        // works. Obsidian does the same: "Wikilinks in frontmatter properties
        // are automatically recognized as Link objects."
        frontmatter: coerceFrontmatter(parsed.frontmatter, this),
        size: stat.size,
        mtime: stat.mtime,
      });
    }

    // Index every path under the four keys Obsidian's resolver accepts:
    // full path, path without extension, and the basename.
    for (const p of this.notes.keys()) {
      this.register(p, p);
      this.register(p, stripExtension(p));
      const base = p.slice(p.lastIndexOf("/") + 1);
      this.register(p, base);
      this.register(p, stripExtension(base));
    }
    this.bases.sort();
    this.ready = true;
  }

  /** First registration wins, matching the shortest-path ambiguity rule. */
  private register(path: string, key: string): void {
    if (key === "") return;
    if (!this.byPath.has(key)) this.byPath.set(key, path);
  }

  notePaths(): string[] {
    return [...this.notes.keys()];
  }

  basePaths(): string[] {
    return [...this.bases];
  }

  note(path: string): NoteRecord | undefined {
    return this.notes.get(path);
  }

  async readText(path: string): Promise<string> {
    return this.source.readText(path);
  }

  /** Resolve a link target to a vault path, or undefined when unresolved. */
  resolve(target: string): string | undefined {
    return matchPath(target, this.byPath);
  }

  /** Build a FileValue with accessors bound to this vault. */
  fileValue(path: string): FileValue {
    const accessors: FileAccessors = {
      tags: () => this.tagsFor(path),
      links: () => this.linksFor(path),
      embeds: () => this.embedsFor(path),
      backlinks: () => this.backlinksFor(path),
      properties: () => this.note(path)?.frontmatter ?? {},
      ctime: () => new DateValue(this.note(path)?.mtime.getTime() ?? 0, true),
      mtime: () => new DateValue(this.note(path)?.mtime.getTime() ?? 0, true),
      size: () => this.note(path)?.size ?? 0,
      tasks: () => this.tasksFor(path),
      resolve: (target) => {
        const resolved = this.resolve(target);
        return resolved === undefined ? undefined : this.fileValue(resolved);
      },
      linksTo: () => false,
    };
    return new FileValue(path, accessors);
  }

  /**
   * `file.tags` covers frontmatter and body, and every element keeps its `#`
   * prefix -- confirmed against `base:query format=json` on Obsidian 1.13.7,
   * which emits `"Tags": "#Contacts, #Kontakte"`.
   */
  tagsFor(path: string): BasesValue[] {
    const rec = this.note(path);
    if (rec === undefined) return [];
    const out: BasesValue[] = [];
    const fmTags = rec.frontmatter["tags"];
    // `tags` may be a single string or a list.
    for (const t of toList(fmTags)) {
      if (typeof t === "string") {
        // A frontmatter tag may itself be `business-idea` or `#x`; normalise
        // to the `#`-prefixed display form.
        const clean = t.trim().replace(/^#+/, "");
        if (clean !== "") out.push(`#${clean}`);
      }
    }
    for (const tag of rec.parsed.inlineTags) {
      out.push(`#${tag}`);
    }
    return dedupe(out);
  }

  /** `file.links` includes links found in frontmatter as well as the body. */
  linksFor(path: string): BasesValue[] {
    const rec = this.note(path);
    if (rec === undefined) return [];
    const out: BasesValue[] = [];
    for (const l of rec.parsed.links) {
      if (l.embedded) continue;
      out.push(this.linkValue(l.target, l.display));
    }
    for (const key of ["link", "links", "related", "projects", "project"]) {
      const v = rec.frontmatter[key];
      if (v === undefined) continue;
      for (const item of toList(v)) {
        if (typeof item !== "string") continue;
        out.push(this.linkValue(stripBrackets(item)));
      }
    }
    return dedupe(out);
  }

  embedsFor(path: string): BasesValue[] {
    const rec = this.note(path);
    if (rec === undefined) return [];
    return rec.parsed.embeds.map((e) => this.linkValue(e.target, e.display));
  }

  /** `file.backlinks` is indexed, not read live -- see docs/divergences.md. */
  backlinksFor(path: string): BasesValue[] {
    const target = stripExtension(path);
    const out: string[] = [];
    for (const other of this.notes.keys()) {
      if (other === path) continue;
      for (const l of this.linksFor(other)) {
        if (!(l instanceof LinkValue)) continue;
        const linkTarget = l.resolvedPath ?? l.target;
        if (stripExtension(linkTarget) === target) {
          out.push(other);
          break;
        }
      }
    }
    return out;
  }

  /** `file.tasks` is a documented extension, not part of the official surface. */
  tasksFor(path: string): BasesValue[] {
    const rec = this.note(path);
    if (rec === undefined) return [];
    return rec.parsed.tasks.map((t: TaskItem) => taskValue(t));
  }

  /** Wrap a link target as a LinkValue, resolving it when possible. */
  linkValue(target: string, display?: string | null): LinkValue {
    const resolved = this.resolve(target);
    return new LinkValue(target, display ?? undefined, resolved);
  }

  /** A FileValue for `path`, or undefined when the note does not exist. */
  fileFor(path: string): FileValue | undefined {
    return this.notes.has(path) ? this.fileValue(path) : undefined;
  }
}

/**
 * Convert frontmatter values that Obsidian treats as links.
 *
 * A property value written as `[[Some Note]]` or `[[Some Note|Alias]]` is a
 * Link object in Bases, compared by resolved target rather than by text. Without
 * this coercion `project.contains(link(this.file.name))` silently fails, because
 * one side would be a Link and the other a plain string.
 */
export function coerceFrontmatter(
  data: Record<string, BasesValue>,
  vault: Vault,
): Record<string, BasesValue> {
  const out: Record<string, BasesValue> = {};
  for (const [k, v] of Object.entries(data)) {
    out[k] = coerceValue(v, vault);
  }
  return out;
}

function coerceValue(v: BasesValue, vault: Vault): BasesValue {
  if (Array.isArray(v)) return v.map((x) => coerceValue(x, vault));
  if (typeof v !== "string") return v;

  const wikilink = /^\[\[([^\]|#]+)(?:#[^\]|]*)?(?:\|([^\]]*))?\]\]$/.exec(v.trim());
  if (wikilink !== null) {
    const target = wikilink[1]!;
    return vault.linkValue(target, wikilink[2] ?? null);
  }

  const mdlink = /^\[([^\]]*)\]\(([^)\s]+)\)$/.exec(v.trim());
  if (mdlink !== null) {
    const target = mdlink[2]!;
    if (!/^[a-z]+:\/\//i.test(target) && !target.startsWith("#")) {
      return vault.linkValue(target.replace(/\.md$/, ""), mdlink[1] || null);
    }
  }

  return v;
}

export interface TaskValueShape extends Record<string, BasesValue> {
  text: string;
  completed: boolean;
}

/**
 * A task's shape follows the Obsidian CLI's `tasks format=json` output:
 * `{ status, text, file, line }`.
 */
export function taskValue(t: TaskItem): TaskValueShape {
  return {
    status: t.checked ? "x" : " ",
    text: t.text,
    completed: t.checked,
  };
}

function toList(v: BasesValue | undefined): BasesValue[] {
  if (v === undefined || v === null) return [];
  return Array.isArray(v) ? v : [v];
}

function stripBrackets(s: string): string {
  const m = /^\[\[([^\]|#]+)(?:#[^\]|]*)?(?:\|[^\]]*)?\]\]$/.exec(s.trim());
  return m !== null ? m[1]! : s.trim();
}

/**
 * Deduplicate by each value's own string form.
 *
 * This used to key on `String(item)`, which is `[object Object]` for every
 * `LinkValue` — so `file.links` collapsed to a single element for any note with
 * two links, and `backlinksFor`, which is built on `linksFor`, could lose a
 * backlink behind the first. `Root Project.md` in the testing vault carries
 * three links in its frontmatter and returned one.
 */
function dedupe<T extends BasesValue>(items: T[]): T[] {
  const seen = new Set<string>();
  const out: T[] = [];
  for (const i of items) {
    const key = toDisplayString(i);
    if (seen.has(key)) continue;
    seen.add(key);
    out.push(i);
  }
  return out;
}

export type { WikiLink };
