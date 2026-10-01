/**
 * Projection and reconciliation.
 *
 * A Projection is the agent-facing rendering of a note: each Base region is
 * replaced by a ```base-rendered fence carrying its provenance. The fence
 * language is deliberately NOT `base` -- Obsidian treats a ```base fence as
 * live base YAML, so rendered markdown inside one would fail to parse. Any
 * other language renders as an inert code block.
 *
 * A Projection is NEVER written back to disk. Writing it would destroy the live
 * embed. Patches are applied by RECONCILING the agent's text against the
 * original segments, never by writing the projection.
 */

import {
  type BaseEmbedSegment,
  type BaseFenceSegment,
  isBaseRegion,
  type ParsedNote,
  parseNoteWithEmbeds,
  type Segment,
  serialise,
  splitBaseEmbeds,
} from "../note/parse";

/** The fence language for a rendered base. Deliberately not `base`. */
export const RENDER_FENCE = "base-rendered";

/** Provenance recorded in the fence info string. */
export interface FenceProvenance {
  /** The `.base` file the region came from, when it was a file embed. */
  path?: string;
  /** The view name rendered. */
  view?: string;
  /** The host note bound to `this`, when one was supplied. */
  context?: string;
  /** Rows the view resolved to. */
  rows?: number;
}

/** Escape a value for the quoted attribute form of a fence info string. */
function escapeAttr(value: string): string {
  return value.replace(/\\/g, "\\\\").replace(/"/g, '\\"');
}

/** Build the fence info string, e.g. `path="T.base" view="All" context="P.md"`. */
export function fenceInfo(p: FenceProvenance): string {
  const parts: string[] = [RENDER_FENCE];
  if (p.path !== undefined) parts.push(`path="${escapeAttr(p.path)}"`);
  if (p.view !== undefined) parts.push(`view="${escapeAttr(p.view)}"`);
  if (p.context !== undefined) parts.push(`context="${escapeAttr(p.context)}"`);
  if (p.rows !== undefined) parts.push(`rows="${p.rows}"`);
  return parts.join(" ");
}

/** Parse a fence info string back into provenance. */
export function parseFenceInfo(info: string): FenceProvenance | null {
  const trimmed = info.trim();
  if (!trimmed.startsWith(RENDER_FENCE)) return null;
  const out: FenceProvenance = {};
  const re = /(\w+)="([^"]*)"/g;
  let m: RegExpExecArray | null;
  while ((m = re.exec(trimmed)) !== null) {
    const key = m[1] as keyof FenceProvenance;
    const value = m[2]!;
    if (key === "rows") out.rows = Number(value);
    else (out as Record<string, unknown>)[key] = value;
  }
  return out;
}

/** The rendered body that stands in for one base region. */
export interface RenderedRegion {
  /** The fence block, ready to splice into a projection. */
  text: string;
  provenance: FenceProvenance;
}

/** Wrap rendered markdown in a provenance fence. */
export function wrapInFence(body: string, p: FenceProvenance): RenderedRegion {
  return {
    text: [`\`\`\`${fenceInfo(p)}`, body.replace(/\n+$/, ""), "```"].join("\n"),
    provenance: p,
  };
}

/**
 * Build the projection of a note by substituting each base region.
 *
 * `render` receives the region's identity and returns the markdown body. It may
 * be async, because resolving a region reads the target `.base` file.
 */
export async function project(
  note: ParsedNote,
  render: (region: BaseRegionRef) => string | RenderedRegion | Promise<string | RenderedRegion>,
): Promise<string> {
  const out: string[] = [];
  for (let at = 0; at < note.segments.length; at++) {
    const seg = note.segments[at]!;
    if (!isBaseRegion(seg)) {
      out.push(seg.raw);
      continue;
    }
    const ref = regionRef(seg);
    const rendered = await render(ref);
    const text = typeof rendered === "string" ? rendered : rendered.text;
    out.push(text);
    // A Base region occupies whole lines, so its replacement has to end at a
    // line boundary. It does NOT get a newline added when the boundary is
    // already there, and it usually is: the prose after a region begins with the
    // newline that ended the region's own line. Adding one anyway left a stray
    // blank line in every Projection, and because `write_note` reconciles rather
    // than writes, that newline came back out as a spurious edit.
    if (!endsItsOwnLine(text, note.segments[at + 1])) {
      out.push("\n");
    }
  }
  return out.join("");
}

/**
 * Whether this replacement already ends where a line ends.
 *
 * True when the text ends with a newline, when nothing follows the region (the
 * region was last in the note), or when the following segment starts with the
 * newline that terminated the region's own line.
 */
function endsItsOwnLine(text: string, next: ParsedNote["segments"][number] | undefined): boolean {
  if (text.endsWith("\n")) return true;
  if (next === undefined) return true;
  return next.raw.startsWith("\n");
}

/** Identifies one base region within a note. */
export interface BaseRegionRef {
  /** Index of the region among the note's base regions. */
  index: number;
  /** The `.base` file, for an embed. Undefined for an inline fence. */
  basePath?: string;
  /** The `#View` selector the embed pinned, if any. */
  viewName?: string | null;
  /** Inline base YAML, for a ```base fence. */
  yaml?: string;
  /** True for a ```base-rendered fence: a region handed back by a Projection. */
  rendered?: boolean;
  /** Byte span in the original note. */
  start: number;
  end: number;
}

function regionRef(seg: Segment): BaseRegionRef {
  if (seg.kind === "baseEmbed") {
    const e = seg as BaseEmbedSegment;
    return {
      index: 0,
      basePath: e.basePath,
      viewName: e.viewName,
      start: e.start,
      end: e.end,
    };
  }
  const f = seg as BaseFenceSegment;
  // A rendered fence carries the Base it came from on its info string, which is
  // what lets it be paired with the region it replaced. A live ```base fence
  // carries its YAML instead and is matched on that.
  return {
    index: 0,
    yaml: f.yaml,
    basePath: f.basePath,
    viewName: f.viewName,
    rendered: f.rendered === true,
    start: f.start,
    end: f.end,
  };
}

/** Every base region in a note, in document order and 0-indexed. */
export function baseRegions(note: ParsedNote): BaseRegionRef[] {
  const out: BaseRegionRef[] = [];
  for (const seg of note.segments) {
    if (!isBaseRegion(seg)) continue;
    const ref = regionRef(seg);
    ref.index = out.length;
    out.push(ref);
  }
  return out;
}

// ---------------------------------------------------------------------------
// Reconciliation
// ---------------------------------------------------------------------------

export interface RefusedRegion {
  index: number;
  /** The base the region belongs to, when known. */
  basePath?: string;
  reason: string;
  /** Actionable guidance, including what the agent should do instead. */
  guidance: string;
}

export interface ReconcileResult {
  /** The note text to write, with base regions restored. */
  text: string;
  /** Base regions the agent tried to change. */
  refused: RefusedRegion[];
  /** True when the agent deleted a base region outright. */
  removedRegion: boolean;
}

/**
 * Reconcile an agent's edited note against the stored original.
 *
 * Rules, in order:
 *  1. A base region present in both and unchanged is kept as-is.
 *  2. A base region present in both but modified is RESTORED to the original,
 *     and the attempt is reported. Adding rows to a rendered base is not a
 *     supported edit: rows come from notes, not from the base.
 *  3. A base region the agent DELETED is restored, and reported.
 *  4. All non-base changes are applied.
 *
 * The result therefore applies the agent's real edits while guaranteeing the
 * base regions survive byte-for-byte.
 */
export function reconcileNote(notePath: string, original: string, edited: string): ReconcileResult {
  const before = parseNoteWithEmbeds(notePath, original);
  const after = parseNoteWithEmbeds(notePath, edited);

  const originalRegions = baseRegions(before);
  const editedRegions = baseRegions(after);

  const refused: RefusedRegion[] = [];
  let removedRegion = false;

  // Pair each edited region with the original region it replaced.
  //
  // A rendered fence is matched on the Base path carried in its info string,
  // because that is the only link back to the region it came from -- the fence
  // body is rendered rows, not YAML, so position alone would mispair a note
  // whose regions were reordered. Everything else is matched by position,
  // since an embed or a live ```base fence is identified by its own text.
  const pairs = new Map<number, string>();
  const claimed = new Set<number>();
  editedRegions.forEach((ref, i) => {
    const orig = originalRegions[i];
    if (orig === undefined) return;
    // Same base and same pinned view? Then it is the same region.
    const sameBase =
      ref.basePath === orig.basePath &&
      (ref.viewName ?? null) === (orig.viewName ?? null) &&
      (ref.yaml === undefined) === (orig.yaml === undefined);
    if (sameBase) {
      pairs.set(i, original.slice(orig.start, orig.end));
      claimed.add(i);
    }
  });

  // Second pass for rendered fences whose position shifted, matched by path.
  editedRegions.forEach((ref, i) => {
    if (pairs.has(i) || ref.rendered !== true || ref.basePath === undefined) return;
    const hit = originalRegions.findIndex(
      (orig, j) => !claimed.has(j) && orig.basePath === ref.basePath,
    );
    if (hit < 0) return;
    const hitRef = originalRegions[hit]!;
    pairs.set(i, original.slice(hitRef.start, hitRef.end));
    claimed.add(hit);
  });

  const missing = originalRegions.filter((_, i) => !claimed.has(i));
  if (missing.length > 0) removedRegion = true;

  // Rebuild: walk the edited segments, restoring base regions, and drop any
  // region the agent added.
  const out: string[] = [];
  for (const seg of after.segments) {
    if (!isBaseRegion(seg)) {
      out.push(seg.raw);
      continue;
    }
    const index = baseIndexOf(after, seg);
    const originalRaw = pairs.get(index);
    if (originalRaw !== undefined) {
      // A rendered fence is replaced silently. Round-tripping a Projection --
      // read the note, edit the prose, write it back -- is the DESIGNED flow, so
      // reporting a refusal every time would mark the happy path
      // `partial-with-errors` and teach an agent to ignore refusals, which
      // costs more than it buys. We cannot tell an untouched fence from an
      // edited one here anyway: the rendered rows were never the source of
      // truth, and the live region is restored byte for byte either way.
      const isRendered = seg.kind === "baseFence" && seg.rendered === true;
      if (!isRendered && originalRaw !== seg.raw) {
        refused.push({
          index,
          basePath: seg.kind === "baseEmbed" ? seg.basePath : undefined,
          reason: "The rendered base region was modified.",
          guidance:
            "Rows in a base come from notes, not from the base file. To add a row, " +
            "create a note whose properties satisfy the base's filter, then re-read the " +
            "host note. Use add_note_to_base to be guided through creating a matching note.",
        });
      }
      out.push(originalRaw);
      continue;
    }
    refused.push({
      index,
      basePath: seg.kind === "baseEmbed" ? seg.basePath : undefined,
      reason: "A new base region was inserted.",
      guidance:
        "Adding a base region to a note is not supported by default. Edit the .base " +
        "file itself, then re-read this note.",
    });
  }

  // Re-insert any region the agent deleted, at its original position.
  let text = out.join("");
  if (missing.length > 0) {
    for (const ref of missing) {
      const raw = original.slice(ref.start, ref.end);
      const anchor = text.indexOf("\n", ref.start);
      text =
        anchor === -1
          ? `${text}\n${raw}\n`
          : `${text.slice(0, anchor + 1)}${raw}\n${text.slice(anchor + 1)}`;
      refused.push({
        index: ref.index,
        basePath: ref.basePath,
        reason: "The base region was removed.",
        guidance:
          "Base regions are never removed by a note edit. Restore the embed, or delete " +
          "the base region deliberately outside this tool.",
      });
    }
  }

  return { text, refused, removedRegion };
}

function baseIndexOf(note: ParsedNote, target: Segment): number {
  let i = 0;
  for (const seg of note.segments) {
    if (!isBaseRegion(seg)) continue;
    if (seg === target) return i;
    i++;
  }
  return -1;
}

export { serialise, splitBaseEmbeds };
