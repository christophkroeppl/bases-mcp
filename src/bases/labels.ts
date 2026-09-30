/**
 * Property display labels.
 *
 * Obsidian does not render a Property ID as a column header. It renders a
 * *display label* derived from the ID, and that label is what appears both as
 * the `format=json` object key and as the `format=md` table header. Getting
 * this wrong is invisible in the UI and fatal to the parity suite, so the whole
 * rule lives here and both renderers call it.
 *
 * The labels below were probed directly against `obsidian base:query` on
 * Obsidian 1.13.7 rather than inferred from the docs. The docs publish types
 * and descriptions for `file.*` but no label table, and the labels are not
 * mechanical: `file.name` becomes `file name` while `file.folder` becomes
 * `folder`, dropping the namespace entirely.
 *
 * The fallback is "strip the namespace prefix and otherwise leave the ID
 * alone": `formula.priority_display` labels as `priority_display`, NOT
 * `Priority Display`. Verified with a probe base carrying no `displayName`.
 *
 * An explicit `properties.<id>.displayName` always wins, and is used verbatim
 * -- Obsidian does not title-case it either (`PR Priority` stays `PR
 * Priority`).
 */

import type { BaseFile } from "./parse";

/**
 * `file.*` labels that are not derivable from the ID.
 *
 * Every entry was confirmed by a live `format=json` probe. Keys absent here
 * fall through to the generic rule and label as their bare segment, which is
 * also correct for the undocumented `file.basename` -> `file base name` only
 * because it is listed explicitly.
 */
const FILE_LABELS: Record<string, string> = {
  "file.name": "file name",
  "file.basename": "file base name",
  "file.path": "file path",
  // The root folder is "/", not "", and a folder drops the namespace prefix.
  "file.folder": "folder",
  "file.ext": "file extension",
  "file.size": "file size",
  "file.ctime": "created time",
  "file.mtime": "modified time",
  "file.tags": "file tags",
  "file.links": "file links",
  "file.embeds": "file embeds",
  "file.backlinks": "file backlinks",
  "file.properties": "properties",
  "file.file": "file",
};

/** Strip a leading `note.` / `file.` / `formula.` namespace. */
export function stripNamespace(id: string): string {
  for (const prefix of ["note.", "file.", "formula."]) {
    if (id.startsWith(prefix)) return id.slice(prefix.length);
  }
  return id;
}

/**
 * The label Obsidian shows for a property, ignoring any configured
 * `displayName`. Callers must consult `displayNameFor` first.
 */
export function labelForId(canonicalId: string): string {
  const known = FILE_LABELS[canonicalId];
  if (known !== undefined) return known;
  return stripNamespace(canonicalId);
}

/**
 * The header text for a column: a configured `displayName` when the base sets
 * one for this property, otherwise the probed default label.
 *
 * Lookup is by CANONICAL ID only. Probed on Obsidian 1.13.7: a base with
 * `properties: {note.status: {displayName: PrefixedKeyed}}` labels a column
 * ordered as the bare `status`, while the same base keyed as
 * `properties: {status: ...}` is IGNORED and the column labels as `status`.
 * So the prefixed spelling is the one Obsidian matches on, and a bare key is
 * dead config rather than a fallback.
 */
export function displayNameFor(base: BaseFile, id: string): string {
  const c = canonicalIdOf(id);
  const cfg = base.properties[c];
  if (cfg !== undefined && typeof cfg["displayName"] === "string") return cfg["displayName"];
  return labelForId(c);
}

/** Normalise a Property ID to its `note.`-prefixed canonical spelling. */
export function canonicalIdOf(id: string): string {
  if (id.startsWith("note.") || id.startsWith("file.") || id.startsWith("formula.")) return id;
  return `note.${id}`;
}
