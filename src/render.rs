//! Markdown rendering and the note Projection.
//!
//! Two surfaces meet here, and they are deliberately not the same thing.
//!
//! [`markdown`] renders a resolved query either byte-exactly as
//! `obsidian base:query format=md` does (`RenderStyle::Flat`) or as the
//! agent-facing Projection (`RenderStyle::Structured`). The first is a parity
//! obligation and the second carries none, which is why [`markdown`] can keep
//! two different table layouts without either of them being a bug.
//!
//! [`project`] builds that Projection and reconciles it back onto the stored
//! note. Neither half is ever allowed to change a Base region, and both work off
//! [`crate::note`]'s segmentation rather than re-parsing anything.

pub mod markdown;
pub mod project;
