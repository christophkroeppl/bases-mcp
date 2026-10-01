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
    File(std::rc::Rc<FileValue>),
    /// A property namespace: `note`, `formula`, or `file.properties`.
    ///
    /// A `BTreeMap` rather than a struct because these come from YAML and from
    /// user keys, so the set of names is only known at runtime. Member access on
    /// it yields the stored value; `.keys()` and `.values()` work on it.
    Namespace(std::rc::Rc<std::collections::BTreeMap<String, BasesValue>>),
}

/// Turn a link-ish string into a `FileValue`, or `None` when unresolvable.
pub type Resolver = std::rc::Rc<dyn Fn(&str) -> Option<FileValue>>;

/// A file plus the accessors the vault supplies.
///
/// `Rc` rather than `Arc`: a query is single-threaded, and the reference cycles
/// a shared-mutable-accessor design would need are avoided entirely because the
/// accessors are pure functions of the vault.
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
    pub accessors: std::rc::Rc<FileAccessors>,
    /// The host note's frontmatter, when this value stands in for `this`.
    ///
    /// `this` has to answer both as a File and as a bag of frontmatter keys,
    /// because real vaults write `this.path` and `this.projects` with the same
    /// receiver. Rust has no proxy, so the extra keys ride along here and
    /// `read_member` consults them first.
    pub overrides: Option<std::rc::Rc<std::collections::BTreeMap<String, BasesValue>>>,
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
    pub tags: std::rc::Rc<dyn Fn() -> Vec<BasesValue>>,
    pub links: std::rc::Rc<dyn Fn() -> Vec<BasesValue>>,
    pub embeds: std::rc::Rc<dyn Fn() -> Vec<BasesValue>>,
    pub backlinks: std::rc::Rc<dyn Fn() -> Vec<BasesValue>>,
    pub properties: std::rc::Rc<dyn Fn() -> std::collections::BTreeMap<String, BasesValue>>,
    pub ctime: std::rc::Rc<dyn Fn() -> BasesDate>,
    pub mtime: std::rc::Rc<dyn Fn() -> BasesDate>,
    pub size: std::rc::Rc<dyn Fn() -> u64>,
    /// `file.tasks` is an extension: absent in Obsidian 1.13.7, which returns
    /// `null`. See `docs/divergences.md` D3.
    pub tasks: std::rc::Rc<dyn Fn() -> Vec<BasesValue>>,
    pub resolve: Resolver,
    pub links_to: std::rc::Rc<dyn Fn(&str) -> bool>,
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
            accessors: std::rc::Rc::new(accessors),
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
        BasesValue::Namespace(std::rc::Rc::new(std::collections::BTreeMap::new()))
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
        (Link { .. }, Link { .. }) => Some(a.link_target().cmp(&b.link_target())),
        (File(x), File(y)) => Some(x.path.cmp(&y.path)),
        // Namespaces compare by rendered content: `BasesValue` is not `Ord`, and
        // a namespace only ever reaches here when a filter sorts on it.
        (Namespace(x), Namespace(y)) => Some(render_namespace(x).cmp(&render_namespace(y))),
        // A number and the string that spells it compare equal, which is what
        // makes `file.size == 42` and `status == "active"` both behave.
        (Number(n), String(s)) | (String(s), Number(n)) => {
            s.parse::<f64>().ok().and_then(|p| p.partial_cmp(n))
        }
        (Bool(x), String(s)) | (String(s), Bool(x)) => {
            let want = if *x { "true" } else { "false" };
            Some(want.cmp(s.as_str()))
        }
        _ => None,
    }
}

/// Equality as Bases defines it: link equality by resolved target, scalars
/// across types by their string form.
pub fn values_equal(a: &BasesValue, b: &BasesValue) -> bool {
    match (a, b) {
        (BasesValue::Null, other) | (other, BasesValue::Null) => other.is_empty(),
        _ => compare(a, b) == Some(std::cmp::Ordering::Equal),
    }
}
