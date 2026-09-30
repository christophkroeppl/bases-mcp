//! Lexer for the Bases expression language.
//!
//! The only genuinely hard case is `/`: it is both the division operator and
//! the regex literal opener. We decide based on the previous significant token —
//! division is only legal after a value-ish token, so `/` elsewhere starts a
//! regex. That is the same rule JavaScript itself uses.
//!
//! Byte offsets are carried on every token, not character offsets: a note with
//! emoji in it must still produce offsets that index the real string.

use crate::error::{BasesError, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tok {
    Number,
    Str,
    Regex,
    Identifier,
    /// `and`, `or`, `not`. Keywords, but they parse as ordinary operands.
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
    Eof,
}

/// A compiled regex literal: its body and flags, kept as text.
///
/// Kept uncompiled because the Rust `regex` crate has no flags equivalent to
/// JavaScript's `g`/`y`/`u`, and the operations Bases exposes (`matches`,
/// `replace` with `$1`) are expressible on body+flags directly.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RegexLiteral {
    pub body: String,
    pub flags: String,
}

impl RegexLiteral {
    pub fn to_pattern(&self) -> String {
        let mut out = self.body.clone();
        if self.flags.contains('i') {
            out = format!("(?i){out}");
        }
        if self.flags.contains('m') {
            out = format!("(?m){out}");
        }
        if self.flags.contains('s') {
            out = format!("(?s){out}");
        }
        out
    }
}

#[derive(Debug, Clone)]
pub struct Token {
    pub kind: Tok,
    /// Source text, for error messages.
    pub raw: String,
    /// Present for Str, Regex and Identifier.
    pub text: Option<String>,
    /// Present for Number.
    pub number: Option<f64>,
    /// Present for Regex.
    pub regex: Option<RegexLiteral>,
    pub start: usize,
    pub end: usize,
}

impl Token {
    pub fn text(&self) -> &str {
        self.text.as_deref().unwrap_or(&self.raw)
    }

    /// Whether this token can end an expression, which is what makes a following
    /// `/` a division rather than a regex.
    pub fn ends_value(&self) -> bool {
        matches!(
            self.kind,
            Tok::Number
                | Tok::Str
                | Tok::Regex
                | Tok::Identifier
                | Tok::Keyword
                | Tok::True
                | Tok::False
                | Tok::Null
                | Tok::RParen
                | Tok::RBracket
        )
    }
}

pub fn lex(input: &str) -> Result<Vec<Token>> {
    let bytes = input.as_bytes();
    let mut tokens: Vec<Token> = Vec::new();
    let mut i = 0usize;
    let mut after_value = false;

    while i < bytes.len() {
        let c = bytes[i];

        if matches!(c, b' ' | b'\t' | b'\n' | b'\r') {
            i += 1;
            continue;
        }

        let start = i;

        if c == b'/' && !after_value {
            let (end, regex) = read_regex(input, i)?;
            tokens.push(Token {
                kind: Tok::Regex,
                raw: input[start..end].to_string(),
                text: None,
                number: None,
                regex: Some(regex),
                start,
                end,
            });
            i = end;
            after_value = true;
            continue;
        }

        if c.is_ascii_digit() || (c == b'.' && bytes.get(i + 1).is_some_and(u8::is_ascii_digit)) {
            let end = read_number(input, i);
            let text = &input[start..end];
            let number = text.parse::<f64>().map_err(|_| {
                BasesError::new(format!("Invalid number \"{text}\"")).with_position(start)
            })?;
            tokens.push(Token {
                kind: Tok::Number,
                raw: text.to_string(),
                text: None,
                number: Some(number),
                regex: None,
                start,
                end,
            });
            i = end;
            after_value = true;
            continue;
        }

        if c == b'"' || c == b'\'' {
            let end = read_string(input, i)?;
            let raw = &input[start..end];
            tokens.push(Token {
                kind: Tok::Str,
                raw: raw.to_string(),
                text: Some(decode_string(raw)),
                number: None,
                regex: None,
                start,
                end,
            });
            i = end;
            after_value = true;
            continue;
        }

        if is_ident_start(input, i) {
            let mut end = i;
            while end < input.len() && is_ident_part(input, end) {
                end += char_len(input, end);
            }
            let raw = &input[i..end];
            i = end;
            let kind = match raw {
                "true" => Tok::True,
                "false" => Tok::False,
                "null" => Tok::Null,
                "and" | "or" | "not" => Tok::Keyword,
                _ => Tok::Identifier,
            };
            tokens.push(Token {
                kind,
                raw: raw.to_string(),
                text: Some(raw.to_string()),
                number: None,
                regex: None,
                start,
                end,
            });
            after_value = true;
            continue;
        }

        if let Some((kind, len)) = match_symbol(input, i) {
            tokens.push(Token {
                kind,
                raw: input[i..i + len].to_string(),
                text: None,
                number: None,
                regex: None,
                start,
                end: i + len,
            });
            i += len;
            after_value = matches!(kind, Tok::RParen | Tok::RBracket);
            continue;
        }

        if let Some(kind) = match_single(c) {
            tokens.push(Token {
                kind,
                raw: (c as char).to_string(),
                text: None,
                number: None,
                regex: None,
                start,
                end: i + 1,
            });
            i += 1;
            after_value = matches!(kind, Tok::RParen | Tok::RBracket);
            continue;
        }

        return Err(BasesError::new(format!("Unexpected character \"{}\"", c as char))
            .with_position(start));
    }

    tokens.push(Token {
        kind: Tok::Eof,
        raw: String::new(),
        text: None,
        number: None,
        regex: None,
        start: i,
        end: i,
    });
    Ok(tokens)
}

fn match_symbol(input: &str, i: usize) -> Option<(Tok, usize)> {
    let rest = input.get(i..)?;
    let two = rest.get(..2)?;
    let kind = match two {
        "==" => Tok::Eq,
        "!=" => Tok::NotEq,
        ">=" => Tok::GtEq,
        "<=" => Tok::LtEq,
        "&&" => Tok::AndAnd,
        "||" => Tok::OrOr,
        _ => return None,
    };
    Some((kind, 2))
}

fn match_single(c: u8) -> Option<Tok> {
    Some(match c {
        b'+' => Tok::Plus,
        b'-' => Tok::Minus,
        b'*' => Tok::Star,
        b'/' => Tok::Slash,
        b'%' => Tok::Percent,
        b'!' => Tok::Bang,
        b'>' => Tok::Gt,
        b'<' => Tok::Lt,
        b'(' => Tok::LParen,
        b')' => Tok::RParen,
        b'[' => Tok::LBracket,
        b']' => Tok::RBracket,
        b',' => Tok::Comma,
        b'.' => Tok::Dot,
        _ => return None,
    })
}

fn read_number(input: &str, start: usize) -> usize {
    let bytes = input.as_bytes();
    let mut i = start;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    if bytes.get(i) == Some(&b'.') {
        i += 1;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
    }
    i
}

fn read_string(input: &str, start: usize) -> Result<usize> {
    let bytes = input.as_bytes();
    let quote = bytes[start];
    let mut i = start + 1;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            b'\n' => break,
            c if c == quote => return Ok(i + 1),
            _ => i += char_len(input, i),
        }
    }
    Err(BasesError::new("Unterminated string literal").with_position(start))
}

fn read_regex(input: &str, start: usize) -> Result<(usize, RegexLiteral)> {
    let bytes = input.as_bytes();
    let mut i = start + 1;
    let mut in_class = false;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            b'\n' => break,
            b'[' => {
                in_class = true;
                i += 1;
            }
            b']' => {
                in_class = false;
                i += 1;
            }
            b'/' if !in_class => break,
            _ => i += char_len(input, i),
        }
    }
    if bytes.get(i) != Some(&b'/') {
        return Err(
            BasesError::new("Unterminated regular expression").with_position(start)
        );
    }
    let body_end = i;
    i += 1;
    let flag_start = i;
    while i < input.len() && is_ident_part(input, i) {
        i += char_len(input, i);
    }
    let body = input[start + 1..body_end].to_string();
    let flags = input[flag_start..i].to_string();
    if !flags.is_empty() && !flags.chars().all(|c| "gimsuy".contains(c)) {
        return Err(BasesError::new(format!("Invalid regular expression flags \"{flags}\""))
            .with_position(start));
    }
    // Compile once here so an invalid pattern fails at lex time, as it does in
    // the TypeScript original, rather than at the point of use.
    let literal = RegexLiteral { body, flags };
    regex::Regex::new(&literal.to_pattern()).map_err(|e| {
        BasesError::new(format!("Invalid regular expression: {e}")).with_position(start)
    })?;
    Ok((i, literal))
}

fn decode_string(raw: &str) -> String {
    let inner = &raw[1..raw.len() - 1];
    if !inner.contains('\\') {
        return inner.to_string();
    }
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('\\') => out.push('\\'),
            Some('"') => out.push('"'),
            Some('\'') => out.push('\''),
            // A trailing backslash is kept literally, as in the original.
            None => out.push('\\'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
        }
    }
    out
}

/// The byte length of the character starting at `i`. Non-ASCII identifiers are
/// permitted by the documented grammar even though Obsidian's own parser
/// rejects them (obsidian-help issue #1095).
fn char_len(input: &str, i: usize) -> usize {
    input[i..].chars().next().map(char::len_utf8).unwrap_or(1)
}

fn is_ident_start(input: &str, i: usize) -> bool {
    match input[i..].chars().next() {
        Some(c) => c.is_ascii_alphabetic() || c == '_' || c == '$' || (c as u32) > 127,
        None => false,
    }
}

fn is_ident_part(input: &str, i: usize) -> bool {
    match input[i..].chars().next() {
        Some(c) => c.is_ascii_alphanumeric() || c == '_' || c == '$' || (c as u32) > 127,
        None => false,
    }
}
