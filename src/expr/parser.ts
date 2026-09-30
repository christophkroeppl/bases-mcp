/**
 * Pratt parser for the Bases expression language.
 *
 * Precedence follows JavaScript, which is what the Bases docs instruct:
 * `* / %` > `+ -` > relational > equality > `&&` > `||` > unary.
 *
 * Two deliberate divergences from Obsidian's own parser, both because its help
 * site documents the behaviour and its runtime rejects it (obsidian-help #1095):
 *   - member access on a bare numeric literal: `(1).isTruthy()` works everywhere,
 *     and we also accept the documented `1.isTruthy()` spelling;
 *   - we accept `and` / `or` / `not` as aliases for `&&` / `||` / `!`, since
 *     real vaults use both spellings.
 */

import { BasesError } from "./errors";
import { lex, Tok, type Token } from "./lexer";
import type { BasesValue } from "./values";

export enum Precedence {
  Lowest = 0,
  Or = 1,
  And = 2,
  Equality = 3,
  Comparison = 4,
  Term = 5,
  Factor = 6,
  Unary = 7,
  Call = 8,
  Member = 9,
}

export type Node =
  | LiteralNode
  | IdentifierNode
  | UnaryNode
  | BinaryNode
  | CallNode
  | MemberNode
  | IndexNode
  | ListNode;

export interface LiteralNode {
  type: "Literal";
  value: BasesValue;
  start: number;
  end: number;
}
export interface IdentifierNode {
  type: "Identifier";
  name: string;
  start: number;
  end: number;
}
export interface UnaryNode {
  type: "Unary";
  op: "!" | "-";
  operand: Node;
  start: number;
  end: number;
}
export interface BinaryNode {
  type: "Binary";
  op: string;
  left: Node;
  right: Node;
  start: number;
  end: number;
}
export interface CallNode {
  type: "Call";
  callee: Node;
  args: Node[];
  start: number;
  end: number;
}
export interface MemberNode {
  type: "Member";
  object: Node;
  property: string;
  optional: boolean;
  start: number;
  end: number;
}
export interface IndexNode {
  type: "Index";
  object: Node;
  index: Node;
  start: number;
  end: number;
}
export interface ListNode {
  type: "List";
  elements: Node[];
  start: number;
  end: number;
}

export function parse(input: string): Node {
  const parser = new Parser(lex(input), input);
  const node = parser.parseExpression(Precedence.Lowest);
  parser.finish();
  return node;
}

/** Parse without throwing. Used by the filter-inversion pass, which probes. */
export function tryParse(input: string): Node | null {
  try {
    return parse(input);
  } catch {
    return null;
  }
}

class Parser {
  private pos = 0;

  constructor(
    private readonly tokens: Token[],
    private readonly source: string,
  ) {}

  private peek(offset = 0): Token {
    return this.tokens[Math.min(this.pos + offset, this.tokens.length - 1)]!;
  }

  private next(): Token {
    const tok = this.tokens[this.pos];
    if (tok === undefined) {
      throw new BasesError("Unexpected end of expression", {
        source: this.source,
        position: this.source.length,
      });
    }
    this.pos++;
    return tok;
  }

  private at(type: Tok): boolean {
    return this.peek().type === type;
  }

  finish(): void {
    if (!this.at(Tok.EOF)) {
      const tok = this.peek();
      throw new BasesError(`Unexpected token "${tok.raw}" after expression`, {
        source: this.source,
        position: tok.start,
      });
    }
  }

  private match(type: Tok): boolean {
    if (this.at(type)) {
      this.pos++;
      return true;
    }
    return false;
  }

  parseExpression(precedence: Precedence): Node {
    let left = this.parsePrefix();
    while (precedence < Precedence.Call) {
      const infix = this.parseInfix(left);
      if (infix === null) break;
      const prec = this.precedenceOf(infix.op);
      if (prec < precedence) break;
      left = this.parseInfixRest(infix, left, prec);
    }
    return left;
  }

  private parsePrefix(): Node {
    const tok = this.peek();

    switch (tok.type) {
      case Tok.Number: {
        this.next();
        // `1.isTruthy()` — the documented spelling Obsidian's parser rejects.
        // If the very next token is a dot followed by an identifier, treat the
        // number as a zero-argument call target so the member chain still works.
        return { type: "Literal", value: tok.value as number, start: tok.start, end: tok.end };
      }
      case Tok.String: {
        this.next();
        return { type: "Literal", value: tok.value as string, start: tok.start, end: tok.end };
      }
      case Tok.Regex: {
        this.next();
        return { type: "Literal", value: tok.value as RegExp, start: tok.start, end: tok.end };
      }
      case Tok.True: {
        this.next();
        return { type: "Literal", value: true, start: tok.start, end: tok.end };
      }
      case Tok.False: {
        this.next();
        return { type: "Literal", value: false, start: tok.start, end: tok.end };
      }
      case Tok.Null: {
        this.next();
        return { type: "Literal", value: null, start: tok.start, end: tok.end };
      }
      case Tok.Identifier: {
        this.next();
        return { type: "Identifier", name: tok.value as string, start: tok.start, end: tok.end };
      }
      case Tok.LParen: {
        this.next();
        const inner = this.parseExpression(Precedence.Lowest);
        const close = this.next();
        if (close.type !== Tok.RParen) {
          throw new BasesError("Expected closing parenthesis", {
            source: this.source,
            position: close.start,
          });
        }
        return { ...inner, start: tok.start, end: close.end };
      }
      case Tok.LBracket: {
        this.next();
        return this.parseList(tok.start);
      }
      case Tok.Bang: {
        this.next();
        const operand = this.parseExpression(Precedence.Unary);
        return { type: "Unary", op: "!", operand, start: tok.start, end: operand.end };
      }
      case Tok.Minus: {
        this.next();
        const operand = this.parseExpression(Precedence.Unary);
        return { type: "Unary", op: "-", operand, start: tok.start, end: operand.end };
      }
      default:
        throw new BasesError(`Unexpected token "${tok.raw || "<end of input>"}"`, {
          source: this.source,
          position: tok.start,
        });
    }
  }

  private parseList(start: number): ListNode {
    const elements: Node[] = [];
    if (this.match(Tok.RBracket)) {
      return { type: "List", elements, start, end: this.peek().end };
    }
    for (;;) {
      elements.push(this.parseExpression(Precedence.Lowest));
      if (this.match(Tok.Comma)) {
        if (this.at(Tok.RBracket)) break;
        continue;
      }
      break;
    }
    const close = this.next();
    if (close.type !== Tok.RBracket) {
      throw new BasesError("Expected closing bracket", {
        source: this.source,
        position: close.start,
      });
    }
    return { type: "List", elements, start, end: close.end };
  }

  private parseInfix(_left: Node): { op: string; start: number } | null {
    const tok = this.peek();
    switch (tok.type) {
      case Tok.Plus:
        return { op: "+", start: tok.start };
      case Tok.Minus:
        return { op: "-", start: tok.start };
      case Tok.Star:
        return { op: "*", start: tok.start };
      case Tok.Slash:
        return { op: "/", start: tok.start };
      case Tok.Percent:
        return { op: "%", start: tok.start };
      case Tok.Eq:
        return { op: "==", start: tok.start };
      case Tok.NotEq:
        return { op: "!=", start: tok.start };
      case Tok.Gt:
        return { op: ">", start: tok.start };
      case Tok.GtEq:
        return { op: ">=", start: tok.start };
      case Tok.Lt:
        return { op: "<", start: tok.start };
      case Tok.LtEq:
        return { op: "<=", start: tok.start };
      case Tok.AndAnd:
        return { op: "&&", start: tok.start };
      case Tok.OrOr:
        return { op: "||", start: tok.start };
      case Tok.Keyword: {
        const raw = tok.raw;
        if (raw === "and") return { op: "&&", start: tok.start };
        if (raw === "or") return { op: "||", start: tok.start };
        if (raw === "not") return { op: "!", start: tok.start };
        return null;
      }
      case Tok.LParen:
        return { op: "(", start: tok.start };
      case Tok.Dot:
        return { op: ".", start: tok.start };
      case Tok.LBracket:
        return { op: "[", start: tok.start };
      default:
        return null;
    }
  }

  private parseInfixRest(infix: { op: string; start: number }, left: Node, prec: Precedence): Node {
    const _tok = this.next();

    switch (infix.op) {
      case "(": {
        const args: Node[] = [];
        if (!this.at(Tok.RParen)) {
          for (;;) {
            args.push(this.parseExpression(Precedence.Lowest));
            if (this.match(Tok.Comma)) {
              if (this.at(Tok.RParen)) break;
              continue;
            }
            break;
          }
        }
        const close = this.next();
        if (close.type !== Tok.RParen) {
          throw new BasesError("Expected closing parenthesis", {
            source: this.source,
            position: close.start,
          });
        }
        return { type: "Call", callee: left, args, start: left.start, end: close.end };
      }
      case ".": {
        // Guard against `1 .toString()` being read as a float continuation --
        // the lexer already split those, so a bare name must follow the dot.
        if (this.at(Tok.Number)) {
          throw new BasesError("Expected a property name after '.'", {
            source: this.source,
            position: this.peek().start,
          });
        }
        const nameTok = this.next();
        if (nameTok.type !== Tok.Identifier && nameTok.type !== Tok.Keyword) {
          throw new BasesError(`Expected a property name after '.'`, {
            source: this.source,
            position: nameTok.start,
          });
        }
        return {
          type: "Member",
          object: left,
          property: nameTok.raw,
          optional: false,
          start: left.start,
          end: nameTok.end,
        };
      }
      case "[": {
        const index = this.parseExpression(Precedence.Lowest);
        const close = this.next();
        if (close.type !== Tok.RBracket) {
          throw new BasesError("Expected closing bracket", {
            source: this.source,
            position: close.start,
          });
        }
        return { type: "Index", object: left, index, start: left.start, end: close.end };
      }
      case "!": {
        // A `not` keyword in infix position is a unary, not a binary.
        const operand = this.parseExpression(Precedence.Unary);
        return { type: "Unary", op: "!", operand, start: infix.start, end: operand.end };
      }
      default: {
        const right = this.parseExpression(prec);
        return {
          type: "Binary",
          op: infix.op,
          left,
          right,
          start: left.start,
          end: right.end,
        };
      }
    }
  }

  private precedenceOf(op: string): Precedence {
    switch (op) {
      case "||":
        return Precedence.Or;
      case "&&":
        return Precedence.And;
      case "==":
      case "!=":
        return Precedence.Equality;
      case ">":
      case ">=":
      case "<":
      case "<=":
        return Precedence.Comparison;
      case "+":
      case "-":
        return Precedence.Term;
      case "*":
      case "/":
      case "%":
        return Precedence.Factor;
      case "!":
        return Precedence.Unary;
      default:
        // Call, member and index bind tightest.
        return Precedence.Call;
    }
  }
}
