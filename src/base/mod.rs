//! `.base` files and the query that resolves them.
//!
//! Two modules, in pipeline order: [`parse`] turns YAML into a [`BaseFile`], and
//! [`query`] runs the formulas, filters, sort, grouping and limit that produce the
//! rows an agent reads.
//!
//! Between them they hold three decisions that are easy to get wrong and
//! expensive to debug, so each is stated where it happens:
//!
//!   - the view level is an OPEN namespace, and unknown keys are preserved
//!     verbatim rather than dropped or refused ([`parse::BaseView::extra`]);
//!   - formulas are evaluated once per note and topologically ordered, so one may
//!     reference another ([`query::order_formulas`]);
//!   - an unsorted view orders by `file.name`, not by path ([`query::query_base`]).

pub mod parse;
pub mod query;

pub use parse::{
    BaseFile, BaseView, Direction, FilterNode, GroupBy, PropertyConfig, SortEntry, normalise_filters,
    parse_base, select_view,
};
pub use query::{
    QueryGroup, QueryOptions, QueryResult, ResolvedRow, canonical, order_formulas, query_base,
    resolve_host_note, resolve_property,
};