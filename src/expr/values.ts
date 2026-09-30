/**
 * Core Bases value types.
 *
 * The expression language distinguishes a handful of value kinds that plain
 * JavaScript primitives cannot express: Links (compared by resolved target,
 * not text), Dates (with moment-style formatting and duration arithmetic) and
 * Durations (a distinct type that Obsidian returns from date subtraction even
 * though its docs claim milliseconds).
 */

/** A resolved link to another note. Compared by target, not by its text. */
export class LinkValue {
  /** The link target as written, e.g. `Projects/SomeProject` or `SomeProject.md`. */
  readonly target: string;
  /** Optional display text: the `|Display` half of a wikilink. */
  readonly display: string | undefined;
  /**
   * Resolved vault-relative path (`Projects/SomeProject.md`) when the target
   * matched a file in the vault, else undefined. Two links that resolve to the
   * same file are equal regardless of how either was written.
   */
  readonly resolvedPath: string | undefined;

  constructor(target: string, display?: string | null, resolvedPath?: string) {
    this.target = target;
    this.display = display ?? undefined;
    this.resolvedPath = resolvedPath;
  }

  /** Text Obsidian would show for this link. */
  get label(): string {
    if (this.display !== undefined) return this.display;
    if (this.resolvedPath !== undefined) {
      const base = basename(this.resolvedPath);
      return stripExtension(base);
    }
    const segs = this.target.split("/");
    return stripExtension(segs[segs.length - 1] ?? this.target);
  }

  /** The `[[...]]` text form, used by the `link()` global. */
  toWikilink(): string {
    return this.display !== undefined ? `[[${this.target}|${this.display}]]` : `[[${this.target}]]`;
  }
}

/**
 * A date/time. Mirrors what Obsidian stores: `YYYY-MM-DD` for dates and
 * ISO `YYYY-MM-DDTHH:mm:ss` for date-times. Date-only values are constructed in
 * LOCAL time, not UTC, to match Obsidian.
 */
export class DateValue {
  /** Epoch milliseconds. */
  readonly ms: number;
  /** True when the source had no time component. */
  readonly dateOnly: boolean;

  constructor(ms: number, dateOnly = false) {
    this.ms = ms;
    this.dateOnly = dateOnly;
  }

  static fromParts(
    y: number,
    mo: number,
    d: number,
    h = 0,
    mi = 0,
    s = 0,
    ms = 0,
    dateOnly = false,
  ): DateValue {
    return new DateValue(new Date(y, mo - 1, d, h, mi, s, ms).getTime(), dateOnly);
  }

  get year(): number {
    return this.local(this.ms).getFullYear();
  }
  /** 1-12, matching the spec (not the JS 0-11 convention). */
  get month(): number {
    return this.local(this.ms).getMonth() + 1;
  }
  get day(): number {
    return this.local(this.ms).getDate();
  }
  /** 0-23. */
  get hour(): number {
    return this.local(this.ms).getHours();
  }
  get minute(): number {
    return this.local(this.ms).getMinutes();
  }
  get second(): number {
    return this.local(this.ms).getSeconds();
  }
  get millisecond(): number {
    return this.local(this.ms).getMilliseconds();
  }

  private local(ms: number): Date {
    return new Date(ms);
  }

  /** Midnight of this date, dropping the time component. */
  date(): DateValue {
    return DateValue.fromParts(this.year, this.month, this.day, 0, 0, 0, 0, true);
  }

  /** `HH:mm:ss`. */
  time(): string {
    return `${pad(this.hour)}:${pad(this.minute)}:${pad(this.second)}`;
  }

  toString(): string {
    const base = `${this.year}-${pad(this.month)}-${pad(this.day)}`;
    if (this.dateOnly) return base;
    return `${base}T${this.time()}`;
  }
}

/** Units accepted in duration literals, e.g. `"1d"` or `duration("2w")`. */
const DURATION_UNITS: Record<string, number> = {
  ms: 1,
  millisecond: 1,
  milliseconds: 1,
  s: 1000,
  sec: 1000,
  secs: 1000,
  second: 1000,
  seconds: 1000,
  m: 60_000,
  min: 60_000,
  mins: 60_000,
  minute: 60_000,
  minutes: 60_000,
  h: 3_600_000,
  hr: 3_600_000,
  hrs: 3_600_000,
  hour: 3_600_000,
  hours: 3_600_000,
  d: 86_400_000,
  day: 86_400_000,
  days: 86_400_000,
  w: 604_800_000,
  week: 604_800_000,
  weeks: 604_800_000,
};

/** Units that are calendar-relative and so cannot be a fixed millisecond span. */
const CALENDAR_UNITS = new Set(["M", "month", "months", "y", "year", "years"]);

/**
 * A span of time. Obsidian's runtime returns this from date subtraction
 * (`date(a) - date(b)`) even though its own docs say that yields milliseconds.
 * We model it as a first-class type with field accessors, because real vaults
 * use `(now() - file.mtime).days`. `number()` on a duration still yields
 * milliseconds, so the documented idiom works too.
 */
export class DurationValue {
  readonly ms: number;
  /**
   * Calendar components preserved so that `+ "1M"` means "one month later"
   * rather than an approximation in milliseconds.
   */
  readonly months: number;
  readonly years: number;

  constructor(ms: number, months = 0, years = 0) {
    this.ms = ms;
    this.months = months;
    this.years = years;
  }

  get days(): number {
    return Math.trunc(this.ms / 86_400_000);
  }
  get hours(): number {
    return Math.trunc(this.ms / 3_600_000);
  }
  get minutes(): number {
    return Math.trunc(this.ms / 60_000);
  }
  get seconds(): number {
    return Math.trunc(this.ms / 1000);
  }
  get milliseconds(): number {
    return this.ms;
  }

  toString(): string {
    return humanDuration(this.ms);
  }
}

/** Type guard helpers ------------------------------------------------------ */

export function isLink(v: unknown): v is LinkValue {
  return v instanceof LinkValue;
}
export function isDate(v: unknown): v is DateValue {
  return v instanceof DateValue;
}
export function isDuration(v: unknown): v is DurationValue {
  return v instanceof DurationValue;
}
export function isList(v: unknown): v is BasesValue[] {
  return Array.isArray(v);
}

/** A plain object value, e.g. the result of `file.properties`. */
export interface ObjectValue {
  [key: string]: BasesValue;
}

/**
 * The full set of runtime value kinds an expression can produce.
 *
 * `ObjectValue` breaks what would otherwise be a circular type alias: it is a
 * plain interface rather than an inline index signature.
 */
export type BasesValue =
  | string
  | number
  | boolean
  | null
  | BasesValue[]
  | LinkValue
  | DateValue
  | DurationValue
  | RegExp
  | FileValue
  | ObjectValue;

/**
 * The `file` object exposed by `file.*` and `this.file`. Distinct from
 * DateValue/LinkValue, which are leaf values; a FileValue is a live view onto a
 * note and can resolve links and links-to.
 */
export class FileValue {
  readonly path: string;
  readonly name: string;
  readonly basename: string;
  readonly folder: string;
  readonly ext: string;
  /** Lazy accessors, supplied by the vault layer. */
  readonly accessors: FileAccessors;

  constructor(path: string, accessors: FileAccessors) {
    this.path = path;
    this.name = path;
    // `basename` excludes the extension. Probed against a live Obsidian 1.13.7:
    // the `file.name` column renders as "Alpha" for "Alpha.md", so Obsidian's
    // `file.name` behaves as a basename despite the docs describing otherwise.
    // Recorded in docs/divergences.md.
    this.basename = stripExtension(basename(path));
    this.folder = folderOf(path);
    this.ext = extOf(path);
    this.accessors = accessors;
  }
}

export interface FileAccessors {
  tags(): BasesValue[];
  links(): BasesValue[];
  embeds(): BasesValue[];
  backlinks(): BasesValue[];
  properties(): Record<string, BasesValue>;
  ctime(): DateValue;
  mtime(): DateValue;
  size(): number;
  /**
   * Checkboxes parsed from the note body. NOT part of the documented `file.*`
   * surface -- Obsidian's co-founder states Bases does not read file contents
   * -- so this is shipped as a documented extension.
   */
  tasks(): BasesValue[];
  /** Resolve a link target to a file value, or undefined when unresolved. */
  resolve(target: string): FileValue | undefined;
  /** All link targets pointing at this file, for `link.linksTo`. */
  linksTo(other: FileValue): boolean;
}

/** Parsing helpers --------------------------------------------------------- */

export function basename(path: string): string {
  const segs = path.split("/");
  return segs[segs.length - 1] ?? path;
}

export function extOf(path: string): string {
  const base = basename(path);
  const i = base.lastIndexOf(".");
  return i <= 0 ? "" : base.slice(i + 1);
}

export function stripExtension(path: string): string {
  const i = path.lastIndexOf(".");
  return i <= 0 ? path : path.slice(0, i);
}

/**
 * The containing folder of a path.
 *
 * A vault-root note reports `/`, not the empty string -- confirmed against
 * `base:query format=json` on Obsidian 1.13.7, where `Root Project.md` emits
 * `"folder": "/"` while `Projects/SomeProject.md` emits `"folder": "Projects"`.
 */
export function folderOf(path: string): string {
  const i = path.lastIndexOf("/");
  return i < 0 ? "/" : path.slice(0, i);
}

function pad(n: number): string {
  return n < 10 ? `0${n}` : String(n);
}

function humanDuration(ms: number): string {
  const abs = Math.abs(ms);
  if (abs === 86_400_000) return "a day";
  if (abs === 3_600_000) return "an hour";
  if (abs === 60_000) return "a minute";
  return `${ms} ms`;
}

/**
 * Parse a duration literal such as `"1d"`, `"2w"` or `"1M"`.
 *
 * Returns calendar months/years separately because those are not fixed-length;
 * `M` is a month and `m` is a minute, following the Moment.js convention Obsidian
 * documents.
 */
export function parseDurationLiteral(input: string): DurationValue {
  const text = input.trim();
  if (text === "") return new DurationValue(0);
  if (/^-?\d+(\.\d+)?$/.test(text)) {
    // A bare number is milliseconds.
    return new DurationValue(Number(text));
  }

  const re = /(-?\d+(?:\.\d+)?)\s*([a-zA-Z]+)/g;
  let total = 0;
  let months = 0;
  let years = 0;
  let matched = false;
  let m: RegExpExecArray | null;
  while ((m = re.exec(text)) !== null) {
    const n = Number(m[1]);
    const unit = m[2] ?? "";
    if (CALENDAR_UNITS.has(unit)) {
      if (unit === "M" || unit.startsWith("month")) months += n;
      else years += n;
      matched = true;
      continue;
    }
    const mult = DURATION_UNITS[unit];
    if (mult === undefined) {
      throw new Error(`Unrecognised duration unit "${unit}" in "${input}"`);
    }
    total += n * mult;
    matched = true;
  }
  if (!matched) {
    throw new Error(`Could not parse duration "${input}"`);
  }
  return new DurationValue(total, months, years);
}

/**
 * Parse a date from the forms Obsidian accepts: `YYYY-MM-DD`,
 * `YYYY-MM-DDTHH:mm:ss`, and the documented formula input `YYYY-MM-DD HH:mm:ss`.
 *
 * Date-only strings are built in LOCAL time so that `date("2024-12-01")` does
 * not shift a day in negative-offset timezones.
 */
export function parseDateValue(input: string): DateValue {
  const text = input.trim();
  let m = /^(\d{4})-(\d{2})-(\d{2})$/.exec(text);
  if (m) {
    return DateValue.fromParts(+m[1], +m[2], +m[3], 0, 0, 0, 0, true);
  }
  m = /^(\d{4})-(\d{2})-(\d{2})[T ](\d{2}):(\d{2})(?::(\d{2}))?(?:\.(\d{1,3}))?/.exec(text);
  if (m) {
    return DateValue.fromParts(
      +m[1],
      +m[2],
      +m[3],
      +m[4],
      +m[5],
      m[6] ? +m[6] : 0,
      m[7] ? +m[7].padEnd(3, "0") : 0,
      false,
    );
  }
  throw new Error(`Invalid date format: "${input}"`);
}
