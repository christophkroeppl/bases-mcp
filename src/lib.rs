//! An MCP server that resolves Obsidian `.base` files to human-readable markdown.
//!
//! The layers, bottom up, and the direction the dependency runs:
//!
//!   - [`value`], [`lexer`], [`parser`], [`ast`], [`evaluator`], [`stdlib`] — the
//!     Bases expression language: values, a Pratt parser, and a tree-walking
//!     evaluator. Pure; no vault required.
//!   - [`note`] — a note as ordered byte spans, so a Base region can be preserved
//!     verbatim while everything around it is editable.
//!   - [`vault`] — the one I/O boundary, with a local filesystem and a WebDAV
//!     implementation behind a single trait, plus the index over both.
//!   - [`base`] — `.base` parsing and the query pipeline: formulas, filters,
//!     sort, grouping, limit.
//!   - [`labels`] — the display label Obsidian renders a Property ID as.
//!   - [`render`] — the flat CLI-parity markdown surface and the structured
//!     Projection, plus the reconciler that takes an edited Projection apart.
//!   - [`drafts`] — the two-call handshake behind adding a row to a Base.
//!   - [`service`] — the Resolver: the single entry point everything above is
//!     reached through.
//!   - [`tools`] — the six MCP tools, and [`config`] — startup configuration.
//!
//! Two decisions shape the top of that stack and are stated where they happen
//! rather than here: the Resolver's renders are two surfaces, not one (see
//! [`service`]), and a tool's `isError` is reserved for a failure rather than a
//! partial apply (see [`tools`]).

pub mod ast;
pub mod base;
pub mod config;
pub mod drafts;
pub mod error;
pub mod evaluator;
pub mod labels;
pub mod lexer;
pub mod note;
pub mod parser;
pub mod render;
pub mod service;
pub mod stdlib;
pub mod tools;
pub mod value;
pub mod vault;
