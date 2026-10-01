//! Markdown rendering.
//!
//! Two surfaces, and the split is deliberate. `flat` reproduces what
//! `obsidian base:query format=md` emits, probed on Obsidian 1.13.7: ONE
//! GitHub-flavoured markdown table for every view type, with the cells CENTRED,
//! no group headers and no summaries footer. It is a lossy export format, and it
//! is compared against the CLI byte for byte, so its centring and its column
//! widths are load-bearing rather than cosmetic.
//!
//! `structured` is the agent-facing Projection. It keeps what an agent needs in
//! order to read a view -- `groupBy` headers, a `summaries` footer, `list` and
//! `map` as markdown lists -- and LEFT-aligns its cells, which scans far better
//! than the CLI's centred output. It is never written to disk, so it carries no
//! parity obligation. See `docs/divergences.md`.
//!
//! Cards and kanban have no faithful markdown equivalent, so they are flattened
//! into a table. That is an intentional simplification, not an accident.
//!
//! Observed details both surfaces reproduce:
//!   * Column headers are DISPLAY NAMES, not Property IDs.
//!   * A null or absent cell renders as an EMPTY cell, not `-` or `null`.
//!   * List values are joined with ", " and tags keep their leading `#`.
//!   * Cells are centred with padding, which `flat` must keep for parity.

use std::collections::BTreeSet;

use crate::base::{canonical, BaseFile, BaseView, QueryResult, ResolvedRow};
use crate::error::{BasesError, Result};
use crate::labels::display_name_for;
use crate::value::{format_number, BasesDate, BasesValue};

/// A rendered column: the Property ID its rows are keyed by, and its header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderColumn {
    /// Canonical Property ID.
    pub id: String,
    /// Header text: the display name when one is configured.
    pub header: String,
}

/// How much structure a rendered view keeps.
///
/// `Flat` is CLI-exact: `obsidian base:query format=md` renders EVERY view type
/// as the same centred flat markdown table, ignoring `groupBy` and `summaries`.
///
/// `Structured` renders `list` and `map` as markdown lists and every other view
/// type as a left-aligned markdown table, keeping group headers and a summaries
/// footer. This is the agent-facing Projection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RenderStyle {
    /// The `format=md` export format. The parity surface.
    #[default]
    Flat,
    /// The Projection surface.
    Structured,
}

/// How a table pads its cells to a common width.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TableAlign {
    /// What Obsidian does, and therefore what `flat` must do.
    Centre,
    /// Left alignment, so a column's text lines up down the table.
    Left,
}

/// View types `structured` renders as a markdown list. Everything else is a
/// table, including types no version of this server has heard of: the view type
/// is a documented open namespace that plugins own, so an unrecognised one
/// degrades to a table rather than failing the whole render.
fn renders_as_list(view: &BaseView) -> bool {
    matches!(view.view_type.as_str(), "list" | "map")
}

/// The columns a view shows, in order.
///
/// A view with no `order` shows `file.name` followed by every property the Base
/// declares, which is what Obsidian shows for an unordered table.
fn view_order(base: &BaseFile, view: &BaseView) -> Vec<String> {
    match view.order.as_deref() {
        Some(ids) if !ids.is_empty() => ids.to_vec(),
        _ => {
            let mut ids = vec!["file.name".to_string()];
            ids.extend(base.properties.keys().cloned());
            ids
        }
    }
}

/// Render a resolved query as markdown.
pub fn render_markdown(
    base: &BaseFile,
    result: &QueryResult,
    style: RenderStyle,
) -> Result<String> {
    if style == RenderStyle::Flat {
        let columns = render_columns(base, &result.view);
        return Ok(table_for(&columns, &result.rows, TableAlign::Centre, None));
    }
    if renders_as_list(&result.view) {
        return Ok(render_as_list(base, result));
    }
    render_as_table(base, result)
}

/// The columns a view renders, as Property IDs paired with their headers.
pub fn render_columns(base: &BaseFile, view: &BaseView) -> Vec<RenderColumn> {
    view_order(base, view)
        .into_iter()
        .map(|id| RenderColumn {
            id: canonical(&id),
            // `display_name_for` canonicalises again, and Obsidian matches a
            // configured `displayName` on the canonical ID only -- a bare key is
            // dead config, not a fallback. See `src/labels.rs`.
            header: display_name_for(base, &id),
        })
        .collect()
}

/// The value a row holds for a Property ID, or `None` when it holds none.
///
/// Absent and `null` are the same thing to a CELL -- both render blank, exactly
/// as they do in Obsidian -- but they are not the same thing to a summary, which
/// counts a column's blanks, so the difference is kept rather than flattened.
fn cell_value<'a>(row: &'a ResolvedRow, id: &str) -> Option<&'a BasesValue> {
    row.values
        .get(id)
        .filter(|value| !matches!(value, BasesValue::Null))
}

/// A cell's text. An absent value is an EMPTY cell, matching Obsidian -- note
/// this is not the same as an empty-string property, but both render blank.
fn to_cell_text(value: Option<&BasesValue>) -> String {
    value.map_or_else(String::new, BasesValue::to_display_string)
}

// ---------------------------------------------------------------------------
// Tables
// ---------------------------------------------------------------------------

/// Render a table of `columns` over `rows`.
///
/// `widths` is `None` for a standalone table, and supplied when a table and its
/// summaries footer must line up.
fn table_for(
    columns: &[RenderColumn],
    rows: &[ResolvedRow],
    align: TableAlign,
    widths: Option<&[usize]>,
) -> String {
    let measured;
    let widths = match widths {
        Some(known) => known,
        None => {
            measured = column_widths(columns, rows, &[]);
            &measured
        }
    };

    let headers: Vec<&str> = columns
        .iter()
        .map(|column| column.header.as_str())
        .collect();
    let body: Vec<String> = rows
        .iter()
        .map(|row| format!("| {} |", pad_cells(&row_cells(columns, row), widths, align)))
        .collect();

    let mut lines = vec![
        format!("| {} |", pad_cells(&headers, widths, align)),
        format!("| {} |", rule(widths)),
    ];
    lines.extend(body);
    lines.join("\n")
}

/// Every column's cell text for one row, in column order.
fn row_cells(columns: &[RenderColumn], row: &ResolvedRow) -> Vec<String> {
    columns
        .iter()
        .map(|column| to_cell_text(cell_value(row, &column.id)))
        .collect()
}

/// The `---` rule under a header row, one dash run per column.
fn rule(widths: &[usize]) -> String {
    widths
        .iter()
        .map(|width| "-".repeat(*width))
        .collect::<Vec<_>>()
        .join(" | ")
}

/// Column widths, shared by a table and the summaries footer beneath it.
///
/// The footer has to be measured TOGETHER with the rows, or it comes out
/// narrower than the table it is summarising and the whole point of the footer
/// -- reading it against the column above -- is lost.
fn column_widths(
    columns: &[RenderColumn],
    rows: &[ResolvedRow],
    extra_cells: &[String],
) -> Vec<usize> {
    columns
        .iter()
        .enumerate()
        .map(|(index, column)| {
            let mut width = display_width(&column.header).max(display_width(
                extra_cells.get(index).map_or("", String::as_str),
            ));
            for row in rows {
                width = width.max(display_width(&to_cell_text(cell_value(row, &column.id))));
            }
            // Obsidian never emits a narrower column than the rule it needs.
            width.max(3)
        })
        .collect()
}

/// Pad each cell to its column's width, then join with ` | `.
///
/// Generic over the cell spelling because a header row borrows its text while a
/// body row and a summary row own theirs, and the padding is identical either
/// way.
fn pad_cells<S: AsRef<str>>(cells: &[S], widths: &[usize], align: TableAlign) -> String {
    cells
        .iter()
        .enumerate()
        .map(|(index, cell)| {
            let cell = cell.as_ref();
            let width = widths
                .get(index)
                .copied()
                .unwrap_or_else(|| display_width(cell));
            match align {
                TableAlign::Centre => centre(cell, width),
                TableAlign::Left => pad_end(cell, width),
            }
        })
        .collect::<Vec<_>>()
        .join(" | ")
}

/// Obsidian centres its table cells, so `flat` does too.
fn centre(text: &str, width: usize) -> String {
    let pad = width.saturating_sub(display_width(text));
    if pad == 0 {
        return text.to_string();
    }
    let left = pad / 2;
    format!("{}{}{}", " ".repeat(left), text, " ".repeat(pad - left))
}

/// Left-alignment, so a column's text lines up down the table.
fn pad_end(text: &str, width: usize) -> String {
    let pad = width.saturating_sub(display_width(text));
    format!("{text}{}", " ".repeat(pad))
}

/// How many terminal columns `text` occupies.
///
/// Character count, not UTF-16 code units or bytes, so emoji in a cell do not
/// skew the alignment. East-Asian wide characters and emoji are two columns,
/// combining marks and the zero-width joiner are none.
pub fn display_width(text: &str) -> usize {
    text.chars()
        .map(|ch| {
            let code = ch as u32;
            // Zero-width joiner, variation selectors and combining marks.
            if code == 0x200d || (0x0300..=0x036f).contains(&code) {
                return 0;
            }
            if code == 0xfe0f || code == 0xfe0e {
                return 0;
            }
            // One column, or two -- never zero: a zero here would silently eat a
            // character and under-size every column it appears in.
            if is_east_asian_wide(code) {
                2
            } else {
                1
            }
        })
        .sum()
}

/// Whether a code point takes two terminal columns.
///
/// The ranges are the TypeScript's, probed rather than derived from a table:
/// the top one stops at `0x3fffd`, which excludes the two unassigned code points
/// below the plane-3 boundary. Widening it would be a silent divergence.
fn is_east_asian_wide(code: u32) -> bool {
    matches!(code,
        0x1100..=0x115f
        | 0x2e80..=0xa4cf
        | 0xac00..=0xd7a3
        | 0xf900..=0xfaff
        | 0xfe30..=0xfe6f
        | 0xff00..=0xff60
        | 0xffe0..=0xffe6
        | 0x1f300..=0x1f9ff
        | 0x20000..=0x3fffd)
}

// ---------------------------------------------------------------------------
// Grouped and list rendering
// ---------------------------------------------------------------------------

/// A table view, keeping its group headers and its summaries footer.
fn render_as_table(base: &BaseFile, result: &QueryResult) -> Result<String> {
    let columns = render_columns(base, &result.view);

    if let Some(groups) = &result.groups {
        let mut out = String::new();
        for group in groups {
            out.push_str(&format!("**{}**\n\n", group.key));
            out.push_str(&table_for(&columns, &group.rows, TableAlign::Left, None));
            out.push_str("\n\n");
        }
        return Ok(one_trailing_newline(&out));
    }

    // Measure the summaries footer together with the rows, so the footer lines up
    // under the table instead of being squeezed to its own narrower widths.
    let cells = summary_cells(&columns, &result.view, &result.rows)?;
    let widths = column_widths(&columns, &result.rows, cells.as_deref().unwrap_or(&[]));

    let table = table_for(&columns, &result.rows, TableAlign::Left, Some(&widths));
    let Some(cells) = cells else {
        return Ok(table);
    };
    Ok(format!(
        "{table}\n\n{}",
        summary_table(&columns, &cells, &widths)
    ))
}

/// A list view: the primary column becomes the bullet text and the rest are
/// appended as `header: value` pairs.
///
/// Blank values are dropped rather than rendered as `- Header: `, because a row
/// with three missing properties should not be three lines of nothing.
fn render_as_list(base: &BaseFile, result: &QueryResult) -> String {
    let columns = render_columns(base, &result.view);
    let primary = columns
        .first()
        .expect("view_order always yields at least file.name");

    let emit = |rows: &[ResolvedRow]| -> Vec<String> {
        let mut out = Vec::new();
        for row in rows {
            let title = to_cell_text(cell_value(row, &primary.id));
            out.push(format!(
                "- {}",
                if title.is_empty() {
                    "(untitled)"
                } else {
                    title.as_str()
                }
            ));
            for column in &columns[1..] {
                let value = to_cell_text(cell_value(row, &column.id));
                if value.is_empty() {
                    continue;
                }
                out.push(format!("  - {}: {}", column.header, value));
            }
        }
        out
    };

    let Some(groups) = &result.groups else {
        return emit(&result.rows).join("\n");
    };

    let mut out = String::new();
    for group in groups {
        out.push_str(&format!("**{}**\n\n", group.key));
        for line in emit(&group.rows) {
            out.push_str(&line);
            out.push('\n');
        }
        out.push('\n');
    }
    one_trailing_newline(&out)
}

/// Collapse every trailing newline to exactly one.
///
/// Grouped output is assembled with a blank line after each block, so the last
/// one leaves a stray newline behind that no reader wants.
fn one_trailing_newline(text: &str) -> String {
    format!("{}\n", text.trim_end_matches('\n'))
}

// ---------------------------------------------------------------------------
// Summaries
// ---------------------------------------------------------------------------

/// The summarised value for each column, or `""` for a column the view does not
/// summarise.
///
/// Returned as cells rather than a rendered table so the caller can measure them
/// into the column widths first.
fn summary_cells(
    columns: &[RenderColumn],
    view: &BaseView,
    rows: &[ResolvedRow],
) -> Result<Option<Vec<String>>> {
    let Some(summaries) = &view.summaries else {
        return Ok(None);
    };
    let cells = columns
        .iter()
        .map(|column| {
            // Keyed on the canonical ID, so a view may summarise `status` or
            // `note.status` and mean the same column. A summary for a column the
            // view does not render is inert config, not an error.
            let found = summaries
                .iter()
                .find(|(id, _)| canonical(id) == column.id)
                .map(|(_, spec)| spec.as_str());
            let Some(spec) = found else {
                return Ok(String::new());
            };
            let values: Vec<&BasesValue> = rows
                .iter()
                .filter_map(|row| cell_value(row, &column.id))
                .collect();
            summarise(spec, &values)
        })
        .collect::<Result<Vec<String>>>()?;
    Ok(Some(cells))
}

/// The footer: a one-row table reusing the main table's column widths.
fn summary_table(columns: &[RenderColumn], cells: &[String], widths: &[usize]) -> String {
    let labels: Vec<&str> = columns
        .iter()
        .map(|column| column.header.as_str())
        .collect();
    [
        format!("| {} |", pad_cells(&labels, widths, TableAlign::Left)),
        format!("| {} |", rule(widths)),
        format!("| {} |", pad_cells(cells, widths, TableAlign::Left)),
    ]
    .join("\n")
}

/// Reduce a column's values to its summary cell.
///
/// `values` has already had the absent and `null` cells removed, so `Count` and
/// `Empty` both count over what is here rather than over every row.
fn summarise(spec: &str, values: &[&BasesValue]) -> Result<String> {
    let mut numbers: Vec<f64> = Vec::new();
    let mut unique: BTreeSet<String> = BTreeSet::new();
    let mut filled = 0usize;
    let mut checked = 0usize;
    let mut unchecked = 0usize;

    for value in values {
        // An empty string is skipped for every count but `Count`: it IS a value
        // the row holds, it just holds nothing.
        if matches!(value, BasesValue::String(text) if text.is_empty()) {
            continue;
        }
        filled += 1;
        if let BasesValue::Bool(is_checked) = value {
            if *is_checked {
                checked += 1;
            } else {
                unchecked += 1;
            }
        }
        if let Some(number) = numeric(value) {
            numbers.push(number);
        }
        unique.insert(value.to_display_string());
    }

    Ok(match spec {
        // The six numeric summaries read the same numeric view of the column, and
        // an empty column has nothing to reduce.
        "Average" => reduce_numbers(&numbers, |column| sum(column) / column.len() as f64),
        "Sum" => reduce_numbers(&numbers, sum),
        "Min" => reduce_numbers(&numbers, min_of),
        "Max" => reduce_numbers(&numbers, max_of),
        // Range over a date column is blank, not a millisecond span. A date
        // renders as text, `Number()` of that text is `NaN`, so there is nothing
        // to take the difference of -- and the TypeScript, which has a date branch
        // here, never reaches it: its own empty-column guard fires first.
        "Range" => reduce_numbers(&numbers, |column| max_of(column) - min_of(column)),
        "Median" => reduce_numbers(&numbers, median),
        // A single point has no spread, so a one-row column reports nothing.
        "Stddev" if numbers.len() < 2 => String::new(),
        "Stddev" => reduce_numbers(&numbers, stddev),
        "Unique" => unique.len().to_string(),
        "Filled" => filled.to_string(),
        "Empty" => (values.len() - filled).to_string(),
        "Checked" => checked.to_string(),
        "Unchecked" => unchecked.to_string(),
        "Count" => values.len().to_string(),
        "Earliest" => boundary_time(values, f64::min),
        "Latest" => boundary_time(values, f64::max),
        // A custom summary expression over `values` is not supported yet.
        _ => {
            return Err(BasesError::new(format!(
                "Custom summary \"{spec}\" is not implemented; use a built-in summary name \
(Average, Min, Max, Sum, Range, Median, Stddev, Unique, Filled, Empty, Checked, Unchecked, \
Count, Earliest, Latest)."
            ))
            .with_construct(spec))
        }
    })
}

/// Reduce a column's numbers to a summary cell, or `""` for an empty column.
fn reduce_numbers(numbers: &[f64], reduce: impl Fn(&[f64]) -> f64) -> String {
    if numbers.is_empty() {
        return String::new();
    }
    rounded(reduce(numbers))
}

/// The earliest or latest date in a column, as a bare `YYYY-MM-DD`.
///
/// Both are documented as date summaries, and both are overloaded onto whatever
/// orderable values are present. A date-only summary keeps no time component
/// because a summary of timestamps is unreadable at a glance.
fn boundary_time(values: &[&BasesValue], pick: fn(f64, f64) -> f64) -> String {
    let times: Vec<f64> = values
        .iter()
        .filter_map(|value| epoch_millis(value))
        .collect();
    let Some(first) = times.first() else {
        return String::new();
    };
    let picked = times.iter().copied().fold(*first, pick);
    let date = BasesDate::from_millis(picked as i64);
    format!("{:04}-{:02}-{:02}", date.year(), date.month(), date.day())
}

/// The epoch milliseconds of a date or a duration, and `None` for anything else.
fn epoch_millis(value: &BasesValue) -> Option<f64> {
    match value {
        BasesValue::Date(date) => Some(date.millis() as f64),
        BasesValue::Duration(duration) => Some(duration.millis as f64),
        _ => None,
    }
}

/// The number a value contributes to a numeric summary, or `None` when it is
/// not one.
///
/// A string that spells a number counts, because a frontmatter `priority: "2"`
/// and `priority: 2` have to summarise identically.
fn numeric(value: &BasesValue) -> Option<f64> {
    if let BasesValue::Number(number) = value {
        return Some(*number);
    }
    js_number(&value.to_display_string())
}

/// `Number(text)` as JavaScript defines it.
///
/// Whitespace is trimmed and an empty string is zero, because that is what a
/// cell holding an empty list contributes; anything unparseable is `None`,
/// which the caller treats exactly as `NaN` was.
fn js_number(text: &str) -> Option<f64> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Some(0.0);
    }
    trimmed
        .parse::<f64>()
        .ok()
        .filter(|number| !number.is_nan())
}

/// Format a summary number, or `""` when there was nothing to summarise.
fn rounded(number: f64) -> String {
    format_number(round3(number))
}

fn sum(numbers: &[f64]) -> f64 {
    numbers.iter().sum()
}

fn min_of(numbers: &[f64]) -> f64 {
    numbers.iter().copied().fold(f64::INFINITY, f64::min)
}

fn max_of(numbers: &[f64]) -> f64 {
    numbers.iter().copied().fold(f64::NEG_INFINITY, f64::max)
}

fn median(numbers: &[f64]) -> f64 {
    let mut sorted = numbers.to_vec();
    sorted.sort_by(f64::total_cmp);
    let mid = sorted.len() / 2;
    if sorted.len() % 2 == 1 {
        return sorted[mid];
    }
    (sorted[mid - 1] + sorted[mid]) / 2.0
}

/// The POPULATION standard deviation, which is what Obsidian reports.
///
/// A sample deviation would make a one-row column read as zero rather than as
/// having nothing to measure.
fn stddev(numbers: &[f64]) -> f64 {
    let mean = sum(numbers) / numbers.len() as f64;
    let squared: f64 = numbers.iter().map(|value| (value - mean).powi(2)).sum();
    (squared / numbers.len() as f64).sqrt()
}

/// Round to three decimal places, as JavaScript's `Math.round(x * 1000) / 1000`
/// does -- that is, halves go UP, not away from zero.
fn round3(number: f64) -> f64 {
    ((number * 1000.0) + 0.5).floor() / 1000.0
}
