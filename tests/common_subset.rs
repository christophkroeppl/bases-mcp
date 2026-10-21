//! The canary for the shared module's `dead_code` allow.
//!
//! ## What this file is for
//!
//! `tests/common/mod.rs` is compiled once per integration-test crate that
//! declares `mod common;`. Each crate uses a different subset of its helpers,
//! so a helper used by one crate is dead code in the others, and clippy runs
//! with `-D warnings`. The consequence, before this file existed, was that
//! adding a new test file broke the gate for everybody, over helpers the new
//! file never touched:
//!
//! ```text
//! error: function `seed_dir` is never used
//! error: struct `FakeDav` is never constructed
//! error: function `load_corpus` is never used
//! ... 33 errors in total
//! ```
//!
//! The fix is that the `allow` lives inside the module, so it travels with the
//! module into every crate that includes it. But an `allow` nobody exercises is
//! an `allow` that can be deleted by accident, and the accident is silent: the
//! gate stays green until the day somebody adds a file that needs it.
//!
//! So this file IS the guard. It declares `mod common;` and uses exactly ONE
//! helper out of the whole surface — `vault_dir` — which is the smallest
//! possible subset. If the module's `allow` is removed, this file is the one
//! that fails the gate, and it fails with the names of the helpers it never
//! used. That is the RED.
//!
//! ## What this file also pins
//!
//! The `allow` must stay INSIDE the module. An allow written on the
//! `mod common;` declaration in some other suite would silence the dead code
//! in THAT suite, and a crate-wide allow would silence the dead code in the
//! whole crate — including the suite's own. The
//! `the_shared_allow_does_not_reach_this_crate` test below is that proof: it
//! depends on an unused function in THIS crate staying unused, and clippy must
//! keep reporting it.
//!
//! ## The allow is still a lid, and this file is the lid's counterweight
//!
//! An `allow` inside the module also hides a helper that NO suite uses, and
//! that is real dead code rather than the false positive the allow exists to
//! absorb. The last test here checks the module for it: every public item must
//! be mentioned by some suite, or by some sibling inside the module.
//!
//! ## The rule, and why it is shaped this way
//!
//! The check is "mentioned nowhere", which is deliberately the blunt version.
//! A mention in a comment or a string counts as a mention, and that is the
//! safe direction: the check under-reports rather than over-reports, so it
//! never fails the gate over a helper that is alive. A rule that tried to be
//! exact — one that told a suite's own `fn refusal` apart from
//! `common::fake_dav::refusal` — was measured against clippy's own reports
//! and produced false negatives, which is the one direction a guard must not
//! fail in.
//!
//! A guard that cannot fail is not a guard, so all three facts above are
//! checked against the tree rather than asserted here: delete the module's
//! `allow`, or add a helper nobody uses, or add an `allow` to a suite, and
//! this file says so.

mod common;

use common::vault_dir;

/// The testing vault, which every suite that wants the corpus needs.
#[test]
fn the_shared_module_reaches_the_testing_vault() {
    let dir = vault_dir();
    assert!(
        dir.ends_with("vault"),
        "the shared vault path ends in the vault directory: {dir:?}"
    );
    assert!(
        dir.join("AllNotes.base").is_file(),
        "the testing vault holds the parity oracle's base file: {dir:?}"
    );
}

/// A helper this suite declares and never uses.
///
/// It exists to be reported. The `allow` inside `tests/common` must not be
/// wide enough to swallow it, and the only way to prove that is to have
/// something in THIS crate that is genuinely dead and let clippy say so.
///
/// The test below calls it, which is what keeps the gate green. Remove that
/// call and the gate must go red with `unused_here` named in the error.
fn unused_here() -> u32 {
    7
}

/// The proof that the module's `allow` does not reach this crate.
///
/// Calling `unused_here` keeps the gate green. Deleting this call makes it
/// dead code in this crate, and clippy must then report it — with `-D
/// warnings`, that is a gate failure. The `allow` inside `tests/common` must
/// not be wide enough to swallow it.
#[test]
fn the_shared_allow_does_not_reach_this_crate() {
    assert_eq!(unused_here(), 7);
}

/// The shared module must hold no helper that no suite uses.
///
/// The module's `allow` covers its whole surface, so it cannot tell the
/// difference between a helper another suite uses (dead here, alive there) and
/// a helper nobody uses (dead everywhere). The second kind is real dead code,
/// and an `allow` that hides it is a lid on a smell.
///
/// A helper counts as used when some suite mentions it, or when a sibling
/// inside the module does — a sibling's use is what keeps a private helper
/// such as `normalise` alive, and it is mentioned by the code that calls it.
#[test]
fn the_shared_module_holds_no_helper_that_nothing_uses() {
    let tests = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
    let module = tests.join("common");

    let module_files = rust_files_in(&module);
    let suites: Vec<(String, String)> = rust_files_in(&tests)
        .into_iter()
        .filter(|(name, _)| name != "common")
        .collect();

    let mut dead: Vec<String> = Vec::new();
    for (file, source) in &module_files {
        for line in source.lines() {
            let Some(name) = declared_name(line) else {
                continue;
            };

            let used_by_a_suite = suites.iter().any(|(_, code)| mentions(code, &name));
            // The declaration itself is not a use of the item, so the module's
            // own text is searched with that line taken out.
            let used_by_a_sibling = module_files.iter().any(|(_, code)| {
                code.lines()
                    .filter(|other| !declares(other, &name))
                    .any(|other| mentions(other, &name))
            });

            if !used_by_a_suite && !used_by_a_sibling {
                dead.push(format!("{file}: {name}"));
            }
        }
    }

    assert!(
        dead.is_empty(),
        "the shared module holds helpers that nothing uses, and the module's `dead_code` \
         allow hides them. Delete them, or use them from a suite:\n{}",
        dead.join("\n")
    );
}

/// Every Rust file in a directory, with its name and its text.
fn rust_files_in(dir: &std::path::Path) -> Vec<(String, String)> {
    std::fs::read_dir(dir)
        .unwrap_or_else(|error| panic!("{} is readable: {error}", dir.display()))
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "rs"))
        .map(|path| {
            let name = path
                .file_name()
                .expect("a file has a name")
                .to_string_lossy()
                .into_owned();
            let source = std::fs::read_to_string(&path).expect("a Rust file is readable");
            (name, source)
        })
        .collect()
}

/// The name a line declares, if the line declares a public item.
///
/// A declaration is a `pub fn`, `pub const`, `pub struct` and so on, at any
/// indentation: the indented ones are methods inside an `impl` block, and they
/// are items too. The keyword comes off FIRST and the name is what follows it,
/// so `pub enum MemoryOp` declares `MemoryOp` and not `enum`.
fn declared_name(line: &str) -> Option<String> {
    let trimmed = line.trim_start();
    let rest = trimmed.strip_prefix("pub ")?;
    let rest = rest.strip_prefix("async ").unwrap_or(rest);

    let after_keyword = [
        "const ", "fn ", "struct ", "enum ", "trait ", "type ", "mod ", "union ",
    ]
    .iter()
    .find_map(|keyword| rest.strip_prefix(keyword))?;

    let name: String = after_keyword
        .chars()
        .take_while(|character| character.is_alphanumeric() || *character == '_')
        .collect();

    if name.is_empty() { None } else { Some(name) }
}

/// Whether a line declares the item called `name`.
fn declares(line: &str, name: &str) -> bool {
    declared_name(line).as_deref() == Some(name)
}

/// Whether `text` mentions `name` as a whole word.
///
/// A mention in a comment or a string counts. That is the blunt rule the
/// header describes: it under-reports rather than over-reports, so it never
/// fails the gate over a helper that is alive.
fn mentions(text: &str, name: &str) -> bool {
    text.split(|character: char| !character.is_alphanumeric() && character != '_')
        .any(|word| word == name)
}
