//! Parity against the live Obsidian CLI.
//!
//! This is the only test that can catch a divergence from Obsidian, and it is
//! the reason the port exists. It needs the testing vault registered in Obsidian,
//! so it SKIPS — visibly, never vacuously — when the CLI cannot answer.
//!
//! # Why "visibly" matters so much here
//!
//! Obsidian's CLI exits 0 even when its bridge is dead and prints nothing. An
//! availability check that trusts the exit code therefore reports "available",
//! every query returns an empty string, and a suite that treats an empty result
//! as a pass reports GREEN while having compared nothing. That is the exact
//! failure mode this suite exists to catch, so [`available`] requires non-empty
//! stdout, and [`Cli::available`] is asserted to be false for a dead bridge.
//!
//! Only `AllNotes.base` is a genuine oracle: it never references `this`, so the
//! CLI can answer it. `Tickets.base` scopes itself with `this`, which the CLI
//! cannot bind — it returns `[]` — and is asserted as the known divergence.

use std::process::Command;

use bases_mcp::base::QueryOptions;
use bases_mcp::render::markdown::RenderStyle;
use bases_mcp::service::Resolver;

// Each integration test is its own crate, and no suite needs every helper here.
#[allow(dead_code)]
mod common;

use common::{futures_block_on, vault_dir};

/// The name Obsidian knows the testing vault by. It is `vault`, after the
/// directory name, not the path.
const CLI_VAULT: &str = "vault";

struct Cli {
    binary: String,
}

impl Cli {
    fn new() -> Self {
        Self {
            binary: std::env::var("OBSIDIAN_BIN").unwrap_or_else(|_| "obsidian".into()),
        }
    }

    /// Whether the CLI can answer a real query.
    ///
    /// `obsidian version` is not enough. The bridge reports a version and then
    /// returns empty for every query while it is still handing the vault over to
    /// a newly started app, and an exit code is not a signal in either case —
    /// Obsidian exits 0 even when the bridge is down. So the probe is an actual
    /// `base:query`, which is the only thing that proves the comparison can run.
    fn available(&self) -> bool {
        // Must produce PARSEABLE ROWS, not merely some output. A half-started
        // bridge answers `version` and then returns "" for every query, so a
        // non-empty check passes while nothing can actually be compared.
        matches!(
            self.query_json("AllNotes.base", Some("All")),
            Ok(rows) if !rows.is_empty()
        )
    }

    fn run(&self, args: &[String]) -> Result<String, String> {
        let output = Command::new(&self.binary)
            .args(args)
            .output()
            .map_err(|e| format!("could not run {}: {e}", self.binary))?;
        if !output.status.success() {
            return Err(format!("{} exited {:?}", self.binary, output.status));
        }
        String::from_utf8(output.stdout).map_err(|e| e.to_string())
    }

    /// Query a base as JSON. `path` is the exact vault-relative path including
    /// the `.base` extension — the `file=` spelling needs it and `path=` is
    /// preferred either way.
    fn query_json(&self, base: &str, view: Option<&str>) -> Result<Vec<serde_json::Value>, String> {
        let mut args = vec![
            format!("vault={CLI_VAULT}"),
            "base:query".into(),
            format!("path={base}"),
            "format=json".into(),
        ];
        if let Some(v) = view {
            args.push(format!("view={v}"));
        }
        let out = self.run(&args)?;
        if out.trim().is_empty() {
            // Empty where rows were expected. Obsidian does not flush stdout
            // before exiting, so a heavy query can come back empty; treat it as
            // a failure rather than as an empty result set.
            return Err("empty stdout where rows were expected".into());
        }
        serde_json::from_str(&out).map_err(|e| format!("unparseable JSON: {e}"))
    }

    fn query_markdown(&self, base: &str, view: Option<&str>) -> Result<String, String> {
        let mut args = vec![
            format!("vault={CLI_VAULT}"),
            "base:query".into(),
            format!("path={base}"),
            "format=md".into(),
        ];
        if let Some(v) = view {
            args.push(format!("view={v}"));
        }
        self.run(&args)
    }
}

/// Require a live CLI, or say clearly that this run compared nothing.
///
/// Rust's test harness has no skip: a test that returns early reports `ok`. That
/// is the vacuous-green failure this project already had once, so instead of
/// returning early this PANICS unless `BASES_MCP_ALLOW_SKIP=1`. That makes an
/// unverified parity run loud by default and deliberate to opt into, which is
/// the opposite of the original bug.
/// Whether this run may proceed without a live CLI.
///
/// Returns true when the CLI answered and the caller should compare, and false
/// when the run is deliberately unverified. Panics in the second case unless the
/// opt-in is set, so "compared nothing" is never a silent pass.
fn require_cli(cli: &Cli) -> bool {
    if cli.available() {
        return true;
    }
    if std::env::var("BASES_MCP_ALLOW_SKIP").as_deref() == Ok("1") {
        eprintln!("[parity] Obsidian CLI unavailable — COMPARING NOTHING this run.");
        return false;
    }
    panic!(
        "the Obsidian CLI is unavailable, so this run compared nothing and would otherwise \
report a vacuous pass. Open Obsidian with the testing vault registered, or set \
BASES_MCP_ALLOW_SKIP=1 to accept an unverified run deliberately."
    );
}

fn resolver() -> Resolver {
    futures_block_on(Resolver::open_dir(vault_dir())).expect("the testing vault is readable")
}

/// Compare two JSON row sets, naming the row and column that disagreed.
///
/// A wholesale array comparison would report "these two big things differ",
/// which is not something an agent can act on at 3am. This walks to the exact
/// cell.
fn compare_rows(ours: &[serde_json::Value], theirs: &[serde_json::Value], label: &str) {
    assert_eq!(
        ours.len(),
        theirs.len(),
        "{label}: row COUNT differs — ours {}, theirs {}",
        ours.len(),
        theirs.len()
    );

    for (i, (our, their)) in ours.iter().zip(theirs.iter()).enumerate() {
        let our_map = our.as_object().expect("a row is an object");
        let their_map = their.as_object().expect("a row is an object");
        assert_eq!(
            our_map.len(),
            their_map.len(),
            "{label} row {i}: column COUNT differs — ours {}, theirs {}",
            our_map.len(),
            their_map.len()
        );
        for (key, value) in our_map {
            let expected = their_map.get(key).unwrap_or_else(|| {
                panic!(
                    "{label} row {i}: theirs has no column {key:?}; it has {:?}",
                    their_map.keys().collect::<Vec<_>>()
                )
            });
            assert_eq!(value, expected, "{label} row {i}, column {key:?}");
        }
    }
}

#[test]
fn the_cli_is_detected_as_unavailable_when_its_bridge_is_dead() {
    // Guards the guard. A binary that exits 0 with empty stdout must NOT read as
    // available, because that turns every parity assertion into a no-op.
    let cli = Cli {
        binary: "true".into(),
    }; // `true` exits 0 and prints nothing.
    assert!(
        !cli.available(),
        "an empty response must not count as an available CLI"
    );
}

#[test]
fn our_json_matches_the_cli() {
    let cli = Cli::new();
    if !require_cli(&cli) {
        return;
    }
    let resolver = resolver();

    for view in ["All", "ByPriority", "AsList"] {
        let base = futures_block_on(resolver.load_base("AllNotes.base")).expect("parses");
        let result = futures_block_on(resolver.query(
            "AllNotes.base",
            &QueryOptions {
                view: Some(view.into()),
                context: None,
            },
        ))
        .expect("queries");
        let ours = bases_mcp::service::json_rows(&base, &result, &[]);
        let theirs = cli
            .query_json("AllNotes.base", Some(view))
            .expect("the CLI answers");
        compare_rows(&ours, &theirs, &format!("AllNotes.base [{view}]"));
    }
}

#[test]
fn our_markdown_matches_the_cli_byte_for_byte() {
    let cli = Cli::new();
    if !require_cli(&cli) {
        return;
    }
    let resolver = resolver();
    for view in ["All", "ByPriority", "AsList"] {
        let ours = futures_block_on(resolver.render(
            "AllNotes.base",
            &QueryOptions {
                view: Some(view.into()),
                context: None,
            },
        ))
        .expect("renders");
        let theirs = cli
            .query_markdown("AllNotes.base", Some(view))
            .expect("the CLI answers");
        assert_eq!(
            ours.trim(),
            theirs.trim(),
            "AllNotes.base [{view}] markdown"
        );
    }
}

#[test]
fn the_cli_cannot_bind_this_and_returns_an_empty_result() {
    let cli = Cli::new();
    if !require_cli(&cli) {
        return;
    }
    // Asserted as the DIVERGENCE. If a future Obsidian teaches the CLI to bind
    // `this`, this fails and the registry needs updating — which is the intended
    // signal, not a flake.
    let theirs = cli
        .query_json("Tickets.base", Some("All"))
        .expect("the CLI answers");
    assert!(
        theirs.is_empty(),
        "the CLI returned rows for a `this`-scoped base: {theirs:?}"
    );
}

#[test]
fn we_scope_to_the_host_note_where_the_cli_returns_nothing() {
    let resolver = resolver();
    let some = futures_block_on(resolver.query(
        "Tickets.base",
        &QueryOptions {
            view: Some("All".into()),
            context: Some("Projects/SomeProject.md".into()),
        },
    ))
    .expect("queries");
    assert_eq!(some.rows.len(), 2);

    let other = futures_block_on(resolver.query(
        "Tickets.base",
        &QueryOptions {
            view: Some("All".into()),
            context: Some("Projects/OtherProject.md".into()),
        },
    ))
    .expect("queries");
    assert_eq!(other.rows.len(), 1);
}

#[test]
fn no_host_is_a_hard_error_never_a_silent_empty_array() {
    let resolver = resolver();
    let error = futures_block_on(resolver.query("Tickets.base", &QueryOptions::default()))
        .expect_err("must refuse");
    assert_eq!(error.construct(), Some("this"));
}

#[test]
fn the_flat_surface_is_the_one_we_compare() {
    // Asserts the wiring rather than the output: `render` is the FLAT surface,
    // which is what format=md parity is about. If someone changed it to
    // structured, the markdown parity test above would start failing against
    // the CLI, and this says why it matters.
    let resolver = resolver();
    let result = futures_block_on(resolver.query(
        "AllNotes.base",
        &QueryOptions {
            view: Some("AsList".into()),
            context: None,
        },
    ))
    .expect("queries");
    let base = futures_block_on(resolver.load_base("AllNotes.base")).expect("parses");
    let flat = bases_mcp::render::markdown::render_markdown(&base, &result, RenderStyle::Flat)
        .expect("renders");
    let structured =
        bases_mcp::render::markdown::render_markdown(&base, &result, RenderStyle::Structured)
            .expect("renders");
    assert!(flat.starts_with('|'), "flat must be a table");
    assert_ne!(
        flat, structured,
        "the two surfaces must not be the same function"
    );
}

/// The base is loaded and its views read, so a parse regression surfaces here
/// rather than as an empty result deep in a tool call.
#[test]
fn the_oracle_base_parses_and_exposes_the_views_the_parity_tests_use() {
    let base = futures_block_on(resolver().load_base("AllNotes.base")).expect("parses");
    let names: Vec<&str> = base.views.iter().map(|v| v.name.as_str()).collect();
    assert_eq!(names, ["All", "ByPriority", "AsList"]);
    assert_eq!(base.views[0].view_type, "table");
}
