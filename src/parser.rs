//! Pratt parser for the Bases expression language.
//!
//! Precedence follows JavaScript, which is what the Bases docs instruct:
//! `* / %` > `+ -` > relational > equality > `&&` > `||` > unary.
//!
//! Two deliberate divergences from Obsidian's own parser, both because its help
//! site documents the behaviour and its runtime rejects it (obsidian-help
//! #1095):
//!   - member access on a bare numeric literal: `(1).isTruthy()` works, and we
//!     also accept the documented `1.isTruthy()` spelling;
//!   - `and` / `or` / `not` are accepted as aliases for `&&` / `||` / `!`, since
//!     real vaults use both spellings.

use crate::ast::{BinOp, Literal, Node, NodeKind, Precedence, Span, UnaryOp};
use crate::error::{BasesError, Result};
use crate::lexer::{lex, Tok, Token};

pub fn parse(input: &str) -> Result<Node> {
    let mut parser = Parser {
        tokens: lex(input)?,
        pos: 0,
        source_len: input.len(),
    };
    let node = parser.parse_expression(Precedence::Lowest)?;
    parser.finish()?;
    Ok(node)
}

/// Parse without throwing. Used by the filter-inversion pass, which probes
/// whether an expression is shaped like something it can invert.
pub fn try_parse(input: &str) -> Option<Node> {
    parse(input).ok()
}

struct Parser {
    tokens: Vec<Token>,
    pos: usize,
    source_len: usize,
}

impl Parser {
    fn peek(&self) -> &Token {
        // The token list always ends in Eof, so this cannot run off the end.
        self.tokens.get(self.pos).unwrap_or_else(|| self.tokens.last().expect("non-empty"))
    }

    fn next(&mut self) -> Result<Token> {
        let tok = self
            .tokens
            .get(self.pos)
            .cloned()
            .ok_or_else(|| BasesError::new("Unexpected end of expression").with_position(self.source_len))?;
        self.pos += 1;
        Ok(tok)
    }

    fn at(&self, kind: Tok) -> bool {
        self.peek().kind == kind
    }

    fn advance(&mut self) {
        self.pos += 1;
    }

    fn finish(&self) -> Result<()> {
        if !self.at(Tok::Eof) {
            let tok = self.peek();
            return Err(BasesError::new(format!("Unexpected token \"{}\" after expression", tok.raw))
                .with_position(tok.start));
        }
        Ok(())
    }

    fn parse_expression(&mut self, precedence: Precedence) -> Result<Node> {
        let mut left = self.parse_prefix()?;
        while let Some((op, start)) = self.peek_infix() {
            let prec = precedence_of(op);
            if prec < precedence {
                break;
            }
            left = self.parse_infix_rest(op, start, left, prec)?;
        }
        Ok(left)
    }

    fn parse_prefix(&mut self) -> Result<Node> {
        let tok = self.peek().clone();
        let span = Span { start: tok.start, end: tok.end };
        match tok.kind {
            Tok::Number => {
                self.advance();
                Ok(Node::new(
                    NodeKind::Literal(Literal::Number(tok.number.unwrap_or_default())),
                    span,
                ))
            }
            Tok::Str => {
                self.advance();
                Ok(Node::new(
                    NodeKind::Literal(Literal::String(tok.text.clone().unwrap_or_default())),
                    span,
                ))
            }
            Tok::Regex => {
                self.advance();
                Ok(Node::new(
                    NodeKind::Literal(Literal::Regex(tok.regex.clone().unwrap_or_default())),
                    span,
                ))
            }
            Tok::True => {
                self.advance();
                Ok(Node::new(NodeKind::Literal(Literal::Bool(true)), span))
            }
            Tok::False => {
                self.advance();
                Ok(Node::new(NodeKind::Literal(Literal::Bool(false)), span))
            }
            Tok::Null => {
                self.advance();
                Ok(Node::new(NodeKind::Literal(Literal::Null), span))
            }
            Tok::Identifier => {
                self.advance();
                Ok(Node::new(
                    NodeKind::Identifier(tok.text.clone().unwrap_or_default()),
                    span,
                ))
            }
            Tok::LParen => {
                self.advance();
                let inner = self.parse_expression(Precedence::Lowest)?;
                let close = self.next()?;
                if close.kind != Tok::RParen {
                    return Err(
                        BasesError::new("Expected closing parenthesis").with_position(close.start)
                    );
                }
                // The group's own span, so an error inside it still points at
                // the inner token rather than the open paren.
                Ok(Node::new(
                    inner.kind,
                    Span { start: span.start, end: close.end },
                ))
            }
            Tok::LBracket => {
                self.advance();
                self.parse_list(span.start)
            }
            Tok::Bang => {
                self.advance();
                let operand = self.parse_expression(Precedence::Unary)?;
                let end = operand.span.end;
                Ok(Node::new(
                    NodeKind::Unary { op: UnaryOp::Not, operand: Box::new(operand) },
                    Span { start: span.start, end },
                ))
            }
            Tok::Minus => {
                self.advance();
                let operand = self.parse_expression(Precedence::Unary)?;
                let end = operand.span.end;
                Ok(Node::new(
                    NodeKind::Unary { op: UnaryOp::Negate, operand: Box::new(operand) },
                    Span { start: span.start, end },
                ))
            }
            _ => Err(BasesError::new(format!(
                "Unexpected token \"{}\"",
                if tok.raw.is_empty() { "<end of input>" } else { &tok.raw }
            ))
            .with_position(tok.start)),
        }
    }

    fn parse_list(&mut self, start: usize) -> Result<Node> {
        let mut elements = Vec::new();
        if self.at(Tok::RBracket) {
            let end = self.peek().end;
            self.advance();
            return Ok(Node::new(NodeKind::List(elements), Span { start, end }));
        }
        loop {
            elements.push(self.parse_expression(Precedence::Lowest)?);
            if self.at(Tok::Comma) {
                self.advance();
                if self.at(Tok::RBracket) {
                    break;
                }
                continue;
            }
            break;
        }
        let close = self.next()?;
        if close.kind != Tok::RBracket {
            return Err(BasesError::new("Expected closing bracket").with_position(close.start));
        }
        Ok(Node::new(NodeKind::List(elements), Span { start, end: close.end }))
    }

    /// The operator that would continue the expression here, if any.
    fn peek_infix(&self) -> Option<(Infix, usize)> {
        let tok = self.peek();
        let op = match tok.kind {
            Tok::Plus => Infix::Binary(BinOp::Add),
            Tok::Minus => Infix::Binary(BinOp::Sub),
            Tok::Star => Infix::Binary(BinOp::Mul),
            Tok::Slash => Infix::Binary(BinOp::Div),
            Tok::Percent => Infix::Binary(BinOp::Rem),
            Tok::Eq => Infix::Binary(BinOp::Eq),
            Tok::NotEq => Infix::Binary(BinOp::NotEq),
            Tok::Gt => Infix::Binary(BinOp::Gt),
            Tok::GtEq => Infix::Binary(BinOp::GtEq),
            Tok::Lt => Infix::Binary(BinOp::Lt),
            Tok::LtEq => Infix::Binary(BinOp::LtEq),
            Tok::AndAnd => Infix::Binary(BinOp::And),
            Tok::OrOr => Infix::Binary(BinOp::Or),
            Tok::Keyword => match tok.raw.as_str() {
                "and" => Infix::Binary(BinOp::And),
                "or" => Infix::Binary(BinOp::Or),
                "not" => Infix::Not,
                _ => return None,
            },
            Tok::LParen => Infix::Call,
            Tok::Dot => Infix::Member,
            Tok::LBracket => Infix::Index,
            _ => return None,
        };
        Some((op, tok.start))
    }

    fn parse_infix_rest(
        &mut self,
        op: Infix,
        start: usize,
        left: Node,
        prec: Precedence,
    ) -> Result<Node> {
        // The operator token itself is consumed. Its value is discarded, but the
        // advance is load-bearing: the caller's loop would otherwise never make
        // progress.
        self.advance();
        let left_span = left.span;

        match op {
            Infix::Call => {
                let mut args = Vec::new();
                if !self.at(Tok::RParen) {
                    loop {
                        args.push(self.parse_expression(Precedence::Lowest)?);
                        if self.at(Tok::Comma) {
                            self.advance();
                            if self.at(Tok::RParen) {
                                break;
                            }
                            continue;
                        }
                        break;
                    }
                }
                let close = self.next()?;
                if close.kind != Tok::RParen {
                    return Err(
                        BasesError::new("Expected closing parenthesis").with_position(close.start)
                    );
                }
                Ok(Node::new(
                    NodeKind::Call { callee: Box::new(left), args },
                    Span { start: left_span.start, end: close.end },
                ))
            }
            Infix::Member => {
                // Guard against `1 .toString()` reading as a float continuation.
                if self.at(Tok::Number) {
                    return Err(BasesError::new("Expected a property name after '.'")
                        .with_position(self.peek().start));
                }
                let name = self.next()?;
                if name.kind != Tok::Identifier && name.kind != Tok::Keyword {
                    return Err(
                        BasesError::new("Expected a property name after '.'").with_position(name.start)
                    );
                }
                Ok(Node::new(
                    NodeKind::Member {
                        object: Box::new(left),
                        property: name.raw.clone(),
                    },
                    Span { start: left_span.start, end: name.end },
                ))
            }
            Infix::Index => {
                let index = self.parse_expression(Precedence::Lowest)?;
                let close = self.next()?;
                if close.kind != Tok::RBracket {
                    return Err(BasesError::new("Expected closing bracket").with_position(close.start));
                }
                Ok(Node::new(
                    NodeKind::Index { object: Box::new(left), index: Box::new(index) },
                    Span { start: left_span.start, end: close.end },
                ))
            }
            Infix::Not => {
                let operand = self.parse_expression(Precedence::Unary)?;
                let end = operand.span.end;
                Ok(Node::new(
                    NodeKind::Unary { op: UnaryOp::Not, operand: Box::new(operand) },
                    Span { start, end },
                ))
            }
            Infix::Binary(bin) => {
                let right = self.parse_expression(prec)?;
                let end = right.span.end;
                Ok(Node::new(
                    NodeKind::Binary {
                        op: bin,
                        left: Box::new(left),
                        right: Box::new(right),
                    },
                    Span { start: left_span.start, end },
                ))
            }
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum Infix {
    Binary(BinOp),
    Call,
    Member,
    Index,
    Not,
}

fn precedence_of(op: Infix) -> Precedence {
    match op {
        Infix::Binary(BinOp::Or) => Precedence::Or,
        Infix::Binary(BinOp::And) => Precedence::And,
        Infix::Binary(BinOp::Eq) | Infix::Binary(BinOp::NotEq) => Precedence::Equality,
        Infix::Binary(BinOp::Gt)
        | Infix::Binary(BinOp::GtEq)
        | Infix::Binary(BinOp::Lt)
        | Infix::Binary(BinOp::LtEq) => Precedence::Comparison,
        Infix::Binary(BinOp::Add) | Infix::Binary(BinOp::Sub) => Precedence::Term,
        Infix::Binary(BinOp::Mul) | Infix::Binary(BinOp::Div) | Infix::Binary(BinOp::Rem) => {
            Precedence::Factor
        }
        Infix::Not => Precedence::Unary,
        // Call, member and index bind tightest.
        Infix::Call | Infix::Member | Infix::Index => Precedence::Call,
    }
}
