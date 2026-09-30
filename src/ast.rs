//! The expression AST.
//!
//! Spans are byte offsets, and every node carries them: a hard error has to
//! point at the token that caused it, and an error without a position is a
//! sentence the agent cannot act on.

use crate::lexer::RegexLiteral;
use crate::value::BasesValue;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Precedence {
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}

/// A literal, with the value already resolved.
///
/// Literals are values rather than tokens because the evaluator must not have
/// to re-interpret a token to produce one, and because a `Literal` for a string
/// is the only place a raw source span stops being relevant.
#[derive(Debug, Clone, PartialEq)]
pub enum Literal {
    Null,
    Bool(bool),
    Number(f64),
    String(String),
    Regex(RegexLiteral),
}

impl From<Literal> for BasesValue {
    fn from(l: Literal) -> Self {
        match l {
            Literal::Null => BasesValue::Null,
            Literal::Bool(b) => BasesValue::Bool(b),
            Literal::Number(n) => BasesValue::Number(n),
            Literal::String(s) => BasesValue::String(s),
            Literal::Regex(r) => BasesValue::String(r.body),
        }
    }
}

#[derive(Debug, Clone)]
pub enum NodeKind {
    Literal(Literal),
    Identifier(String),
    /// `!` or `-`.
    Unary { op: UnaryOp, operand: Box<Node> },
    Binary { op: BinOp, left: Box<Node>, right: Box<Node> },
    Call { callee: Box<Node>, args: Vec<Node> },
    Member { object: Box<Node>, property: String },
    Index { object: Box<Node>, index: Box<Node> },
    List(Vec<Node>),
}

#[derive(Debug, Clone)]
pub struct Node {
    pub kind: NodeKind,
    pub span: Span,
}

impl Node {
    pub fn new(kind: NodeKind, span: Span) -> Self {
        Self { kind, span }
    }

    /// Whether this subtree mentions the formula `name`.
    ///
    /// Used twice: to order formula evaluation, and to decide whether a filter
    /// binds `this` (which is the divergence the whole project exists for).
    pub fn mentions_formula(&self, name: &str) -> bool {
        match &self.kind {
            NodeKind::Identifier(n) => n == name,
            NodeKind::Member { object, .. } => object.mentions_formula(name),
            NodeKind::Index { object, index } => {
                object.mentions_formula(name) || index.mentions_formula(name)
            }
            NodeKind::Unary { operand, .. } => operand.mentions_formula(name),
            NodeKind::Binary { left, right, .. } => {
                left.mentions_formula(name) || right.mentions_formula(name)
            }
            // A call's callee is a function name, not a formula reference.
            NodeKind::Call { args, .. } => args.iter().any(|a| a.mentions_formula(name)),
            NodeKind::Literal(_) | NodeKind::List(_) => false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnaryOp {
    Not,
    Negate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    Eq,
    NotEq,
    Gt,
    GtEq,
    Lt,
    LtEq,
    And,
    Or,
}

impl BinOp {
    /// The spelling used in error messages and by the filter-inversion pass.
    pub fn as_str(&self) -> &'static str {
        match self {
            BinOp::Add => "+",
            BinOp::Sub => "-",
            BinOp::Mul => "*",
            BinOp::Div => "/",
            BinOp::Rem => "%",
            BinOp::Eq => "==",
            BinOp::NotEq => "!=",
            BinOp::Gt => ">",
            BinOp::GtEq => ">=",
            BinOp::Lt => "<",
            BinOp::LtEq => "<=",
            BinOp::And => "&&",
            BinOp::Or => "||",
        }
    }
}
