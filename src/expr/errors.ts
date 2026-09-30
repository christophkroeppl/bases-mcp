/**
 * Error type for the Bases engine.
 *
 * The whole engine fails loudly: an unimplemented construct must surface as a
 * `BasesError` rather than a silent `null`, because a wrong `null` looks exactly
 * like real data to whoever is reading the rendered table.
 */

export interface BasesErrorContext {
  /** Character offset in the source expression. */
  position?: number;
  /** The source expression, for a caret-annotated message. */
  source?: string;
  /** The property ID or segment path we were resolving, when known. */
  property?: string;
  /** The view name, when the error came from view evaluation. */
  view?: string;
  /** The note path, when the error came from evaluating one note. */
  note?: string;
  /** Which construct was being evaluated, e.g. a function name. */
  construct?: string;
}

export class BasesError extends Error {
  readonly position: number | undefined;
  readonly property: string | undefined;
  readonly view: string | undefined;
  readonly note: string | undefined;
  readonly construct: string | undefined;

  constructor(message: string, context: BasesErrorContext = {}) {
    super(BasesError.format(message, context));
    this.name = "BasesError";
    this.position = context.position;
    this.property = context.property;
    this.view = context.view;
    this.note = context.note;
    this.construct = context.construct;
  }

  private static format(message: string, context: BasesErrorContext): string {
    const parts = [message];
    if (context.construct) parts.push(`(construct: ${context.construct})`);
    if (context.property) parts.push(`(property: ${context.property})`);
    if (context.view) parts.push(`(view: ${context.view})`);
    if (context.note) parts.push(`(note: ${context.note})`);
    let out = parts.join(" ");
    if (context.source !== undefined && context.position !== undefined) {
      out += `\n  ${context.source}\n  ${" ".repeat(Math.max(0, context.position))}^`;
    }
    return out;
  }
}

/** Raised when a base references `this` but no host note was supplied. */
export class MissingThisContextError extends BasesError {
  constructor(reference: string) {
    super(
      `This base references "this" (${reference}) but no host note was supplied. ` +
        `The Obsidian CLI cannot bind "this" at all and returns an empty result; ` +
        `we require an explicit host note so the result is never silently wrong.`,
      { construct: "this" },
    );
    this.name = "MissingThisContextError";
  }
}
