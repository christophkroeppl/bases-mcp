//! `.base` file parsing.
//!
//! The view level is a documented OPEN namespace: plugins persist arbitrary keys
//! there (`cardOrders`, `columnColors`, `swimlaneOrders`, and composite keys joined
//! by `\x1f` or `||`). Those keys are PRESERVED verbatim and never interpreted --
//! a parser that rejects or drops them corrupts real vaults, so
//! [`BaseView::extra`] and [`BaseFile::extra`] hold raw YAML and nothing reads
//! them back.
//!
//! A [`FilterNode`] is an enum rather than a struct with three optional keys
//! because [`normalise_filters`] has already proved exactly one of `and`/`or`/
//! `not` is present by the time anything else sees it. Two sibling keys is an
//! error at the boundary, not a state the rest of the engine has to keep
//! checking for.

use std::collections::BTreeMap;

use serde_yaml::{Mapping, Value as Yaml};

use crate::depth::{Depth, FILTER};
use crate::error::{BasesError, Result};

/// Sort or group direction. Anything Obsidian does not spell `DESC` is `ASC`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Asc,
    Desc,
}

/// One `sort` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SortEntry {
    /// The Property ID to sort on, spelled as written and canonicalised later.
    pub property: String,
    pub direction: Direction,
}

/// A view's `groupBy`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupBy {
    pub property: String,
    pub direction: Direction,
}

/// A filter tree, already validated.
///
/// The three group keys are variants rather than fields because a group holds
/// exactly one of them: [`normalise_filters`] refuses two siblings with
/// Obsidian's own wording, so no downstream code has to wonder which applies.
#[derive(Debug, Clone, PartialEq)]
pub enum FilterNode {
    /// A single expression, evaluated for its truthiness.
    Expression(String),
    /// All of these are true.
    And(Vec<FilterNode>),
    /// At least one of these is true.
    Or(Vec<FilterNode>),
    /// "None of the following are true" -- NAND, not logical negation.
    Not(Vec<FilterNode>),
}

/// The display config for one Property ID.
///
/// `displayName` is separated out because the label rule reads it; every other
/// key is opaque and preserved, since Obsidian grows this object over releases.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PropertyConfig {
    /// Used verbatim as the column header. Never title-cased.
    pub display_name: Option<String>,
    /// Every other key of the config, untouched.
    pub extra: Mapping,
}

/// One View: a query-and-layout pair.
///
/// [`BaseView::extra`] is the whole point of this struct's shape. The view level
/// is where plugins write, so a view is modelled as the handful of keys the
/// engine reads plus everything else, verbatim.
///
/// [`Default`] exists for constructing a view in a test, not because an empty
/// view is meaningful: [`parse_view`] refuses a view with no `type`, and that
/// refusal is the invariant. A defaulted view is a starting point to fill in.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BaseView {
    /// The `type:` key. Spelled out rather than `kind` so the YAML name and the
    /// field name are the same word.
    pub view_type: String,
    pub name: String,
    pub limit: Option<f64>,
    pub filters: Option<FilterNode>,
    pub order: Option<Vec<String>>,
    pub group_by: Option<GroupBy>,
    pub sort: Option<Vec<SortEntry>>,
    pub summaries: Option<BTreeMap<String, String>>,
    /// Every other key at the view level, preserved untouched. Includes the
    /// documented core keys (`rowHeight`, `columnSize`) and every plugin key.
    pub extra: Mapping,
}

/// A parsed `.base` file.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BaseFile {
    /// Global filters, ANDed with each view's own filters.
    pub filters: Option<FilterNode>,
    /// Formula name -> expression.
    pub formulas: BTreeMap<String, String>,
    /// Property ID -> display config. Keyed as WRITTEN, so a bare `status` and a
    /// `note.status` are two different entries here; see
    /// [`crate::labels::display_name_for`] for which one is consulted.
    pub properties: BTreeMap<String, PropertyConfig>,
    /// Custom summary name -> expression over `values`.
    pub summaries: BTreeMap<String, String>,
    pub views: Vec<BaseView>,
    /// Unrecognised top-level keys, preserved for round-tripping.
    pub extra: Mapping,
}

/// Top-level keys Obsidian defines; anything else is preserved, not dropped.
const BASE_CORE_KEYS: [&str; 5] = ["filters", "formulas", "properties", "summaries", "views"];

/// View-level keys the engine reads. Everything else lands in
/// [`BaseView::extra`].
const VIEW_CORE_KEYS: [&str; 8] = [
    "type",
    "name",
    "limit",
    "filters",
    "order",
    "groupBy",
    "sort",
    "summaries",
];

/// Parse `.base` YAML into a [`BaseFile`].
///
/// Every refusal names the path, because a base that fails to parse is a file
/// the agent can see and cannot use, and "invalid YAML" alone does not say which.
pub fn parse_base(path: &str, text: &str) -> Result<BaseFile> {
    // Obsidian's own writer escapes `|` inside filter expressions, which would
    // otherwise be read as a YAML block scalar indicator.
    let unescaped = text.replace("\\|", "|");
    let raw: Yaml = serde_yaml::from_str(&unescaped).map_err(|error| {
        BasesError::new(format!("Invalid YAML in {path}: {error}")).with_note(path)
    })?;
    let Some(root) = raw.as_mapping() else {
        return Err(
            BasesError::new(format!("A base file must be a YAML mapping: {path}")).with_note(path),
        );
    };

    let Some(views_raw) = present(root, "views").and_then(Yaml::as_sequence) else {
        return Err(
            BasesError::new(format!("A base file must define a \"views\" list: {path}"))
                .with_note(path),
        );
    };

    let views = views_raw
        .iter()
        .enumerate()
        .map(|(index, view)| parse_view(view, index, path))
        .collect::<Result<Vec<_>>>()?;
    if views.is_empty() {
        return Err(
            BasesError::new(format!("A base file must define at least one view: {path}"))
                .with_note(path),
        );
    }

    Ok(BaseFile {
        filters: normalise_filters(present(root, "filters"), path, "filters")?,
        formulas: string_map(present(root, "formulas"), "formulas", path)?,
        properties: property_configs(present(root, "properties"), path)?,
        summaries: string_map(present(root, "summaries"), "summaries", path)?,
        views,
        extra: extras(root, &BASE_CORE_KEYS),
    })
}

fn parse_view(raw: &Yaml, index: usize, path: &str) -> Result<BaseView> {
    let Some(view) = raw.as_mapping() else {
        return Err(
            BasesError::new(format!("views[{index}] must be a mapping in {path}")).with_note(path),
        );
    };
    // A view with no type is unusable, and guessing one would silently render
    // the wrong layout.
    let Some(view_type) = present(view, "type")
        .and_then(Yaml::as_str)
        .filter(|t| !t.is_empty())
    else {
        return Err(
            BasesError::new(format!("views[{index}] is missing a \"type\" in {path}"))
                .with_note(path),
        );
    };

    let mut parsed = BaseView {
        view_type: view_type.to_string(),
        name: present(view, "name")
            .and_then(Yaml::as_str)
            .map_or_else(|| format!("View {}", index + 1), str::to_string),
        limit: present(view, "limit").and_then(Yaml::as_f64),
        filters: normalise_filters(
            present(view, "filters"),
            path,
            &format!("views[{index}].filters"),
        )?,
        order: None,
        group_by: None,
        sort: None,
        summaries: None,
        extra: extras(view, &VIEW_CORE_KEYS),
    };

    if let Some(order) = present(view, "order").and_then(Yaml::as_sequence) {
        parsed.order = Some(
            order
                .iter()
                .filter_map(Yaml::as_str)
                .map(str::to_string)
                .collect(),
        );
    }

    if let Some(group) = present(view, "groupBy").and_then(Yaml::as_mapping) {
        if let Some(property) = present(group, "property").and_then(Yaml::as_str) {
            parsed.group_by = Some(GroupBy {
                property: property.to_string(),
                direction: read_direction(present(group, "direction")),
            });
        }
    }

    if let Some(entries) = present(view, "sort").and_then(Yaml::as_sequence) {
        parsed.sort = Some(
            entries
                .iter()
                .filter_map(Yaml::as_mapping)
                .filter_map(|entry| {
                    // Obsidian 1.9 wrote `column:`; it is `property:` now. Both
                    // occur in the wild -- TaskNotes still emits `column:` -- so
                    // accept either.
                    let property =
                        present(entry, "property").or_else(|| present(entry, "column"))?;
                    let property = property.as_str()?;
                    Some(SortEntry {
                        property: property.to_string(),
                        direction: read_direction(present(entry, "direction")),
                    })
                })
                .collect(),
        );
    }

    if present(view, "summaries").is_some() {
        parsed.summaries = Some(string_map(
            present(view, "summaries"),
            &format!("views[{index}].summaries"),
            path,
        )?);
    }

    Ok(parsed)
}

/// Anything that is not `DESC` sorts ascending, which is what Obsidian does with
/// a missing or misspelled direction.
fn read_direction(value: Option<&Yaml>) -> Direction {
    match value.and_then(Yaml::as_str) {
        Some(text) if text.eq_ignore_ascii_case("DESC") => Direction::Desc,
        _ => Direction::Asc,
    }
}

/// A mapping of strings, with non-string values dropped rather than refused.
///
/// Refusing would break a base Obsidian opens happily, and a non-string formula
/// has no meaning we could report usefully.
fn string_map(value: Option<&Yaml>, what: &str, path: &str) -> Result<BTreeMap<String, String>> {
    let Some(raw) = value else {
        return Ok(BTreeMap::new());
    };
    let Some(entries) = raw.as_mapping() else {
        return Err(BasesError::new(format!("{what} must be a mapping in {path}")).with_note(path));
    };
    Ok(entries
        .iter()
        .filter_map(|(key, value)| Some((yaml_key(key), value.as_str()?.to_string())))
        .collect())
}

/// The display config for each Property ID.
///
/// A value that is not a mapping is skipped: `properties: {note.x: null}` says
/// nothing about how to display `note.x`, and refusing the file over it would
/// lose the views that are perfectly readable.
fn property_configs(value: Option<&Yaml>, path: &str) -> Result<BTreeMap<String, PropertyConfig>> {
    let Some(raw) = value else {
        return Ok(BTreeMap::new());
    };
    let Some(entries) = raw.as_mapping() else {
        return Err(
            BasesError::new(format!("properties must be a mapping in {path}")).with_note(path),
        );
    };
    Ok(entries
        .iter()
        .filter_map(|(key, value)| {
            let fields = value.as_mapping()?;
            let mut config = PropertyConfig {
                display_name: present(fields, "displayName")
                    .and_then(Yaml::as_str)
                    .map(str::to_string),
                extra: Mapping::new(),
            };
            config.extra = extras(fields, &["displayName"]);
            Some((yaml_key(key), config))
        })
        .collect())
}

/// Validate a filter node.
///
/// A filter object may contain exactly ONE of `and`/`or`/`not`; siblings are an
/// error, not a silent merge. Obsidian's own wording for that case is
/// `"filters" may only have one of an "and", "or", or "not" keys.`
pub fn normalise_filters(
    value: Option<&Yaml>,
    path: &str,
    where_: &str,
) -> Result<Option<FilterNode>> {
    normalise_filters_within(value, path, where_, &Depth::new())
}

/// The walk itself, against a budget the caller holds.
///
/// Split from [`normalise_filters`] for the same reason [`crate::evaluator`]'s
/// is: the group keys recurse, and a filter tree is exactly as attacker-shaped
/// as an expression. Measured, this walk is not the one that kills the process —
/// `serde_yaml` refuses a tree deeper than 63 levels before it reaches here —
/// so the guard is here to make the refusal ours and legible, not to save the
/// stack.
fn normalise_filters_within(
    value: Option<&Yaml>,
    path: &str,
    where_: &str,
    depth: &Depth,
) -> Result<Option<FilterNode>> {
    let _level = depth.enter(FILTER)?;
    let Some(raw) = value else { return Ok(None) };

    if let Some(expression) = raw.as_str() {
        return Ok(Some(FilterNode::Expression(expression.to_string())));
    }
    if raw.is_sequence() {
        // A bare YAML list under `filters:` is invalid in Obsidian.
        return Err(BasesError::new(format!(
            "\"filters\" must be a string or a filter object, not a bare list \
             (at {where_} in {path})"
        ))
        .with_note(path));
    }
    let Some(entries) = raw.as_mapping() else {
        return Err(
            BasesError::new(format!("Invalid filter at {where_} in {path}")).with_note(path),
        );
    };

    // A key present with a null value still counts as a sibling: the author wrote
    // two group keys, and which of them they meant is a question we cannot answer.
    let present_keys: Vec<FilterGroup> = FilterGroup::ALL
        .into_iter()
        .filter(|group| contains(entries, group.key()))
        .collect();
    let [group] = present_keys.as_slice() else {
        let refusal = if present_keys.is_empty() {
            format!(
                "A filter object must contain one of \"and\", \"or\", or \"not\" \
                 (at {where_} in {path})"
            )
        } else {
            "\"filters\" may only have one of an \"and\", \"or\", or \"not\" keys.".to_string()
        };
        return Err(BasesError::new(refusal).with_note(path));
    };

    let children = normalise_list(
        present(entries, group.key()),
        path,
        &format!("{where_}.{}", group.key()),
        depth,
    )?;
    Ok(Some(group.node(children)))
}

/// The three filter group keys, in the order Obsidian's error message names them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FilterGroup {
    And,
    Or,
    Not,
}

impl FilterGroup {
    const ALL: [FilterGroup; 3] = [FilterGroup::And, FilterGroup::Or, FilterGroup::Not];

    fn key(self) -> &'static str {
        match self {
            FilterGroup::And => "and",
            FilterGroup::Or => "or",
            FilterGroup::Not => "not",
        }
    }

    fn node(self, children: Vec<FilterNode>) -> FilterNode {
        match self {
            FilterGroup::And => FilterNode::And(children),
            FilterGroup::Or => FilterNode::Or(children),
            FilterGroup::Not => FilterNode::Not(children),
        }
    }
}

/// A group's operands. `not:` accepts a single scalar as well as a list.
fn normalise_list(
    value: Option<&Yaml>,
    path: &str,
    where_: &str,
    depth: &Depth,
) -> Result<Vec<FilterNode>> {
    if let Some(expression) = value.and_then(Yaml::as_str) {
        return Ok(vec![FilterNode::Expression(expression.to_string())]);
    }
    let Some(items) = value.and_then(Yaml::as_sequence) else {
        return Err(BasesError::new(format!(
            "Filter group \"{where_}\" in {path} must be a list"
        ))
        .with_note(path));
    };
    items
        .iter()
        .filter_map(|item| normalise_filters_within(Some(item), path, where_, depth).transpose())
        .collect::<Result<Vec<_>>>()
}

/// Select a view by name; falls back to the first view, as Obsidian does.
pub fn select_view<'a>(base: &'a BaseFile, view_name: Option<&str>) -> Result<&'a BaseView> {
    let Some(name) = view_name.filter(|name| !name.is_empty()) else {
        return base
            .views
            .first()
            .ok_or_else(|| BasesError::new("This base defines no views"));
    };
    base.views
        .iter()
        .find(|view| view.name == name)
        .ok_or_else(|| {
            let available = base
                .views
                .iter()
                .map(|view| view.name.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            BasesError::new(format!(
                "No view named \"{name}\" in this base. Available views: {available}"
            ))
            .with_view(name)
        })
}

// ---------------------------------------------------------------------------
// YAML helpers
// ---------------------------------------------------------------------------

/// A mapping entry that is actually there.
///
/// Absent and explicitly null are the same thing to Obsidian's reader -- both
/// leave the field unset -- so they are one case here rather than two that could
/// disagree.
fn present<'a>(entries: &'a Mapping, key: &str) -> Option<&'a Yaml> {
    entries
        .get(Yaml::String(key.to_string()))
        .filter(|value| !value.is_null())
}

/// Whether a mapping has the key at all, whatever its value.
fn contains(entries: &Mapping, key: &str) -> bool {
    entries.contains_key(Yaml::String(key.to_string()))
}

/// Every key not in `core`, verbatim.
fn extras(entries: &Mapping, core: &[&str]) -> Mapping {
    entries
        .iter()
        .filter(|(key, _)| {
            let name = yaml_key(key);
            !core.contains(&name.as_str())
        })
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

/// A YAML key as the name Bases looks it up by.
///
/// Keys are copied as written, control characters and all. A composite key like
/// `"formula.priority_display\x1fnote.type"` is two Property IDs the plugin
/// chose to join with `\x1f`, and rewriting it would corrupt the config it
/// addresses. Only a non-string key is coerced, because that is the one spelling
/// YAML allows and JavaScript silently stringified.
fn yaml_key(key: &Yaml) -> String {
    match key {
        Yaml::String(text) => text.clone(),
        other => serde_yaml::to_string(other)
            .map_or_else(|_| String::new(), |text| text.trim_end().to_string()),
    }
}
