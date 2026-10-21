//! The value lattice.
//!
//! Bases has a small closed set of value types and the evaluator is total over
//! it. The one decision worth defending: `null` and "absent" are the same thing
//! here, because Obsidian's JSON does the same. Everywhere else a missing
//! property is genuinely `Null`.
//!
//! Dates and durations are distinct types and stay distinct. The official docs
//! say date subtraction yields milliseconds; the runtime returns a `Duration`
//! and `number(duration)` throws. Real vaults depend on BOTH idioms, so both
//! are supported rather than picking a side. See `docs/divergences.md` in the
//! TypeScript tree.

use std::fmt;

use chrono::{DateTime, FixedOffset};

/// An instant, kept with its offset so `file.mtime` round-trips the way it was
/// read rather than being silently normalised to UTC.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BasesDate(pub DateTime<FixedOffset>);

impl BasesDate {
    pub fn year(&self) -> i32 {
        use chrono::Datelike;
        self.0.year()
    }
    pub fn month(&self) -> u32 {
        use chrono::Datelike;
        self.0.month()
    }
    pub fn day(&self) -> u32 {
        use chrono::Datelike;
        self.0.day()
    }
    pub fn hour(&self) -> u32 {
        use chrono::Timelike;
        self.0.hour()
    }
    pub fn minute(&self) -> u32 {
        use chrono::Timelike;
        self.0.minute()
    }
    pub fn second(&self) -> u32 {
        use chrono::Timelike;
        self.0.second()
    }
    pub fn millisecond(&self) -> u32 {
        self.0.timestamp_subsec_millis()
    }

    pub fn from_millis(millis: i64) -> Self {
        let dt = DateTime::from_timestamp_millis(millis)
            .unwrap_or_else(|| DateTime::from_timestamp_millis(0).expect("epoch is representable"));
        Self(dt.fixed_offset())
    }

    pub fn millis(&self) -> i64 {
        self.0.timestamp_millis()
    }
}

impl fmt::Display for BasesDate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}",
            self.0.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
        )
    }
}

/// A span in milliseconds.
///
/// Both `.days` and `number()` are supported, so `((number(date(due)) -
/// number(today())) / 86400000).floor()` and `(now() - file.mtime).days` both
/// work. See the module docs of `lib.rs` and `docs/divergences.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct Duration {
    pub millis: i64,
    /// Calendar months, kept separately because a month is not a fixed span.
    pub months: i64,
    /// Calendar years, likewise.
    pub years: i64,
}

impl Duration {
    pub const ZERO: Duration = Duration {
        millis: 0,
        months: 0,
        years: 0,
    };

    pub fn from_millis(millis: i64) -> Self {
        Self {
            millis,
            months: 0,
            years: 0,
        }
    }

    /// Whole days in the span. `M`/month is a CALENDAR field, not this.
    pub fn days(&self) -> i64 {
        self.millis.div_euclid(86_400_000)
    }
    pub fn hours(&self) -> i64 {
        self.millis.div_euclid(3_600_000)
    }
    /// Whole minutes in the span, from `m`. The calendar `months` field is
    /// reached as `.months`, which is why the method cannot be named that.
    pub fn minutes(&self) -> i64 {
        self.millis.div_euclid(60_000)
    }
    pub fn seconds(&self) -> i64 {
        self.millis.div_euclid(1_000)
    }
}

impl fmt::Display for Duration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let abs = self.millis.abs();
        if abs == 0 {
            return write!(f, "0 seconds");
        }
        let (n, unit) = if abs % 86_400_000 == 0 {
            (abs / 86_400_000, "day")
        } else if abs % 3_600_000 == 0 {
            (abs / 3_600_000, "hour")
        } else if abs % 60_000 == 0 {
            (abs / 60_000, "minute")
        } else if abs % 1_000 == 0 {
            (abs / 1_000, "second")
        } else {
            (abs, "millisecond")
        };
        let plural = if n == 1 { "" } else { "s" };
        if self.millis < 0 {
            write!(f, "-{n} {unit}{plural}")
        } else {
            write!(f, "{n} {unit}{plural}")
        }
    }
}

/// A value, as an expression produces it.
#[derive(Debug, Clone, PartialEq)]
pub enum BasesValue {
    /// A missing property, an explicit `null`, and the empty list all render
    /// blank in Obsidian's JSON, so they share this.
    Null,
    Bool(bool),
    Number(f64),
    String(String),
    /// A tag: a string that Obsidian treats as a type of its own.
    ///
    /// Obsidian has a `TagValue` class that extends `StringValue`, and a
    /// `TagListValue` that extends `ListValue`. The class is load-bearing:
    /// `looseEquals` compares `constructor`s, so a tag is NOT loose-equal to a
    /// plain string. Measured against Obsidian 1.14.4: `tags == "project"` is
    /// false for every tag shape, while `tags[0] == "#project"` is true.
    ///
    /// The text carries the `#`. Obsidian stores a tag as `#project` and
    /// `tags[0].length` is 8, so the prefix is data rather than a display
    /// convention.
    Tag(String),
    Date(BasesDate),
    Duration(Duration),
    List(Vec<BasesValue>),
    /// A link to another note.
    ///
    /// `resolved` is the path it points at, when the vault could resolve it.
    /// Link equality compares resolved targets, not text, so `[[Note]]` and a
    /// frontmatter link to the same file compare equal.
    Link {
        target: String,
        display: Option<String>,
        resolved: Option<String>,
    },
    /// A file, and the lazy accessors that need the vault. Only usable inside
    /// method calls.
    File(std::sync::Arc<FileValue>),
    /// A property namespace: `note`, `formula`, or `file.properties`.
    ///
    /// A `BTreeMap` rather than a struct because these come from YAML and from
    /// user keys, so the set of names is only known at runtime. Member access on
    /// it yields the stored value; `.keys()` and `.values()` work on it.
    Namespace(std::sync::Arc<std::collections::BTreeMap<String, BasesValue>>),
}

/// Turn a link-ish string into a `FileValue`, or `None` when unresolvable.
pub type Resolver = std::sync::Arc<dyn Fn(&str) -> Option<FileValue> + Send + Sync>;

/// A file plus the accessors the vault supplies.
///
/// `Arc` rather than `Rc`, and `Send + Sync` rather than plain: rmcp serves the
/// tool surface across worker threads, so the closures have to be able to cross
/// them.
#[derive(Debug, Clone)]
pub struct FileValue {
    pub path: String,
    pub name: String,
    pub basename: String,
    /// The containing folder. The vault ROOT is `"/"`, not `""` -- confirmed
    /// against `base:query format=json` on Obsidian 1.13.7, where a root note
    /// emits `"folder": "/"` while a nested one emits `"folder": "Projects"`.
    pub folder: String,
    pub ext: String,
    pub accessors: std::sync::Arc<FileAccessors>,
    /// The host note's frontmatter, when this value stands in for `this`.
    ///
    /// `this` has to answer both as a File and as a bag of frontmatter keys,
    /// because real vaults write `this.path` and `this.projects` with the same
    /// receiver. Rust has no proxy, so the extra keys ride along here and
    /// `read_member` consults them first.
    pub overrides: Option<std::sync::Arc<std::collections::BTreeMap<String, BasesValue>>>,
}

impl PartialEq for FileValue {
    fn eq(&self, other: &Self) -> bool {
        self.path == other.path
    }
}

/// The vault-side operations a `FileValue` needs.
///
/// These are the only calls into the vault from inside the evaluator, which is
/// what keeps the evaluator testable without one.
///
/// `Debug` is hand-written rather than derived: the closures cannot implement
/// it, and a derive would force every `FileValue` in a debug log to be
/// unprintable.
#[derive(Clone)]
pub struct FileAccessors {
    pub tags: std::sync::Arc<dyn Fn() -> Vec<BasesValue> + Send + Sync>,
    pub links: std::sync::Arc<dyn Fn() -> Vec<BasesValue> + Send + Sync>,
    pub embeds: std::sync::Arc<dyn Fn() -> Vec<BasesValue> + Send + Sync>,
    pub backlinks: std::sync::Arc<dyn Fn() -> Vec<BasesValue> + Send + Sync>,
    pub properties:
        std::sync::Arc<dyn Fn() -> std::collections::BTreeMap<String, BasesValue> + Send + Sync>,
    pub ctime: std::sync::Arc<dyn Fn() -> BasesDate + Send + Sync>,
    pub mtime: std::sync::Arc<dyn Fn() -> BasesDate + Send + Sync>,
    pub size: std::sync::Arc<dyn Fn() -> u64 + Send + Sync>,
    /// `file.tasks` is an extension: absent in Obsidian 1.13.7, which returns
    /// `null`. See `docs/divergences.md` D3.
    pub tasks: std::sync::Arc<dyn Fn() -> Vec<BasesValue> + Send + Sync>,
    pub resolve: Resolver,
    pub links_to: std::sync::Arc<dyn Fn(&str) -> bool + Send + Sync>,
}

impl std::fmt::Debug for FileAccessors {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("FileAccessors { .. }")
    }
}

impl FileValue {
    pub fn new(path: impl Into<String>, accessors: FileAccessors) -> Self {
        let path = path.into();
        let base = path.rsplit('/').next().unwrap_or(&path).to_string();
        let (stem, ext) = match base.rfind('.') {
            Some(i) if i > 0 => (base[..i].to_string(), base[i + 1..].to_string()),
            _ => (base.clone(), String::new()),
        };
        Self {
            folder: folder_of(&path),
            name: base.clone(),
            basename: stem,
            path,
            ext,
            accessors: std::sync::Arc::new(accessors),
            overrides: None,
        }
    }
}

/// The containing folder of a path. The root is `"/"`.
pub fn folder_of(path: &str) -> String {
    match path.rfind('/') {
        Some(i) => path[..i].to_string(),
        None => "/".to_string(),
    }
}

/// A path without its extension. A leading dot is not an extension.
pub fn strip_extension(path: &str) -> String {
    match path.rfind('.') {
        Some(i) if i > 0 => path[..i].to_string(),
        _ => path.to_string(),
    }
}

impl BasesValue {
    /// Obsidian's truthiness: an empty list is falsy, a zero is truthy, and
    /// `""` is truthy. Only the empty list and null are falsy among the
    /// non-boolean types.
    pub fn is_truthy(&self) -> bool {
        match self {
            BasesValue::Null => false,
            BasesValue::Bool(b) => *b,
            BasesValue::List(items) => !items.is_empty(),
            _ => true,
        }
    }

    pub fn is_empty(&self) -> bool {
        match self {
            BasesValue::Null => true,
            BasesValue::List(items) => items.is_empty(),
            _ => false,
        }
    }

    /// The tag text, if this value is a tag.
    pub fn as_tag(&self) -> Option<&str> {
        match self {
            BasesValue::Tag(text) => Some(text),
            _ => None,
        }
    }

    /// A tag, built from a frontmatter value.
    ///
    /// Obsidian forces the leading `#` on every tag (`MW`), so `project` and
    /// `#project` are the same tag. A value that is not a string is NOT a tag:
    /// Obsidian's frontmatter loader only builds a tag list when every element
    /// is a string, and a mixed list keeps its elements as they are.
    pub fn tag(text: impl Into<String>) -> Self {
        BasesValue::Tag(ensure_hash(text.into()))
    }

    pub fn as_number(&self) -> Option<f64> {
        match self {
            BasesValue::Number(n) => Some(*n),
            _ => None,
        }
    }

    /// A string, or the numeric/boolean spelling of a scalar.
    ///
    /// This is what makes `==` work across types the way Obsidian's does: a
    /// number compared with the string that spells it is equal.
    pub fn to_display_string(&self) -> String {
        match self {
            BasesValue::Null => String::new(),
            BasesValue::Bool(b) => b.to_string(),
            BasesValue::Number(n) => format_number(*n),
            BasesValue::String(s) => s.clone(),
            // A tag renders as its text, `#` and all.
            BasesValue::Tag(s) => s.clone(),
            BasesValue::Date(d) => d.to_string(),
            BasesValue::Duration(d) => d.to_string(),
            BasesValue::List(items) => items
                .iter()
                .map(|i| i.to_display_string())
                .collect::<Vec<_>>()
                .join(", "),
            BasesValue::Link {
                target, display, ..
            } => match display {
                Some(d) => format!("[[{target}|{d}]]"),
                None => format!("[[{target}]]"),
            },
            BasesValue::File(f) => f.path.clone(),
            BasesValue::Namespace(map) => map
                .iter()
                .map(|(k, v)| format!("{k}: {}", v.to_display_string()))
                .collect::<Vec<_>>()
                .join(", "),
        }
    }

    /// The list view, used by `contains` and by list methods. A scalar is a
    /// one-element list; null is the empty list.
    pub fn to_list(&self) -> Vec<BasesValue> {
        match self {
            BasesValue::List(items) => items.clone(),
            BasesValue::Null => Vec::new(),
            other => vec![other.clone()],
        }
    }

    /// An empty namespace, used where a namespace is required but absent.
    pub fn empty_namespace() -> Self {
        BasesValue::Namespace(std::sync::Arc::new(std::collections::BTreeMap::new()))
    }

    /// A link's resolved target, for link equality.
    pub fn link_target(&self) -> Option<&str> {
        match self {
            BasesValue::Link {
                target, resolved, ..
            } => Some(resolved.as_deref().unwrap_or(target)),
            _ => None,
        }
    }
}

/// A stable, total string for a namespace, used only for ordering.
fn render_namespace(map: &std::collections::BTreeMap<String, BasesValue>) -> String {
    map.iter()
        .map(|(k, v)| format!("{k}={}", v.to_display_string()))
        .collect::<Vec<_>>()
        .join("\u{1}")
}

/// The tag text with a leading `#`, which is how Obsidian stores one.
///
/// Obsidian's `MW`: a tag is `#name`, and the `#` is part of the data rather
/// than a display convention -- `tags[0].length` is 8 for `#project`.
pub fn ensure_hash(text: String) -> String {
    if text.starts_with('#') {
        text
    } else {
        format!("#{text}")
    }
}

/// Whether a tag matches a needle, the way Obsidian's `tagMatches` does.
///
/// The rule, measured against Obsidian 1.14.4:
///   * both sides are lowercased, so the match is case-insensitive;
///   * the needle is normalised to a `#`-prefixed tag, so `project` and
///     `#project` are the same needle;
///   * the match is a PREFIX match on the tag, not an equality: `project`
///     matches `#project/alpha`, but `proj` matches nothing;
///   * the prefix must land on a `/` boundary, so `project` does not match
///     `#projectile`;
///   * an empty needle never matches.
///
/// This is what `tags.contains(x)` and `file.hasTag(x)` do. It is NOT what
/// `==` does, which compares the text exactly.
pub fn tag_matches(tag: &str, needle: &str) -> bool {
    let tag_lower = tag.to_lowercase();
    let needle_lower = ensure_hash(needle.to_string()).to_lowercase();
    if needle_lower.len() > tag_lower.len() {
        return false;
    }
    if !tag_lower.starts_with(&needle_lower) {
        return false;
    }
    tag_lower.len() == needle_lower.len() || tag_lower.as_bytes()[needle_lower.len()] == b'/'
}

/// A number as Obsidian spells it: integral values have no `.0`.
pub fn format_number(n: f64) -> String {
    if n.is_finite() && n.fract() == 0.0 && n.abs() < 1e15 {
        format!("{}", n as i64)
    } else {
        format!("{n}")
    }
}

/// The union of equality, ordering, and truthiness into one total comparison.
///
/// Returns `None` when the two values are not comparable at all, which is what
/// lets `==` be false rather than an error for mismatched types.
pub fn compare(a: &BasesValue, b: &BasesValue) -> Option<std::cmp::Ordering> {
    use std::cmp::Ordering;
    use BasesValue::*;

    match (a, b) {
        (Null, Null) => Some(Ordering::Equal),
        (Null, _) | (_, Null) => Some(Ordering::Less),
        (Bool(x), Bool(y)) => Some(x.cmp(y)),
        (Number(x), Number(y)) => x.partial_cmp(y),
        (String(x), String(y)) => Some(x.cmp(y)),
        // A TAG IS A STRING, AND THE `#` IS PART OF THE TEXT.
        //
        // Obsidian stores a tag as `#project`, so `tags[0] == "#project"` is
        // true and `tags[0] == "project"` is false. The `#` is data, not a
        // display convention: `tags[0].length` is 8 for `#project`.
        (Tag(x), Tag(y)) => Some(x.cmp(y)),
        // A tag against a plain string compares the TEXT, so `#project` and
        // `#project` are equal but `#project` and `project` are not.
        (Tag(x), String(y)) => Some(x.cmp(y)),
        (String(x), Tag(y)) => Some(x.cmp(y)),
        (Date(x), Date(y)) => Some(x.cmp(y)),
        (Duration(x), Duration(y)) => Some(x.cmp(y)),
        (List(x), List(y)) => {
            for (xa, ya) in x.iter().zip(y.iter()) {
                match compare(xa, ya) {
                    Some(Ordering::Equal) | None => continue,
                    other => return other,
                }
            }
            Some(x.len().cmp(&y.len()))
        }
        // A list compares equal to a bare scalar only when it holds exactly one
        // element, which is Obsidian's ListValue.looseEquals against a
        // non-list. Wrapping the pair in lists instead would recurse forever,
        // because the wrapper is itself a list.
        (List(x), other) => match x.as_slice() {
            [only] => compare(only, other),
            _ => None,
        },
        (other, List(y)) => match y.as_slice() {
            [only] => compare(other, only),
            _ => None,
        },
        (Link { .. }, Link { .. }) => Some(a.link_target().cmp(&b.link_target())),
        (File(x), File(y)) => Some(x.path.cmp(&y.path)),
        // Namespaces compare by rendered content: `BasesValue` is not `Ord`, and
        // a namespace only ever reaches here when a filter sorts on it.
        (Namespace(x), Namespace(y)) => Some(render_namespace(x).cmp(&render_namespace(y))),
        // A number and the string that spells it compare equal, which is what
        // makes `file.size == 42` and `status == "active"` both behave.
        //
        // The empty string coerces to 0 rather than failing to parse, because
        // Obsidian compares raw data with JavaScript `==` and ToNumber("") is 0.
        // A string that does not convert is genuinely unordered against a
        // number, which is why `"abc" > 1` is false for every operator.
        // Split rather than merged into one `|` arm, because the arms below are
        // not symmetric: merging them compares the parsed string against the
        // number on both sides, which reverses the answer.
        (Number(n), String(s)) => js_to_number(s).and_then(|p| n.partial_cmp(&p)),
        (String(s), Number(n)) => js_to_number(s).and_then(|p| p.partial_cmp(n)),
        // A checkbox crosses with a number the same way, because a boolean's raw
        // data is 1 or 0: `true == 1`, `false == 0`, `true == 2` false.
        (Bool(x), Number(n)) => f64::from(u8::from(*x)).partial_cmp(n),
        (Number(n), Bool(x)) => n.partial_cmp(&f64::from(u8::from(*x))),
        (Bool(x), String(s)) => f64::from(u8::from(*x)).partial_cmp(&js_to_number(s)?),
        (String(s), Bool(x)) => js_to_number(s)?.partial_cmp(&f64::from(u8::from(*x))),
        _ => None,
    }
}

/// JavaScript's ToNumber for a string, as `==` and `<`/`>` apply it.
///
/// The empty string is 0, which is load-bearing: it is why `"" == 0` and
/// `"" == false` both hold in Obsidian. Anything that does not convert is None,
/// which reads as "not comparable" rather than as an error.
fn js_to_number(s: &str) -> Option<f64> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return Some(0.0);
    }
    trimmed.parse::<f64>().ok().filter(|n| n.is_finite())
}

/// Equality as Bases defines it: link equality by resolved target, scalars
/// across types by their string form.
pub fn values_equal(a: &BasesValue, b: &BasesValue) -> bool {
    // Only null equals null. Obsidian's looseEquals is false for null against
    // anything else, which is what makes an empty string and an empty list both
    // *not* null: `"" == null` and `[] == null` are both false, so `!= null` is
    // the test for "this property has a value at all".
    match (a, b) {
        (BasesValue::Null, BasesValue::Null) => true,
        (BasesValue::Null, _) | (_, BasesValue::Null) => false,
        _ => compare(a, b) == Some(std::cmp::Ordering::Equal),
    }
}
