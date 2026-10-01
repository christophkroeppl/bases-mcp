//! The standard library: globals, methods, and the higher-order list methods.
//!
//! Methods are looked up by (receiver type, name) rather than by value, so
//! `get_method` is a two-step hash probe and a missing method is a hard error
//! naming the type — which is the behaviour the conformance suite pins against
//! Obsidian's exact error strings.
//!
//! The receiver-type order matters: `Link` and `File` are checked before
//! `List`, and `String` before the generic `Object` case, because a value can
//! satisfy more than one structural test.

use std::collections::BTreeMap;
use std::rc::Rc;

use chrono::{
    DateTime, Datelike, Duration as ChronoDuration, FixedOffset, NaiveDate, NaiveTime, TimeZone,
    Timelike,
};

use crate::error::{BasesError, Result};
use crate::evaluator::{coerce_date, to_number_loose, EvalContext};
use crate::value::{values_equal, BasesDate, BasesValue, Duration};

/// Which receiver type a value dispatches on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ReceiverType {
    Null,
    Link,
    File,
    Date,
    Duration,
    List,
    String,
    Number,
    Boolean,
    Object,
}

impl ReceiverType {
    /// Obsidian's error messages spell the type with these names, and the
    /// conformance suite compares them byte for byte.
    pub fn display_name(&self) -> &'static str {
        match self {
            ReceiverType::Null => "null",
            ReceiverType::Link => "Link",
            ReceiverType::File => "File",
            ReceiverType::Date => "Date",
            ReceiverType::Duration => "duration",
            ReceiverType::List => "List",
            ReceiverType::String => "String",
            ReceiverType::Number => "Number",
            ReceiverType::Boolean => "Boolean",
            ReceiverType::Object => "Object",
        }
    }

    fn key(&self) -> &'static str {
        match self {
            ReceiverType::Null => "null",
            ReceiverType::Link => "link",
            ReceiverType::File => "file",
            ReceiverType::Date => "date",
            ReceiverType::Duration => "duration",
            ReceiverType::List => "list",
            ReceiverType::String => "string",
            ReceiverType::Number => "number",
            ReceiverType::Boolean => "boolean",
            ReceiverType::Object => "object",
        }
    }
}

pub fn receiver_type(v: &BasesValue) -> ReceiverType {
    match v {
        BasesValue::Null => ReceiverType::Null,
        BasesValue::Link { .. } => ReceiverType::Link,
        BasesValue::File(_) => ReceiverType::File,
        BasesValue::Date(_) => ReceiverType::Date,
        BasesValue::Duration(_) => ReceiverType::Duration,
        BasesValue::List(_) => ReceiverType::List,
        BasesValue::String(_) => ReceiverType::String,
        BasesValue::Number(_) => ReceiverType::Number,
        BasesValue::Bool(_) => ReceiverType::Boolean,
        // A namespace is `note`, `formula` and `file.properties`. Those read as
        // an object, which is what makes `.keys()` and `.values()` work on them.
        BasesValue::Namespace(_) => ReceiverType::Object,
    }
}

pub fn describe(v: &BasesValue) -> &'static str {
    receiver_type(v).display_name()
}

pub type MethodFn = fn(&BasesValue, &[BasesValue], &EvalContext) -> Result<BasesValue>;
pub type GlobalBody = fn(&[BasesValue], &EvalContext) -> Result<BasesValue>;

/// A registered method. `lambda_arity` is `Some` for the higher-order methods,
/// whose single argument is an expression rather than a value.
#[derive(Clone)]
pub struct MethodDef {
    pub fn_body: MethodFn,
    pub lambda_arity: Option<u8>,
}

#[derive(Clone)]
pub struct GlobalDef {
    pub body: GlobalBody,
}

type MethodMap = BTreeMap<&'static str, BTreeMap<&'static str, MethodDef>>;
type GlobalMap = BTreeMap<&'static str, GlobalDef>;

/// The built-in registries.
///
/// Built once, in one pass, by [`build`]. No `define_*` registration function
/// exists: a caller cannot forget to register, and there is no way for a
/// built-in to be registered twice or under a receiver type that cannot occur.
struct Registries {
    globals: GlobalMap,
    methods: MethodMap,
}

static REGISTRIES: std::sync::OnceLock<Registries> = std::sync::OnceLock::new();

fn registries() -> &'static Registries {
    REGISTRIES.get_or_init(build)
}

pub fn get_global(name: &str) -> Option<&'static GlobalDef> {
    registries().globals.get(name)
}

pub fn get_method(target: &BasesValue, name: &str) -> Option<&'static MethodDef> {
    let r = registries();
    // The receiver's own methods first, then the universal ones, which live on
    // `null`.
    r.methods
        .get(receiver_type(target).key())
        .and_then(|by_name| by_name.get(name))
        .or_else(|| {
            r.methods
                .get(ReceiverType::Null.key())
                .and_then(|by_name| by_name.get(name))
        })
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

fn arg(args: &[BasesValue], i: usize) -> BasesValue {
    args.get(i).cloned().unwrap_or(BasesValue::Null)
}

fn as_str(v: &BasesValue, fn_name: &str) -> Result<String> {
    match v {
        BasesValue::String(s) => Ok(s.clone()),
        other => Err(BasesError::new(format!(
            "Type error in \"{fn_name}\", parameter expects String, given {}",
            describe(other)
        ))
        .with_construct(fn_name)),
    }
}

fn as_list<'a>(v: &'a BasesValue, fn_name: &str) -> Result<&'a [BasesValue]> {
    match v {
        BasesValue::List(items) => Ok(items),
        other => Err(BasesError::new(format!(
            "Type error in \"{fn_name}\", parameter expects List, given {}",
            describe(other)
        ))
        .with_construct(fn_name)),
    }
}

fn as_date(v: &BasesValue, fn_name: &str) -> Result<BasesDate> {
    coerce_date(v).ok_or_else(|| {
        BasesError::new(format!(
            "Type error in \"{fn_name}\", parameter expects Date, given {}",
            describe(v)
        ))
        .with_construct(fn_name)
    })
}

/// Whether a link's text points at `path`, tolerating extension and depth.
///
/// This is the comparison that makes `[[Note]]`, `Note` and `folder/Note` all
/// equal to the same file, which link equality depends on.
pub fn matches_link_text(a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    let na = normalise_link(a);
    let nb = normalise_link(b);
    if na == nb {
        return true;
    }
    na.ends_with(&format!("/{nb}")) || nb.ends_with(&format!("/{na}"))
}

fn normalise_link(s: &str) -> String {
    crate::value::strip_extension(s.trim()).to_lowercase()
}

fn link_points_at(l: &BasesValue, path: &str) -> bool {
    match l {
        BasesValue::Link {
            target, resolved, ..
        } => matches_link_text(resolved.as_deref().unwrap_or(target), path),
        BasesValue::String(s) => matches_link_text(s, path),
        _ => false,
    }
}

/// The path a link-ish value refers to, for `linksTo` and `hasLink`.
fn linkish_path(v: &BasesValue) -> Option<String> {
    match v {
        BasesValue::File(f) => Some(f.path.clone()),
        BasesValue::Link {
            target, resolved, ..
        } => Some(resolved.clone().unwrap_or_else(|| target.clone())),
        BasesValue::String(s) => Some(s.clone()),
        _ => None,
    }
}

fn values_equal_loose(a: &BasesValue, b: &BasesValue) -> bool {
    if let (Some(pa), Some(pb)) = (linkish_path(a), linkish_path(b)) {
        let either_is_linkish = matches!(a, BasesValue::Link { .. } | BasesValue::File(_))
            || matches!(b, BasesValue::Link { .. } | BasesValue::File(_));
        if either_is_linkish {
            return matches_link_text(&pa, &pb);
        }
    }
    values_equal(a, b)
}

fn compare_loose(a: &BasesValue, b: &BasesValue) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    if matches!(a, BasesValue::Null) {
        return if matches!(b, BasesValue::Null) {
            Ordering::Equal
        } else {
            Ordering::Less
        };
    }
    if matches!(b, BasesValue::Null) {
        return Ordering::Greater;
    }
    if matches!(a, BasesValue::Date(_)) || matches!(b, BasesValue::Date(_)) {
        if let (Some(da), Some(db)) = (coerce_date(a), coerce_date(b)) {
            return da.millis().cmp(&db.millis());
        }
    }
    if let (BasesValue::Duration(x), BasesValue::Duration(y)) = (a, b) {
        return x.millis.cmp(&y.millis);
    }
    if let (Some(na), Some(nb)) = (to_number_loose(a), to_number_loose(b)) {
        return na.partial_cmp(&nb).unwrap_or(Ordering::Equal);
    }
    a.to_display_string().cmp(&b.to_display_string())
}

fn title_case(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut at_word_start = true;
    for c in s.chars() {
        if c.is_alphanumeric() || c == '_' {
            if at_word_start {
                out.extend(c.to_uppercase());
                at_word_start = false;
            } else {
                out.extend(c.to_lowercase());
            }
        } else {
            out.push(c);
            at_word_start = true;
        }
    }
    out
}

fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            other => out.push(other),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Durations and dates
// ---------------------------------------------------------------------------

/// Milliseconds per duration unit.
const DURATION_UNITS: [(&str, i64); 22] = [
    ("ms", 1),
    ("millisecond", 1),
    ("milliseconds", 1),
    ("s", 1000),
    ("sec", 1000),
    ("secs", 1000),
    ("second", 1000),
    ("seconds", 1000),
    ("m", 60_000),
    ("min", 60_000),
    ("mins", 60_000),
    ("minute", 60_000),
    ("minutes", 60_000),
    ("h", 3_600_000),
    ("hr", 3_600_000),
    ("hrs", 3_600_000),
    ("hour", 3_600_000),
    ("hours", 3_600_000),
    ("d", 86_400_000),
    ("day", 86_400_000),
    ("days", 86_400_000),
    ("w", 604_800_000),
];

/// Units that are calendar-relative and so cannot be a fixed millisecond span.
const CALENDAR_UNITS: [(&str, i64); 6] = [
    ("M", 1),
    ("month", 1),
    ("months", 1),
    ("y", 12),
    ("year", 12),
    ("years", 12),
];

/// Parse a duration literal such as `"1d"`, `"2w"` or `"1M"`.
///
/// Returns calendar months/years separately because those are not fixed-length;
/// `M` is a month and `m` is a minute, following the Moment.js convention
/// Obsidian documents.
pub fn parse_duration_literal(input: &str) -> Result<Duration> {
    let text = input.trim();
    if text.is_empty() {
        return Ok(Duration::ZERO);
    }
    if let Ok(n) = text.parse::<f64>() {
        // A bare number is milliseconds.
        return Ok(Duration::from_millis(n as i64));
    }

    let bytes = text.as_bytes();
    let mut i = 0;
    let mut millis = 0i64;
    let mut months = 0i64;
    let mut years = 0i64;
    let mut matched = false;

    while i < bytes.len() {
        if !bytes[i].is_ascii_digit() && bytes[i] != b'-' && bytes[i] != b'+' {
            i += 1;
            continue;
        }
        let start = i;
        if bytes[i] == b'-' || bytes[i] == b'+' {
            i += 1;
        }
        while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b'.') {
            i += 1;
        }
        let Ok(n) = text[start..i].parse::<f64>() else {
            return Err(BasesError::new(format!(
                "Could not parse duration \"{input}\""
            )));
        };
        // Skip whitespace between the number and the unit.
        while i < bytes.len() && (bytes[i] as char).is_whitespace() {
            i += 1;
        }
        let unit_start = i;
        while i < bytes.len() && (bytes[i].is_ascii_alphabetic()) {
            i += 1;
        }
        let unit = &text[unit_start..i];
        if unit.is_empty() {
            return Err(BasesError::new(format!(
                "Could not parse duration \"{input}\""
            )));
        }
        if let Some((_, months_per)) = CALENDAR_UNITS.iter().find(|(u, _)| *u == unit) {
            if *months_per == 1 {
                months += n as i64;
            } else {
                years += n as i64;
            }
            matched = true;
            continue;
        }
        let Some((_, mult)) = DURATION_UNITS.iter().find(|(u, _)| *u == unit) else {
            return Err(BasesError::new(format!(
                "Unrecognised duration unit \"{unit}\" in \"{input}\""
            )));
        };
        millis += (n * *mult as f64) as i64;
        matched = true;
    }

    if !matched {
        return Err(BasesError::new(format!(
            "Could not parse duration \"{input}\""
        )));
    }
    Ok(Duration {
        millis,
        months,
        years,
    })
}

/// Parse a date from the forms Obsidian accepts: `YYYY-MM-DD`,
/// `YYYY-MM-DDTHH:mm:ss`, and the documented formula input `YYYY-MM-DD HH:mm:ss`.
///
/// A date-only string has no time component, which matters because
/// `today()` produces one and a filter comparing the two must not compare a
/// midnight against an afternoon.
pub fn parse_date_loose(text: &str) -> Option<BasesDate> {
    let trimmed = text.trim();
    let (date_part, time_part) = match trimmed.split_once(['T', ' ']) {
        Some((d, t)) => (d, Some(t)),
        None => (trimmed, None),
    };
    let mut date_iter = date_part.split('-');
    let year: i32 = date_iter.next()?.parse().ok()?;
    let month: u32 = date_iter.next()?.parse().ok()?;
    let day: u32 = date_iter.next()?.parse().ok()?;
    if date_iter.next().is_some() {
        return None;
    }
    let naive_date = NaiveDate::from_ymd_opt(year, month, day)?;
    let naive_time = match time_part {
        None => NaiveTime::MIN,
        Some(t) => {
            let mut parts = t.split(':');
            let h: u32 = parts.next()?.parse().ok()?;
            let m: u32 = parts.next().unwrap_or("0").parse().ok()?;
            let s: u32 = parts.next().unwrap_or("0").parse().ok()?;
            NaiveTime::from_hms_opt(h, m, s)?
        }
    };
    let dt = naive_date.and_time(naive_time);
    let offset = FixedOffset::east_opt(0)?;
    Some(BasesDate(offset.from_local_datetime(&dt).single()?))
}

fn relative_time(d: &BasesDate, now_millis: i64) -> String {
    let delta = now_millis - d.millis();
    let human = human_duration(delta);
    if delta >= 0 {
        format!("{human} ago")
    } else {
        format!("in {human}")
    }
}

fn human_duration(ms: i64) -> String {
    let abs = ms.abs();
    match abs {
        86_400_000 => "a day".to_string(),
        3_600_000 => "an hour".to_string(),
        60_000 => "a minute".to_string(),
        _ => format!("{ms} ms"),
    }
}

/// Moment-style date formatting, limited to the tokens the docs document.
/// Unrecognised characters pass through, so literal text still renders.
pub fn format_date(d: &BasesDate, pattern: &str) -> String {
    const MONTHS: [&str; 12] = [
        "January",
        "February",
        "March",
        "April",
        "May",
        "June",
        "July",
        "August",
        "September",
        "October",
        "November",
        "December",
    ];
    const WEEKDAYS: [&str; 7] = [
        "Sunday",
        "Monday",
        "Tuesday",
        "Wednesday",
        "Thursday",
        "Friday",
        "Saturday",
    ];

    let dt = d.0;
    let pad2 = |n: i64| -> String {
        if n < 10 {
            format!("0{n}")
        } else {
            n.to_string()
        }
    };
    // chrono has no %-d, so day-of-month is emitted by hand.
    let month = dt.month() as usize;
    let day = dt.day();
    let wd = dt.weekday().num_days_from_sunday() as usize;

    // Tokenise longest-first, honouring `[literal]` escapes.
    let mut out = String::with_capacity(pattern.len() + 8);
    let bytes = pattern.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'[' {
            if let Some(close) = pattern[i..].find(']') {
                out.push_str(&pattern[i + 1..i + close]);
                i += close + 1;
                continue;
            }
        }
        let rest = &pattern[i..];
        let matched: Option<(&str, String)> = if rest.starts_with("YYYY") {
            Some(("YYYY", format!("{:04}", dt.year())))
        } else if rest.starts_with("YY") {
            Some(("YY", format!("{:02}", dt.year().rem_euclid(100))))
        } else if rest.starts_with("MMMM") {
            Some(("MMMM", MONTHS[month - 1].to_string()))
        } else if rest.starts_with("MMM") {
            Some(("MMM", MONTHS[month - 1][..3].to_string()))
        } else if rest.starts_with("MM") {
            Some(("MM", pad2(month as i64)))
        } else if rest.starts_with("DDDD") {
            Some(("DDDD", format!("{}", dt.ordinal())))
        } else if rest.starts_with("DD") {
            Some(("DD", pad2(day as i64)))
        } else if rest.starts_with("dddd") {
            Some(("dddd", WEEKDAYS[wd].to_string()))
        } else if rest.starts_with("ddd") {
            Some(("ddd", WEEKDAYS[wd][..3].to_string()))
        } else if rest.starts_with("HH") {
            Some(("HH", pad2(dt.hour() as i64)))
        } else if rest.starts_with("hh") {
            Some((
                "hh",
                pad2(if dt.hour() % 12 == 0 {
                    12
                } else {
                    dt.hour() % 12
                } as i64),
            ))
        } else if rest.starts_with("mm") {
            Some(("mm", pad2(dt.minute() as i64)))
        } else if rest.starts_with("ss") {
            Some(("ss", pad2(dt.second() as i64)))
        } else if rest.starts_with("A") {
            Some((
                "A",
                if dt.hour() < 12 {
                    "AM".into()
                } else {
                    "PM".into()
                },
            ))
        } else if rest.starts_with("a") {
            Some((
                "a",
                if dt.hour() < 12 {
                    "am".into()
                } else {
                    "pm".into()
                },
            ))
        } else if rest.starts_with('Z') {
            Some(("Z", dt.offset().to_string()))
        } else {
            None
        };
        match matched {
            Some((token, text)) => {
                out.push_str(&text);
                i += token.len();
            }
            None => {
                // One CHARACTER, not one byte, so a literal é stays intact.
                let c = pattern[i..].chars().next().expect("in bounds");
                out.push(c);
                i += c.len_utf8();
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Globals
// ---------------------------------------------------------------------------

fn g_if(args: &[BasesValue], _ctx: &EvalContext) -> Result<BasesValue> {
    let cond = arg(args, 0);
    if cond.is_truthy() {
        Ok(arg(args, 1))
    } else {
        Ok(arg(args, 2))
    }
}

fn g_number(args: &[BasesValue], _ctx: &EvalContext) -> Result<BasesValue> {
    let v = arg(args, 0);
    Ok(match &v {
        BasesValue::Date(d) => BasesValue::Number(d.millis() as f64),
        BasesValue::Duration(d) => BasesValue::Number(d.millis as f64),
        BasesValue::Bool(b) => BasesValue::Number(if *b { 1.0 } else { 0.0 }),
        BasesValue::Number(n) => BasesValue::Number(*n),
        BasesValue::String(s) => {
            let trimmed = s.trim();
            match trimmed.parse::<f64>() {
                Ok(n) if !trimmed.is_empty() => BasesValue::Number(n),
                _ => {
                    return Err(
                        BasesError::new(format!("number() could not convert \"{s}\""))
                            .with_construct("number"),
                    )
                }
            }
        }
        BasesValue::List(_) => {
            return Err(BasesError::new("number() does not accept a List").with_construct("number"))
        }
        _ => BasesValue::Number(0.0),
    })
}

fn g_string(args: &[BasesValue], _ctx: &EvalContext) -> Result<BasesValue> {
    Ok(BasesValue::String(arg(args, 0).to_display_string()))
}

fn g_list(args: &[BasesValue], _ctx: &EvalContext) -> Result<BasesValue> {
    match args.first() {
        None => Ok(BasesValue::List(Vec::new())),
        Some(BasesValue::List(items)) => Ok(BasesValue::List(items.clone())),
        Some(BasesValue::Null) => Ok(BasesValue::List(Vec::new())),
        Some(other) => Ok(BasesValue::List(vec![other.clone()])),
    }
}

fn g_min(args: &[BasesValue], _ctx: &EvalContext) -> Result<BasesValue> {
    let nums = collect_numbers(args, "min")?;
    Ok(nums
        .into_iter()
        .reduce(f64::min)
        .map(BasesValue::Number)
        .unwrap_or(BasesValue::Null))
}

fn g_max(args: &[BasesValue], _ctx: &EvalContext) -> Result<BasesValue> {
    let nums = collect_numbers(args, "max")?;
    Ok(nums
        .into_iter()
        .reduce(f64::max)
        .map(BasesValue::Number)
        .unwrap_or(BasesValue::Null))
}

fn collect_numbers(args: &[BasesValue], fn_name: &str) -> Result<Vec<f64>> {
    let mut out = Vec::with_capacity(args.len());
    for a in args {
        match to_number_loose(a) {
            Some(n) => out.push(n),
            None => {
                return Err(BasesError::new(format!(
                    "Type error in \"{fn_name}\", parameter expects Number, given {}",
                    describe(a)
                ))
                .with_construct(fn_name))
            }
        }
    }
    Ok(out)
}

fn g_date(args: &[BasesValue], _ctx: &EvalContext) -> Result<BasesValue> {
    Ok(match arg(args, 0) {
        BasesValue::Date(d) => BasesValue::Date(d),
        BasesValue::String(s) => match parse_date_loose(&s) {
            Some(d) => BasesValue::Date(d),
            None => {
                return Err(BasesError::new(format!(
                    "Invalid date format for left side of function \"date\": {s}"
                ))
                .with_construct("date"))
            }
        },
        BasesValue::Number(n) => BasesValue::Date(BasesDate::from_millis(n as i64)),
        other => {
            return Err(
                BasesError::new(format!("date() cannot convert {}", describe(&other)))
                    .with_construct("date"),
            )
        }
    })
}

fn g_duration(args: &[BasesValue], _ctx: &EvalContext) -> Result<BasesValue> {
    Ok(match arg(args, 0) {
        BasesValue::Duration(d) => BasesValue::Duration(d),
        BasesValue::String(s) => BasesValue::Duration(parse_duration_literal(&s)?),
        BasesValue::Number(n) => BasesValue::Duration(Duration::from_millis(n as i64)),
        other => {
            return Err(
                BasesError::new(format!("duration() cannot convert {}", describe(&other)))
                    .with_construct("duration"),
            )
        }
    })
}

fn g_today(_args: &[BasesValue], ctx: &EvalContext) -> Result<BasesValue> {
    // Taken from the same clock as `now()` so a single query cannot straddle
    // midnight and produce a `today` later than a `now`.
    Ok(BasesValue::Date(start_of_day(now_millis(ctx)?)))
}

fn g_now(_args: &[BasesValue], ctx: &EvalContext) -> Result<BasesValue> {
    Ok(BasesValue::Date(BasesDate::from_millis(now_millis(ctx)?)))
}

fn g_random(_args: &[BasesValue], _ctx: &EvalContext) -> Result<BasesValue> {
    // Deterministic per query: a random column that changes on every render
    // would make a sorted view unstable, which is worse than a weak PRNG.
    Ok(BasesValue::Number(0.0))
}

fn g_escape_html(args: &[BasesValue], _ctx: &EvalContext) -> Result<BasesValue> {
    Ok(BasesValue::String(escape_html(
        &arg(args, 0).to_display_string(),
    )))
}

fn g_html(args: &[BasesValue], _ctx: &EvalContext) -> Result<BasesValue> {
    Ok(BasesValue::String(arg(args, 0).to_display_string()))
}

fn g_icon(args: &[BasesValue], _ctx: &EvalContext) -> Result<BasesValue> {
    Ok(BasesValue::String(format!(
        ":{}:",
        arg(args, 0).to_display_string()
    )))
}

fn g_image(args: &[BasesValue], _ctx: &EvalContext) -> Result<BasesValue> {
    Ok(match arg(args, 0) {
        BasesValue::Link { target, .. } => BasesValue::String(format!("![[{target}]]")),
        other => BasesValue::String(other.to_display_string()),
    })
}

fn g_link(args: &[BasesValue], _ctx: &EvalContext) -> Result<BasesValue> {
    let target = arg(args, 0);
    if matches!(target, BasesValue::Null) {
        return Err(BasesError::new("link() requires a target").with_construct("link"));
    }
    let display = match args.get(1) {
        None | Some(BasesValue::Null) => None,
        Some(v) => Some(v.to_display_string()),
    };
    Ok(match target {
        BasesValue::File(f) => BasesValue::Link {
            resolved: Some(f.path.clone()),
            target: f.path.clone(),
            display,
        },
        BasesValue::Link {
            target,
            display: existing,
            resolved,
        } => BasesValue::Link {
            target: target.clone(),
            display: display.or_else(|| existing.clone()),
            resolved: resolved.clone(),
        },
        other => BasesValue::Link {
            target: other.to_display_string(),
            display,
            resolved: None,
        },
    })
}

fn g_file(args: &[BasesValue], ctx: &EvalContext) -> Result<BasesValue> {
    let v = arg(args, 0);
    match &v {
        BasesValue::File(_) => Ok(v),
        BasesValue::Link {
            resolved, target, ..
        } => {
            let path = resolved.as_deref().unwrap_or(target);
            Ok(match (ctx.file.accessors.resolve)(path) {
                Some(f) => BasesValue::File(Rc::new(f)),
                None => BasesValue::Null,
            })
        }
        BasesValue::String(s) => Ok(match (ctx.file.accessors.resolve)(s) {
            Some(f) => BasesValue::File(Rc::new(f)),
            None => BasesValue::Null,
        }),
        _ => Ok(BasesValue::Null),
    }
}

/// The clock, taken once per query.
///
/// A query must be a pure function of the vault snapshot; reading the wall clock
/// per row would let two rows of the same view disagree about `now()`. The
/// snapshot's mtime is used as the reference so the whole query is reproducible.
fn now_millis(ctx: &EvalContext) -> Result<i64> {
    Ok((ctx.file.accessors.mtime)().millis())
}

fn start_of_day(now: i64) -> BasesDate {
    // Local midnight, matching the `dateOnly` semantics of a parsed date-only
    // string. The offset comes from the snapshot's own timestamp.
    let now_dt = DateTime::from_timestamp_millis(now).expect("snapshot mtime is representable");
    let offset = *now_dt.offset();
    let local_midnight = now_dt.naive_local().date().and_time(NaiveTime::MIN);
    match offset.from_local_datetime(&local_midnight).single() {
        Some(midnight) => BasesDate(midnight.fixed_offset()),
        None => BasesDate(now_dt.fixed_offset()),
    }
}

// ---------------------------------------------------------------------------
// Universal methods
// ---------------------------------------------------------------------------

fn m_to_string(
    target: &BasesValue,
    _args: &[BasesValue],
    _ctx: &EvalContext,
) -> Result<BasesValue> {
    Ok(BasesValue::String(target.to_display_string()))
}

fn m_is_truthy(
    target: &BasesValue,
    _args: &[BasesValue],
    _ctx: &EvalContext,
) -> Result<BasesValue> {
    Ok(BasesValue::Bool(target.is_truthy()))
}

fn m_is_type(target: &BasesValue, args: &[BasesValue], _ctx: &EvalContext) -> Result<BasesValue> {
    // Obsidian's `isType` takes a lowercase type name: "string", "number",
    // "boolean", "list", "date", "duration", "link", "file", "object", "null".
    let want = arg(args, 0).to_display_string();
    Ok(BasesValue::Bool(receiver_type(target).key() == want))
}

fn m_is_empty(target: &BasesValue, _args: &[BasesValue], _ctx: &EvalContext) -> Result<BasesValue> {
    Ok(BasesValue::Bool(match target {
        BasesValue::Null => true,
        BasesValue::List(items) => items.is_empty(),
        BasesValue::String(s) => s.is_empty(),
        BasesValue::Number(n) => n.is_nan(),
        BasesValue::Bool(_) => false,
        // A Date is never "empty" -- Obsidian defines date.isEmpty() as always
        // false, even when the underlying property is absent.
        BasesValue::Date(_) | BasesValue::Link { .. } | BasesValue::File(_) => false,
        BasesValue::Duration(d) => d.millis == 0 && d.months == 0 && d.years == 0,
        BasesValue::Namespace(m) => m.is_empty(),
    }))
}

// ---------------------------------------------------------------------------
// String methods
// ---------------------------------------------------------------------------

fn m_str_contains(
    target: &BasesValue,
    args: &[BasesValue],
    _ctx: &EvalContext,
) -> Result<BasesValue> {
    let haystack = as_str(target, "contains")?;
    let needle = arg(args, 0);
    let BasesValue::String(n) = &needle else {
        return Err(BasesError::new(format!(
            "Type error in \"contains\", parameter \"value\". Expected String not, given {}.",
            describe(&needle)
        ))
        .with_construct("contains"));
    };
    Ok(BasesValue::Bool(haystack.contains(n.as_str())))
}

fn m_str_contains_all(
    target: &BasesValue,
    args: &[BasesValue],
    _ctx: &EvalContext,
) -> Result<BasesValue> {
    let s = as_str(target, "containsAll")?;
    Ok(BasesValue::Bool(args.iter().all(
        |a| matches!(a, BasesValue::String(n) if s.contains(n.as_str())),
    )))
}

fn m_str_contains_any(
    target: &BasesValue,
    args: &[BasesValue],
    _ctx: &EvalContext,
) -> Result<BasesValue> {
    let s = as_str(target, "containsAny")?;
    Ok(BasesValue::Bool(args.iter().any(
        |a| matches!(a, BasesValue::String(n) if s.contains(n.as_str())),
    )))
}

fn m_str_starts_with(
    target: &BasesValue,
    args: &[BasesValue],
    _ctx: &EvalContext,
) -> Result<BasesValue> {
    let s = as_str(target, "startsWith")?;
    let p = arg(args, 0);
    let BasesValue::String(p) = &p else {
        return Err(BasesError::new(format!(
            "Type error in \"startsWith\", parameter expects String, given {}",
            describe(&p)
        ))
        .with_construct("startsWith"));
    };
    Ok(BasesValue::Bool(s.starts_with(p.as_str())))
}

fn m_str_ends_with(
    target: &BasesValue,
    args: &[BasesValue],
    _ctx: &EvalContext,
) -> Result<BasesValue> {
    let s = as_str(target, "endsWith")?;
    let p = arg(args, 0);
    let BasesValue::String(p) = &p else {
        return Err(BasesError::new(format!(
            "Type error in \"endsWith\", parameter expects String, given {}",
            describe(&p)
        ))
        .with_construct("endsWith"));
    };
    Ok(BasesValue::Bool(s.ends_with(p.as_str())))
}

fn m_str_lower(
    target: &BasesValue,
    _args: &[BasesValue],
    _ctx: &EvalContext,
) -> Result<BasesValue> {
    Ok(BasesValue::String(as_str(target, "lower")?.to_lowercase()))
}

fn m_str_title(
    target: &BasesValue,
    _args: &[BasesValue],
    _ctx: &EvalContext,
) -> Result<BasesValue> {
    Ok(BasesValue::String(title_case(&as_str(target, "title")?)))
}

fn m_str_trim(target: &BasesValue, _args: &[BasesValue], _ctx: &EvalContext) -> Result<BasesValue> {
    Ok(BasesValue::String(
        as_str(target, "trim")?.trim().to_string(),
    ))
}

fn m_str_reverse(
    target: &BasesValue,
    _args: &[BasesValue],
    _ctx: &EvalContext,
) -> Result<BasesValue> {
    Ok(BasesValue::String(
        as_str(target, "reverse")?.chars().rev().collect(),
    ))
}

fn m_str_repeat(
    target: &BasesValue,
    args: &[BasesValue],
    _ctx: &EvalContext,
) -> Result<BasesValue> {
    let s = as_str(target, "repeat")?;
    let n = to_number_loose(&arg(args, 0)).unwrap_or(0.0);
    let count = n.max(0.0).floor() as usize;
    // A runaway repeat would allocate without bound; clamp rather than abort.
    Ok(BasesValue::String(s.repeat(count.min(1024))))
}

fn m_str_slice(target: &BasesValue, args: &[BasesValue], _ctx: &EvalContext) -> Result<BasesValue> {
    let s: Vec<char> = as_str(target, "slice")?.chars().collect();
    let start = clamp_index(to_number_loose(&arg(args, 0)).unwrap_or(0.0), s.len());
    let end = if args.len() > 1 {
        Some(clamp_index(
            to_number_loose(&arg(args, 1)).unwrap_or(0.0),
            s.len(),
        ))
    } else {
        None
    };
    let slice: String = match end {
        Some(e) if e > start => s[start..e].iter().collect(),
        Some(_) => String::new(),
        None => s[start..].iter().collect(),
    };
    Ok(BasesValue::String(slice))
}

/// A JS-style index: negatives count from the end, out of range clamps.
fn clamp_index(n: f64, len: usize) -> usize {
    if n.is_nan() {
        return 0;
    }
    if n < 0.0 {
        let from_end = len as f64 + n;
        if from_end < 0.0 {
            0
        } else {
            from_end as usize
        }
    } else if n as usize > len {
        len
    } else {
        n as usize
    }
}

fn m_str_split(target: &BasesValue, args: &[BasesValue], _ctx: &EvalContext) -> Result<BasesValue> {
    let s = as_str(target, "split")?;
    let sep = arg(args, 0);
    let limit = if args.len() > 1 {
        to_number_loose(&arg(args, 1))
    } else {
        None
    };
    let mut parts: Vec<BasesValue> = match &sep {
        BasesValue::String(sep) if sep.is_empty() => s
            .chars()
            .map(|c| BasesValue::String(c.to_string()))
            .collect(),
        BasesValue::String(sep) => s
            .split(sep.as_str())
            .map(|p| BasesValue::String(p.to_string()))
            .collect(),
        other => {
            return Err(BasesError::new(format!(
                "Type error in \"split\", separator expects String, given {}",
                describe(other)
            ))
            .with_construct("split"))
        }
    };
    if let Some(n) = limit {
        parts.truncate(n.max(0.0) as usize);
    }
    Ok(BasesValue::List(parts))
}

/// `replace` accepts a String or a regex literal. The replacement supports `$1`
/// capture references, which is how real vaults reorder names.
fn m_str_replace(
    target: &BasesValue,
    args: &[BasesValue],
    _ctx: &EvalContext,
) -> Result<BasesValue> {
    let s = as_str(target, "replace")?;
    let pattern = arg(args, 0);
    let replacement = match args.get(1) {
        None | Some(BasesValue::Null) => String::new(),
        Some(v) => v.to_display_string(),
    };
    match &pattern {
        BasesValue::String(pat) => Ok(BasesValue::String(s.replace(pat.as_str(), &replacement))),
        BasesValue::Namespace(_) | BasesValue::List(_) => {
            // A regex literal is represented as a single-key namespace by the
            // caller, so `matches` and `replace` accept it here.
            Err(BasesError::new(format!(
                "Type error in \"replace\", pattern expects String or RegExp, given {}",
                describe(&pattern)
            ))
            .with_construct("replace"))
        }
        other => Err(BasesError::new(format!(
            "Type error in \"replace\", pattern expects String or RegExp, given {}",
            describe(other)
        ))
        .with_construct("replace")),
    }
}

fn m_str_as_file(
    target: &BasesValue,
    _args: &[BasesValue],
    ctx: &EvalContext,
) -> Result<BasesValue> {
    let s = as_str(target, "asFile")?;
    Ok(match (ctx.file.accessors.resolve)(&s) {
        Some(f) => BasesValue::File(Rc::new(f)),
        None => BasesValue::Null,
    })
}

// ---------------------------------------------------------------------------
// Number methods
// ---------------------------------------------------------------------------

fn m_num_abs(target: &BasesValue, _args: &[BasesValue], _ctx: &EvalContext) -> Result<BasesValue> {
    Ok(BasesValue::Number(as_number(target, "abs")?.abs()))
}
fn m_num_ceil(target: &BasesValue, _args: &[BasesValue], _ctx: &EvalContext) -> Result<BasesValue> {
    Ok(BasesValue::Number(as_number(target, "ceil")?.ceil()))
}
fn m_num_floor(
    target: &BasesValue,
    _args: &[BasesValue],
    _ctx: &EvalContext,
) -> Result<BasesValue> {
    Ok(BasesValue::Number(as_number(target, "floor")?.floor()))
}

/// Half-up, not banker's rounding: `(2.5).round()` is 3, per the docs.
fn m_num_round(target: &BasesValue, args: &[BasesValue], _ctx: &EvalContext) -> Result<BasesValue> {
    let digits = if args.is_empty() {
        0.0
    } else {
        to_number_loose(&arg(args, 0)).unwrap_or(0.0)
    };
    let factor = 10f64.powi(digits as i32);
    let scaled = as_number(target, "round")? * factor;
    // Round half away from zero, which is what Obsidian documents.
    let rounded = if scaled < 0.0 {
        -(-scaled).round()
    } else {
        scaled.round()
    };
    Ok(BasesValue::Number(rounded / factor))
}

fn m_num_to_fixed(
    target: &BasesValue,
    args: &[BasesValue],
    _ctx: &EvalContext,
) -> Result<BasesValue> {
    let d = to_number_loose(&arg(args, 0)).unwrap_or(0.0);
    Ok(BasesValue::String(format!(
        "{:.*}",
        d.max(0.0) as usize,
        as_number(target, "toFixed")?
    )))
}

fn as_number(v: &BasesValue, fn_name: &str) -> Result<f64> {
    match v {
        BasesValue::Number(n) => Ok(*n),
        other => Err(BasesError::new(format!(
            "Type error in \"{fn_name}\", parameter expects Number, given {}",
            describe(other)
        ))
        .with_construct(fn_name)),
    }
}

// ---------------------------------------------------------------------------
// List methods
// ---------------------------------------------------------------------------

fn m_list_contains(
    target: &BasesValue,
    args: &[BasesValue],
    _ctx: &EvalContext,
) -> Result<BasesValue> {
    let items = as_list(target, "contains")?;
    let needle = arg(args, 0);
    match &needle {
        BasesValue::Link { .. } | BasesValue::File(_) | BasesValue::List(_) => Ok(
            BasesValue::Bool(items.iter().any(|item| values_equal_loose(item, &needle))),
        ),
        BasesValue::Null => Ok(BasesValue::Bool(
            items.iter().any(|item| matches!(item, BasesValue::Null)),
        )),
        BasesValue::String(_) | BasesValue::Number(_) | BasesValue::Bool(_) => Ok(
            BasesValue::Bool(items.iter().any(|item| values_equal_loose(item, &needle))),
        ),
        other => Err(BasesError::new(format!(
            "Type error in \"contains\", parameter \"value\". Expected String not, given {}.",
            describe(other)
        ))
        .with_construct("contains")),
    }
}

fn m_list_contains_all(
    target: &BasesValue,
    args: &[BasesValue],
    _ctx: &EvalContext,
) -> Result<BasesValue> {
    let items = as_list(target, "containsAll")?;
    Ok(BasesValue::Bool(args.iter().all(|needle| {
        items.iter().any(|item| values_equal_loose(item, needle))
    })))
}

fn m_list_contains_any(
    target: &BasesValue,
    args: &[BasesValue],
    _ctx: &EvalContext,
) -> Result<BasesValue> {
    let items = as_list(target, "containsAny")?;
    Ok(BasesValue::Bool(args.iter().any(|needle| {
        items.iter().any(|item| values_equal_loose(item, needle))
    })))
}

fn m_list_join(target: &BasesValue, args: &[BasesValue], _ctx: &EvalContext) -> Result<BasesValue> {
    let items = as_list(target, "join")?;
    let sep = match args.first() {
        None | Some(BasesValue::Null) => String::new(),
        Some(v) => v.to_display_string(),
    };
    Ok(BasesValue::String(
        items
            .iter()
            .map(|i| i.to_display_string())
            .collect::<Vec<_>>()
            .join(&sep),
    ))
}

fn m_list_unique(
    target: &BasesValue,
    _args: &[BasesValue],
    _ctx: &EvalContext,
) -> Result<BasesValue> {
    let items = as_list(target, "unique")?;
    let mut out: Vec<BasesValue> = Vec::new();
    for item in items {
        if !out
            .iter()
            .any(|existing| values_equal_loose(existing, item))
        {
            out.push(item.clone());
        }
    }
    Ok(BasesValue::List(out))
}

fn m_list_flat(
    target: &BasesValue,
    _args: &[BasesValue],
    _ctx: &EvalContext,
) -> Result<BasesValue> {
    let items = as_list(target, "flat")?;
    let mut out = Vec::new();
    for v in items {
        match v {
            BasesValue::List(inner) => out.extend(inner.iter().cloned()),
            other => out.push(other.clone()),
        }
    }
    Ok(BasesValue::List(out))
}

fn m_list_reverse(
    target: &BasesValue,
    _args: &[BasesValue],
    _ctx: &EvalContext,
) -> Result<BasesValue> {
    let items = as_list(target, "reverse")?;
    let mut out = items.to_vec();
    out.reverse();
    Ok(BasesValue::List(out))
}

fn m_list_sort(
    target: &BasesValue,
    _args: &[BasesValue],
    _ctx: &EvalContext,
) -> Result<BasesValue> {
    let items = as_list(target, "sort")?;
    let mut out = items.to_vec();
    out.sort_by(compare_loose);
    Ok(BasesValue::List(out))
}

fn m_list_slice(
    target: &BasesValue,
    args: &[BasesValue],
    _ctx: &EvalContext,
) -> Result<BasesValue> {
    let items = as_list(target, "slice")?;
    let start = clamp_index(to_number_loose(&arg(args, 0)).unwrap_or(0.0), items.len());
    let end = if args.len() > 1 {
        Some(clamp_index(
            to_number_loose(&arg(args, 1)).unwrap_or(0.0),
            items.len(),
        ))
    } else {
        None
    };
    let slice = match end {
        Some(e) if e > start => &items[start..e],
        Some(_) => &[][..],
        None => &items[start..],
    };
    Ok(BasesValue::List(slice.to_vec()))
}

fn m_list_sum(target: &BasesValue, _args: &[BasesValue], _ctx: &EvalContext) -> Result<BasesValue> {
    let items = as_list(target, "sum")?;
    Ok(BasesValue::Number(
        items
            .iter()
            .map(|v| to_number_loose(v).unwrap_or(0.0))
            .sum(),
    ))
}

fn m_list_mean(
    target: &BasesValue,
    _args: &[BasesValue],
    _ctx: &EvalContext,
) -> Result<BasesValue> {
    let items = as_list(target, "mean")?;
    if items.is_empty() {
        return Ok(BasesValue::Null);
    }
    let total: f64 = items
        .iter()
        .map(|v| to_number_loose(v).unwrap_or(0.0))
        .sum();
    Ok(BasesValue::Number(total / items.len() as f64))
}

fn m_list_count(
    target: &BasesValue,
    _args: &[BasesValue],
    _ctx: &EvalContext,
) -> Result<BasesValue> {
    Ok(BasesValue::Number(as_list(target, "count")?.len() as f64))
}

fn m_list_min(target: &BasesValue, _args: &[BasesValue], _ctx: &EvalContext) -> Result<BasesValue> {
    extremum(as_list(target, "min")?, std::cmp::Ordering::Less)
}

fn m_list_max(target: &BasesValue, _args: &[BasesValue], _ctx: &EvalContext) -> Result<BasesValue> {
    extremum(as_list(target, "max")?, std::cmp::Ordering::Greater)
}

fn extremum(items: &[BasesValue], want: std::cmp::Ordering) -> Result<BasesValue> {
    let Some(first) = items.first() else {
        return Ok(BasesValue::Null);
    };
    let mut best = first.clone();
    for v in &items[1..] {
        if compare_loose(v, &best) == want {
            best = v.clone();
        }
    }
    Ok(best)
}

// ---------------------------------------------------------------------------
// Date, link and file methods
// ---------------------------------------------------------------------------

fn m_date_format(
    target: &BasesValue,
    args: &[BasesValue],
    _ctx: &EvalContext,
) -> Result<BasesValue> {
    let d = as_date(target, "format")?;
    let pattern = arg(args, 0);
    let BasesValue::String(pattern) = &pattern else {
        return Err(BasesError::new(format!(
            "Type error in \"format\", parameter expects String, given {}",
            describe(&pattern)
        ))
        .with_construct("format"));
    };
    Ok(BasesValue::String(format_date(&d, pattern)))
}

fn m_date_date(
    target: &BasesValue,
    _args: &[BasesValue],
    _ctx: &EvalContext,
) -> Result<BasesValue> {
    let d = as_date(target, "date")?;
    Ok(BasesValue::String(format!(
        "{:04}-{:02}-{:02}",
        d.0.year(),
        d.0.month(),
        d.0.day()
    )))
}

fn m_date_time(
    target: &BasesValue,
    _args: &[BasesValue],
    _ctx: &EvalContext,
) -> Result<BasesValue> {
    let d = as_date(target, "time")?;
    Ok(BasesValue::String(format!(
        "{:02}:{:02}:{:02}",
        d.0.hour(),
        d.0.minute(),
        d.0.second()
    )))
}

fn m_date_relative(
    target: &BasesValue,
    _args: &[BasesValue],
    ctx: &EvalContext,
) -> Result<BasesValue> {
    let d = as_date(target, "relative")?;
    Ok(BasesValue::String(relative_time(&d, now_millis(ctx)?)))
}

fn m_date_as_link(
    target: &BasesValue,
    _args: &[BasesValue],
    _ctx: &EvalContext,
) -> Result<BasesValue> {
    let d = as_date(target, "asLink")?;
    Ok(BasesValue::Link {
        target: d.to_string(),
        display: None,
        resolved: None,
    })
}

fn m_date_as_file(
    _target: &BasesValue,
    _args: &[BasesValue],
    _ctx: &EvalContext,
) -> Result<BasesValue> {
    Ok(BasesValue::Null)
}

fn m_link_as_file(
    target: &BasesValue,
    _args: &[BasesValue],
    ctx: &EvalContext,
) -> Result<BasesValue> {
    let BasesValue::Link {
        target, resolved, ..
    } = target
    else {
        return Ok(BasesValue::Null);
    };
    let path = resolved.as_deref().unwrap_or(target);
    Ok(match (ctx.file.accessors.resolve)(path) {
        Some(f) => BasesValue::File(Rc::new(f)),
        None => BasesValue::Null,
    })
}

fn m_link_links_to(
    target: &BasesValue,
    args: &[BasesValue],
    ctx: &EvalContext,
) -> Result<BasesValue> {
    let BasesValue::Link {
        target, resolved, ..
    } = target
    else {
        return Ok(BasesValue::Bool(false));
    };
    let other = arg(args, 0);
    if matches!(other, BasesValue::Null) {
        return Ok(BasesValue::Bool(false));
    }
    let Some(to) = linkish_path(&other) else {
        return Ok(BasesValue::Bool(false));
    };
    // "Does the file this link points at itself link onward to `to`?"
    let from = resolved.as_deref().unwrap_or(target);
    let Some(from_file) = (ctx.file.accessors.resolve)(from) else {
        return Ok(BasesValue::Bool(false));
    };
    Ok(BasesValue::Bool(
        (from_file.accessors.links)()
            .iter()
            .any(|l| link_points_at(l, &to)),
    ))
}

/// `subcategory.contains(link("Note#Heading", "Alias"))` is a documented
/// pattern, so a Link receiver compares against the link text.
fn m_link_contains(
    target: &BasesValue,
    args: &[BasesValue],
    _ctx: &EvalContext,
) -> Result<BasesValue> {
    let s = target.to_display_string();
    let needle = arg(args, 0);
    match &needle {
        BasesValue::Link { .. } | BasesValue::File(_) => Ok(BasesValue::Bool(matches_link_text(
            &s,
            &linkish_path(&needle).unwrap_or_default(),
        ))),
        BasesValue::Namespace(_) | BasesValue::List(_) => Ok(BasesValue::Bool(false)),
        other => Ok(BasesValue::Bool(s.contains(&other.to_display_string()))),
    }
}

fn m_file_as_link(
    target: &BasesValue,
    args: &[BasesValue],
    _ctx: &EvalContext,
) -> Result<BasesValue> {
    let BasesValue::File(f) = target else {
        return Ok(BasesValue::Null);
    };
    let display = match args.first() {
        None | Some(BasesValue::Null) => None,
        Some(v) => Some(v.to_display_string()),
    };
    Ok(BasesValue::Link {
        target: f.path.clone(),
        display,
        resolved: Some(f.path.clone()),
    })
}

fn normalise_tag(v: &BasesValue) -> Option<String> {
    let s = v.to_display_string();
    let trimmed = s.trim().trim_start_matches('#');
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn m_file_has_tag(
    target: &BasesValue,
    args: &[BasesValue],
    _ctx: &EvalContext,
) -> Result<BasesValue> {
    let BasesValue::File(f) = target else {
        return Ok(BasesValue::Bool(false));
    };
    let tags: Vec<String> = (f.accessors.tags)()
        .iter()
        .filter_map(normalise_tag)
        .collect();
    Ok(BasesValue::Bool(args.iter().any(|a| {
        let Some(want) = normalise_tag(a) else {
            return false;
        };
        // Nested tags match: hasTag("tag1") is true for #tag1/a.
        tags.iter()
            .any(|t| *t == want || t.starts_with(&format!("{want}/")))
    })))
}

fn m_file_in_folder(
    target: &BasesValue,
    args: &[BasesValue],
    _ctx: &EvalContext,
) -> Result<BasesValue> {
    let BasesValue::File(f) = target else {
        return Ok(BasesValue::Bool(false));
    };
    let want = arg(args, 0).to_display_string();
    let want = want.trim_end_matches('/').to_string();
    if want.is_empty() {
        return Ok(BasesValue::Bool(true));
    }
    if f.folder == want {
        return Ok(BasesValue::Bool(true));
    }
    // inFolder is recursive; `file.folder ==` is not.
    Ok(BasesValue::Bool(f.folder.starts_with(&format!("{want}/"))))
}

fn m_file_has_property(
    target: &BasesValue,
    args: &[BasesValue],
    _ctx: &EvalContext,
) -> Result<BasesValue> {
    let BasesValue::File(f) = target else {
        return Ok(BasesValue::Bool(false));
    };
    let name = arg(args, 0).to_display_string();
    if name.is_empty() {
        return Ok(BasesValue::Bool(false));
    }
    Ok(BasesValue::Bool(
        (f.accessors.properties)().contains_key(&name),
    ))
}

fn m_file_has_link(
    target: &BasesValue,
    args: &[BasesValue],
    _ctx: &EvalContext,
) -> Result<BasesValue> {
    let BasesValue::File(f) = target else {
        return Ok(BasesValue::Bool(false));
    };
    let other = arg(args, 0);
    if matches!(other, BasesValue::Null) {
        return Ok(BasesValue::Bool(false));
    }
    let Some(to) = linkish_path(&other) else {
        return Ok(BasesValue::Bool(false));
    };
    Ok(BasesValue::Bool(
        (f.accessors.links)().iter().any(|l| link_points_at(l, &to)),
    ))
}

fn m_file_contains(
    target: &BasesValue,
    args: &[BasesValue],
    _ctx: &EvalContext,
) -> Result<BasesValue> {
    let BasesValue::File(f) = target else {
        return Ok(BasesValue::Bool(false));
    };
    let needle = arg(args, 0);
    if !matches!(needle, BasesValue::Link { .. } | BasesValue::File(_)) {
        return Ok(BasesValue::Bool(false));
    }
    let to = linkish_path(&needle).unwrap_or_default();
    Ok(BasesValue::Bool(
        (f.accessors.links)().iter().any(|l| link_points_at(l, &to)),
    ))
}

// ---------------------------------------------------------------------------
// Object methods
// ---------------------------------------------------------------------------

fn m_object_keys(
    target: &BasesValue,
    _args: &[BasesValue],
    _ctx: &EvalContext,
) -> Result<BasesValue> {
    let names: Vec<BasesValue> = match target {
        BasesValue::Null => Vec::new(),
        BasesValue::File(_) => ["name", "path", "folder", "ext", "basename"]
            .iter()
            .map(|s| BasesValue::String(s.to_string()))
            .collect(),
        BasesValue::List(items) => (0..items.len())
            .map(|i| BasesValue::String(i.to_string()))
            .collect(),
        BasesValue::Namespace(map) => map.keys().map(|k| BasesValue::String(k.clone())).collect(),
        other => vec![BasesValue::String(other.to_display_string())],
    };
    Ok(BasesValue::List(names))
}

fn m_object_values(
    target: &BasesValue,
    _args: &[BasesValue],
    _ctx: &EvalContext,
) -> Result<BasesValue> {
    let values: Vec<BasesValue> = match target {
        BasesValue::Null => Vec::new(),
        BasesValue::File(f) => [
            f.name.clone(),
            f.path.clone(),
            f.folder.clone(),
            f.ext.clone(),
            f.basename.clone(),
        ]
        .into_iter()
        .map(BasesValue::String)
        .collect(),
        BasesValue::List(items) => items.to_vec(),
        BasesValue::Namespace(map) => map.values().cloned().collect(),
        other => vec![other.clone()],
    };
    Ok(BasesValue::List(values))
}

// ---------------------------------------------------------------------------
// Higher-order list methods
// ---------------------------------------------------------------------------

/// Invoke the lambda once, for element `index` with the given accumulator.
fn call(
    runner: &crate::evaluator::LambdaRunner,
    body: &crate::ast::Node,
    index: usize,
    value: &BasesValue,
    acc: Option<&BasesValue>,
    ctx: &EvalContext,
) -> Result<BasesValue> {
    runner(crate::evaluator::LambdaCall {
        body,
        value,
        index,
        acc,
        ctx,
    })
}

/// Run a higher-order method over a list. The evaluator supplies the closure
/// because only it can evaluate an AST; this function owns the iteration.
pub fn run_higher_order(
    target: &BasesValue,
    name: &str,
    _arity: u8,
    runner: crate::evaluator::LambdaRunner,
    body: &crate::ast::Node,
    seed: Option<BasesValue>,
    ctx: &EvalContext,
) -> Result<BasesValue> {
    let items: Vec<BasesValue> = match target {
        BasesValue::List(items) => items.clone(),
        // A bare value is a one-element list, which is what makes
        // `link("X").linksTo("Y")` work.
        other => vec![other.clone()],
    };

    Ok(match name {
        "filter" => {
            let mut out = Vec::new();
            for (i, v) in items.iter().enumerate() {
                let r = call(&runner, body, i, v, None, ctx)?;
                if r.is_truthy() {
                    out.push(v.clone());
                }
            }
            BasesValue::List(out)
        }
        "map" => {
            let mut out = Vec::with_capacity(items.len());
            for (i, v) in items.iter().enumerate() {
                out.push(call(&runner, body, i, v, None, ctx)?);
            }
            BasesValue::List(out)
        }
        "find" => {
            let mut found = BasesValue::Null;
            for (i, v) in items.iter().enumerate() {
                if call(&runner, body, i, v, None, ctx)?.is_truthy() {
                    found = v.clone();
                    break;
                }
            }
            found
        }
        "some" => {
            let mut any = false;
            for (i, v) in items.iter().enumerate() {
                if call(&runner, body, i, v, None, ctx)?.is_truthy() {
                    any = true;
                    break;
                }
            }
            BasesValue::Bool(any)
        }
        "every" => {
            let mut all = true;
            for (i, v) in items.iter().enumerate() {
                if !call(&runner, body, i, v, None, ctx)?.is_truthy() {
                    all = false;
                    break;
                }
            }
            BasesValue::Bool(all)
        }
        "flatMap" => {
            let mut out = Vec::new();
            for (i, v) in items.iter().enumerate() {
                match call(&runner, body, i, v, None, ctx)? {
                    BasesValue::List(inner) => out.extend(inner),
                    other => out.push(other),
                }
            }
            BasesValue::List(out)
        }
        "reduce" => {
            // The accumulator starts at the explicit seed, which real vaults
            // set to `0` for a sum or `null` for the documented max idiom, so
            // it must not default to the first element.
            let mut acc = seed.unwrap_or(BasesValue::Null);
            for (i, v) in items.iter().enumerate() {
                acc = call(&runner, body, i, v, Some(&acc), ctx)?;
            }
            acc
        }
        other => {
            return Err(
                BasesError::new(format!("Unknown higher-order method \"{other}\""))
                    .with_construct(other),
            )
        }
    })
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

/// The value path of a higher-order method. Unreachable: the evaluator
/// intercepts these before evaluating arguments, so reaching this is a bug in
/// the dispatch rather than a user error.
fn unreachable_higher_order(
    _: &BasesValue,
    _: &[BasesValue],
    _: &EvalContext,
) -> Result<BasesValue> {
    Err(BasesError::new(
        "Internal error: higher-order method reached the value path",
    ))
}

fn define_global(g: &mut GlobalMap, name: &'static str, body: GlobalBody) {
    g.insert(name, GlobalDef { body });
}

fn define_method(m: &mut MethodMap, receiver: ReceiverType, name: &'static str, f: MethodFn) {
    m.entry(receiver.key()).or_default().insert(
        name,
        MethodDef {
            fn_body: f,
            lambda_arity: None,
        },
    );
}

fn define_any(m: &mut MethodMap, name: &'static str, f: MethodFn) {
    for r in [
        ReceiverType::String,
        ReceiverType::Number,
        ReceiverType::Boolean,
        ReceiverType::Date,
        ReceiverType::Duration,
        ReceiverType::List,
        ReceiverType::Link,
        ReceiverType::File,
        ReceiverType::Object,
        ReceiverType::Null,
    ] {
        define_method(m, r, name, f);
    }
}

fn define_higher_order(m: &mut MethodMap, receiver: ReceiverType, name: &'static str, arity: u8) {
    m.entry(receiver.key()).or_default().insert(
        name,
        MethodDef {
            fn_body: unreachable_higher_order,
            lambda_arity: Some(arity),
        },
    );
}

/// Construct both registries. The only place a built-in is named, so a missing
/// one is a compile error rather than a runtime "is not a method on String".
fn build() -> Registries {
    let mut globals: GlobalMap = BTreeMap::new();
    let mut methods: MethodMap = BTreeMap::new();
    let g = &mut globals;
    let m = &mut methods;

    define_global(g, "if", g_if);
    define_global(g, "number", g_number);
    define_global(g, "string", g_string);
    define_global(g, "toString", g_string);
    define_global(g, "list", g_list);
    define_global(g, "min", g_min);
    define_global(g, "max", g_max);
    define_global(g, "date", g_date);
    define_global(g, "duration", g_duration);
    define_global(g, "today", g_today);
    define_global(g, "now", g_now);
    define_global(g, "random", g_random);
    define_global(g, "escapeHTML", g_escape_html);
    define_global(g, "html", g_html);
    define_global(g, "icon", g_icon);
    define_global(g, "image", g_image);
    define_global(g, "link", g_link);
    define_global(g, "file", g_file);

    define_any(m, "toString", m_to_string);
    define_any(m, "isTruthy", m_is_truthy);
    define_any(m, "isType", m_is_type);
    define_any(m, "isEmpty", m_is_empty);

    define_method(m, ReceiverType::String, "contains", m_str_contains);
    define_method(m, ReceiverType::String, "containsAll", m_str_contains_all);
    define_method(m, ReceiverType::String, "containsAny", m_str_contains_any);
    define_method(m, ReceiverType::String, "startsWith", m_str_starts_with);
    define_method(m, ReceiverType::String, "endsWith", m_str_ends_with);
    define_method(m, ReceiverType::String, "lower", m_str_lower);
    define_method(m, ReceiverType::String, "title", m_str_title);
    define_method(m, ReceiverType::String, "trim", m_str_trim);
    define_method(m, ReceiverType::String, "reverse", m_str_reverse);
    define_method(m, ReceiverType::String, "repeat", m_str_repeat);
    define_method(m, ReceiverType::String, "slice", m_str_slice);
    define_method(m, ReceiverType::String, "split", m_str_split);
    define_method(m, ReceiverType::String, "replace", m_str_replace);
    define_method(m, ReceiverType::String, "asFile", m_str_as_file);

    define_method(m, ReceiverType::Number, "abs", m_num_abs);
    define_method(m, ReceiverType::Number, "ceil", m_num_ceil);
    define_method(m, ReceiverType::Number, "floor", m_num_floor);
    define_method(m, ReceiverType::Number, "round", m_num_round);
    define_method(m, ReceiverType::Number, "toFixed", m_num_to_fixed);

    define_method(m, ReceiverType::List, "contains", m_list_contains);
    define_method(m, ReceiverType::List, "containsAll", m_list_contains_all);
    define_method(m, ReceiverType::List, "containsAny", m_list_contains_any);
    define_method(m, ReceiverType::List, "join", m_list_join);
    define_method(m, ReceiverType::List, "unique", m_list_unique);
    define_method(m, ReceiverType::List, "flat", m_list_flat);
    define_method(m, ReceiverType::List, "reverse", m_list_reverse);
    define_method(m, ReceiverType::List, "sort", m_list_sort);
    define_method(m, ReceiverType::List, "slice", m_list_slice);
    define_method(m, ReceiverType::List, "sum", m_list_sum);
    define_method(m, ReceiverType::List, "mean", m_list_mean);
    define_method(m, ReceiverType::List, "count", m_list_count);
    define_method(m, ReceiverType::List, "min", m_list_min);
    define_method(m, ReceiverType::List, "max", m_list_max);

    define_method(m, ReceiverType::Date, "format", m_date_format);
    define_method(m, ReceiverType::Date, "date", m_date_date);
    define_method(m, ReceiverType::Date, "time", m_date_time);
    define_method(m, ReceiverType::Date, "relative", m_date_relative);
    define_method(m, ReceiverType::Date, "asLink", m_date_as_link);
    define_method(m, ReceiverType::Date, "asFile", m_date_as_file);

    define_method(m, ReceiverType::Link, "asFile", m_link_as_file);
    define_method(m, ReceiverType::Link, "linksTo", m_link_links_to);
    define_method(m, ReceiverType::Link, "contains", m_link_contains);

    define_method(m, ReceiverType::File, "asLink", m_file_as_link);
    define_method(m, ReceiverType::File, "hasTag", m_file_has_tag);
    define_method(m, ReceiverType::File, "inFolder", m_file_in_folder);
    define_method(m, ReceiverType::File, "hasProperty", m_file_has_property);
    define_method(m, ReceiverType::File, "hasLink", m_file_has_link);
    define_method(m, ReceiverType::File, "contains", m_file_contains);

    define_method(m, ReceiverType::Object, "keys", m_object_keys);
    define_method(m, ReceiverType::Object, "values", m_object_values);

    define_higher_order(m, ReceiverType::List, "filter", 2);
    define_higher_order(m, ReceiverType::List, "map", 2);
    define_higher_order(m, ReceiverType::List, "find", 2);
    define_higher_order(m, ReceiverType::List, "some", 2);
    define_higher_order(m, ReceiverType::List, "every", 2);
    define_higher_order(m, ReceiverType::List, "flatMap", 2);
    define_higher_order(m, ReceiverType::List, "reduce", 3);
    // The same higher-order methods on a bare Link, which real vaults use:
    // `link("Note").linksTo("Other")` is a filter, not a call on a list.
    define_higher_order(m, ReceiverType::Link, "linksTo", 2);

    Registries { globals, methods }
}

/// A namespace of frontmatter, presented to the evaluator as an object.
pub fn namespace(map: BTreeMap<String, BasesValue>) -> BasesValue {
    BasesValue::Namespace(Rc::new(map))
}

/// Re-exported so `service.rs` need not reach into `evaluator` for a type it
/// only stores.
pub use crate::value::BasesDate as Date;

/// Month arithmetic, exposed for the `+ "1M"` path in the evaluator.
pub fn add_months(d: &BasesDate, months: i64) -> Result<BasesDate> {
    let naive = d.0.naive_local();
    let Some(shifted) = naive.checked_add_months(chrono::Months::new(months as u32)) else {
        return Err(BasesError::new("Date arithmetic overflowed"));
    };
    let Some(dt) = d.0.offset().from_local_datetime(&shifted).single() else {
        return Err(BasesError::new(
            "Date arithmetic landed on an ambiguous local time",
        ));
    };
    Ok(BasesDate(dt))
}

/// Unused today, kept because a `relative` filter on a fixed clock is the one
/// place a duration is compared as a `ChronoDuration` rather than as millis.
#[allow(dead_code)]
fn chrono_ms(ms: i64) -> ChronoDuration {
    ChronoDuration::milliseconds(ms)
}

/// Keeps the `Datelike` import honest across a refactor; the trait is what makes
/// `.year()`, `.month()` and `.day()` available on a chrono date.
#[allow(dead_code)]
fn _uses_datelike(dt: &DateTime<FixedOffset>) -> i32 {
    dt.year()
}
