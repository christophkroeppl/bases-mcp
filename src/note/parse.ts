/**
 * Note segmentation.
 *
 * A note is parsed into ordered segments that concatenate back to the original
 * bytes exactly. That invariant is what makes the write path safe: a base
 * region is a byte span we can preserve verbatim while everything around it is
 * editable.
 *
 * Each segment carries its own source text, so serialising a parsed note is
 * `segments.map(s => s.raw).join("")` -- never a reformat.
 */

import { parse as parseYaml } from "yaml";

import type { BasesValue } from "../expr/values";

export type Segment = FrontmatterSegment | BaseEmbedSegment | BaseFenceSegment | ProseSegment;

interface SegmentBase {
  /** Exact source text, including any delimiters. */
  raw: string;
  start: number;
  end: number;
}

export interface FrontmatterSegment extends SegmentBase {
  kind: "frontmatter";
  /** Parsed YAML, or `{}` when absent or malformed. */
  data: Record<string, BasesValue>;
  /** True when the block was present but failed to parse. */
  malformed: boolean;
}

export interface ProseSegment extends SegmentBase {
  kind: "prose";
}

export interface BaseEmbedSegment extends SegmentBase {
  kind: "baseEmbed";
  /** The `.base` file this embed points at, as written in the link. */
  basePath: string;
  /** The `#View` selector, when the embed pins one. */
  viewName: string | null;
  /** True for `![[X.base]]` (always) — an embed is the only .base form in prose. */
  embedded: true;
}

/**
 * An inline ```` ```base ```` fenced block. The YAML lives in the note rather
 * than a `.base` file, and Obsidian binds `this` to the containing note.
 */
export interface BaseFenceSegment extends SegmentBase {
  kind: "baseFence";
  /** The base YAML carried in the fence. */
  yaml: string | undefined;
  /**
   * True for a ```base-rendered fence -- a Base region handed back by a
   * Projection. It has no live YAML, so it is replaced rather than stored.
   */
  rendered?: boolean;
  /** The Base a rendered fence was rendered from, from its `path=` attribute. */
  basePath?: string;
  /** The `#View` a rendered fence pinned, from its `view=` attribute. */
  viewName?: string | null;
}

/** `![[Target]]` with optional `#heading`, `#^block` and `|display`. */
export interface WikiLink {
  target: string;
  /** The `#...` subpath, if any. Not used for link equality. */
  subpath: string | null;
  display: string | null;
  embedded: boolean;
  start: number;
  end: number;
}

export interface TaskItem {
  checked: boolean;
  /** The text after the checkbox, with the checkbox stripped. */
  text: string;
  line: number;
}

export interface ParsedNote {
  path: string;
  segments: Segment[];
  frontmatter: Record<string, BasesValue>;
  /** Every link in the body, in document order. */
  links: WikiLink[];
  /** Every embed target in the body, in document order. */
  embeds: WikiLink[];
  /** Checkboxes found in the body, in document order. */
  tasks: TaskItem[];
  /** Inline `#tags` found in the body, without the `#`. */
  inlineTags: string[];
  malformedFrontmatter: boolean;
}

// ---------------------------------------------------------------------------
// Fence handling
// ---------------------------------------------------------------------------

const FRONTMATTER_RE = /^﻿?---[ \t]*\r?\n([\s\S]*?)\r?\n---[ \t]*(?:\r?\n|$)/;

/**
 * Match a fenced code block starting at `pos`, honouring Obsidian's rule that
 * an unclosed fence runs to end of file. Returns the end offset past the
 * closing fence, or null when the fence is not at a line start.
 */
function matchFence(
  text: string,
  pos: number,
): { body: string; end: number; lang: string; info: string } | null {
  const lineStart = text.lastIndexOf("\n", pos - 1) + 1;
  if (text.slice(lineStart, pos).trim() !== "") return null;

  const open = /^(`{3,})[ \t]*([^`\n]*)[ \t]*$/.exec(text.slice(lineStart).split(/\r?\n/)[0] ?? "");
  if (open === null) return null;

  const ticks = open[1]!;
  const info = (open[2] ?? "").trim();
  const lang = info.toLowerCase();
  const bodyStart = lineStart + (text.slice(lineStart).indexOf("\n") + 1);
  if (bodyStart <= lineStart) return null;

  // An unclosed fence runs to EOF, matching Obsidian.
  const closeRe = new RegExp(`^[ \\t>]*${ticks}[ \\t]*$`, "m");
  const rest = text.slice(bodyStart);
  const m = closeRe.exec(rest);
  if (m === null) {
    return { body: rest, end: text.length, lang, info };
  }
  return { body: rest.slice(0, m.index), end: bodyStart + m.index + m[0].length, lang, info };
}

/**
 * The `key="value"` pairs on a fence info string.
 *
 * The Projection writes ```base-rendered path="Tickets.base" view="All"```, and
 * those are the only thing tying a rendered fence back to the Base region it
 * came from. Reading them is what lets `write_note` round-trip a Projection the
 * agent edited, instead of writing the rendered fence to disk.
 */
function fenceAttrs(info: string): Record<string, string> {
  const attrs: Record<string, string> = {};
  const re = /([A-Za-z_][\w-]*)\s*=\s*"([^"]*)"/g;
  let m: RegExpExecArray | null;
  while ((m = re.exec(info)) !== null) attrs[m[1]!] = m[2]!;
  return attrs;
}

/**
 * The fence language of a rendered Base region.
 *
 * Duplicated from `render/project.ts` rather than imported: the renderer
 * depends on this parser, so importing the constant back would close the cycle.
 * `wrapInFence` in the renderer emits this exact string, and a round-trip test
 * in `test/unit/project.test.ts` pins the two spellings together.
 */
const RENDER_FENCE_LANG = "base-rendered";

/** Parse a note into ordered segments plus the derived facts Bases needs. */
export function parseNote(path: string, text: string): ParsedNote {
  const segments: Segment[] = [];
  let pos = 0;

  // Frontmatter must be the very first thing in the file.
  const fm = FRONTMATTER_RE.exec(text);
  let frontmatter: Record<string, BasesValue> = {};
  let malformed = false;
  if (fm !== null) {
    const raw = fm[0];
    try {
      const parsed = parseYaml(fm[1] ?? "");
      frontmatter = (parsed ?? {}) as Record<string, BasesValue>;
    } catch {
      // A malformed frontmatter block degrades to "no properties" rather than
      // failing the whole note, matching how loaders generally behave.
      malformed = true;
      frontmatter = {};
    }
    segments.push({
      kind: "frontmatter",
      raw,
      start: 0,
      end: raw.length,
      data: frontmatter,
      malformed,
    });
    pos = raw.length;
  }

  while (pos < text.length) {
    const fence = matchFence(text, pos);
    if (fence !== null) {
      const raw = text.slice(pos, fence.end);
      // The language is the FIRST token of the info string; the rest are
      // attributes. ```base-rendered path="Tickets.base" is a rendered region,
      // not prose -- the attributes are what tie it back to its Base.
      const lang = fence.lang.split(/\s+/)[0] ?? "";
      if (lang === "base") {
        segments.push({ kind: "baseFence", raw, start: pos, end: fence.end, yaml: fence.body });
      } else if (lang === RENDER_FENCE_LANG) {
        // A rendered fence is a Base region the agent got back from a
        // Projection. It carries no live YAML, so it must never reach disk --
        // Obsidian would treat it as an inert block, leaving a dead copy of the
        // rendered rows in the note. Tagging it here lets `reconcileNote` swap
        // it back for the region it replaced.
        const attrs = fenceAttrs(fence.info);
        segments.push({
          kind: "baseFence",
          raw,
          start: pos,
          end: fence.end,
          yaml: undefined,
          rendered: true,
          basePath: attrs["path"],
          viewName: attrs["view"] ?? null,
        });
      } else {
        segments.push({ kind: "prose", raw, start: pos, end: fence.end });
      }
      pos = fence.end;
      continue;
    }

    // Prose up to the next fence that starts a line.
    const next = findNextFence(text, pos);
    const raw = text.slice(pos, next);
    segments.push({ kind: "prose", raw, start: pos, end: next });
    pos = next;
  }

  const links = extractLinks(text);
  const embeds = links.filter((l) => l.embedded);
  const tasks = extractTasks(text);
  const inlineTags = extractInlineTags(text);

  return {
    path,
    segments,
    frontmatter,
    links,
    embeds,
    tasks,
    inlineTags,
    malformedFrontmatter: malformed,
  };
}

function findNextFence(text: string, from: number): number {
  const re = /(^|\n)[ \t>]*`{3,}/g;
  re.lastIndex = from;
  const m = re.exec(text);
  return m === null ? text.length : m.index + (m[1] === "\n" ? 1 : 0);
}

/** Segment kinds that occupy a Base region. */
export function isBaseRegion(s: Segment): s is BaseEmbedSegment | BaseFenceSegment {
  return s.kind === "baseEmbed" || s.kind === "baseFence";
}

export function isBaseEmbed(s: Segment): s is BaseEmbedSegment {
  return s.kind === "baseEmbed";
}

/** Serialise segments back to text. Byte-exact when unmodified. */
export function serialise(segments: Segment[]): string {
  return segments.map((s) => s.raw).join("");
}

// ---------------------------------------------------------------------------
// Embed detection
// ---------------------------------------------------------------------------

/**
 * Split the prose into base-embed segments and the prose between them. A
 * `.base` embed occupies its own region; the surrounding prose is untouched.
 */
export function splitBaseEmbeds(segments: Segment[]): Segment[] {
  const out: Segment[] = [];
  for (const seg of segments) {
    if (seg.kind !== "prose") {
      out.push(seg);
      continue;
    }
    let cursor = 0;
    // Matches `![[Foo.base]]` / `![[Foo.base#View]]`, optionally with a
    // display suffix, on its own line.
    //
    // The missing `\r?` before `$` is deliberate, and the reason is a language
    // difference rather than an oversight. JavaScript's multiline `$` matches
    // before `\r`, `\n`, `\u2028` and `\u2029`; Rust's `(?m)` treats only `\n` as
    // a line terminator. So on a CRLF Host note this pattern still matches, and
    // the region ends before the `\r` -- which leaves it in the following prose
    // segment, where `serialise` puts it straight back. The Rust port, reading
    // the same pattern with `(?m)`, matched nothing at all on a CRLF note, which
    // made the region invisible and `write_note` then persisted the agent's
    // deletion of it while reporting `health: ok`. Adding a `\r?` here would be
    // harmless but would change where the region boundary falls, so the
    // asymmetry is left explicit and pinned by
    // `test/unit/note.test.ts` instead of papered over.
    const re = /^([ \t]*)!\[\[([^\]|#]+?\.base)(#[^\]|]*)?(\|[^\]]*)?\]\][ \t]*$/gm;
    let m: RegExpExecArray | null;
    while ((m = re.exec(seg.raw)) !== null) {
      if (m.index > cursor) {
        out.push({
          kind: "prose",
          raw: seg.raw.slice(cursor, m.index),
          start: seg.start + cursor,
          end: seg.start + m.index,
        });
      }
      const target = m[2]!;
      const subpath = m[3]?.replace(/^#/, "") ?? null;
      out.push({
        kind: "baseEmbed",
        raw: m[0],
        start: seg.start + m.index,
        end: seg.start + m.index + m[0].length,
        basePath: target,
        viewName: subpath !== null && subpath !== "" ? subpath : null,
        embedded: true,
      });
      cursor = m.index + m[0].length;
    }
    if (cursor < seg.raw.length) {
      out.push({
        kind: "prose",
        raw: seg.raw.slice(cursor),
        start: seg.start + cursor,
        end: seg.end,
      });
    }
  }
  return out;
}

/** Parse a note and split base embeds into their own segments. */
export function parseNoteWithEmbeds(path: string, text: string): ParsedNote {
  const base = parseNote(path, text);
  const withEmbeds = splitBaseEmbeds(base.segments);
  const merged: Segment[] = [];
  for (const s of withEmbeds) {
    const prev = merged[merged.length - 1];
    if (prev !== undefined && prev.kind === "prose" && s.kind === "prose" && prev.end === s.start) {
      merged[merged.length - 1] = {
        kind: "prose",
        raw: prev.raw + s.raw,
        start: prev.start,
        end: s.end,
      };
    } else {
      merged.push(s);
    }
  }
  return { ...base, segments: merged };
}

// ---------------------------------------------------------------------------
// Links, tags, tasks
// ---------------------------------------------------------------------------

const LINK_RE = /(!)?\[\[([^\]\n]+?)\]\]/g;
const MD_LINK_RE = /(?<!!)\[([^\]\n]*)\]\(([^)\s]+)(?:\s+"[^"]*")?\)/g;
const TAG_RE = /(?<![\w#/:])#([A-Za-z0-9_][A-Za-z0-9_/-]*)/g;
const TASK_RE = /^[ \t]*[-*+][ \t]+\[([ xX/-])\][ \t]*(.*)$/;

/** Every wikilink and markdown link, offsets relative to `text`. */
export function extractLinks(text: string): WikiLink[] {
  const out: WikiLink[] = [];
  LINK_RE.lastIndex = 0;
  let m: RegExpExecArray | null;
  while ((m = LINK_RE.exec(text)) !== null) {
    const inner = m[2]!;
    const pipe = inner.indexOf("|");
    const targetWithSub = pipe === -1 ? inner : inner.slice(0, pipe);
    const display = pipe === -1 ? null : inner.slice(pipe + 1);

    // A subpath may also appear before the pipe.
    let target = targetWithSub;
    let subpath: string | null = null;
    const hash = target.indexOf("#");
    if (hash !== -1) {
      target = target.slice(0, hash);
      subpath = targetWithSub.slice(hash + 1);
    }

    out.push({
      target,
      subpath,
      display,
      embedded: m[1] === "!",
      start: m.index,
      end: m.index + m[0].length,
    });
  }

  MD_LINK_RE.lastIndex = 0;
  while ((m = MD_LINK_RE.exec(text)) !== null) {
    const href = m[2]!;
    // External and in-page links are not vault references.
    if (/^[a-z]+:\/\//i.test(href) || href.startsWith("#") || href.startsWith("mailto:")) continue;
    out.push({
      target: href.replace(/\.md$/, ""),
      subpath: null,
      display: m[1] || null,
      embedded: false,
      start: m.index,
      end: m.index + m[0].length,
    });
  }

  return out.sort((a, b) => a.start - b.start);
}

/**
 * Inline `#tags`. Code spans and fenced blocks are excluded, which is where a
 * naive regex picks up hex colours like `#FFF` inside `style="color: #FFF"`.
 */
export function extractInlineTags(text: string): string[] {
  const stripped = stripCode(text);
  const out: string[] = [];
  TAG_RE.lastIndex = 0;
  let m: RegExpExecArray | null;
  while ((m = TAG_RE.exec(stripped)) !== null) {
    const tag = m[1]!;
    // Skip numeric fragments, which are hex colours rather than tags.
    if (/^#?\d/.test(m[0])) continue;
    if (out.includes(tag)) continue;
    out.push(tag);
  }
  return out;
}

/** Checkboxes in the body. Powers the documented `file.tasks` extension. */
export function extractTasks(text: string): TaskItem[] {
  const out: TaskItem[] = [];
  const body = stripFrontmatter(text);
  const lines = body.split(/\r?\n/);
  for (let i = 0; i < lines.length; i++) {
    const m = TASK_RE.exec(lines[i]!);
    if (m === null) continue;
    const mark = m[1]!;
    out.push({
      checked: mark.toLowerCase() === "x",
      text: (m[2] ?? "").trim(),
      line: i,
    });
  }
  return out;
}

function stripFrontmatter(text: string): string {
  const fm = FRONTMATTER_RE.exec(text);
  return fm === null ? text : text.slice(fm[0].length);
}

/** Blank out fenced blocks and inline code so tag scanning cannot see them. */
function stripCode(text: string): string {
  let out = text;
  // Fenced blocks, including unclosed ones (which run to EOF).
  out = out.replace(/^([ \t>]*`{3,})[^\n]*\n[\s\S]*?(?:^[ \t>]*\1[ \t]*$|$)/gm, (m) =>
    m.replace(/[^\n]/g, " "),
  );
  // Inline code spans.
  out = out.replace(/`+[^`\n]*`+/g, (m) => " ".repeat(m.length));
  return out;
}
