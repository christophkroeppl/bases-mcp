/**
 * Link resolution, matching Obsidian's rules.
 *
 * The vault index registers each note under four keys -- full path, path
 * without extension, basename, basename without extension -- and first
 * registration wins. Because the index is built from a sorted path list, that
 * ordering yields Obsidian's documented tie-break: an ambiguous link resolves to
 * the SHORTEST path, not to a note in the same folder.
 */

import { stripExtension } from "../expr/values";

/** ASCII-fold and lowercase, so `Página` matches `Pagina`. */
export function foldKey(s: string): string {
  return s
    .normalize("NFKD")
    .replace(/[\u0300-\u036f]/g, "")
    .toLowerCase();
}

/**
 * Resolve a link target against the index.
 *
 * Handles the forms Obsidian accepts: `Note`, `Note.md`, `folder/Note` and
 * `folder/Note.md`. Aliases are deliberately NOT consulted -- a bare
 * `[[Alias]]` does not resolve in Obsidian.
 */
export function matchPath(target: string, byPath: Map<string, string>): string | undefined {
  if (target === "") return undefined;

  const direct = byPath.get(target);
  if (direct !== undefined) return direct;

  const noExt = stripExtension(target);
  const withoutExt = byPath.get(noExt);
  if (withoutExt !== undefined) return withoutExt;

  // Fall back to a folded comparison so case and diacritics do not matter.
  const wanted = foldKey(noExt);
  let best: string | undefined;
  let bestLength = Infinity;
  for (const [key, value] of byPath) {
    if (foldKey(stripExtension(key)) !== wanted) continue;
    if (value.length < bestLength) {
      best = value;
      bestLength = value.length;
    }
  }
  return best;
}

/**
 * Whether a link target points at `path`. Used for link equality, which the
 * docs define as "equivalent as long as they point to the same file".
 */
export function pointsAt(linkTarget: string, path: string): boolean {
  const a = foldKey(stripExtension(linkTarget));
  const b = foldKey(stripExtension(path));
  return a === b || a.endsWith(`/${b}`) || b.endsWith(`/${a}`);
}
