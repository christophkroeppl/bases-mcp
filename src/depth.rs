//! How deep a recursive walk over untrusted shape is allowed to go.
//!
//! Five walks in this crate recurse over data that arrived in a `.base` file:
//! the Pratt parser ([`crate::parser`]), the evaluator ([`crate::evaluator`]),
//! and the three walks over a filter tree ([`crate::base::parse`]'s
//! `normalise_filters`, [`crate::base::query`]'s `run_filter`, and
//! [`crate::drafts`]'s `probe`). A file that arrives through a bad merge, a
//! plugin, or a hand edit can nest a paren as deep as it likes, and a Rust stack
//! overflow is NOT catchable -- the process aborts, the MCP client sees the
//! transport drop with no tool result, the only diagnostic is one line on
//! stderr, and every other tool is dead until someone restarts the server.
//!
//! So each walk spends from one shared budget, and running out is a
//! [`BasesError`] naming the construct and the number. That is the whole point:
//! turn a process death into a refusal an agent can read and act on.
//!
//! ## Why 96
//!
//! Measured, at `opt-level = 0` in a 2 MiB thread -- the tightest configuration
//! that runs this crate's own gate, because `cargo test` hands each test a
//! spawned thread -- these are the deepest nestings that survive:
//!
//! | walk | survives | because |
//! |---|---|---|
//! | [`crate::parser`] | **471** | two frames per level, and a `Result` in each |
//! | [`crate::evaluator`] | 1448 | one frame per level |
//! | `Node`'s drop glue | 18634 | `Box<Node>` chains, almost no frame |
//! | `Node::mentions_formula` | 10034 | one `&Node` in, no `Result` |
//! | `normalise_filters` | 63 | serde_yaml refuses a deeper YAML tree first |
//!
//! The parser is the binding constraint by 3x, so it sets the number. 96 is
//! **4.9x below** the parser's measured edge: the guard fires while most of the
//! stack is still unspent, which is the margin that matters, because a frame
//! that grows by half would otherwise move the edge to 314 and leave a limit of
//! 96 uncomfortably close to it.
//!
//! It is also **2x above anything a person writes**. A real filter is three to
//! five deep. The deepest legitimate expression this project has been asked to
//! accept is fifty, and it passes.
//!
//! 96 is deliberately NOT 64, even though that would be tidier. serde_yaml
//! already caps a filter tree at 63 levels, and a filter may nest `and:` 63 deep
//! *and* carry an expression 96 deep; choosing 64 would couple two unrelated
//! limits and refuse a Base for no reason.
//!
//! ## What is deliberately not bounded
//!
//! [`crate::lexer`] recurses nowhere: `lex` is a flat loop over bytes, and a
//! three-megabyte flat expression lexes fine. A deeply nested regex literal is
//! refused by the `regex` crate's own size limit at compile time, as an error.
//!
//! `find_this_reference` and `Node::mentions_formula` are not guarded either.
//! They only ever walk a tree `parse` produced, the parser's limit bounds that
//! tree, and they survive 10034 levels where the parser survives 471 -- so a
//! guard there would change two more public signatures and change nothing about
//! what the process survives. [`crate::evaluator`] is guarded anyway for a
//! different reason, and states it where it happens.

use std::cell::Cell;

use crate::error::{BasesError, Result};

/// Nesting levels one walk may spend, in total.
///
/// The outermost expression is level 1, so a tree of [`MAX_DEPTH`] levels is
/// `MAX_DEPTH - 1` enclosing groups around a leaf. Stated here because "96" and
/// "95 parens" differ by one and a reader should not have to run it to find out
/// which: the guard refuses at `MAX_DEPTH`, so exactly [`MAX_DEPTH`] levels
/// pass.
pub const MAX_DEPTH: usize = 96;

/// What a walk names itself in the refusal. The construct is the thing the
/// author wrote, not the function that walked it.
pub const EXPRESSION: &str = "Expression";

/// See [`EXPRESSION`].
pub const FILTER: &str = "Filter group";

/// A budget for one walk, threaded through its recursive functions.
///
/// Not a field of the thing being walked: a guard that borrows the counter would
/// overlap the `&mut self` borrow every recursive function already needs, and a
/// budget the reader cannot see in the signature is a budget a later edit will
/// forget to thread.
#[derive(Debug, Default)]
pub struct Depth {
    /// A `Cell` rather than a plain `usize` because a guard holding `&mut Depth`
    /// for its whole scope would overlap the `&mut Depth` every recursive call
    /// needs, and the borrow checker is right to refuse that. Interior
    /// mutability is the smallest thing that lets the guard and the recursion
    /// share one counter, and `Depth` stays a local the caller owns.
    level: Cell<usize>,
}

/// One spent level, released on drop.
///
/// A guard rather than an increment the caller has to undo, because every one of
/// these functions returns through `?`, and a decrement written after the call
/// would be skipped by every early return in between.
pub struct Level<'a> {
    depth: &'a Depth,
}

impl Depth {
    pub fn new() -> Self {
        Self::default()
    }

    /// Spend one level, or refuse the walk.
    pub fn enter(&self, construct: &str) -> Result<Level<'_>> {
        let level = self.level.get();
        if level >= MAX_DEPTH {
            return Err(too_deep(construct));
        }
        self.level.set(level + 1);
        Ok(Level { depth: self })
    }
}

impl Drop for Level<'_> {
    fn drop(&mut self) {
        self.depth.level.set(self.depth.level.get() - 1);
    }
}

/// The one refusal, naming the construct and the limit.
///
/// Both, because neither is actionable alone: the construct says which Base key
/// to go and edit, and the number says how far the author has to pull it back.
fn too_deep(construct: &str) -> BasesError {
    BasesError::new(format!(
        "{construct} nests deeper than {MAX_DEPTH} levels, which is the limit. Nothing written \
         by hand comes close, so this is a mistake or a generated file: flatten the nesting, or \
         split it across several expressions."
    ))
    .with_construct(construct)
}
