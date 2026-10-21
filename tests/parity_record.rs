//! End-to-end tests for the parity recorder, checker and comparator.
//!
//! The real oracle is the live Obsidian CLI against `test/vault`, and nothing
//! here pretends otherwise: the end-to-end cases stand in a fake CLI that
//! answers from files the test wrote. That proves the recorder's contract —
//! which arguments it sends, what it refuses to accept, what it stores, and how
//! it reports a difference — without launching Obsidian or touching the parity
//! oracle.
//!
//! The fake CLI deliberately exits 0 when it has no answer, because Obsidian's
//! bridge does exactly that when it is half up. Two of the cases below exist
//! only for that: empty stdout must be a failed query, never an empty result
//! set, and a vault the CLI rewrote must abort the record.
//!
//! These tests do NOT declare `mod common;`: this suite needs none of the
//! shared corpus helpers, and a test file that includes `common` while using
//! part of it makes clippy's dead-code pass fail for every other suite that
//! shares the module.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Output};

use bases_mcp::base::QueryOptions;
use bases_mcp::parity::{self, Recorded};
use bases_mcp::service::{json_rows, Resolver};
use serde_json::{json, Value};

const BIN: &str = env!("CARGO_BIN_EXE_parity-record");

const BASE: &str = "\
filters:
  and:
    - file.hasTag(\"note\")
views:
  - type: table
    name: All
    order:
      - file.name
      - status
  - type: list
    name: AsList
    order:
      - file.name
";

const TICKETS_BASE: &str = "\
filters:
  and:
    - file.hasTag(\"ticket\")
    - project.contains(link(this.file.name))
views:
  - type: table
    name: All
    order:
      - file.name
";

const FAKE_CLI: &str = r#"#!/bin/sh
# A stand-in for the Obsidian CLI: answers from $FAKE_ANSWERS, keyed by the base
# path, view and format. It exits 0 even when it has nothing to say, which is
# what Obsidian's bridge does when it is half up.
if [ "$1" = "version" ]; then
  printf '%s\n' "${FAKE_VERSION:-1.14.4}"
  exit 0
fi
base=""
format="json"
view=""
for arg in "$@"; do
  case "$arg" in
    path=*) base="${arg#path=}" ;;
    format=*) format="${arg#format=}" ;;
    view=*) view="${arg#view=}" ;;
  esac
done
if [ -n "${FAKE_MUTATE:-}" ]; then
  printf 'x' >> "$FAKE_MUTATE"
fi
answer="$FAKE_ANSWERS/$base/$view.$format"
if [ -f "$answer" ]; then
  cat "$answer"
fi
exit 0
"#;

struct Fixture {
    _tmp: tempfile::TempDir,
    vault: PathBuf,
    answers: PathBuf,
    config: PathBuf,
    data: PathBuf,
    cli: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let tmp = tempfile::tempdir().expect("a temp directory");
        let root = tmp.path();
        let vault = root.join("vault");
        std::fs::create_dir_all(vault.join("Notes")).expect("the fixture vault is writable");
        std::fs::write(vault.join("AllNotes.base"), BASE).expect("the base is writable");
        std::fs::write(
            vault.join("Notes/Alpha.md"),
            "---\ntags:\n  - note\nstatus: todo\n---\n\n# Alpha\n",
        )
        .expect("a note is writable");
        std::fs::write(
            vault.join("Notes/Beta.md"),
            "---\ntags:\n  - note\nstatus: done\n---\n\n# Beta\n",
        )
        .expect("a note is writable");

        let config = root.join("config");
        std::fs::create_dir_all(&config).expect("the config directory is writable");
        let registry = json!({ "vaults": { "fixture": { "path": vault.display().to_string() } } });
        std::fs::write(config.join("obsidian.json"), registry.to_string())
            .expect("the config is writable");

        let cli = root.join("fake-obsidian");
        std::fs::write(&cli, FAKE_CLI).expect("the fake CLI is writable");
        let mut mode = std::fs::metadata(&cli)
            .expect("the fake CLI exists")
            .permissions();
        mode.set_mode(0o755);
        std::fs::set_permissions(&cli, mode).expect("the fake CLI is executable");

        Self {
            answers: root.join("answers"),
            data: root.join("data"),
            _tmp: tmp,
            vault,
            config,
            cli,
        }
    }

    /// Add the `this`-scoped base to the fixture vault.
    fn with_tickets(self) -> Self {
        std::fs::write(self.vault.join("Tickets.base"), TICKETS_BASE)
            .expect("the tickets base is writable");
        // A ticket note with a `project` link, so the `this`-scoped filter is
        // actually reached. Without it the `and` short-circuits on the tag and a
        // missing Host note would never be evaluated.
        std::fs::write(
            self.vault.join("Notes/Gamma.md"),
            "---\ntags:\n  - ticket\nproject:\n  - \"[[Alpha]]\"\n---\n\n# Gamma\n",
        )
        .expect("a ticket note is writable");
        self
    }

    fn run(&self, args: &[&str]) -> Output {
        self.run_with(args, &[])
    }

    fn run_with(&self, args: &[&str], envs: &[(&str, &str)]) -> Output {
        let mut command = Command::new(BIN);
        command
            .args(args)
            .env("OBSIDIAN_BIN", &self.cli)
            .env("OBSIDIAN_CONFIG_DIR", &self.config)
            .env("FAKE_ANSWERS", &self.answers)
            .env_remove("OBSIDIAN_VAULT")
            .env_remove("FAKE_MUTATE")
            .env_remove("FAKE_VERSION");
        for (key, value) in envs {
            command.env(key, value);
        }
        command.output().expect("the binary runs")
    }

    fn record_args(&self) -> Vec<String> {
        vec![
            "record".into(),
            "--vault".into(),
            "vault".into(),
            "--vault-dir".into(),
            self.vault.display().to_string(),
            "--data-dir".into(),
            self.data.display().to_string(),
        ]
    }

    fn record(&self) -> Output {
        let args = self.record_args();
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        self.run(&refs)
    }

    /// Make the fake CLI answer with what our own engine answers, which is the
    /// only way an offline comparison can start out matching.
    fn answer_from_our_engine(&self) {
        let resolver = block_on(Resolver::open_dir(&self.vault)).expect("the fixture vault opens");
        for base in block_on(resolver.list_bases()).expect("the bases list") {
            for view in base.views {
                let options = QueryOptions {
                    view: Some(view.name.clone()),
                    context: None,
                };
                let (rows, markdown) = match block_on(resolver.query(&base.path, &options)) {
                    Ok(result) => {
                        let parsed =
                            block_on(resolver.load_base(&base.path)).expect("the base parses");
                        let rows = json_rows(&parsed, &result, &[]);
                        (
                            serde_json::to_string(&rows).expect("rows serialise"),
                            block_on(resolver.render(&base.path, &options))
                                .expect("our markdown renders"),
                        )
                    }
                    Err(error) if error.construct() == Some("this") => {
                        ("[]".to_string(), String::new())
                    }
                    Err(error) => panic!("{} [{}]: {error}", base.path, view.name),
                };
                self.write_answer(&base.path, &view.name, &rows, &markdown);
            }
        }
    }

    fn write_answer(&self, base: &str, view: &str, rows: &str, markdown: &str) {
        let dir = self.answers.join(base);
        std::fs::create_dir_all(&dir).expect("the answers directory is writable");
        std::fs::write(dir.join(format!("{view}.json")), rows).expect("the answer is writable");
        std::fs::write(dir.join(format!("{view}.md")), markdown).expect("the answer is writable");
    }

    fn answer_path(&self, base: &str, view: &str, format: &str) -> PathBuf {
        self.answers.join(base).join(format!("{view}.{format}"))
    }

    /// The one recorded corpus under the data root.
    fn corpus(&self) -> PathBuf {
        let dirs: Vec<PathBuf> = std::fs::read_dir(&self.data)
            .expect("the data root exists")
            .map(|entry| entry.expect("a readable entry").path())
            .filter(|path| path.is_dir())
            .collect();
        assert_eq!(dirs.len(), 1, "expected one corpus, got {dirs:?}");
        dirs.into_iter().next().expect("the corpus")
    }

    fn has_corpus(&self) -> bool {
        self.data.exists()
            && std::fs::read_dir(&self.data)
                .expect("the data root is readable")
                .any(|entry| entry.expect("a readable entry").path().is_dir())
    }
}

fn block_on<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a current-thread runtime")
        .block_on(future)
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "expected success, got {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        stdout(output),
        stderr(output)
    );
}

fn assert_failure(output: &Output, needle: &str) {
    assert!(
        !output.status.success(),
        "expected a failure, got success\nstdout:\n{}\nstderr:\n{}",
        stdout(output),
        stderr(output)
    );
    let stderr = stderr(output);
    assert!(
        stderr.contains(needle),
        "stderr did not contain {needle:?}:\n{stderr}"
    );
}

// ---------------------------------------------------------------------------
// record
// ---------------------------------------------------------------------------

#[test]
fn record_writes_a_corpus_keyed_by_the_digest_of_its_answers() {
    let fixture = Fixture::new();
    fixture.answer_from_our_engine();

    let output = fixture.record();
    assert_success(&output);

    let corpus = fixture.corpus();
    let name = corpus
        .file_name()
        .expect("the corpus has a name")
        .to_string_lossy()
        .into_owned();
    assert_eq!(name.len(), 64, "the directory is a sha256: {name}");
    assert!(
        name.chars().all(|c| c.is_ascii_hexdigit()),
        "the directory is hexadecimal: {name}"
    );

    let loaded = parity::load_corpus(&corpus).expect("the recorded corpus validates");
    assert_eq!(loaded.entries.len(), 2, "two views were recorded");
    assert_eq!(loaded.manifest.recorded_with.cli, "fake-obsidian");
    assert_eq!(loaded.manifest.recorded_with.version, "1.14.4");
    assert_eq!(loaded.manifest.recorded_with.vault, "vault");
    assert_eq!(parity::digest_of(&loaded.entries), name);

    let all = loaded
        .entries
        .iter()
        .find(|entry| entry.base == "AllNotes.base" && entry.view == "All")
        .expect("the table view was recorded");
    assert_eq!(all.rows.len(), 2, "both fixture notes are rows");
    assert!(all.markdown.contains("Alpha"), "the markdown was stored");
}

#[test]
fn record_refuses_a_vault_the_cli_is_not_answering_for() {
    let fixture = Fixture::new();
    fixture.answer_from_our_engine();
    let other = fixture
        .vault
        .parent()
        .expect("a parent")
        .join("other")
        .join("vault");
    std::fs::create_dir_all(&other).expect("the other directory is writable");
    let registry = json!({ "vaults": { "fixture": { "path": other.display().to_string() } } });
    std::fs::write(fixture.config.join("obsidian.json"), registry.to_string())
        .expect("the config is writable");

    let output = fixture.record();
    assert_failure(&output, "is registered as");
    let stderr = stderr(&output);
    assert!(
        stderr.contains(&other.display().to_string())
            && stderr.contains(&fixture.vault.display().to_string()),
        "the refusal names both directories:\n{stderr}"
    );
    assert!(!fixture.has_corpus(), "nothing may be recorded");
}

#[test]
fn record_refuses_empty_stdout_instead_of_recording_an_empty_result() {
    let fixture = Fixture::new();
    fixture.answer_from_our_engine();
    std::fs::write(fixture.answer_path("AllNotes.base", "All", "json"), "")
        .expect("the answer is writable");

    let output = fixture.record();
    assert_failure(&output, "empty stdout where rows were expected");
    assert_failure(&output, "does not flush stdout");
    assert!(!fixture.has_corpus(), "nothing may be recorded");
}

#[test]
fn record_refuses_a_vault_the_cli_rewrote() {
    let fixture = Fixture::new();
    fixture.answer_from_our_engine();
    let mutated = fixture.vault.join("AllNotes.base");

    let args = fixture.record_args();
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let output = fixture.run_with(
        &refs,
        &[("FAKE_MUTATE", mutated.to_str().expect("a UTF-8 path"))],
    );
    assert_failure(&output, "CHANGED the vault");
    assert_failure(&output, "AllNotes.base");
    assert!(!fixture.has_corpus(), "nothing may be recorded");
}

#[test]
fn record_refuses_a_corpus_directory_inside_the_vault() {
    let fixture = Fixture::new();
    fixture.answer_from_our_engine();
    let inside = fixture.vault.join("corpus");
    let args = [
        "record".to_string(),
        "--vault".to_string(),
        "vault".to_string(),
        "--vault-dir".to_string(),
        fixture.vault.display().to_string(),
        "--data-dir".to_string(),
        inside.display().to_string(),
    ];
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();

    let output = fixture.run(&refs);
    assert_failure(&output, "inside the vault");
    assert!(!inside.exists(), "the guard must not create it first");
}

#[test]
fn record_is_idempotent_for_identical_answers() {
    let fixture = Fixture::new();
    fixture.answer_from_our_engine();

    assert_success(&fixture.record());
    let corpus = fixture.corpus();
    let first_manifest = std::fs::read(corpus.join("manifest.json")).expect("a manifest");

    let output = fixture.record();
    assert_success(&output);
    assert!(
        stdout(&output).contains("already recorded"),
        "the second run reuses the corpus:\n{}",
        stdout(&output)
    );
    assert_eq!(
        std::fs::read(corpus.join("manifest.json")).expect("a manifest"),
        first_manifest,
        "the corpus was not rewritten"
    );
}

// ---------------------------------------------------------------------------
// check
// ---------------------------------------------------------------------------

#[test]
fn check_refuses_to_pass_without_a_corpus() {
    let fixture = Fixture::new();
    let output = fixture.run(&[
        "check",
        "--data-dir",
        fixture.data.to_str().expect("a UTF-8 path"),
    ]);
    assert_failure(&output, "there is no snapshot corpus");
    assert_failure(&output, "just record");
}

#[test]
fn check_accepts_a_recorded_corpus() {
    let fixture = Fixture::new();
    fixture.answer_from_our_engine();
    assert_success(&fixture.record());

    let output = fixture.run(&[
        "check",
        "--data-dir",
        fixture.data.to_str().expect("a UTF-8 path"),
    ]);
    assert_success(&output);
    assert!(
        stdout(&output).contains("2 entries"),
        "the check reports what it validated:\n{}",
        stdout(&output)
    );
}

#[test]
fn check_rejects_a_corpus_whose_bytes_do_not_match_the_manifest() {
    let fixture = Fixture::new();
    fixture.answer_from_our_engine();
    assert_success(&fixture.record());

    let markdown = fixture.corpus().join("AllNotes/All.md");
    let mut bytes = std::fs::read(&markdown).expect("the recorded markdown");
    bytes.push(0);
    std::fs::write(&markdown, bytes).expect("the markdown is writable");

    let output = fixture.run(&[
        "check",
        "--data-dir",
        fixture.data.to_str().expect("a UTF-8 path"),
    ]);
    assert_failure(&output, "does not match the digest");
    assert_failure(&output, "All.md");
}

#[test]
fn check_rejects_a_stray_file_in_a_corpus() {
    let fixture = Fixture::new();
    fixture.answer_from_our_engine();
    assert_success(&fixture.record());

    std::fs::write(
        fixture.corpus().join("scratch.txt"),
        "not part of the corpus",
    )
    .expect("the stray file is writable");

    let output = fixture.run(&[
        "check",
        "--data-dir",
        fixture.data.to_str().expect("a UTF-8 path"),
    ]);
    assert_failure(&output, "is not named by the manifest");
    assert_failure(&output, "scratch.txt");
}

// ---------------------------------------------------------------------------
// compare
// ---------------------------------------------------------------------------

#[test]
fn compare_matches_what_the_cli_answered() {
    let fixture = Fixture::new();
    fixture.answer_from_our_engine();
    assert_success(&fixture.record());

    let output = fixture.run(&[
        "compare",
        "--vault-dir",
        fixture.vault.to_str().expect("a UTF-8 path"),
        "--data-dir",
        fixture.data.to_str().expect("a UTF-8 path"),
    ]);
    assert_success(&output);
    assert!(
        stdout(&output).contains("2 entries compared, 0 skipped, 0 failed"),
        "the comparison covers the corpus:\n{}",
        stdout(&output)
    );
}

#[test]
fn compare_names_the_cell_that_differs() {
    let fixture = Fixture::new();
    fixture.answer_from_our_engine();
    let answer = fixture.answer_path("AllNotes.base", "All", "json");
    let mut rows: Vec<Value> =
        serde_json::from_str(&std::fs::read_to_string(&answer).expect("the answer is readable"))
            .expect("the answer is JSON");
    let key = rows[0]
        .as_object()
        .expect("a row is an object")
        .keys()
        .next()
        .expect("a column")
        .clone();
    rows[0]
        .as_object_mut()
        .expect("a row is an object")
        .insert(key.clone(), Value::String("theirs".into()));
    std::fs::write(
        &answer,
        serde_json::to_string(&rows).expect("rows serialise"),
    )
    .expect("the answer is writable");
    assert_success(&fixture.record());

    let output = fixture.run(&[
        "compare",
        "--vault-dir",
        fixture.vault.to_str().expect("a UTF-8 path"),
        "--data-dir",
        fixture.data.to_str().expect("a UTF-8 path"),
    ]);
    assert_failure(&output, "row 0, column");
    assert_failure(&output, &format!("column {key:?}"));
    assert_failure(&output, "theirs");
}

#[test]
fn compare_skips_a_this_scoped_base_and_requires_the_empty_answer() {
    let fixture = Fixture::new().with_tickets();
    fixture.answer_from_our_engine();
    assert_success(&fixture.record());

    let output = fixture.run(&[
        "compare",
        "--vault-dir",
        fixture.vault.to_str().expect("a UTF-8 path"),
        "--data-dir",
        fixture.data.to_str().expect("a UTF-8 path"),
    ]);
    assert_success(&output);
    assert!(
        stdout(&output).contains("this-scoped"),
        "the divergence is named:\n{}",
        stdout(&output)
    );
    assert!(
        stdout(&output).contains("1 skipped"),
        "the `this` view is skipped rather than compared:\n{}",
        stdout(&output)
    );
}

#[test]
fn compare_refuses_a_stale_corpus_when_the_vault_gained_a_view() {
    let fixture = Fixture::new();
    fixture.answer_from_our_engine();
    assert_success(&fixture.record());

    let base =
        std::fs::read_to_string(fixture.vault.join("AllNotes.base")).expect("the base is readable");
    std::fs::write(
        fixture.vault.join("AllNotes.base"),
        format!("{base}  - type: table\n    name: Extra\n    order:\n      - file.name\n"),
    )
    .expect("the base is writable");

    let output = fixture.run(&[
        "compare",
        "--vault-dir",
        fixture.vault.to_str().expect("a UTF-8 path"),
        "--data-dir",
        fixture.data.to_str().expect("a UTF-8 path"),
    ]);
    assert_failure(&output, "does not record it");
    assert_failure(&output, "Extra");
}

#[test]
fn compare_refuses_when_the_corpus_holds_rows_for_a_this_scoped_base() {
    let fixture = Fixture::new().with_tickets();
    fixture.answer_from_our_engine();
    // Deliberately record rows for a base the CLI cannot bind `this` in. That is
    // the future-Obsidian signal: it must fail, not quietly compare nothing.
    fixture.write_answer(
        "Tickets.base",
        "All",
        r#"[{"Name":"Gamma"}]"#,
        "| Name |\n| --- |\n| Gamma |\n",
    );
    assert_success(&fixture.record());

    let output = fixture.run(&[
        "compare",
        "--vault-dir",
        fixture.vault.to_str().expect("a UTF-8 path"),
        "--data-dir",
        fixture.data.to_str().expect("a UTF-8 path"),
    ]);
    assert_failure(&output, "needs a Host note");
    assert_failure(&output, "divergence registry");
}

// ---------------------------------------------------------------------------
// The digest
// ---------------------------------------------------------------------------

fn recorded(rows: Value, markdown: &str) -> Recorded {
    Recorded {
        base: "AllNotes.base".into(),
        view: "All".into(),
        rows: match rows {
            Value::Array(rows) => rows,
            other => panic!("rows must be an array: {other}"),
        },
        markdown: markdown.into(),
    }
}

#[test]
fn the_digest_ignores_key_order_but_not_values() {
    let ordered = recorded(json!([{ "Name": "Alpha", "Status": "todo" }]), "| a |\n");
    let reordered = recorded(json!([{ "Status": "todo", "Name": "Alpha" }]), "| a |\n");
    let changed = recorded(json!([{ "Name": "Alpha", "Status": "done" }]), "| a |\n");

    assert_eq!(
        parity::digest_of(std::slice::from_ref(&ordered)),
        parity::digest_of(&[reordered]),
        "JSON key order is spelling, not a different answer"
    );
    assert_ne!(
        parity::digest_of(std::slice::from_ref(&ordered)),
        parity::digest_of(&[changed]),
        "a different cell is a different corpus"
    );
    assert_ne!(
        parity::digest_of(std::slice::from_ref(&ordered)),
        parity::digest_of(&[recorded(
            json!([{ "Name": "Alpha", "Status": "todo" }]),
            "| b |\n"
        )]),
        "a different rendering is a different corpus"
    );
}

#[test]
fn the_digest_ignores_the_order_entries_were_recorded_in() {
    let a = recorded(json!([{ "Name": "Alpha" }]), "| a |\n");
    let mut b = recorded(json!([{ "Name": "Beta" }]), "| b |\n");
    b.view = "AsList".into();

    let forward = parity::digest_of(&[a.clone(), b.clone()]);
    let backward = parity::digest_of(&[b, a]);
    assert_eq!(
        forward, backward,
        "enumeration order is not part of the key"
    );
}
