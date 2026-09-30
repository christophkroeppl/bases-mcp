/**
 * Lexer for the Bases expression language.
 *
 * The only genuinely hard case is `/`: it is both the division operator and the
 * regex literal opener. We decide based on the previous significant token —
 * division is only legal after a value-ish token, so `/` elsewhere starts a
 * regex. That is the same rule JavaScript itself uses.
 */

import { BasesError } from "./errors";

export enum Tok {
  Number,
  String,
  Regex,
  Identifier,
  Keyword,
  True,
  False,
  Null,
  Plus,
  Minus,
  Star,
  Slash,
  Percent,
  Bang,
  Eq,
  NotEq,
  Gt,
  GtEq,
  Lt,
  LtEq,
  AndAnd,
  OrOr,
  LParen,
  RParen,
  LBracket,
  RBracket,
  Comma,
  Dot,
  EOF,
}

const KEYWORDS: Record<string, Tok> = {
  and: Tok.AndAnd,
  or: Tok.OrOr,
  not: Tok.Bang,
};

export interface Token {
  type: Tok;
  /** Raw source text of the token, used for error messages. */
  raw: string;
  /** Parsed value for String / Regex / Identifier / Number tokens. */
  value?: string | number | RegExp;
  start: number;
  end: number;
}

export function lex(input: string): Token[] {
  const tokens: Token[] = [];
  let i = 0;
  /** True when the previous token can end an expression, so `/` means divide. */
  let afterValue = false;

  while (i < input.length) {
    const c = input[i]!;

    if (c === " " || c === "\t" || c === "\n" || c === "\r") {
      i++;
      continue;
    }

    const start = i;

    if (c === "/" && !afterValue) {
      const rx = readRegex(input, i);
      tokens.push({
        type: Tok.Regex,
        raw: input.slice(start, rx.end),
        value: rx.value,
        start,
        end: rx.end,
      });
      i = rx.end;
      afterValue = true;
      continue;
    }

    if (isDigit(c) || (c === "." && isDigit(input[i + 1] ?? ""))) {
      const { end, text } = readNumber(input, i);
      tokens.push({ type: Tok.Number, raw: text, value: Number(text), start, end });
      i = end;
      afterValue = true;
      continue;
    }

    if (c === '"' || c === "'") {
      const { end, text } = readString(input, i);
      tokens.push({ type: Tok.String, raw: text, value: decodeString(text, c), start, end });
      i = end;
      afterValue = true;
      continue;
    }

    if (isIdentStart(c)) {
      let end = i;
      while (end < input.length && isIdentPart(input[end]!)) end++;
      const raw = input.slice(i, end);
      i = end;
      if (raw === "true") {
        tokens.push({ type: Tok.True, raw, start, end });
      } else if (raw === "false") {
        tokens.push({ type: Tok.False, raw, start, end });
      } else if (raw === "null") {
        tokens.push({ type: Tok.Null, raw, start, end });
      } else {
        const kw = KEYWORDS[raw];
        if (kw !== undefined) {
          tokens.push({ type: Tok.Keyword, raw, start, end });
        } else {
          tokens.push({ type: Tok.Identifier, raw, value: raw, start, end });
        }
      }
      afterValue = true;
      continue;
    }

    const two = input.slice(i, i + 2);
    const simple = matchSymbol(two, i);
    if (simple) {
      tokens.push(simple.token);
      i = simple.next;
      afterValue = simple.token.type === Tok.RParen || simple.token.type === Tok.RBracket;
      continue;
    }

    const one = matchSingle(c);
    if (one) {
      tokens.push({ type: one, raw: c, start, end: i + 1 });
      i++;
      afterValue = one === Tok.RParen || one === Tok.RBracket;
      continue;
    }

    throw new BasesError(`Unexpected character "${c}"`, { position: start, source: input });
  }

  tokens.push({ type: Tok.EOF, raw: "", start: i, end: i });
  return tokens;
}

function matchSymbol(two: string, start: number): { token: Token; next: number } | null {
  switch (two) {
    case "==":
      return { token: { type: Tok.Eq, raw: "==", start, end: start + 2 }, next: start + 2 };
    case "!=":
      return { token: { type: Tok.NotEq, raw: "!=", start, end: start + 2 }, next: start + 2 };
    case ">=":
      return { token: { type: Tok.GtEq, raw: ">=", start, end: start + 2 }, next: start + 2 };
    case "<=":
      return { token: { type: Tok.LtEq, raw: "<=", start, end: start + 2 }, next: start + 2 };
    case "&&":
      return { token: { type: Tok.AndAnd, raw: "&&", start, end: start + 2 }, next: start + 2 };
    case "||":
      return { token: { type: Tok.OrOr, raw: "||", start, end: start + 2 }, next: start + 2 };
    default:
      return null;
  }
}

function matchSingle(c: string): Tok | null {
  switch (c) {
    case "+":
      return Tok.Plus;
    case "-":
      return Tok.Minus;
    case "*":
      return Tok.Star;
    case "/":
      return Tok.Slash;
    case "%":
      return Tok.Percent;
    case "!":
      return Tok.Bang;
    case ">":
      return Tok.Gt;
    case "<":
      return Tok.Lt;
    case "(":
      return Tok.LParen;
    case ")":
      return Tok.RParen;
    case "[":
      return Tok.LBracket;
    case "]":
      return Tok.RBracket;
    case ",":
      return Tok.Comma;
    case ".":
      return Tok.Dot;
    default:
      return null;
  }
}

function readNumber(input: string, start: number): { end: number; text: string } {
  let i = start;
  while (i < input.length && isDigit(input[i]!)) i++;
  if (input[i] === ".") {
    i++;
    while (i < input.length && isDigit(input[i]!)) i++;
  }
  return { end: i, text: input.slice(start, i) };
}

function readString(input: string, start: number): { end: number; text: string } {
  const quote = input[start];
  let i = start + 1;
  while (i < input.length) {
    const ch = input[i]!;
    if (ch === "\\") {
      i += 2;
      continue;
    }
    if (ch === quote) {
      return { end: i + 1, text: input.slice(start, i + 1) };
    }
    if (ch === "\n") break;
    i++;
  }
  throw new BasesError("Unterminated string literal", { position: start, source: input });
}

function readRegex(input: string, start: number): { end: number; value: RegExp } {
  let i = start + 1;
  let inClass = false;
  while (i < input.length) {
    const ch = input[i]!;
    if (ch === "\\") {
      i += 2;
      continue;
    }
    if (ch === "\n") break;
    if (ch === "[") inClass = true;
    else if (ch === "]") inClass = false;
    else if (ch === "/" && !inClass) break;
    i++;
  }
  if (input[i] !== "/") {
    throw new BasesError("Unterminated regular expression", { position: start, source: input });
  }
  const bodyEnd = i;
  i++;
  const flagStart = i;
  while (i < input.length && isIdentPart(input[i]!)) i++;
  const body = input.slice(start + 1, bodyEnd);
  const flags = input.slice(flagStart, i);
  if (flags !== "" && !/^[gimsuy]*$/.test(flags)) {
    throw new BasesError(`Invalid regular expression flags "${flags}"`, {
      position: start,
      source: input,
    });
  }
  let value: RegExp;
  try {
    value = new RegExp(body, flags);
  } catch (err) {
    throw new BasesError(`Invalid regular expression: ${(err as Error).message}`, {
      position: start,
      source: input,
    });
  }
  return { end: i, value };
}

function decodeString(raw: string, quote: string): string {
  const inner = raw.slice(1, -1);
  if (!inner.includes("\\")) return inner;
  let out = "";
  for (let i = 0; i < inner.length; i++) {
    const ch = inner[i]!;
    if (ch !== "\\") {
      out += ch;
      continue;
    }
    const next = inner[++i];
    switch (next) {
      case "n":
        out += "\n";
        break;
      case "t":
        out += "\t";
        break;
      case "r":
        out += "\r";
        break;
      case "\\":
        out += "\\";
        break;
      case '"':
        out += '"';
        break;
      case "'":
        out += "'";
        break;
      case undefined:
        out += "\\";
        break;
      default:
        out += `\\${next}`;
    }
  }
  // A double-quoted string in YAML is still delivered without the outer quotes.
  void quote;
  return out;
}

function isDigit(c: string): boolean {
  return c >= "0" && c <= "9";
}

function isIdentStart(c: string): boolean {
  return (
    (c >= "a" && c <= "z") ||
    (c >= "A" && c <= "Z") ||
    c === "_" ||
    c === "$" ||
    // Non-ASCII identifiers. Obsidian's own parser rejects these (obsidian-help
    // issue #1095) but we follow the documented grammar, which permits them.
    c.charCodeAt(0) > 127
  );
}

function isIdentPart(c: string): boolean {
  return isIdentStart(c) || isDigit(c);
}
