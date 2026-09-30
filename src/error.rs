//! Errors.
//!
//! One error type, and it carries structure rather than prose. A failure has to
//! survive all the way to an MCP tool result, where the agent sees structured
//! fields it can act on -- a hard error naming a construct is the whole point,
//! because a silently-null result is the failure mode this project exists to
//! prevent.
//!
//! The variant is boxed. Without it `Result<T, BasesError>` is over 130 bytes,
//! because every field is an owned `String`, and clippy is right that returning
//! a value that large from every fallible function is a real cost rather than a
//! style note: it makes the `Err` path memcpy on every `?`.

use std::fmt;

/// A failure, with whatever detail is known.
///
/// `construct` names the language or spec feature at fault (`this`, `contains`,
/// `webdav`). `note`/`view`/`property`/`position` locate it. A message with no
/// fields is still a complete sentence on its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BasesError {
    inner: Box<ErrorDetail>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct ErrorDetail {
    message: String,
    construct: Option<String>,
    note: Option<String>,
    view: Option<String>,
    property: Option<String>,
    /// A byte offset into the expression, when the failure is lexical.
    position: Option<usize>,
}

impl BasesError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            inner: Box::new(ErrorDetail {
                message: message.into(),
                ..Default::default()
            }),
        }
    }

    /// Tag the construct at fault.
    pub fn with_construct(mut self, construct: impl Into<String>) -> Self {
        self.inner.construct = Some(construct.into());
        self
    }

    pub fn with_note(mut self, note: impl Into<String>) -> Self {
        self.inner.note = Some(note.into());
        self
    }

    pub fn with_view(mut self, view: impl Into<String>) -> Self {
        self.inner.view = Some(view.into());
        self
    }

    pub fn with_property(mut self, property: impl Into<String>) -> Self {
        self.inner.property = Some(property.into());
        self
    }

    pub fn with_position(mut self, position: usize) -> Self {
        self.inner.position = Some(position);
        self
    }

    pub fn message(&self) -> &str {
        &self.inner.message
    }

    pub fn construct(&self) -> Option<&str> {
        self.inner.construct.as_deref()
    }

    pub fn note(&self) -> Option<&str> {
        self.inner.note.as_deref()
    }

    pub fn view(&self) -> Option<&str> {
        self.inner.view.as_deref()
    }

    pub fn property(&self) -> Option<&str> {
        self.inner.property.as_deref()
    }

    pub fn position(&self) -> Option<usize> {
        self.inner.position
    }

    /// The message with the construct appended, which is how the TypeScript
    /// original spelled every error.
    pub fn display_message(&self) -> String {
        match &self.inner.construct {
            Some(c) => format!("{} (construct: {})", self.inner.message, c),
            None => self.inner.message.clone(),
        }
    }
}

impl fmt::Display for BasesError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.display_message())
    }
}

impl std::error::Error for BasesError {}

pub type Result<T> = std::result::Result<T, BasesError>;

/// A base references `this` but no host note was supplied.
///
/// Its own type because it is the project's central divergence: Obsidian binds
/// `this` to whatever note the base is displayed in, and the CLI cannot express
/// that at all -- it returns `[]`. Returning an empty array would be a silent
/// wrong answer, so this is an error instead.
#[derive(Debug, Clone)]
pub struct MissingThisContext {
    pub base: String,
}

impl fmt::Display for MissingThisContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "This base references \"this\" (this.file) but no host note was supplied. \
The Obsidian CLI cannot bind \"this\" at all and returns an empty result; we require an \
explicit host note so the result is never silently wrong. (construct: this)"
        )
    }
}

impl std::error::Error for MissingThisContext {}

impl From<MissingThisContext> for BasesError {
    fn from(e: MissingThisContext) -> Self {
        BasesError::new(e.to_string()).with_construct("this")
    }
}