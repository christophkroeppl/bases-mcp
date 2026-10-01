//! The draft / verify / commit handshake behind `add_note_to_base`.
//!
//! A row in a Base IS a note, so "add a row" means "author a note the Base's
//! filter will actually match". The filter is the ground truth: the draft we
//! return is a best-effort INVERSION of it, and the verify step — which runs
//! that real filter against the draft's own frontmatter — is the only thing
//! that makes the result trustworthy.
//!
//! Two calls, never one. The first returns a draft; the agent edits it and calls
//! again with `draft_id` plus the edited content. Only then do we touch the
//! vault. Splitting it that way is the point: inversion cannot be complete
//! (arbitrary expressions are not invertible), so the agent has to see what we
//! guessed. And a verify failure names the expressions that did not match and
//! hands back the Base's raw YAML, so the correction is made against the filter
//! rather than against our guess at it.
//!
//! Verification runs against an overlay vault that splices the unsaved note into
//! the real index and REFUSES every write. That is deliberate: link resolution,
//! tag collection and frontmatter coercion are exactly the machinery a filter
//! like `project.contains(link(this.file.name))` depends on, and a hand-built
//! stand-in would be a second, drifting implementation of all three. Making the
//! overlay read-only makes "verify cannot write" structural rather than a
//! promise.
//!
//! The dependency runs one way: the Resolver calls into this module, never the
//! reverse. The methods below are therefore `impl Resolver` blocks rather than a
//! `DraftHost` trait — Rust's module system already states the direction, and a
//! one-implementor trait would only restate it.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use async_trait::async_trait;
use chrono::{DateTime, Local, SecondsFormat, Utc};
use serde::Deserialize;
use serde_yaml::{Mapping, Value as Yaml};

use crate::ast::{Literal, Node, NodeKind};
use crate::base::{order_formulas, resolve_host_note, select_view, BaseFile, BaseView, FilterNode};
use crate::depth::{Depth, FILTER};
use crate::error::{BasesError, Result};
use crate::evaluator::{evaluate, EvalContext, RESERVED};
use crate::note::parse_note_with_embeds;
use crate::parser::{parse, try_parse};
use crate::service::Resolver;
use crate::value::{strip_extension, BasesValue};
use crate::vault::source::{FileStat, SourceKind, VaultSource};
use crate::vault::{content_hash, Vault};

// ---------------------------------------------------------------------------
// Surface
// ---------------------------------------------------------------------------

/// What the agent sent. Every field is optional, and only `draft_id` is load
/// bearing: it is the one field that says which half of the handshake this is.
///
/// The wire spelling is camelCase for `includeAllFormulas`-style arguments and
/// `draft_id` verbatim, because that is the name the tool description tells the
/// agent to send back.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AddNoteOptions {
    /// Path to the `.base` the new note has to match. First call only — the
    /// second call reads the Base back out of the stored draft.
    pub base: Option<String>,
    /// Vault-relative path of the note to create. First call only, for the same
    /// reason. The agent sends back `draft_id` and `content`, nothing else.
    pub path: Option<String>,
    /// Host note, which binds `this` for a scoped Base. First call only.
    pub context: Option<String>,
    /// View whose filters the note has to satisfy. Defaults to the first view.
    pub view: Option<String>,
    /// Second call only: the id returned by the first.
    #[serde(rename = "draft_id")]
    pub draft_id: Option<String>,
    /// Second call only: the agent's edited note.
    pub content: Option<String>,
}

/// A proposed note, handed back for editing. Nothing has been written.
#[derive(Debug, Clone, PartialEq)]
pub struct DraftProposal {
    pub draft_id: String,
    pub path: String,
    /// The proposed note. A suggestion, never a guarantee of a match.
    pub content: String,
    /// Absolute expiry as epoch milliseconds.
    pub expires_at: i64,
    /// The resolved `.base` path the draft was inverted from.
    pub base: String,
    pub view: String,
    pub context: Option<String>,
}

/// A note written because the Base's real filter matched it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DraftCommit {
    pub written: String,
    pub draft_id: String,
    /// Always true: a commit that did not verify does not happen.
    pub verified: bool,
}

/// Which half of the handshake came back.
#[derive(Debug, Clone, PartialEq)]
pub enum AddNoteToBaseResult {
    Proposal(DraftProposal),
    Commit(DraftCommit),
}

impl AddNoteToBaseResult {
    /// The note that was written, when one was.
    pub fn written(&self) -> Option<&str> {
        match self {
            Self::Commit(commit) => Some(&commit.written),
            Self::Proposal(_) => None,
        }
    }

    /// The draft id either way, so a caller can report which one it holds.
    pub fn draft_id(&self) -> &str {
        match self {
            Self::Commit(commit) => &commit.draft_id,
            Self::Proposal(proposal) => &proposal.draft_id,
        }
    }

    /// True when the call consumed a draft and wrote a note.
    pub fn is_commit(&self) -> bool {
        matches!(self, Self::Commit(_))
    }
}

// ---------------------------------------------------------------------------
// The draft store
// ---------------------------------------------------------------------------

/// Thirty minutes: long enough to write a note, short enough that a stale
/// `draft_id` cannot surface in a later session and author a row against a
/// filter nobody is looking at any more.
pub const DRAFT_TTL_MS: i64 = 30 * 60 * 1000;

/// What the commit needs, kept until the note is written or the draft expires.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredDraft {
    pub id: String,
    /// Resolved `.base` path — the filter this draft was inverted from.
    pub base: String,
    /// View name, so the commit verifies against the filters we actually read.
    pub view: String,
    pub path: String,
    pub context: Option<String>,
    /// The draft exactly as proposed, kept so a later call can diff the edits.
    pub original: String,
    /// Absolute expiry, epoch milliseconds.
    pub expires_at: i64,
}

/// The seed a new draft is built from; the store supplies the id and the expiry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DraftSeed {
    pub base: String,
    pub view: String,
    pub path: String,
    pub context: Option<String>,
    pub original: String,
}

/// Live drafts, keyed by id.
///
/// Bounded by TTL rather than by a timer: nothing lives long enough to pile up,
/// so there is no sweep task to keep alive and no state that survives a restart
/// by accident. `RefCell` because a server is one vault in one process and
/// answers one call at a time.
#[derive(Debug)]
pub struct DraftStore {
    drafts: RefCell<HashMap<String, StoredDraft>>,
    ttl_ms: i64,
}

impl Default for DraftStore {
    fn default() -> Self {
        Self::new(DRAFT_TTL_MS)
    }
}

impl DraftStore {
    /// A store whose drafts live for `ttl_ms`. Zero expires them immediately,
    /// which is how the expiry path is exercised without waiting.
    pub fn new(ttl_ms: i64) -> Self {
        Self {
            drafts: RefCell::new(HashMap::new()),
            ttl_ms,
        }
    }

    /// Store a draft and hand back the record, id and expiry included.
    pub fn put(&self, seed: DraftSeed) -> StoredDraft {
        self.prune();
        let record = StoredDraft {
            id: uuid::Uuid::new_v4().to_string(),
            base: seed.base,
            view: seed.view,
            path: seed.path,
            context: seed.context,
            original: seed.original,
            expires_at: now_ms() + self.ttl_ms,
        };
        self.drafts
            .borrow_mut()
            .insert(record.id.clone(), record.clone());
        record
    }

    /// Look up a live draft. Expiry is a miss, not a special case to handle.
    pub fn get(&self, id: &str) -> Result<StoredDraft> {
        let draft = self.drafts.borrow().get(id).cloned();
        let Some(draft) = draft else {
            return Err(expired_draft(id, self.ttl_ms));
        };
        if draft.expires_at <= now_ms() {
            self.release(id);
            return Err(expired_draft(id, self.ttl_ms));
        }
        Ok(draft)
    }

    /// Drop a draft. One draft commits one note, so a commit consumes it.
    pub fn release(&self, id: &str) {
        self.drafts.borrow_mut().remove(id);
    }

    /// Drop everything. Exists so a test starts from a known-empty store.
    pub fn clear(&self) {
        self.drafts.borrow_mut().clear();
    }

    pub fn len(&self) -> usize {
        self.drafts.borrow().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn prune(&self) {
        let now = now_ms();
        self.drafts
            .borrow_mut()
            .retain(|_, draft| draft.expires_at > now);
    }
}

fn now_ms() -> i64 {
    Utc::now().timestamp_millis()
}

fn expired_draft(id: &str, ttl_ms: i64) -> BasesError {
    BasesError::new(format!(
        "Draft {id} is unknown or has expired (drafts live {} minutes). \
         Call add_note_to_base again WITHOUT draft_id to get a fresh draft.",
        (ttl_ms as f64 / 60000.0).round()
    ))
    .with_construct("draft_id")
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Draft a note, or commit one against a draft already handed out.
///
/// The branch is on `draft_id` alone: that is the only field that says which
/// half of the handshake this is, and it is the field the agent has to send
/// back. Everything else on the commit path — base, path, view, context — is
/// read back from the stored draft, so nothing here may assume it is present.
pub async fn add_note_to_base(
    resolver: &Resolver,
    options: &AddNoteOptions,
) -> Result<AddNoteToBaseResult> {
    if let Some(draft_id) = &options.draft_id {
        return commit_draft(resolver, options, draft_id).await;
    }
    propose_draft(resolver, options).await
}

// ---------------------------------------------------------------------------
// First call: invert the filter into a draft
// ---------------------------------------------------------------------------

async fn propose_draft(
    resolver: &Resolver,
    options: &AddNoteOptions,
) -> Result<AddNoteToBaseResult> {
    // The path is refused before the vault is consulted, so a `.base` never even
    // gets as far as a lookup.
    let path = require_field(options.path.as_deref(), "path")?;
    assert_note_path(&path)?;
    assert_note_absent(resolver, &path)?;

    let requested = require_field(options.base.as_deref(), "base")?;
    let base_path = resolver.resolve_base_path(&requested)?;
    let base = resolver.load_base(&base_path).await?;
    let view = select_view(&base, options.view.as_deref())?.clone();
    let context = options.context.clone();
    let content = render_draft(
        &path,
        &invert_effective(&base, &view, context.as_deref()),
        &view,
    );

    let stored = resolver.drafts().put(DraftSeed {
        base: base_path.clone(),
        view: view.name.clone(),
        path: path.clone(),
        context: context.clone(),
        original: content.clone(),
    });

    Ok(AddNoteToBaseResult::Proposal(DraftProposal {
        draft_id: stored.id,
        path,
        content,
        expires_at: stored.expires_at,
        base: base_path,
        view: view.name,
        context,
    }))
}

/// A field the first call cannot do without.
///
/// The options type keeps `base` and `path` optional because the second call
/// omits them, so the first call has to say plainly which one is missing rather
/// than letting an absent value reach the vault as a path.
fn require_field(value: Option<&str>, name: &str) -> Result<String> {
    value.map(str::to_string).ok_or_else(|| {
        BasesError::new(format!(
            "`{name}` is required when proposing a draft. On the second call, send only \
             `draft_id` and `content` -- the rest is read back from the stored draft."
        ))
    })
}

fn assert_note_absent(resolver: &Resolver, path: &str) -> Result<()> {
    if resolver
        .vault()
        .note_paths()
        .iter()
        .any(|known| known == path)
    {
        return Err(BasesError::new(format!(
            "{path} already exists. add_note_to_base creates a NEW row; use write_note to change a \
             note that is already there, or pick another path."
        ))
        .with_note(path));
    }
    Ok(())
}

/// A row is a note, and a note is a `.md` file.
///
/// `.base` is refused outright: no note operation ever writes a Base, and a Base
/// region is the one thing an agent must not be able to overwrite by accident.
/// Anything that is not `.md` is refused too, because the vault index only
/// recognises `.md` — such a file would be written and then never appear in the
/// Base, which is the exact silent failure this tool exists to prevent.
fn assert_note_path(path: &str) -> Result<()> {
    if path.trim().is_empty() {
        return Err(BasesError::new("A note path is required."));
    }
    if path.ends_with(".base") {
        return Err(BasesError::new(format!(
            "Refusing to write {path}: a Base is never written by a note operation. \
             Rows come from notes -- create a note with add_note_to_base, and edit the .base file \
             itself for view, filter and formula changes."
        ))
        .with_note(path));
    }
    if !path.ends_with(".md") {
        return Err(BasesError::new(format!(
            "Refusing to write {path}: a row in a Base has to be a .md note, because a .md note is \
             the only thing the vault indexes."
        ))
        .with_note(path));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Second call: verify, then commit
// ---------------------------------------------------------------------------

async fn commit_draft(
    resolver: &Resolver,
    options: &AddNoteOptions,
    draft_id: &str,
) -> Result<AddNoteToBaseResult> {
    let stored = resolver.drafts().get(draft_id)?;
    // The stored path is authoritative and is re-checked here, so a draft that
    // somehow names a `.base` is refused on the commit path too rather than only
    // on the propose path.
    assert_note_path(&stored.path)?;

    // `base` and `path` are optional in the options because this call omits them.
    // If a caller DOES send one, it has to agree with the draft.
    if let Some(sent) = &options.base {
        let base_path = resolver.resolve_base_path(sent)?;
        if base_path != stored.base {
            return Err(BasesError::new(format!(
                "Draft {id} was drafted against {drafted}, not {sent_base}. Re-send it without \
                 draft_id to get a draft for the base you actually mean.",
                id = stored.id,
                drafted = stored.base,
                sent_base = base_path
            ))
            .with_note(&stored.path));
        }
    }
    if let Some(sent) = &options.path {
        // A `.base` is refused before it is even compared with the draft, so the
        // agent gets the "a Base is never written" reason rather than a
        // confusing mismatch message.
        assert_note_path(sent)?;
        if *sent != stored.path {
            return Err(BasesError::new(format!(
                "Draft {id} was drafted for {drafted}, not {sent}. The verify step is only sound \
                 for the path the draft was built for.",
                id = stored.id,
                drafted = stored.path,
                sent = sent
            ))
            .with_note(sent));
        }
    }

    let Some(content) = options.content.clone() else {
        return Err(BasesError::new(format!(
            "Draft {id} needs the edited note as \"content\". Send the content you want written, \
             or call add_note_to_base without draft_id to get a fresh draft.",
            id = stored.id
        ))
        .with_note(&stored.path));
    };

    let base = resolver.load_base(&stored.base).await?;
    let view = select_view(&base, Some(&stored.view))?.clone();
    if stored.context.is_none()
        && effective_filters(&base, &view)
            .iter()
            .any(|filter| mentions_this(filter.node))
    {
        return Err(BasesError::new(format!(
            "{base} is scoped to a host note: its filter references \"this\", and none was \
             supplied. Nothing was written. Call add_note_to_base again with \"context\" set to \
             the note the base is embedded in, so \"this\" binds to a real note.",
            base = stored.base
        ))
        .with_view(&view.name)
        .with_construct("this"));
    }

    // Structural validity before semantic validity: malformed frontmatter would
    // otherwise reach the filter as "no properties" and be reported as a mismatch
    // the agent cannot act on.
    let parsed = parse_note_with_embeds(&stored.path, &content);
    if parsed.malformed_frontmatter {
        return Err(BasesError::new(format!(
            "The frontmatter in {path} is not valid YAML, so nothing was written. Fix it and \
             resend the same draft_id.",
            path = stored.path
        ))
        .with_note(&stored.path));
    }

    let failures = verification_failures(resolver, &base, &view, &stored, &content).await?;
    if !failures.is_empty() {
        let yaml = resolver.vault().read_text(&stored.base).await?;
        return Err(verification_error(&stored, &view, &failures, &yaml));
    }

    // The absence check ran when the draft was PROPOSED. Between then and now a
    // human may have created a note at this path -- minutes later, while they
    // read the proposal -- and `create_note` replaces verbatim. Re-check against
    // the vault as it is now, not against the index as it was, so the commit
    // refuses rather than overwriting prose it never saw.
    if resolver.vault().note_paths().contains(&stored.path) {
        return Err(BasesError::new(format!(
            "{path} was created after this draft was proposed, so nothing was written. \
             add_note_to_base only creates a NEW row; use write_note to change a note that \
             already exists, or pick another path.",
            path = stored.path
        ))
        .with_note(&stored.path));
    }

    resolver.create_note(&stored.path, &content).await?;
    resolver.drafts().release(&stored.id);
    Ok(AddNoteToBaseResult::Commit(DraftCommit {
        written: stored.path,
        draft_id: stored.id,
        verified: true,
    }))
}

/// One filter expression, and what it made of the draft.
#[derive(Debug, Clone, PartialEq)]
struct FilterProbe {
    where_: String,
    /// The expression as the Base wrote it.
    expression: String,
    /// What the expression produced.
    outcome: ProbeOutcome,
}

/// What a probe produced.
///
/// A `Threw` is not a value at all: usually the draft is missing the property
/// the expression dereferences, so `project.contains(x)` on a note with no
/// `project` is a null dereference rather than a mismatch.
#[derive(Debug, Clone, PartialEq)]
enum ProbeOutcome {
    Value(BasesValue),
    Threw(String),
}

impl ProbeOutcome {
    fn rendered(&self) -> String {
        match self {
            Self::Value(value) => value.to_display_string(),
            Self::Threw(message) => message.clone(),
        }
    }
}

/// Every filter expression the draft fails, or an empty list when it matches.
///
/// The verdict comes from the Base's real filter tree — Base-level `filters`
/// ANDed with the view's own — run through the same `parse`, `evaluate` and
/// truthiness the query pipeline uses, against a context assembled the way
/// `query_base` assembles one. Only the reporting is ours.
///
/// `query_base` keeps its filter compilation private and hands back only rows,
/// so the tree walk is mirrored rather than shared. [`probe`] below and those
/// two functions have to keep agreeing; `tests/service.rs` pins it by asserting
/// that a note this accepts is a row `Resolver::query` returns.
async fn verification_failures(
    resolver: &Resolver,
    base: &BaseFile,
    view: &BaseView,
    stored: &StoredDraft,
    content: &str,
) -> Result<Vec<FilterProbe>> {
    let vault = vault_with_draft(resolver.shared_backend(), &stored.path, content).await;
    let ctx = draft_eval_context(&vault, &stored.path, base, stored.context.as_deref())?;

    let mut failures = Vec::new();
    for filter in effective_filters(base, view) {
        probe(filter.node, &filter.where_, &ctx, &mut failures, true);
    }
    Ok(failures)
}

/// Build the evaluation context for a note that is not on disk.
///
/// The formula pass mirrors `query_base`: formulas are ordered first (a formula
/// may read another) and written into the bag as each is computed, because that
/// is the graph the query pipeline builds.
fn draft_eval_context(
    vault: &Rc<Vault>,
    path: &str,
    base: &BaseFile,
    context: Option<&str>,
) -> Result<EvalContext> {
    let Some(record) = vault.note(path) else {
        return Err(BasesError::new(format!(
            "Could not index the draft for {path}; refusing to write it."
        ))
        .with_note(path));
    };
    let mut ctx = EvalContext::new(vault.file_value(path));
    ctx.note = (*record.frontmatter).clone();
    // The same resolver the query pipeline uses, so a bad host note is refused
    // in one voice here and in `resolve_base` rather than two.
    ctx.this_value = resolve_host_note(vault, context)?;

    for name in order_formulas(&base.formulas)? {
        let expression = &base.formulas[&name];
        let value = evaluate(&parse(expression)?, &ctx)?;
        ctx.formula.insert(name, value);
    }
    Ok(ctx)
}

/// Run one filter node, recording every leaf that came out falsy.
///
/// `or` and `not` are reported as a whole rather than branch by branch. Which
/// branch an author meant is a guess, and naming a branch they did not write
/// would be a worse error than naming the group.
fn probe(
    node: &FilterNode,
    where_: &str,
    ctx: &EvalContext,
    failures: &mut Vec<FilterProbe>,
    collect: bool,
) -> bool {
    probe_within(node, where_, ctx, failures, collect, &Depth::new())
}

/// The walk itself, against a budget the caller holds.
///
/// Bounded for the same reason [`crate::base::query`]'s `run_filter` is, and
/// because `probe` is a third independent copy of the tree walk that this
/// module has to keep agreeing with it.
///
/// A refusal is not an error return, because `probe` answers `bool`, so an
/// over-nested subtree reports itself as a leaf that did not match — which is
/// the shape an agent correcting its frontmatter already knows how to read.
/// Under a `not:` that inverts into a group that holds, since a child that did
/// not match is exactly what `not:` wants. That costs a third return value to
/// say properly, and the path is not reachable to get it wrong: the YAML loader
/// refuses a filter tree past 63 levels and [`crate::depth`] spends
/// [`crate::depth::MAX_DEPTH`], so the refusal never fires from a parsed Base.
/// Recorded rather than threaded through, because the day it does become
/// reachable it should be fixed loudly.
fn probe_within(
    node: &FilterNode,
    where_: &str,
    ctx: &EvalContext,
    failures: &mut Vec<FilterProbe>,
    collect: bool,
    depth: &Depth,
) -> bool {
    let Ok(_level) = depth.enter(FILTER) else {
        return false;
    };
    match node {
        // A draft that omits a property the filter dereferences makes the
        // expression THROW rather than evaluate to false. From the agent's point
        // of view that is still just "this filter did not pass", and reporting
        // it as a raw type error would hide the filter and the Base YAML behind
        // it. So an evaluation error is recorded as a failure of that one leaf,
        // and the loop carries on to the others.
        FilterNode::Expression(source) => match parse(source) {
            Err(error) => {
                if collect {
                    failures.push(FilterProbe {
                        where_: where_.to_string(),
                        expression: source.clone(),
                        outcome: ProbeOutcome::Threw(format!("threw: {}", error.display_message())),
                    });
                }
                false
            }
            Ok(node) => match evaluate(&node, ctx) {
                Err(error) => {
                    if collect {
                        failures.push(FilterProbe {
                            where_: where_.to_string(),
                            expression: source.clone(),
                            outcome: ProbeOutcome::Threw(format!(
                                "threw: {}",
                                error.display_message()
                            )),
                        });
                    }
                    false
                }
                Ok(value) if value.is_truthy() => true,
                Ok(value) => {
                    if collect {
                        failures.push(FilterProbe {
                            where_: where_.to_string(),
                            expression: source.clone(),
                            outcome: ProbeOutcome::Value(value),
                        });
                    }
                    false
                }
            },
        },
        // Every conjunct is evaluated, not short-circuited: an agent correcting
        // its frontmatter needs all the expressions that did not match, not the
        // first one. Spelled as a loop rather than a `&&` chain precisely so
        // nothing here can short-circuit.
        FilterNode::And(children) => {
            let mut all = true;
            for (index, child) in children.iter().enumerate() {
                let held = probe_within(
                    child,
                    &format!("{where_}.and[{index}]"),
                    ctx,
                    failures,
                    collect,
                    depth,
                );
                all = all && held;
            }
            all
        }
        FilterNode::Or(children) => {
            let ok = children.iter().any(|child| {
                probe_within(child, &format!("{where_}.or"), ctx, failures, false, depth)
            });
            if !ok && collect {
                failures.push(FilterProbe {
                    where_: where_.to_string(),
                    expression: format!("any of ({})", describe_all(children)),
                    outcome: ProbeOutcome::Value(BasesValue::Bool(false)),
                });
            }
            ok
        }
        // `not` is NAND, not negation: none of these may be true.
        FilterNode::Not(children) => {
            let ok = children.iter().all(|child| {
                !probe_within(child, &format!("{where_}.not"), ctx, failures, false, depth)
            });
            if !ok && collect {
                failures.push(FilterProbe {
                    where_: where_.to_string(),
                    expression: format!("none of ({})", describe_all(children)),
                    outcome: ProbeOutcome::Value(BasesValue::Bool(true)),
                });
            }
            ok
        }
    }
}

/// One labelled filter of the tree the verdict comes from.
struct LabelledFilter<'a> {
    where_: String,
    node: &'a FilterNode,
}

/// The filter tree the verdict comes from: Base-level `filters` ANDed with the
/// view's own. Labelled the way the Base writes them, so a failure points at
/// the key the author has to edit.
fn effective_filters<'a>(base: &'a BaseFile, view: &'a BaseView) -> Vec<LabelledFilter<'a>> {
    let mut out = Vec::new();
    if let Some(node) = &base.filters {
        out.push(LabelledFilter {
            where_: "filters".to_string(),
            node,
        });
    }
    if let Some(node) = &view.filters {
        out.push(LabelledFilter {
            where_: format!("views.{}.filters", view.name),
            node,
        });
    }
    out
}

/// Does anything in the filter refer to `this`?
///
/// Checked before anything is evaluated, because the alternative is a type error
/// from a property the draft has not set yet — `"contains" is not a method on
/// null` says nothing about the real problem, which is an unbound host.
fn mentions_this(node: &FilterNode) -> bool {
    match node {
        FilterNode::Expression(source) => {
            try_parse(source).is_some_and(|ast| mentions_this_node(&ast))
        }
        FilterNode::And(children) | FilterNode::Or(children) | FilterNode::Not(children) => {
            children.iter().any(mentions_this)
        }
    }
}

fn mentions_this_node(node: &Node) -> bool {
    match &node.kind {
        NodeKind::Identifier(name) => name == "this",
        NodeKind::Literal(_) => false,
        NodeKind::Member { object, .. } => mentions_this_node(object),
        NodeKind::Unary { operand, .. } => mentions_this_node(operand),
        NodeKind::Binary { left, right, .. } => {
            mentions_this_node(left) || mentions_this_node(right)
        }
        NodeKind::Index { object, index } => {
            mentions_this_node(object) || mentions_this_node(index)
        }
        NodeKind::List(items) => items.iter().any(mentions_this_node),
        NodeKind::Call { callee, args } => {
            mentions_this_node(callee) || args.iter().any(mentions_this_node)
        }
    }
}

/// The failure an agent has to act on, so it carries everything needed to act.
///
/// The Base's YAML is inlined verbatim rather than summarised: the agent is
/// about to edit frontmatter, and the only authority on what that frontmatter
/// has to satisfy is the filter itself. A paraphrase would be a second thing to
/// get wrong.
fn verification_error(
    stored: &StoredDraft,
    view: &BaseView,
    failures: &[FilterProbe],
    yaml: &str,
) -> BasesError {
    let reported = failures
        .iter()
        .map(|failure| {
            format!(
                "  - {expr}  ->  {value}   [{where_}]",
                expr = failure.expression,
                value = failure.outcome.rendered(),
                where_ = failure.where_
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    BasesError::new(format!(
        "{path} does not match view \"{view}\" of {base}, so nothing was written.\n\
         These filter expressions did not match the draft's frontmatter:\n{reported}\n\
         Correct the note and call add_note_to_base again with draft_id {id} and the edited \
         content; the draft is still live until {expires}.\n\
         The base's own filter, verbatim ({base}):\n{yaml}",
        path = stored.path,
        view = view.name,
        base = stored.base,
        reported = reported,
        id = stored.id,
        expires = iso_millis(stored.expires_at),
        yaml = yaml
    ))
}

// ---------------------------------------------------------------------------
// Filter inversion
// ---------------------------------------------------------------------------

/// One proposed frontmatter key and the value to write for it.
#[derive(Debug, Clone, PartialEq)]
struct Entry {
    key: String,
    value: BasesValue,
}

/// What inversion could and could not turn into frontmatter.
#[derive(Debug, Clone, Default, PartialEq)]
struct Inversion {
    entries: Vec<Entry>,
    /// Constructs no inversion covers. They become TODO comments in the draft:
    /// dropping a conjunct would produce a note that looks right and matches
    /// nothing, which is the failure this whole tool is built to avoid.
    unresolved: Vec<String>,
}

impl Inversion {
    fn unresolved(source: impl Into<String>) -> Self {
        Self {
            entries: Vec::new(),
            unresolved: vec![source.into()],
        }
    }
}

/// Invert the Base-level filter ANDed with the view's own.
///
/// Only a conjunction of invertible leaves yields frontmatter. `or` and `not`
/// are reported instead of guessed: a value we invented for a disjunction would
/// be a coin flip about which branch the author meant, and a coin flip that
/// happens to verify is still a coin flip.
fn invert_effective(base: &BaseFile, view: &BaseView, context: Option<&str>) -> Inversion {
    merge([
        invert(base.filters.as_ref(), context),
        invert(view.filters.as_ref(), context),
    ])
}

/// One node: recurse through a conjunction, parse an expression, or give up.
fn invert(node: Option<&FilterNode>, context: Option<&str>) -> Inversion {
    let Some(node) = node else {
        return Inversion::default();
    };

    match node {
        FilterNode::Expression(source) => match try_parse(source) {
            None => Inversion::unresolved(source.clone()),
            Some(ast) => invert_node(&ast, source, context),
        },
        FilterNode::And(children) => {
            merge_of(children.iter().map(|child| invert(Some(child), context)))
        }
        // `or` and `not` are the two shapes a single value cannot express.
        FilterNode::Or(_) | FilterNode::Not(_) => Inversion::unresolved(describe(node)),
    }
}

/// Invert one filter node, or report it as un-invertible.
///
/// The four forms that invert are the ones real Bases are built from:
/// `file.hasTag("x")`, `<prop>.contains(link(this.file.name))`,
/// `<prop>.contains(link("Literal"))`, and `<prop> == "literal"` (either way
/// round). Everything else — `file.*` comparisons, `inFolder`, bare formula
/// predicates, `!=`, regex, lambdas — becomes a TODO comment, because a
/// half-right guess is indistinguishable from a right one until the row silently
/// fails to appear.
fn invert_node(node: &Node, source: &str, context: Option<&str>) -> Inversion {
    let nothing = || Inversion::unresolved(source.to_string());

    // `a && b` is as invertible as its parts: both halves have to hold anyway.
    if let NodeKind::Binary {
        op: crate::ast::BinOp::And,
        left,
        right,
    } = &node.kind
    {
        return merge([
            invert_node(left, source, context),
            invert_node(right, source, context),
        ]);
    }

    if let Some(tag) = as_call_tag(node) {
        return Inversion {
            entries: vec![Entry {
                key: "tags".to_string(),
                value: tag,
            }],
            unresolved: Vec::new(),
        };
    }

    if let Some((property, needle)) = as_contains(node) {
        let Some(key) = note_property_key(property) else {
            return nothing();
        };
        let Some(args) = link_argument(needle) else {
            return nothing();
        };

        if is_host_file_name(args) {
            // `this.file.name` is the host note, so only a known host can be written.
            let Some(host) = context.filter(|host| !host.is_empty()) else {
                return Inversion::unresolved(format!(
                    "{source} -- pass the host note as \"context\" and re-draft, or set {key} \
                     yourself"
                ));
            };
            let link = format!("[[{}]]", strip_extension(basename(host)));
            return Inversion {
                entries: vec![Entry {
                    key,
                    value: BasesValue::List(vec![BasesValue::String(link)]),
                }],
                unresolved: Vec::new(),
            };
        }
        if let NodeKind::Literal(Literal::String(target)) = &args.kind {
            let link = format!("[[{target}]]");
            return Inversion {
                entries: vec![Entry {
                    key,
                    value: BasesValue::List(vec![BasesValue::String(link)]),
                }],
                unresolved: Vec::new(),
            };
        }
        return nothing();
    }

    // Equality is symmetric, so `status == "x"` and `"x" == status` agree.
    if let NodeKind::Binary {
        op: crate::ast::BinOp::Eq,
        left,
        right,
    } = &node.kind
    {
        if let Some(key) = note_property_key(left) {
            if let Some(value) = plain_literal(right) {
                return Inversion {
                    entries: vec![Entry { key, value }],
                    unresolved: Vec::new(),
                };
            }
        }
        if let Some(key) = note_property_key(right) {
            if let Some(value) = plain_literal(left) {
                return Inversion {
                    entries: vec![Entry { key, value }],
                    unresolved: Vec::new(),
                };
            }
        }
    }

    nothing()
}

/// Combine inversions, refusing to let two conjuncts fight over one key.
fn merge<const N: usize>(parts: [Inversion; N]) -> Inversion {
    merge_of(parts)
}

/// [`merge`] over a sequence, for a conjunction of unknown length.
fn merge_of(parts: impl IntoIterator<Item = Inversion>) -> Inversion {
    let mut out = Inversion::default();
    for part in parts {
        out.unresolved.extend(part.unresolved);
        for entry in part.entries {
            match out
                .entries
                .iter_mut()
                .find(|existing| existing.key == entry.key)
            {
                None => out.entries.push(entry),
                Some(existing) => match (&mut existing.value, entry.value) {
                    // Two list values CONJOIN: a tag filter and a `contains`
                    // filter over the same key both have to hold.
                    (BasesValue::List(have), BasesValue::List(add)) => {
                        for value in add {
                            if !have.contains(&value) {
                                have.push(value);
                            }
                        }
                    }
                    (have, want) => out.unresolved.push(format!(
                        "filters disagree about \"{key}\": {have} vs {want}",
                        key = entry.key,
                        have = have.to_display_string(),
                        want = want.to_display_string()
                    )),
                },
            }
        }
    }
    out
}

/// Human-readable form of a filter node, for the placeholders we cannot invert.
fn describe(node: &FilterNode) -> String {
    match node {
        FilterNode::Expression(source) => source.clone(),
        FilterNode::And(children) => format!("all of ({})", describe_all(children)),
        FilterNode::Or(children) => format!("any of ({})", describe_all(children)),
        FilterNode::Not(children) => format!("none of ({})", describe_all(children)),
    }
}

fn describe_all(children: &[FilterNode]) -> String {
    children.iter().map(describe).collect::<Vec<_>>().join(", ")
}

// -- node shapes ------------------------------------------------------------

/// The frontmatter key a node reads, or `None` when it is not a note property.
///
/// `file.*`, `formula.*` and `this.*` all read from somewhere frontmatter
/// cannot write, so they are deliberately not invertible. A filter over them
/// still gets a TODO in the draft — un-invertible, not invisible.
fn note_property_key(node: &Node) -> Option<String> {
    match &node.kind {
        NodeKind::Identifier(name) => {
            if RESERVED.contains(&name.as_str()) {
                None
            } else {
                Some(name.clone())
            }
        }
        NodeKind::Member { object, property } if matches!(&object.kind, NodeKind::Identifier(name) if name == "note") => {
            Some(property.clone())
        }
        NodeKind::Index { object, index } if matches!(&object.kind, NodeKind::Identifier(name) if name == "note") => {
            match &index.kind {
                NodeKind::Literal(Literal::String(key)) => Some(key.clone()),
                _ => None,
            }
        }
        _ => None,
    }
}

/// `file.hasTag("x")`, as the value to write under `tags`.
fn as_call_tag(node: &Node) -> Option<BasesValue> {
    let NodeKind::Call { callee, args } = &node.kind else {
        return None;
    };
    let NodeKind::Member { object, property } = &callee.kind else {
        return None;
    };
    if property != "hasTag"
        || !matches!(&object.kind, NodeKind::Identifier(name) if name == "file")
        || args.len() != 1
    {
        return None;
    }
    match &args[0].kind {
        NodeKind::Literal(Literal::String(tag)) => {
            Some(BasesValue::List(vec![BasesValue::String(tag.clone())]))
        }
        _ => None,
    }
}

/// `link(<arg>)` with exactly one argument, returned.
fn link_argument(node: &Node) -> Option<&Node> {
    let NodeKind::Call { callee, args } = &node.kind else {
        return None;
    };
    if !matches!(&callee.kind, NodeKind::Identifier(name) if name == "link") || args.len() != 1 {
        return None;
    }
    args.first()
}

/// `<prop>.contains(<needle>)`, for any receiver.
fn as_contains(node: &Node) -> Option<(&Node, &Node)> {
    let NodeKind::Call { callee, args } = &node.kind else {
        return None;
    };
    let NodeKind::Member { object, property } = &callee.kind else {
        return None;
    };
    if property != "contains" || args.len() != 1 {
        return None;
    }
    Some((object, &args[0]))
}

/// `this.file.name` or `this.file.basename` — the same note either way.
fn is_host_file_name(node: &Node) -> bool {
    let NodeKind::Member { object, property } = &node.kind else {
        return false;
    };
    if property != "name" && property != "basename" {
        return false;
    }
    let NodeKind::Member {
        object: file,
        property,
    } = &object.kind
    else {
        return false;
    };
    property == "file" && matches!(&file.kind, NodeKind::Identifier(name) if name == "this")
}

/// A literal a frontmatter value can be written as. Links and dates are not.
fn plain_literal(node: &Node) -> Option<BasesValue> {
    match &node.kind {
        NodeKind::Literal(
            literal @ (Literal::String(_) | Literal::Number(_) | Literal::Bool(_)),
        ) => Some(literal.clone().into()),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Draft rendering
// ---------------------------------------------------------------------------

/// Assemble the note: frontmatter first, because that is the part the Base reads.
///
/// The view's `order` columns are seeded with empty values so the agent can see
/// the shape a row is expected to have. Empty is deliberate — it can never
/// satisfy a filter, so a seed can never make verification pass by accident.
fn render_draft(path: &str, inversion: &Inversion, view: &BaseView) -> String {
    let mut taken: Vec<String> = inversion
        .entries
        .iter()
        .map(|entry| entry.key.clone())
        .collect();
    let mut entries = inversion.entries.clone();
    entries.extend(seed_columns(view, &mut taken));

    let frontmatter = frontmatter_lines(&entries, &inversion.unresolved);
    let mut lines: Vec<String> = Vec::new();
    if !frontmatter.is_empty() {
        lines.push("---".to_string());
        lines.extend(frontmatter);
        lines.push("---".to_string());
        lines.push(String::new());
    }
    lines.push(format!("# {}", strip_extension(basename(path))));
    lines.push(String::new());
    lines.push(
        "TODO: replace this paragraph with the note body. The frontmatter above was inverted from \
         the base's filter, so it is a suggestion -- the base's own filter is the ground truth."
            .to_string(),
    );
    format!("{}\n", lines.join("\n"))
}

/// The `order` columns that are note properties, as empty values.
///
/// `file.*` and `formula.*` are skipped: the first is not frontmatter and the
/// second is computed, so neither can be seeded.
fn seed_columns(view: &BaseView, taken: &mut Vec<String>) -> Vec<Entry> {
    let mut out = Vec::new();
    for id in view.order.iter().flatten() {
        let Some(ast) = try_parse(id) else { continue };
        let Some(key) = note_property_key(&ast) else {
            continue;
        };
        if taken.contains(&key) {
            continue;
        }
        taken.push(key.clone());
        out.push(Entry {
            key,
            value: BasesValue::String(String::new()),
        });
    }
    out
}

/// The frontmatter block, line by line, with the placeholders at the end.
fn frontmatter_lines(entries: &[Entry], unresolved: &[String]) -> Vec<String> {
    let mut lines: Vec<String> = entries.iter().map(yaml_entry).collect();
    for expression in unresolved {
        lines.push(format!("# TODO: could not invert {}", collapse(expression)));
    }
    lines
}

/// One frontmatter key, spelled by `serde_yaml`.
///
/// The serialiser decides the quoting; a value like `[[X]]` or `a: b` would be
/// unreadable if we hand-assembled it.
fn yaml_entry(entry: &Entry) -> String {
    let mut mapping = Mapping::new();
    mapping.insert(Yaml::String(entry.key.clone()), yaml_value(&entry.value));
    serde_yaml::to_string(&mapping)
        .unwrap_or_default()
        .trim_end_matches('\n')
        .to_string()
}

/// A Bases value as YAML frontmatter.
///
/// Only the shapes inversion can produce are spelled out — scalars and lists of
/// them. Anything else is unreachable by construction, and a value written as
/// its display text is still something an agent can read and correct.
fn yaml_value(value: &BasesValue) -> Yaml {
    match value {
        BasesValue::Null => Yaml::Null,
        BasesValue::Bool(flag) => Yaml::Bool(*flag),
        BasesValue::Number(number) => Yaml::Number((*number).into()),
        BasesValue::String(text) => Yaml::String(text.clone()),
        BasesValue::List(items) => Yaml::Sequence(items.iter().map(yaml_value).collect()),
        other => Yaml::String(other.to_display_string()),
    }
}

/// A YAML comment must occupy exactly one line.
fn collapse(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The last path segment.
fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

// ---------------------------------------------------------------------------
// The verification vault
// ---------------------------------------------------------------------------

/// The real vault with one unsaved note spliced in, and read-only.
async fn vault_with_draft(inner: Rc<dyn VaultSource>, path: &str, content: &str) -> Rc<Vault> {
    let vault = Rc::new(Vault::new(Box::new(DraftVaultSource {
        inner,
        path: path.to_string(),
        content: content.to_string(),
    })));
    // The index always builds from a listing this backend can produce, so a
    // failure here is a real failure rather than an empty vault.
    vault
        .load()
        .await
        .expect("the verification vault indexes the real listing plus one draft");
    vault
}

/// The overlay the verify step reads through.
struct DraftVaultSource {
    inner: Rc<dyn VaultSource>,
    path: String,
    content: String,
}

impl std::fmt::Debug for DraftVaultSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DraftVaultSource")
            .field("inner", &self.inner.kind())
            .field("path", &self.path)
            .finish()
    }
}

#[async_trait(?Send)]
impl VaultSource for DraftVaultSource {
    fn kind(&self) -> SourceKind {
        self.inner.kind()
    }

    async fn list(&self) -> Result<Vec<String>> {
        // The draft is added even when a note is already at that path, because a
        // commit replaces it; the set is what stops it being indexed twice.
        let mut paths: std::collections::BTreeSet<String> =
            self.inner.list().await?.into_iter().collect();
        paths.insert(self.path.clone());
        Ok(paths.into_iter().collect())
    }

    async fn read_text(&self, path: &str) -> Result<String> {
        if path == self.path {
            return Ok(self.content.clone());
        }
        self.inner.read_text(path).await
    }

    /// The draft overlays the real vault, so a fresh read still has to consult
    /// the draft for its own path before going to the backend for anything else.
    async fn read_fresh(&self, path: &str) -> Result<String> {
        if path == self.path {
            return Ok(self.content.clone());
        }
        self.inner.read_fresh(path).await
    }

    /// Refuses rather than delegating, so verification is incapable of touching
    /// the vault: the note is written once, after its filter has matched.
    async fn write_text(&self, path: &str, _data: &str) -> Result<()> {
        Err(BasesError::new(format!(
            "Refusing to write {path}: a verification vault is read-only. The note is written \
             once, after its filter has matched."
        ))
        .with_note(path))
    }

    async fn stat(&self, path: &str) -> Result<FileStat> {
        if path != self.path {
            return self.inner.stat(path).await;
        }
        // A draft has no mtime of its own, so the write time is the honest answer.
        Ok(FileStat {
            size: self.content.len() as u64,
            mtime: Local::now().fixed_offset(),
        })
    }

    async fn hash(&self, path: &str) -> Result<String> {
        if path != self.path {
            return self.inner.hash(path).await;
        }
        Ok(content_hash(&self.content))
    }

    async fn ensure_dir(&self, _path: &str) -> Result<()> {
        Err(read_only("create a directory"))
    }

    async fn delete(&self, _path: &str) -> Result<()> {
        Err(read_only("delete a file"))
    }
}

fn read_only(what: &str) -> BasesError {
    BasesError::new(format!(
        "Refusing to {what} in a verification vault: it is read-only. The note is written once, \
         after its filter has matched."
    ))
    .with_construct("verification")
}

/// An epoch-millisecond instant as `Date.prototype.toISOString()` spells it.
pub fn iso_millis(millis: i64) -> String {
    DateTime::<Utc>::from_timestamp_millis(millis)
        .unwrap_or_else(|| {
            DateTime::<Utc>::from_timestamp_millis(0).expect("epoch is representable")
        })
        .to_rfc3339_opts(SecondsFormat::Millis, true)
}
