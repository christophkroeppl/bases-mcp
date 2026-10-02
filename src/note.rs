//! Note segmentation.
//!
//! A note is parsed into ordered segments that concatenate back to the original
//! bytes exactly. That invariant is what makes the write path safe: a Base
//! region is a byte span that can be preserved verbatim while everything around
//! it is editable.
//!
//! Each segment carries its own source text, so serialising a parsed note is
//! [`serialise`] over its segments -- never a reformat. Every span is a BYTE
//! offset into the note, and [`Segment::slice`] is the only public constructor:
//! a segment's text is by construction the note's bytes in that span, so the
//! invariant cannot be violated by a caller.
//!
//! Three shapes of fence matter here, and conflating any two of them is how a
//! note gets corrupted:
//!
//!   * a live ` ```base ` fence carries base YAML that Obsidian will parse;
//!   * a ` ```base-rendered ` fence is a Base region handed back by a
//!     Projection, carrying provenance on its info string and no YAML at all;
//!   * any other fence is prose.
//!
//! [`BaseFence`] is an enum rather than a struct with optional fields for
//! exactly that reason: live and rendered are the same type only if a value
//! cannot claim to be both.

use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::OnceLock;

use regex::Regex;
use serde_yaml::Value as Yaml;

use crate::ast::Span;
use crate::value::BasesValue;

/// Note properties: the frontmatter of a note, as the evaluator reads them.
pub type Properties = BTreeMap<String, BasesValue>;

// ---------------------------------------------------------------------------
// Segments
// ---------------------------------------------------------------------------

/// Which of the four segment shapes this is.
///
/// The spellings are the TypeScript's, because they are what a note's own text
/// calls them: `baseEmbed` is `![[X.base]]` and `baseFence` is ` ```base `.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SegmentKind {
    Frontmatter,
    Prose,
    BaseEmbed,
    BaseFence,
}

impl SegmentKind {
    pub fn as_str(self) -> &'static str {
        match self {
            SegmentKind::Frontmatter => "frontmatter",
            SegmentKind::Prose => "prose",
            SegmentKind::BaseEmbed => "baseEmbed",
            SegmentKind::BaseFence => "baseFence",
        }
    }
}

/// What a segment is, beyond its bytes and its span.
#[derive(Debug, Clone, PartialEq)]
pub enum SegmentBody {
    /// The frontmatter block at the top of the note.
    Frontmatter(Frontmatter),
    /// Anything editable: the note's own text.
    Prose,
    /// An inline `![[Foo.base]]` embed, on a line of its own.
    BaseEmbed(BaseEmbed),
    /// An inline ` ```base ` fenced block.
    BaseFence(BaseFence),
}

impl SegmentBody {
    pub fn kind(&self) -> SegmentKind {
        match self {
            SegmentBody::Frontmatter(_) => SegmentKind::Frontmatter,
            SegmentBody::Prose => SegmentKind::Prose,
            SegmentBody::BaseEmbed(_) => SegmentKind::BaseEmbed,
            SegmentBody::BaseFence(_) => SegmentKind::BaseFence,
        }
    }
}

/// The frontmatter block at the top of a note.
#[derive(Debug, Clone, PartialEq)]
pub struct Frontmatter {
    /// The parsed properties, shared with [`ParsedNote::frontmatter`] so a
    /// lookup is not a second copy of the block.
    pub data: Rc<Properties>,
    /// True when the block was present but failed to parse.
    pub malformed: bool,
}

/// An inline `![[Foo.base]]` embed: a Base region, and the only `.base` form
/// that can appear in prose.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaseEmbed {
    /// The `.base` file this embed points at, as written in the link.
    pub base_path: String,
    /// The `#View` selector, when the embed pins one.
    pub view_name: Option<String>,
}

/// An inline ` ```base ` fenced block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BaseFence {
    /// A live fence. The YAML lives in the note rather than in a `.base` file,
    /// and Obsidian binds `this` to the containing note.
    Live {
        /// The base YAML carried in the fence.
        yaml: String,
    },
    /// A ` ```base-rendered ` fence: a Base region handed back by a Projection.
    ///
    /// It has no live YAML, so it is replaced rather than stored -- Obsidian
    /// would keep it as an inert block, leaving a dead copy of the rendered
    /// rows in the note.
    Rendered {
        /// The Base it was rendered from, from the fence's `path=` attribute.
        base_path: Option<String>,
        /// The `#View` it pinned, from the fence's `view=` attribute.
        view_name: Option<String>,
    },
}

impl BaseFence {
    /// The base YAML of a live fence. `None` for a rendered one, which has none.
    pub fn yaml(&self) -> Option<&str> {
        match self {
            BaseFence::Live { yaml } => Some(yaml),
            BaseFence::Rendered { .. } => None,
        }
    }

    /// The Base a rendered fence came from, from its `path=` attribute. `None`
    /// for a live fence, which carries its YAML instead of a path.
    pub fn base_path(&self) -> Option<&str> {
        match self {
            BaseFence::Live { .. } => None,
            BaseFence::Rendered { base_path, .. } => base_path.as_deref(),
        }
    }

    /// The `#View` a rendered fence pinned, from its `view=` attribute.
    pub fn view_name(&self) -> Option<&str> {
        match self {
            BaseFence::Live { .. } => None,
            BaseFence::Rendered { view_name, .. } => view_name.as_deref(),
        }
    }

    /// True for a fence a Projection produced rather than one Obsidian parses.
    pub fn is_rendered(&self) -> bool {
        matches!(self, BaseFence::Rendered { .. })
    }
}

/// One span of a note, carrying the bytes it came from.
#[derive(Debug, Clone, PartialEq)]
pub struct Segment {
    span: Span,
    raw: String,
    body: SegmentBody,
}

impl Segment {
    /// A segment covering `[span]` of `note`, holding exactly those bytes.
    ///
    /// Taking the note and the span together is deliberate: the round-trip
    /// invariant is only worth anything if no caller can invent a segment whose
    /// text disagrees with its span.
    pub fn slice(note: &str, span: Span, body: SegmentBody) -> Self {
        Self::new(note[span.start..span.end].to_string(), span, body)
    }

    fn new(raw: String, span: Span, body: SegmentBody) -> Self {
        Self { span, raw, body }
    }

    /// Exact source text, including any delimiters.
    pub fn raw(&self) -> &str {
        &self.raw
    }

    /// The byte span this segment occupies in its note.
    pub fn span(&self) -> Span {
        self.span
    }

    pub fn start(&self) -> usize {
        self.span.start
    }

    pub fn end(&self) -> usize {
        self.span.end
    }

    pub fn kind(&self) -> SegmentKind {
        self.body.kind()
    }

    pub fn body(&self) -> &SegmentBody {
        &self.body
    }

    pub fn frontmatter(&self) -> Option<&Frontmatter> {
        match &self.body {
            SegmentBody::Frontmatter(fm) => Some(fm),
            _ => None,
        }
    }

    pub fn base_embed(&self) -> Option<&BaseEmbed> {
        match &self.body {
            SegmentBody::BaseEmbed(embed) => Some(embed),
            _ => None,
        }
    }

    pub fn base_fence(&self) -> Option<&BaseFence> {
        match &self.body {
            SegmentBody::BaseFence(fence) => Some(fence),
            _ => None,
        }
    }

    /// The `.base` file this region points at, whether from a link or from a
    /// rendered fence's `path=` attribute. `None` for a live fence, which
    /// carries its YAML instead of a path.
    pub fn base_path(&self) -> Option<&str> {
        match &self.body {
            SegmentBody::BaseEmbed(embed) => Some(&embed.base_path),
            SegmentBody::BaseFence(fence) => fence.base_path(),
            _ => None,
        }
    }

    /// The `#View` this region pins, if any.
    pub fn view_name(&self) -> Option<&str> {
        match &self.body {
            SegmentBody::BaseEmbed(embed) => embed.view_name.as_deref(),
            SegmentBody::BaseFence(fence) => fence.view_name(),
            _ => None,
        }
    }

    /// The live base YAML of a ` ```base ` fence, and nothing else.
    pub fn yaml(&self) -> Option<&str> {
        self.base_fence().and_then(BaseFence::yaml)
    }

    /// True for a fence a Projection produced.
    pub fn is_rendered(&self) -> bool {
        self.base_fence().is_some_and(BaseFence::is_rendered)
    }
}

/// `![[Target]]` with an optional `#heading`, `#^block` and `|display`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WikiLink {
    pub target: String,
    /// The `#...` subpath, if any. Not used for link equality.
    pub subpath: Option<String>,
    pub display: Option<String>,
    pub embedded: bool,
    /// Byte offsets into the note text.
    pub start: usize,
    pub end: usize,
}

impl WikiLink {
    pub fn span(&self) -> Span {
        Span {
            start: self.start,
            end: self.end,
        }
    }
}

/// A checkbox in the body, with the text after it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskItem {
    pub checked: bool,
    /// The text after the checkbox, with the checkbox stripped.
    pub text: String,
    /// The 0-based line within the body, i.e. after the frontmatter block.
    pub line: usize,
}

/// A note, segmented, plus the facts Bases derives from it.
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedNote {
    pub path: String,
    pub segments: Vec<Segment>,
    /// The note's frontmatter properties, shared with its frontmatter segment.
    pub frontmatter: Rc<Properties>,
    /// Every link in the body, in document order.
    pub links: Vec<WikiLink>,
    /// The embeds among those links, in document order.
    pub embeds: Vec<WikiLink>,
    /// Checkboxes found in the body, in document order.
    pub tasks: Vec<TaskItem>,
    /// Inline `#tags` found in the body, without the `#`.
    pub inline_tags: Vec<String>,
    pub malformed_frontmatter: bool,
}

/// Segment kinds that occupy a Base region: an embed or a fence.
pub fn is_base_region(segment: &Segment) -> bool {
    matches!(
        segment.kind(),
        SegmentKind::BaseEmbed | SegmentKind::BaseFence
    )
}

/// Segment kinds that are a `.base` embed rather than an inline fence.
pub fn is_base_embed(segment: &Segment) -> bool {
    segment.kind() == SegmentKind::BaseEmbed
}

/// Serialise segments back to text. Byte-exact when unmodified.
pub fn serialise(segments: &[Segment]) -> String {
    segments.iter().map(Segment::raw).collect()
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

/// Parse a note into ordered segments plus the derived facts Bases needs.
pub fn parse_note(path: impl Into<String>, text: &str) -> ParsedNote {
    let mut segments: Vec<Segment> = Vec::new();
    let mut pos = 0;

    // Frontmatter must be the very first thing in the file.
    let mut properties: Rc<Properties> = Rc::new(Properties::new());
    let mut malformed = false;
    if let Some(block) = frontmatter_block(text) {
        let (parsed, is_malformed) = parse_frontmatter(block.yaml);
        properties = Rc::new(parsed);
        malformed = is_malformed;
        let span = Span {
            start: 0,
            end: block.raw.len(),
        };
        let body = SegmentBody::Frontmatter(Frontmatter {
            data: Rc::clone(&properties),
            malformed,
        });
        segments.push(Segment::slice(text, span, body));
        pos = span.end;
    }

    while pos < text.len() {
        if let Some(fence) = match_fence(text, pos) {
            let body = match fence_language(&fence.info).as_str() {
                // A live fence: the base lives in the note.
                "base" => SegmentBody::BaseFence(BaseFence::Live { yaml: fence.body }),
                // A rendered fence is a Base region the agent got back from a
                // Projection. It carries no live YAML, so it must never reach
                // disk. Tagging it here lets reconciliation swap it back for the
                // region it replaced, matched on the Base path it records.
                RENDER_FENCE_LANG => {
                    let attrs = fence_attrs(&fence.info);
                    SegmentBody::BaseFence(BaseFence::Rendered {
                        base_path: attrs.get("path").cloned(),
                        view_name: attrs.get("view").cloned(),
                    })
                }
                _ => SegmentBody::Prose,
            };
            let span = Span {
                start: pos,
                end: fence.end,
            };
            segments.push(Segment::slice(text, span, body));
            pos = fence.end;
            continue;
        }

        // Prose up to the next fence that starts a line.
        let next = find_next_fence(text, pos);
        segments.push(Segment::slice(
            text,
            Span {
                start: pos,
                end: next,
            },
            SegmentBody::Prose,
        ));
        pos = next;
    }

    let links = extract_links(text);
    ParsedNote {
        path: path.into(),
        malformed_frontmatter: malformed,
        frontmatter: properties,
        // The embeds are the links that are embeds; nothing else produces one.
        embeds: links.iter().filter(|link| link.embedded).cloned().collect(),
        tasks: extract_tasks(text),
        inline_tags: extract_inline_tags(text),
        segments,
        links,
    }
}

/// Parse a note and split base embeds into their own segments.
pub fn parse_note_with_embeds(path: impl Into<String>, text: &str) -> ParsedNote {
    let mut note = parse_note(path, text);
    note.segments = merge_adjacent_prose(split_base_embeds(&note.segments));
    note
}

/// Split the prose into base-embed segments and the prose between them.
///
/// A `.base` embed occupies its own region; the surrounding prose is untouched.
pub fn split_base_embeds(segments: &[Segment]) -> Vec<Segment> {
    let mut out: Vec<Segment> = Vec::with_capacity(segments.len());
    for segment in segments {
        let SegmentBody::Prose = segment.body() else {
            out.push(segment.clone());
            continue;
        };
        let raw = segment.raw();
        let mut cursor = 0;
        // Matches `![[Foo.base]]` / `![[Foo.base#View]]`, optionally with a
        // display suffix, on a line of its own.
        for caps in base_embed_re().captures_iter(raw) {
            let whole = caps.get(0).expect("group 0 of a match always participates");
            if whole.start() > cursor {
                push_prose(
                    &mut out,
                    &raw[cursor..whole.start()],
                    segment.start() + cursor,
                    segment.start() + whole.start(),
                );
            }
            let subpath = caps.get(3).and_then(|m| m.as_str().strip_prefix('#'));
            // The match runs one byte past the region on a CRLF line; see
            // `region_end`.
            let end = region_end(raw, whole.end());
            out.push(Segment::new(
                raw[whole.start()..end].to_string(),
                Span {
                    start: segment.start() + whole.start(),
                    end: segment.start() + end,
                },
                SegmentBody::BaseEmbed(BaseEmbed {
                    base_path: caps
                        .get(2)
                        .expect("group 2 is the target")
                        .as_str()
                        .to_string(),
                    view_name: subpath.filter(|s| !s.is_empty()).map(str::to_string),
                }),
            ));
            cursor = end;
        }
        if cursor < raw.len() {
            push_prose(
                &mut out,
                &raw[cursor..],
                segment.start() + cursor,
                segment.end(),
            );
        }
    }
    out
}

/// Where a `base_embed_re` match ending at `matched_end` really ends.
///
/// The match is one byte longer than the Base region on a CRLF line, and that byte
/// is load-bearing. Rust's `(?m)` `$` does not match before a `\r`, so the pattern
/// has to CONSUME the `\r` to reach `$`; JavaScript's multiline `$` matches there,
/// so the TypeScript tree's span stopped short and the `\r` belonged to the prose
/// after it. The `regex` crate has no look-around to say "but not that `\r`", so
/// the span is trimmed here instead — which is what keeps the two implementations
/// byte-identical rather than merely equivalent.
///
/// **Why this survives normalisation.** Every note this crate reads has been
/// through [`crate::vault::normalise_line_endings`], so a `\r` never sits before
/// a `\n` in practice and the `\r?` in the pattern cannot match. It is kept
/// because `write_note` also parses the text the AGENT sent, which never passed
/// through the backend: an agent whose editor rewrote a Base region's line to CRLF
/// would otherwise have the region fail to match here, the reconciler would read
/// it as deleted, and the restore — which anchors on the surviving regions and
/// finds none — would append the region to the end of the Host note. A parser that
/// cannot see a region cannot protect it.
///
/// A match always contains the embed itself, so trimming one byte can never
/// produce an empty or inverted span.
fn region_end(raw: &str, matched_end: usize) -> usize {
    if raw[..matched_end].ends_with('\r') {
        matched_end - 1
    } else {
        matched_end
    }
}

/// A prose segment carrying `raw`, which spans `[start, end)` of its note.
fn push_prose(out: &mut Vec<Segment>, raw: &str, start: usize, end: usize) {
    out.push(Segment::new(
        raw.to_string(),
        Span { start, end },
        SegmentBody::Prose,
    ));
}

/// Merge prose that ended up split, so a note reads as the blocks it has.
fn merge_adjacent_prose(segments: Vec<Segment>) -> Vec<Segment> {
    let mut merged: Vec<Segment> = Vec::with_capacity(segments.len());
    for segment in segments {
        let joins = merged.last().is_some_and(|prev| {
            prev.kind() == SegmentKind::Prose
                && segment.kind() == SegmentKind::Prose
                && prev.end() == segment.start()
        });
        if !joins {
            merged.push(segment);
            continue;
        }
        let prev = merged
            .last_mut()
            .expect("there is a previous segment to join");
        prev.raw.push_str(&segment.raw);
        prev.span.end = segment.end();
    }
    merged
}

// ---------------------------------------------------------------------------
// Frontmatter
// ---------------------------------------------------------------------------

/// A matched frontmatter block: its whole source text, and its YAML body.
struct FrontmatterBlock<'a> {
    raw: &'a str,
    yaml: &'a str,
}

/// The frontmatter block, when the note opens with one.
///
/// Frontmatter must be the very first thing in the file. The leading BOM is
/// Obsidian's, and the closing `---` may sit at EOF with no newline after it.
fn frontmatter_block(text: &str) -> Option<FrontmatterBlock<'_>> {
    let caps = frontmatter_re().captures(text)?;
    let whole = caps.get(0).expect("group 0 of a match always participates");
    Some(FrontmatterBlock {
        raw: &text[..whole.end()],
        yaml: caps.get(1).map_or("", |m| m.as_str()),
    })
}

/// The properties of a frontmatter block, and whether it failed to parse.
///
/// A malformed block degrades to "no properties" rather than failing the whole
/// note, matching how loaders generally behave. A block that parses to
/// something other than a mapping -- a bare list, a scalar -- has no properties
/// either, and that is not malformed.
fn parse_frontmatter(yaml: &str) -> (Properties, bool) {
    match serde_yaml::from_str::<Yaml>(yaml) {
        Ok(value) => (yaml_properties(&value), false),
        Err(_) => (Properties::new(), true),
    }
}

/// A YAML mapping as note properties.
///
/// Keys are taken in sorted order because [`BasesValue`] has no map type but
/// `BTreeMap`; that is the one place a note's property order is not the order it
/// was written in.
fn yaml_properties(value: &Yaml) -> Properties {
    let Yaml::Mapping(entries) = value else {
        return Properties::new();
    };
    entries
        .iter()
        .map(|(key, item)| (yaml_key(key), yaml_value(item)))
        .collect()
}

/// A YAML value as the value lattice reads it.
///
/// A nested mapping is a namespace, so `note.x.y` reaches into it. A tagged
/// value is the value it tags, which is what an explicit `!!str 3` asks for.
fn yaml_value(value: &Yaml) -> BasesValue {
    match value {
        Yaml::Null => BasesValue::Null,
        Yaml::Bool(b) => BasesValue::Bool(*b),
        Yaml::Number(n) => yaml_number(n),
        Yaml::String(s) => BasesValue::String(s.clone()),
        Yaml::Sequence(items) => BasesValue::List(items.iter().map(yaml_value).collect()),
        Yaml::Mapping(_) => BasesValue::Namespace(Rc::new(yaml_properties(value))),
        Yaml::Tagged(tagged) => yaml_value(&tagged.value),
    }
}

/// A YAML number as a `BasesValue::Number`.
///
/// `.nan` and `.inf` have no number in the lattice, and a property that cannot
/// be compared must not claim to hold one.
fn yaml_number(number: &serde_yaml::Number) -> BasesValue {
    match number.as_f64() {
        Some(n) if n.is_finite() => BasesValue::Number(n),
        _ => BasesValue::Null,
    }
}

/// A YAML key as the property name Bases looks it up by.
///
/// YAML allows non-string keys, and JavaScript coerced them to strings when it
/// built the object, so the same spellings are used rather than dropping the
/// property.
fn yaml_key(key: &Yaml) -> String {
    match key {
        Yaml::String(s) => s.clone(),
        Yaml::Bool(b) => b.to_string(),
        Yaml::Null => "null".to_string(),
        Yaml::Number(n) => match yaml_number(n) {
            BasesValue::Number(n) => crate::value::format_number(n),
            _ => "null".to_string(),
        },
        other => serde_yaml::to_string(other)
            .map_or_else(|_| String::new(), |s| s.trim_end().to_string()),
    }
}

// ---------------------------------------------------------------------------
// Fence handling
// ---------------------------------------------------------------------------

/// The fence language of a rendered Base region.
///
/// Duplicated rather than imported from the renderer: the renderer depends on
/// this parser, so importing the constant back would close the cycle. The
/// renderer emits this exact string, and the round-trip test in `tests/note.rs`
/// pins the two spellings together.
pub const RENDER_FENCE_LANG: &str = "base-rendered";

/// A fenced code block found at a line start.
struct Fence {
    /// The text between the opening and closing fences.
    body: String,
    /// Everything after the opening fence, e.g. `base-rendered path="T.base"`.
    info: String,
    /// The offset just past the closing fence, or EOF for an unclosed one.
    end: usize,
}

/// Match a fenced code block starting at `pos`, honouring Obsidian's rule that
/// an unclosed fence runs to end of file. `None` when no fence starts here.
fn match_fence(text: &str, pos: usize) -> Option<Fence> {
    let bytes = text.as_bytes();
    let line_start = line_start_at(bytes, pos);
    // A fence opens at a line start, never mid-line.
    if bytes[line_start..pos]
        .iter()
        .any(|&b| !b.is_ascii_whitespace())
    {
        return None;
    }
    let line = Lines::from(bytes, line_start).next()?;
    let (ticks, info) = open_fence(&text[line.start..line.content_end])?;
    // A fence with no line terminator after the opening line has no body.
    let body_start = line.next?;
    match find_closing_fence(bytes, body_start, ticks) {
        Some(span) => Some(Fence {
            body: text[body_start..span.start].to_string(),
            end: span.end,
            info,
        }),
        // An unclosed fence runs to EOF, matching Obsidian.
        None => Some(Fence {
            body: text[body_start..].to_string(),
            end: bytes.len(),
            info,
        }),
    }
}

/// The backtick run and info string of a line that opens a fence.
///
/// `None` for anything else, including a line whose info string contains a
/// backtick: the closing fence is matched on a fixed tick count, so a run with
/// no possible close is not a fence.
fn open_fence(line: &str) -> Option<(usize, String)> {
    let caps = open_fence_re().captures(line)?;
    Some((
        caps.get(1)?.as_str().len(),
        caps.get(2)
            .map_or_else(String::new, |m| m.as_str().trim().to_string()),
    ))
}

/// The closing fence at or after `from`: a line of exactly `ticks` backticks
/// after optional quote marks and spacing.
fn find_closing_fence(bytes: &[u8], from: usize, ticks: usize) -> Option<Span> {
    Lines::from(bytes, from)
        .find(|line| is_closing_fence(&bytes[line.start..line.content_end], ticks))
        .map(|line| Span {
            start: line.start,
            end: line.content_end,
        })
}

fn is_closing_fence(line: &[u8], ticks: usize) -> bool {
    let (run, rest) = fence_run(line);
    // Exactly as many backticks as the opener, then nothing but spacing. A
    // blockquote marker before them is fine: a fence inside a blockquote is
    // closed by a blockquoted line.
    run == ticks && rest.iter().all(u8::is_ascii_whitespace)
}

/// The backtick run of a fence line, and everything after it.
///
/// The run is found past the `[ \t>]*` prefix a blockquote adds, because the
/// closing fence may carry that prefix and the opening one never does.
fn fence_run(line: &[u8]) -> (usize, &[u8]) {
    let mut index = 0;
    while index < line.len() && matches!(line[index], b' ' | b'\t' | b'>') {
        index += 1;
    }
    let start = index;
    while index < line.len() && line[index] == b'`' {
        index += 1;
    }
    (index - start, &line[index..])
}

/// The offset of the next fence-looking line after `from`, or the end of the
/// note.
///
/// This is deliberately looser than [`match_fence`]: a line can look like a
/// fence and still not be one -- a blockquoted fence, or an info string holding
/// a backtick -- and the caller re-checks. Only candidates strictly after `from`
/// are returned, because a candidate at `from` would never advance the scan.
fn find_next_fence(text: &str, from: usize) -> usize {
    let bytes = text.as_bytes();
    Lines::at_or_after(bytes, from)
        .filter(|line| fence_run(&bytes[line.start..line.content_end]).0 >= 3)
        .map(|line| line.start)
        .find(|&start| start > from)
        .unwrap_or(bytes.len())
}

/// The `key="value"` pairs on a fence info string.
///
/// The Projection writes ```` ```base-rendered path="Tickets.base" view="All" ````,
/// and those are the only thing tying a rendered fence back to the Base region
/// it came from. Reading them is what lets `write_note` round-trip a Projection
/// the agent edited, instead of writing the rendered fence to disk.
pub fn fence_attrs(info: &str) -> BTreeMap<String, String> {
    let mut attrs = BTreeMap::new();
    for caps in fence_attr_re().captures_iter(info) {
        let key = caps.get(1).expect("group 1 is the key").as_str();
        let value = caps.get(2).expect("group 2 is the value").as_str();
        attrs.insert(key.to_string(), value.to_string());
    }
    attrs
}

/// The language of a fence: the FIRST whitespace-delimited token of its info
/// string.
///
/// The rest of the info string is attributes.
/// ```` ```base-rendered path="Tickets.base" ```` is a rendered region, not
/// prose, and a language that swallowed `path="..."` would match neither.
fn fence_language(info: &str) -> String {
    info.split_whitespace()
        .next()
        .unwrap_or_default()
        .to_lowercase()
}

// ---------------------------------------------------------------------------
// Links, tags, tasks
// ---------------------------------------------------------------------------

/// Every wikilink and markdown link, with offsets relative to `text`.
pub fn extract_links(text: &str) -> Vec<WikiLink> {
    let mut out: Vec<WikiLink> = Vec::new();
    for caps in wiki_link_re().captures_iter(text) {
        let whole = caps.get(0).expect("group 0 of a match always participates");
        let inner = caps.get(2).expect("group 2 is the link body").as_str();
        let (target_with_subpath, display) = match inner.split_once('|') {
            Some((target, display)) => (target, Some(display.to_string())),
            None => (inner, None),
        };
        // A subpath may also appear before the pipe.
        let (target, subpath) = match target_with_subpath.split_once('#') {
            Some((target, subpath)) => (target.to_string(), Some(subpath.to_string())),
            None => (target_with_subpath.to_string(), None),
        };
        out.push(WikiLink {
            target,
            subpath,
            display,
            embedded: caps.get(1).is_some(),
            start: whole.start(),
            end: whole.end(),
        });
    }

    for caps in md_link_re().captures_iter(text) {
        let whole = caps.get(0).expect("group 0 of a match always participates");
        // The TypeScript spelled this `(?<!!)`; the Rust engine has no
        // lookbehind, so the preceding character is checked here instead.
        if text[..whole.start()].ends_with('!') {
            continue;
        }
        let href = caps.get(2).expect("group 2 is the href").as_str();
        // External and in-page links are not vault references.
        if external_scheme_re().is_match(href)
            || href.starts_with('#')
            || href.starts_with("mailto:")
        {
            continue;
        }
        out.push(WikiLink {
            target: href.strip_suffix(".md").unwrap_or(href).to_string(),
            subpath: None,
            display: caps
                .get(1)
                .map(|m| m.as_str())
                .filter(|text| !text.is_empty())
                .map(str::to_string),
            embedded: false,
            start: whole.start(),
            end: whole.end(),
        });
    }

    out.sort_by_key(|link| link.start);
    out
}

/// Inline `#tags`, deduplicated in document order.
///
/// Code spans and fenced blocks are excluded, which is where a naive regex
/// picks up hex colours like `#FFF` inside `style="color: #FFF"`.
pub fn extract_inline_tags(text: &str) -> Vec<String> {
    let stripped = strip_code(text);
    let mut out: Vec<String> = Vec::new();
    for caps in tag_re().captures_iter(&stripped) {
        let whole = caps.get(0).expect("group 0 of a match always participates");
        let tag = caps.get(1).expect("group 1 is the tag").as_str();
        // The TypeScript spelled this `(?<![\w#/:])`; the Rust engine has no
        // lookbehind, so the preceding byte is checked here instead. A `#` after
        // a word character, another `#`, a `/` or a `:` is a fragment of a URL,
        // a path or a heading rather than a tag.
        if stripped.as_bytes()[..whole.start()]
            .last()
            .is_some_and(|&b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'#' | b'/' | b':'))
        {
            continue;
        }
        // Skip numeric fragments, which are hex colours rather than tags.
        if tag.starts_with(|c: char| c.is_ascii_digit()) {
            continue;
        }
        if !out.iter().any(|seen| seen == tag) {
            out.push(tag.to_string());
        }
    }
    out
}

/// Checkboxes in the body. Powers the documented `file.tasks` extension.
pub fn extract_tasks(text: &str) -> Vec<TaskItem> {
    // Frontmatter is not the body, so a `- [ ]` in a property is not a task.
    let body = strip_frontmatter(text);
    let mut out: Vec<TaskItem> = Vec::new();
    for (line, span) in Lines::from(body.as_bytes(), 0).enumerate() {
        let Some(caps) = task_re().captures(&body[span.start..span.content_end]) else {
            continue;
        };
        let mark = caps.get(1).expect("group 1 is the checkbox mark").as_str();
        out.push(TaskItem {
            checked: mark.eq_ignore_ascii_case("x"),
            text: caps.get(2).map_or("", |m| m.as_str()).trim().to_string(),
            line,
        });
    }
    out
}

fn strip_frontmatter(text: &str) -> &str {
    frontmatter_block(text).map_or(text, |block| &text[block.raw.len()..])
}

/// Blank out fenced blocks and inline code so tag scanning cannot see them.
///
/// Bytes are blanked one for one and newlines kept, so an offset into the result
/// still indexes the note it came from.
fn strip_code(text: &str) -> String {
    strip_inline_code(&strip_fenced_blocks(text))
}

/// Blank each fence as far as the end of the line after its opener.
///
/// That is a narrower reach than the name suggests, and it is what the
/// TypeScript regex did: its alternation ended with a bare `$` under the `m`
/// flag, so the "no closing fence, run to end of note" branch actually matched
/// the end of the FIRST body line. Kept as-is because the tag scanner's
/// documented case -- a hex colour on the first line of a fence -- is covered by
/// it, and because a scan that reaches further would report different tags than
/// the vault's own metadata.
fn strip_fenced_blocks(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0;
    let mut lines = Lines::from(bytes, 0);
    while let Some(line) = lines.next() {
        // The opener needs a line terminator after it, or it has no body.
        let Some(next) = line.next else { break };
        if fence_run(&bytes[line.start..line.content_end]).0 < 3 {
            continue;
        }
        let end = Lines::from(bytes, next)
            .next()
            .map_or(bytes.len(), |first| first.content_end);
        out.push_str(&text[cursor..line.start]);
        out.push_str(&blank_out(&bytes[line.start..end]));
        cursor = end;
        // Resume on the line after the one the blanked region ended on, which is
        // where the next opener can be.
        lines = Lines::from(bytes, line_after(bytes, end));
    }
    out.push_str(&text[cursor..]);
    out
}

fn strip_inline_code(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0;
    for span in inline_code_re().find_iter(text) {
        out.push_str(&text[cursor..span.start()]);
        out.push_str(&blank_out(span.as_str().as_bytes()));
        cursor = span.end();
    }
    out.push_str(&text[cursor..]);
    out
}

/// Every byte replaced by a space, newlines kept.
fn blank_out(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len());
    out.extend(bytes.iter().map(|&b| if b == b'\n' { '\n' } else { ' ' }));
    out
}

// ---------------------------------------------------------------------------
// Lines
// ---------------------------------------------------------------------------

/// The span of one line: where it starts, where its content ends, and where the
/// next line begins.
///
/// `content_end` excludes a single trailing `\r`, which is what the TypeScript's
/// `split(/\r?\n/)` did and which makes the field mean what its name says. Worth
/// recording that no consumer currently READS the difference — the fence info
/// string is `.trim()`ed, a closing fence is matched with `is_ascii_whitespace`,
/// a task's text is `.trim()`ed, and the two remaining uses count backticks at the
/// START of a line — so this costs a branch and buys the honest model rather than
/// an observed behaviour. It was audited rather than assumed, and deleting it is a
/// safe follow-up for anyone who wants the field gone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Line {
    start: usize,
    content_end: usize,
    /// The start of the next line, or `None` when this line ends the note.
    next: Option<usize>,
}

/// Line spans of a note, in order.
struct Lines<'a> {
    bytes: &'a [u8],
    next: Option<usize>,
}

impl<'a> Lines<'a> {
    /// Every line from `start` on.
    fn from(bytes: &'a [u8], start: usize) -> Self {
        Lines {
            bytes,
            next: Some(start),
        }
    }

    /// The lines whose start is at or after `pos`: `pos` itself when it begins a
    /// line, otherwise the first line after the one containing it.
    fn at_or_after(bytes: &'a [u8], pos: usize) -> impl Iterator<Item = Line> + 'a {
        Lines::from(bytes, line_start_at(bytes, pos)).skip_while(move |line| line.start < pos)
    }
}

impl Iterator for Lines<'_> {
    type Item = Line;

    fn next(&mut self) -> Option<Line> {
        let start = self.next?;
        if start >= self.bytes.len() {
            return None;
        }
        let end = match self.bytes[start..].iter().position(|&b| b == b'\n') {
            Some(offset) => start + offset,
            None => self.bytes.len(),
        };
        let content_end = if end > start && self.bytes[end - 1] == b'\r' {
            end - 1
        } else {
            end
        };
        self.next = (end < self.bytes.len()).then_some(end + 1);
        Some(Line {
            start,
            content_end,
            next: self.next,
        })
    }
}

/// The start of the line containing `pos`. Position zero is always a line start.
fn line_start_at(bytes: &[u8], pos: usize) -> usize {
    match bytes[..pos].iter().rposition(|&b| b == b'\n') {
        Some(index) => index + 1,
        None => 0,
    }
}

/// The start of the line after the one containing `pos`, or the end of the note.
fn line_after(bytes: &[u8], pos: usize) -> usize {
    Lines::from(bytes, pos)
        .nth(1)
        .map_or(bytes.len(), |line| line.start)
}

// ---------------------------------------------------------------------------
// Patterns
// ---------------------------------------------------------------------------

/// Compile a pattern on first use. Every note in a vault walk hits these, and a
/// pattern is a constant, so it is built once and never rebuilt.
macro_rules! pattern {
    ($name:ident, $re:literal) => {
        fn $name() -> &'static Regex {
            static RE: OnceLock<Regex> = OnceLock::new();
            RE.get_or_init(|| Regex::new($re).expect(concat!("the ", $re, " pattern compiles")))
        }
    };
}

pattern!(
    frontmatter_re,
    r"^(?:\u{FEFF})?---[ \t]*\r?\n([\s\S]*?)\r?\n---[ \t]*(?:\r?\n|$)"
);
pattern!(open_fence_re, r"^(`{3,})[ \t]*([^`\n]*)[ \t]*$");
pattern!(
    fence_attr_re,
    r#"([A-Za-z_][A-Za-z0-9_-]*)\s*=\s*"([^"]*)""#
);
pattern!(wiki_link_re, r"(!)?\[\[([^\]\n]+?)\]\]");
pattern!(md_link_re, r#"\[([^\]\n]*)\]\(([^)\s]+)(?:\s+"[^"]*")?\)"#);
pattern!(tag_re, r"#([A-Za-z0-9_][A-Za-z0-9_/-]*)");
pattern!(task_re, r"^[ \t]*[-*+][ \t]+\[([ xX/-])\][ \t]*(.*)$");
pattern!(inline_code_re, r"`+[^`\n]*`+");
pattern!(external_scheme_re, r"(?i)^[a-z]+://");

// `base_embed_re` is anchored per line, so it needs the multi-line flag; every
// other pattern is applied to a single line or to the whole note.
fn base_embed_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // The trailing `\r?` matches nothing on any note this crate READ — the
        // backend normalises CRLF to LF — and is kept for the text the agent
        // SENT, which never passed through it. Without it, an agent that rewrote a
        // Base region's line to CRLF would have its region invisible here, and an
        // invisible region is one the reconciler restores at the wrong place.
        // JavaScript's multiline `$` matches before `\r`, so the TypeScript tree
        // never needed this and the port did.
        //
        // Consuming the `\r` also makes the match one byte longer than the span
        // JavaScript produced, so `split_base_embeds` trims it back; see
        // `region_end` for why the region must not own that byte.
        Regex::new(r"(?m)^([ \t]*)!\[\[([^\]|#]+?\.base)(#[^\]|]*)?(\|[^\]]*)?\]\][ \t]*\r?$")
            .expect("the base embed pattern compiles")
    })
}
