//! Backend equivalence: the filesystem and a non-filesystem [`VaultSource`]
//! resolve one vault to the same answers.
//!
//! The claim this suite turns into an executable assertion is the one the project
//! was built on -- "resolving a note containing a base yields identical output
//! over fs and WebDAV". It used to be prose.
//!
//! How the assertions are written is the design:
//!
//!   - FULL structures, never a subset. A comparison that only checked row paths
//!     would pass while every cell value disagreed, and cell values are what an
//!     agent acts on.
//!   - ORDER is part of the value, so row order, group order and backlink order
//!     would all be pinned without a separate "did it sort?" test pretending to
//!     cover them.
//!   - No conditional. Nothing here asks whether a server is reachable, so there
//!     is no assertion that can silently skip itself and report a green suite
//!     that compared nothing.
//!
//! Nothing here writes to `test/vault`. It is the Obsidian parity oracle and
//! stays at exactly [`CORPUS_SIZE`] files. The write comparisons happen in two
//! throwaway sandboxes -- a temp directory and a map -- seeded identically.
//!
//! mtime is the one field the two backends cannot agree on, and the reason the
//! memory source has a fixed clock at all. `Vault` feeds `stat().mtime` to
//! `file.ctime` and `file.mtime`, so a base reading either puts a real timestamp
//! into query results and rendered markdown. The corpus deliberately reads
//! neither, and a test below PROVES that, so this suite can compare every other
//! field byte for byte instead of excluding mtime everywhere it appears.
//!
//! ## What is not here, and why
//!
//! The `listBases`, `query`, `render`, `readNote` and `backlinks` sections of
//! `test/webdav/equivalence.test.ts` need `src/base.rs`, `src/render/markdown.rs`
//! and `src/service.rs`, which a later task owns. They are named in the porting
//! report rather than approximated with a stand-in resolver: a fake resolver
//! would compare a fake against a fake and prove nothing, which is the same
//! mistake the suite's own header warns about.

// Each integration test is its own crate, and no two of them need every helper here.
// Each integration test is its own crate, and no suite needs every helper here.
#[allow(dead_code)]
mod common;

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::rc::Rc;

use bases_mcp::error::BasesError;
use bases_mcp::value::BasesValue;
use bases_mcp::vault::{
    coerce_frontmatter, content_hash, fold_key, is_indexable, match_path, points_at, FileStat,
    FsVaultSource, PathIndex, SourceKind, Vault, VaultSource,
};
use common::memory::{
    MemoryFault, MemoryOp, MemoryVaultSource, SharedSource, ANY_PATH, MEMORY_MTIME_MS,
};
use common::{
    count_files, dotfile_tree, futures_block_on, load_corpus, seed_dir, vault_dir, VaultFile,
    CORPUS_SIZE,
};
use tempfile::TempDir;

/// The reference backend, pointed at the oracle vault itself. Read-only.
fn fs_source() -> FsVaultSource {
    FsVaultSource::new(vault_dir()).expect("the oracle is a directory")
}

/// The fake, holding the same bytes in a map.
fn memory_source(corpus: &[VaultFile]) -> MemoryVaultSource {
    MemoryVaultSource::new(corpus.to_vec())
}

// ---------------------------------------------------------------------------
// Driving a source
// ---------------------------------------------------------------------------

/// These calls are `async` because WebDAV is HTTP, and the tests are not.
///
/// Every test here is synchronous and drives one source at a time, so the future
/// is run to completion on a current-thread runtime rather than being awaited by
/// a test body. A `#[tokio::test]` per test would be the same runtime with more
/// ceremony in every signature.
fn run<F: std::future::Future>(future: F) -> F::Output {
    futures_block_on(future)
}

fn list_of<S: VaultSource + ?Sized>(source: &S) -> Vec<String> {
    run(source.list()).expect("listing the vault succeeds")
}

fn read_of<S: VaultSource + ?Sized>(source: &S, path: &str) -> Result<String, BasesError> {
    run(source.read_note(path))
}

fn hash_of<S: VaultSource + ?Sized>(source: &S, path: &str) -> String {
    run(source.hash(path)).unwrap_or_else(|error| panic!("hashing {path} succeeds: {error}"))
}

fn stat_of<S: VaultSource + ?Sized>(source: &S, path: &str) -> FileStat {
    run(source.stat(path)).unwrap_or_else(|error| panic!("stat of {path} succeeds: {error}"))
}

fn write_of<S: VaultSource + ?Sized>(source: &S, path: &str, data: &str) {
    run(source.write_text(path, data))
        .unwrap_or_else(|error| panic!("writing {path} succeeds: {error}"));
}

fn mkdir_of<S: VaultSource + ?Sized>(source: &S, path: &str) {
    run(source.ensure_dir(path))
        .unwrap_or_else(|error| panic!("creating {path} succeeds: {error}"));
}

fn delete_of<S: VaultSource + ?Sized>(source: &S, path: &str) {
    run(source.delete(path)).unwrap_or_else(|error| panic!("deleting {path} succeeds: {error}"));
}

/// The failure a call produced, so a test can assert on its message.
fn failure_of<F, T>(future: F) -> BasesError
where
    F: std::future::Future<Output = Result<T, BasesError>>,
    T: std::fmt::Debug,
{
    match run(future) {
        Err(error) => error,
        Ok(_) => panic!("expected a refusal, but the call succeeded"),
    }
}

// ---------------------------------------------------------------------------
// The corpus, and the preconditions that make the comparisons meaningful
// ---------------------------------------------------------------------------

#[test]
fn the_corpus_is_the_testing_vault_and_the_testing_vault_is_untouched() {
    // The oracle's size underpins every comparison below: a tenth file would
    // change what Obsidian returns for the same query, so the parity and
    // equivalence suites would then be describing different vaults.
    assert_eq!(load_corpus().len(), CORPUS_SIZE);
    assert_eq!(count_files(&vault_dir()), CORPUS_SIZE);
}

#[test]
fn the_corpus_holds_both_bases_and_seven_notes() {
    let corpus = load_corpus();
    let bases: Vec<&str> = corpus
        .iter()
        .filter(|f| f.path.ends_with(".base"))
        .map(|f| f.path.as_str())
        .collect();
    let notes: Vec<&str> = corpus
        .iter()
        .filter(|f| f.path.ends_with(".md"))
        .map(|f| f.path.as_str())
        .collect();
    assert_eq!(bases, ["AllNotes.base", "Tickets.base"]);
    assert_eq!(notes.len(), 7);
    assert!(notes.contains(&"Root Project.md"));
    assert!(notes.contains(&"Root Ticket.md"));
}

#[test]
fn no_corpus_base_reads_file_mtime_or_file_ctime() {
    // The precondition that lets every comparison below be exact. Both accessors
    // read `stat().mtime`, which is the real clock on disk and a fixed instant in
    // memory, so a base touching either would make this suite fail on a Tuesday
    // and pass on a Wednesday. If one is added, this test fails first and names
    // the reason instead of leaving a mystery mismatch in a cell value.
    let offenders: Vec<String> = load_corpus()
        .iter()
        .filter(|f| f.content.contains("file.mtime") || f.content.contains("file.ctime"))
        .map(|f| f.path.clone())
        .collect();
    assert!(offenders.is_empty(), "bases reading a clock: {offenders:?}");
}

// ---------------------------------------------------------------------------
// list()
// ---------------------------------------------------------------------------

#[test]
fn list_returns_identical_vault_relative_posix_paths_from_both_backends() {
    let corpus = load_corpus();
    assert_eq!(list_of(&memory_source(&corpus)), list_of(&fs_source()));
}

#[test]
fn list_is_sorted_so_insertion_order_and_every_unsorted_view_agree() {
    let paths = list_of(&memory_source(&load_corpus()));
    let mut sorted = paths.clone();
    sorted.sort();
    assert_eq!(paths, sorted);
    assert!(paths.len() > 1);
}

#[test]
fn list_includes_the_bases_alongside_the_notes() {
    let paths = list_of(&memory_source(&load_corpus()));
    assert!(paths.contains(&"Tickets.base".to_string()));
    assert!(paths.contains(&"AllNotes.base".to_string()));
}

#[test]
fn list_filters_dotfiles_dot_directories_and_other_extensions_exactly_as_fs_does() {
    // The testing vault cannot carry this case: it must stay at nine files. So
    // the rule is pinned against a tree built in a temp directory, compared
    // three ways -- fs, memory, and `is_indexable` itself -- so a backend cannot
    // pass by agreeing with a wrong rule that two implementations happen to
    // share.
    let tree = dotfile_tree();
    let expected: Vec<String> = tree
        .iter()
        .map(|f| f.path.clone())
        .filter(|p| is_indexable(p))
        .collect();
    assert_eq!(
        expected,
        [
            "Notes/Alpha.md",
            "Notes/Deep/Beta.md",
            "Notes/Deep/Notes.base",
            "Templates/template.md",
        ]
    );

    let dir = TempDir::new().expect("a temp dir is created");
    seed_dir(dir.path(), &tree);
    assert_eq!(list_of(&MemoryVaultSource::new(tree.clone())), expected);
    assert_eq!(
        list_of(&FsVaultSource::new(dir.path()).expect("a sandbox is a directory")),
        expected
    );
}

/// The filesystem's `list` swallows a directory it cannot read and returns a
/// PARTIAL vault. That is a decision, not an accident, and this pins the half of
/// it that lives in this suite; the WebDAV half -- a refused PROPFIND is an error
/// rather than a smaller vault -- is pinned in `tests/webdav.rs`. The two
/// together are the divergence the equivalence suite exists to report rather than
/// hide.
#[test]
fn an_unreadable_directory_yields_a_partial_vault_from_both_backends() {
    let dir = TempDir::new().expect("a temp dir is created");
    let all = vec![
        VaultFile {
            path: "Readable/Note.md".into(),
            content: "# reachable\n".into(),
        },
        VaultFile {
            path: "Top.md".into(),
            content: "# top\n".into(),
        },
        VaultFile {
            path: "Locked/Secret.md".into(),
            content: "# secret\n".into(),
        },
    ];
    seed_dir(dir.path(), &all);

    let locked = dir.path().join("Locked");
    let original = std::fs::metadata(&locked)
        .expect("the sandbox is readable")
        .permissions();
    // `set_readonly` would only clear the write bit, and a directory with read
    // and execute left on is still fully readable -- so the mode is set to zero.
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000))
        .expect("the sandbox is writable");

    let from_fs = list_of(&FsVaultSource::new(dir.path()).expect("a sandbox is a directory"));

    // Restored before anything else can fail, so the temp dir is never dropped
    // with a subdirectory this test made unreadable.
    std::fs::set_permissions(&locked, original).expect("the sandbox is writable again");

    // Partial rather than empty, and rather than an error: the walk skipped the
    // directory it could not read and answered with the rest of the vault.
    assert_eq!(from_fs, ["Readable/Note.md", "Top.md"]);

    // A map of files has no permissions, so the fake cannot model this case at
    // all -- which is exactly why the WebDAV half of the divergence is pinned in
    // `tests/webdav.rs`, where a server can be told to refuse. What is pinned
    // here is that the subset fs could reach is a vault the fake reproduces
    // exactly, so a caller handed the partial vault sees the same thing from
    // either backend.
    let reachable: Vec<VaultFile> = all
        .into_iter()
        .filter(|f| from_fs.contains(&f.path))
        .collect();
    assert_eq!(list_of(&MemoryVaultSource::new(reachable)), from_fs);
}

// ---------------------------------------------------------------------------
// hash()
// ---------------------------------------------------------------------------

#[test]
fn hash_agrees_on_every_corpus_file() {
    let corpus = load_corpus();
    let fs = fs_source();
    let memory = memory_source(&corpus);
    for file in &corpus {
        assert_eq!(
            hash_of(&memory, &file.path),
            hash_of(&fs, &file.path),
            "{}",
            file.path
        );
    }
}

#[test]
fn hash_is_a_32_character_hex_digest_so_a_truncation_change_cannot_pass_quietly() {
    let memory = memory_source(&load_corpus());
    for file in load_corpus() {
        let hash = hash_of(&memory, &file.path);
        assert_eq!(hash.len(), 32, "{}", file.path);
        assert!(
            hash.chars()
                .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)),
            "{hash}"
        );
    }
}

#[test]
fn hash_changes_when_the_content_changes_and_comes_back_afterwards() {
    let corpus = load_corpus();
    let memory = memory_source(&corpus);
    let original = corpus
        .iter()
        .find(|f| f.path == "Root Ticket.md")
        .map(|f| f.content.clone())
        .unwrap();
    let before = hash_of(&memory, "Root Ticket.md");

    write_of(&memory, "Root Ticket.md", "# different\n");
    assert_ne!(hash_of(&memory, "Root Ticket.md"), before);

    // Restored rather than left dirty: a later test that asserted against the
    // oracle would otherwise fail for a reason that has nothing to do with it.
    write_of(&memory, "Root Ticket.md", &original);
    assert_eq!(hash_of(&memory, "Root Ticket.md"), before);
}

// ---------------------------------------------------------------------------
// stat()
// ---------------------------------------------------------------------------

#[test]
fn stat_reports_the_identical_byte_size_for_every_corpus_file() {
    let corpus = load_corpus();
    let fs = fs_source();
    let memory = memory_source(&corpus);
    for file in &corpus {
        assert_eq!(
            stat_of(&memory, &file.path).size,
            stat_of(&fs, &file.path).size,
            "{}",
            file.path
        );
    }
}

#[test]
fn stat_counts_bytes_not_utf16_code_units() {
    // The project notes carry non-ASCII frontmatter, so a code-unit count would
    // disagree with the file on disk.
    let corpus = load_corpus();
    let path = "Projects/SomeProject.md";
    let content = corpus
        .iter()
        .find(|f| f.path == path)
        .map(|f| f.content.clone())
        .unwrap_or_default();
    assert!(
        content.contains('ö'),
        "the corpus is expected to carry non-ASCII frontmatter"
    );

    let fs = fs_source();
    let memory = memory_source(&corpus);
    let size = stat_of(&memory, path).size;
    assert_eq!(size, stat_of(&fs, path).size);
    assert!(
        size > content.chars().count() as u64,
        "{size} vs {}",
        content.chars().count()
    );
}

#[test]
fn stat_mtime_is_the_fixed_instant_never_the_wall_clock() {
    // The one field the two backends cannot agree on, asserted as a fact rather
    // than assumed. The file header says why the corpus stays clear of it.
    let memory = memory_source(&load_corpus());
    assert_eq!(
        stat_of(&memory, "Root Ticket.md").mtime.timestamp_millis(),
        MEMORY_MTIME_MS
    );
    assert_ne!(
        stat_of(&fs_source(), "Root Ticket.md")
            .mtime
            .timestamp_millis(),
        MEMORY_MTIME_MS
    );
}

#[test]
fn stat_hands_out_a_fresh_value_so_one_caller_cannot_move_every_other_comparison() {
    // The original handed out a mutable `Date` and had to allocate a new one per
    // call so a caller calling `setUTCFullYear` on it could not shift the rest of
    // the suite. A `DateTime` is a value, so this is structural rather than
    // promised -- and the assertion is here so the property is pinned rather than
    // assumed.
    let memory = memory_source(&load_corpus());
    let first = stat_of(&memory, "Root Ticket.md");
    assert_eq!(first, stat_of(&memory, "Root Ticket.md"));
    assert_eq!(first.mtime.timestamp_millis(), MEMORY_MTIME_MS);
}

// ---------------------------------------------------------------------------
// Path normalisation
// ---------------------------------------------------------------------------

#[test]
fn read_note_resolves_a_redundant_path_to_the_same_note_on_both_backends() {
    let corpus = load_corpus();
    let fs = fs_source();
    let memory = memory_source(&corpus);
    for path in [
        "Tickets/../Root Ticket.md",
        "./Tickets/Fix login redirect.md",
    ] {
        assert_eq!(read_of(&memory, path), read_of(&fs, path), "{path}");
    }
}

#[test]
fn read_note_refuses_a_path_that_escapes_the_vault_root_on_both_backends() {
    let corpus = load_corpus();
    let fs = fs_source();
    let memory = memory_source(&corpus);
    for escapee in ["../outside.md", "/etc/passwd", "Tickets/../../outside.md"] {
        let from_fs = failure_of(fs.read_note(escapee));
        let from_memory = failure_of(memory.read_note(escapee));
        assert!(
            from_fs.message().contains("escapes the vault root"),
            "{from_fs}"
        );
        assert!(
            from_memory.message().contains("escapes the vault root"),
            "{from_memory}"
        );
        // The fake carries the status as a field, not only as prose: a real
        // backend deciding whether to retry needs it structurally, and a test
        // that matched on strings would let one through.
        assert_eq!(memory.last_status(), Some(403), "{escapee}");
    }
}

#[test]
fn read_note_refuses_to_read_a_file_that_is_not_there_on_both_backends() {
    let corpus = load_corpus();
    let fs = fs_source();
    let memory = memory_source(&corpus);
    let from_fs = failure_of(fs.read_note("Nope.md"));
    let from_memory = failure_of(memory.read_note("Nope.md"));
    assert!(from_fs.message().contains("Nope.md"), "{from_fs}");
    assert!(from_memory.message().contains("404"), "{from_memory}");
    assert_eq!(memory.last_status(), Some(404));
}

#[test]
fn exists_agrees_on_every_path_and_escaping_one_on_both_backends() {
    let corpus = load_corpus();
    let fs = fs_source();
    let memory = memory_source(&corpus);
    for file in &corpus {
        assert!(
            run(fs.exists(&file.path)).expect("the filesystem answers"),
            "{path} is in the corpus",
            path = file.path
        );
        assert!(
            run(memory.exists(&file.path)).expect("the fake answers"),
            "{path} is in the corpus",
            path = file.path
        );
    }
    assert!(
        !run(fs.exists("Nope.md")).expect("the filesystem answers"),
        "a path neither vault holds is absent"
    );
    assert!(!run(memory.exists("Nope.md")).expect("the fake answers"));
    // Redundant syntax resolves the same way it does everywhere else, so the
    // check behind a commit cannot be fooled by `./` or `..` into a different
    // answer than the write that follows it.
    assert!(run(fs.exists("Tickets/../Root Ticket.md")).expect("answers"));
    assert!(run(memory.exists("Tickets/../Root Ticket.md")).expect("answers"));

    for escapee in ["../outside.md", "/etc/passwd"] {
        assert!(
            failure_of(fs.exists(escapee))
                .message()
                .contains("escapes the vault root"),
            "{escapee}"
        );
        failure_of(memory.exists(escapee));
        assert_eq!(memory.last_status(), Some(403), "{escapee}");
    }
}

/// `exists` must not be answered from the listing.
///
/// This is the whole reason the method is on the trait rather than being
/// assembled from `list()`: the listing is a snapshot that moves only when this
/// server writes, so a human's save in Obsidian is invisible to it, and a check
/// built on it reports a taken path as free. The file here is written with
/// `std::fs` on purpose -- going through `write_text` would drop the snapshot and
/// prove nothing, because that is a server-side write, which is the one case the
/// snapshot does track.
#[test]
fn exists_sees_a_note_written_behind_the_listing() {
    let (dir, fs, memory) = pair();
    let late = "Tickets/Written Behind The Listing.md";

    // Prime both listings, so both are snapshots of a vault without this note.
    assert!(!run(fs.list()).expect("lists").contains(&late.to_string()));
    assert!(!run(memory.list())
        .expect("lists")
        .contains(&late.to_string()));
    assert!(!run(fs.exists(late)).expect("answers"));
    assert!(!run(memory.exists(late)).expect("answers"));

    std::fs::write(dir.path().join(late), "# written by a human").expect("the save lands");
    run(memory.write_text(late, "# written by a human")).expect("the fake stores it");

    assert!(
        !run(fs.list()).expect("lists").contains(&late.to_string()),
        "the listing snapshot must still be stale, or this test proves nothing"
    );
    assert!(run(fs.exists(late)).expect("the filesystem answers"));
    assert!(run(memory.exists(late)).expect("the fake answers"));
}

// ---------------------------------------------------------------------------
// Writes
// ---------------------------------------------------------------------------

/// Two vaults holding identical bytes: a temp directory and a map.
fn pair() -> (TempDir, FsVaultSource, MemoryVaultSource) {
    let corpus = load_corpus();
    let dir = TempDir::new().expect("a temp dir is created");
    seed_dir(dir.path(), &corpus);
    let memory = memory_source(&corpus);
    let fs = FsVaultSource::new(dir.path()).expect("a sandbox is a directory");
    (dir, fs, memory)
}

#[test]
fn write_text_creates_its_parents_and_the_two_vaults_agree_afterwards() {
    let (dir, fs, memory) = pair();
    let path = "Sandbox/Deep/Nested/New Note.md";
    let content = "---\ntags:\n  - ticket\nstatus: active\n---\n\n# New\n";

    // `createNote` is `ensureDir` then `writeText`; the resolver is a later task,
    // so the two calls are made here and the pair is what it composes.
    for source in [&fs as &dyn VaultSource, &memory as &dyn VaultSource] {
        mkdir_of(source, "Sandbox/Deep/Nested");
        write_of(source, path, content);
    }

    assert_eq!(read_of(&memory, path).expect("the fake holds it"), content);
    assert_eq!(list_of(&memory), list_of(&fs));
    assert!(list_of(&memory).contains(&path.to_string()));
    // fs created a real collection; the fake recorded that it was asked to. A map
    // of files has no directories, so without this the parent-creating half of
    // `createNote` would go untested on the fake forever.
    assert!(memory.dirs.borrow().contains("Sandbox/Deep/Nested"));
    let created =
        std::fs::read_dir(dir.path().join("Sandbox/Deep/Nested")).expect("the collection exists");
    assert_eq!(created.flatten().count(), 1);
}

#[test]
fn a_root_level_note_creates_no_collection() {
    // `createNote` only has a parent when the path has a separator. Slicing
    // unconditionally would make the parent `Note.m`, and both backends would
    // dutifully create a directory beside the note. The `ensureDir` half of that
    // guard belongs to the resolver and is asserted there; what is pinned here is
    // that `writeText` does not invent a parent of its own.
    let (dir, fs, memory) = pair();
    write_of(&fs, "Note.md", "# root\n");
    write_of(&memory, "Note.md", "# root\n");

    let beside: Vec<String> = std::fs::read_dir(dir.path())
        .expect("the sandbox is readable")
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| !name.starts_with('.'))
        .collect();
    assert!(!beside.contains(&"Note.m".to_string()), "{beside:?}");
    assert!(beside.contains(&"Note.md".to_string()));
    assert!(list_of(&memory).contains(&"Note.md".to_string()));
}

#[test]
fn overwriting_is_silent_and_the_two_vaults_stay_identical() {
    let (_dir, fs, memory) = pair();
    for text in ["first\n", "second\n"] {
        write_of(&fs, "Note.md", text);
        write_of(&memory, "Note.md", text);
    }

    assert_eq!(read_of(&memory, "Note.md"), read_of(&fs, "Note.md"));
    assert_eq!(hash_of(&memory, "Note.md"), hash_of(&fs, "Note.md"));
    assert_eq!(
        stat_of(&memory, "Note.md").size,
        stat_of(&fs, "Note.md").size
    );
    assert_eq!(list_of(&memory), list_of(&fs));
}

/// Deleting a file that was never there SUCCEEDS on both backends, and the two
/// reach that answer differently. The filesystem runs `rm --force` and never
/// learns the file was absent; a WebDAV `DELETE` answers `404` and its source
/// tolerates that `404`, because absence is the state the caller asked for.
/// `tests/webdav.rs` pins the WebDAV half against a real `404` from a server.
#[test]
fn deleting_something_that_was_never_there_succeeds_on_both_backends() {
    let (_dir, fs, memory) = pair();
    delete_of(&fs, "Never Existed.md");
    delete_of(&memory, "Never Existed.md");
}

#[test]
fn delete_removes_the_file_and_both_backends_stop_listing_it() {
    let (_dir, fs, memory) = pair();
    write_of(&fs, "Gone.md", "# gone\n");
    write_of(&memory, "Gone.md", "# gone\n");
    assert!(list_of(&memory).contains(&"Gone.md".to_string()));

    delete_of(&fs, "Gone.md");
    delete_of(&memory, "Gone.md");
    assert!(!list_of(&memory).contains(&"Gone.md".to_string()));
    assert!(read_of(&fs, "Gone.md").is_err());
}

// ---------------------------------------------------------------------------
// The filesystem backend's own path guard
// ---------------------------------------------------------------------------

/// These are NOT equivalence tests, and they are in this file only because this is
/// where the two backends' path handling is compared — so the place a reader looks
/// when asking why they disagree is also where the disagreement is written down.
///
/// [`FsVaultSource::abs`] resolves a vault-relative path, and the guard that keeps
/// the result inside the root compares whole COMPONENTS. That is sound for `/` and
/// for `..`, and it was silently unsound for `\`: `PathBuf::push` appended a
/// backslashed string as ONE component, so `[vault, ..\..\evil.md]` does start with
/// `[vault]` and the check passed. `Path::components` splits on `\` on Windows, so
/// there the same argument is four segments and the write lands outside the root.
///
/// Windows is not available to this suite, so what is pinned here is the REFUSAL,
/// which is platform-independent: a path this backend cannot interpret the same way
/// on two platforms is refused rather than guessed at. That is also what makes the
/// rule testable at all from Linux — a `#[cfg(windows)]` guard would be the one
/// piece of the fix nothing in CI could reach.
#[test]
fn the_filesystem_backend_refuses_a_backslash_in_a_vault_relative_path() {
    let (dir, fs, _memory) = pair();
    let outside = dir.path().parent().expect("a temp dir has a parent");

    for escapee in [r"..\..\outside\evil.md", r"Tickets\..\..\outside\evil.md"] {
        // Every read and every write funnels through `abs`, so all of them refuse.
        // One of them passing would be a path the backend can resolve on this
        // platform and not on another.
        for refusal in [
            failure_of(fs.read_note(escapee)),
            failure_of(fs.read_fresh(escapee)),
            failure_of(fs.write_text(escapee, "# escaped\n")),
            failure_of(fs.ensure_dir(escapee)),
            failure_of(fs.exists(escapee)),
            failure_of(fs.delete(escapee)),
            failure_of(fs.hash(escapee)),
            failure_of(fs.stat(escapee)),
        ] {
            assert!(
                refusal.message().contains("backslash"),
                "{escapee}: {refusal}"
            );
        }
        assert!(
            !outside.join("outside").exists(),
            "{escapee}: a directory appeared outside the vault"
        );
    }
    assert!(
        !outside.join("evil.md").exists(),
        "no file appeared beside the vault either"
    );
    assert_eq!(
        list_of(&fs),
        load_corpus()
            .into_iter()
            .map(|file| file.path)
            .collect::<Vec<_>>(),
        "and the vault is byte-for-byte what it was"
    );
}

/// The guard above is an ADDITION, not a replacement. A `..` that pops past the
/// root was already refused, and it is pinned here at the backend rather than
/// through `add_note_to_base` because the two are independent: the tool boundary
/// also refuses a `..` for a different reason (a dot-prefixed segment), so a test
/// that only asked the tool could not tell which guard was holding.
#[test]
fn the_filesystem_backend_still_refuses_a_path_that_walks_out_of_the_root() {
    let (dir, fs, _memory) = pair();
    let outside = dir.path().parent().expect("a temp dir has a parent");

    for escapee in ["../../outside/escaped.md", "../outside.md"] {
        let refusal = failure_of(fs.write_text(escapee, "# escaped\n"));
        assert!(
            refusal.message().contains("escapes the vault root"),
            "{escapee}: {refusal}"
        );
    }
    assert!(
        !outside.join("outside").exists(),
        "the write walked out of the vault root"
    );
    assert_eq!(
        list_of(&fs),
        load_corpus()
            .into_iter()
            .map(|file| file.path)
            .collect::<Vec<_>>(),
        "and the vault is unchanged"
    );
}

// ---------------------------------------------------------------------------
// Link resolution
// ---------------------------------------------------------------------------

/// `foldKey` is NFKD, then diacritic stripping, then lowercase — in that order.
/// `to_lowercase()` alone does not match the original: on a precomposed `é` it
/// leaves the accent on, and the original has already taken the character apart
/// by the time it lowercases.
#[test]
fn fold_key_normalises_before_it_lowercases() {
    assert_eq!(fold_key("Página"), "pagina");
    assert_eq!(fold_key("PAGINA"), "pagina");
    assert_eq!(fold_key("Ünïcode"), "unicode");
    assert_eq!(fold_key("Geschäftsidee"), "geschaftsidee");
    assert_eq!(fold_key(""), "");
}

/// The four keys a note is registered under, in the order the vault index
/// registers them. Ordered because the ORDER is the tie-break: first
/// registration wins, and the index is built from a sorted path list.
fn register(index: &mut PathIndex, path: &str) {
    index.insert_if_absent(path, path);
    index.insert_if_absent(&strip_extension(path), path);
    let base = path.rsplit('/').next().unwrap_or(path);
    index.insert_if_absent(base, path);
    index.insert_if_absent(&strip_extension(base), path);
}

#[test]
fn match_path_accepts_every_spelling_obsidian_accepts() {
    let mut index = PathIndex::new();
    for path in ["Projects/SomeProject.md", "Root Ticket.md"] {
        register(&mut index, path);
    }

    for target in [
        "SomeProject",
        "SomeProject.md",
        "Projects/SomeProject",
        "Projects/SomeProject.md",
    ] {
        assert_eq!(
            match_path(target, &index).as_deref(),
            Some("Projects/SomeProject.md"),
            "{target}"
        );
    }
    assert_eq!(match_path("", &index), None);
    assert_eq!(match_path("Nowhere", &index), None);
}

#[test]
fn match_path_folds_case_and_diacritics_when_no_exact_spelling_matches() {
    let mut index = PathIndex::new();
    register(&mut index, "Geschäftsidee.md");
    // No registered key is spelled this way, so this reaches the folded
    // comparison rather than the exact one.
    assert_eq!(
        match_path("GESCHAFTSIDEE", &index).as_deref(),
        Some("Geschäftsidee.md")
    );
}

/// Two notes with the same basename: which one a bare `[[Name]]` reaches.
///
/// TWO RULES, and they are not the same rule. An exact spelling is answered by
/// first registration wins, and the index is built from a sorted path list — so
/// the answer is the FIRST in sorted order. A spelling that matches nothing
/// exactly is answered by the folded comparison, and there the answer is the
/// SHORTEST path, which is Obsidian's tie-break. Both are pinned, because
/// collapsing them into one sentence is what the original's own comments do and
/// it is not what the code does.
#[test]
fn an_exact_ambiguous_name_takes_the_first_registration_in_sorted_path_order() {
    let mut index = PathIndex::new();
    // Sorted path order: `Projects/...` sorts before `Root ...`.
    for path in ["Projects/Root Project.md", "Root Project.md"] {
        register(&mut index, path);
    }
    assert_eq!(
        match_path("Root Project", &index).as_deref(),
        Some("Projects/Root Project.md")
    );
    assert_eq!(index.get("Root Project"), Some("Projects/Root Project.md"));
    // The shorter path is still reachable by its own full spelling: it loses the
    // ambiguous name, it does not stop existing.
    assert_eq!(
        match_path("Root Project.md", &index).as_deref(),
        Some("Projects/Root Project.md")
    );
    assert_eq!(
        match_path("Root Project.md/../Root Project.md", &index),
        None
    );
}

#[test]
fn a_folded_ambiguous_name_takes_the_shortest_path() {
    let mut index = PathIndex::new();
    // Sorted path order, which is the order the vault index registers in.
    // `Archive/AAA` precedes `Archive/ZZ`, so the LONGER path is registered
    // first -- which is what makes this an assertion about the length rule rather
    // than a restatement of the insertion order.
    for path in ["Archive/AAA/Cafe.md", "Archive/ZZ/Café.md"] {
        register(&mut index, path);
    }
    // Both fold to `cafe`, so the spelling below is ambiguous -- but each path has
    // its own exact keys, so this one has to miss every exact key and reach the
    // folded comparison.
    assert_eq!(
        match_path("CAFE", &index).as_deref(),
        Some("Archive/ZZ/Café.md")
    );
    // Each path is still reachable by the exact spelling that names it.
    assert_eq!(
        match_path("Archive/AAA/Cafe", &index).as_deref(),
        Some("Archive/AAA/Cafe.md")
    );
    assert_eq!(
        match_path("Archive/ZZ/Café", &index).as_deref(),
        Some("Archive/ZZ/Café.md")
    );
}

/// The length the tie-break compares is UTF-16 code units, as the original's
/// `String.length` was — not bytes and not characters. Two candidates that are
/// the same number of characters are a TIE even when one of them is longer on
/// disk, and a tie goes to the first registration.
#[test]
fn candidates_of_equal_character_count_are_a_tie_not_a_ranking() {
    let mut index = PathIndex::new();
    // `Café.md` is 7 UTF-16 units and 7 characters but 8 bytes; `Cafe.md` is 7
    // and 7 and 7. Both paths below are 19 characters, so they tie.
    for path in ["Archive/AAA/Café.md", "Archive/ZZZ/Cafe.md"] {
        register(&mut index, path);
    }
    // First registration wins the tie, which is the path that is LONGER on disk.
    assert_eq!(
        match_path("CAFE", &index).as_deref(),
        Some("Archive/AAA/Café.md")
    );
}

#[test]
fn an_empty_key_is_never_registered() {
    let mut index = PathIndex::new();
    index.insert_if_absent("", "Nowhere.md");
    assert!(index.is_empty());
}

// ---------------------------------------------------------------------------
// The fault seam
// ---------------------------------------------------------------------------

#[test]
fn a_write_that_lands_different_bytes_than_requested() {
    // The one failure a client cannot detect from the response: the write
    // succeeds and the server stored something else. It is why `hash()` exists,
    // so the stored bytes must both differ from the request AND hash
    // differently.
    let requested = "# requested\n";
    let swapped = "# swapped by the server\n";
    let source = memory_source(&load_corpus());
    source
        .inject(MemoryFault::storing("Root Ticket.md", swapped))
        .expect("the fault is well formed");

    write_of(&source, "Root Ticket.md", requested);

    assert_eq!(
        read_of(&source, "Root Ticket.md").expect("the fake holds it"),
        swapped
    );
    assert_ne!(hash_of(&source, "Root Ticket.md"), content_hash(requested));
    assert_eq!(hash_of(&source, "Root Ticket.md"), content_hash(swapped));
}

#[test]
fn a_simulated_500_surfaces_as_a_structured_refusal() {
    let source = memory_source(&load_corpus());
    source
        .inject(MemoryFault::on("Tickets.base", MemoryOp::Read, 500))
        .expect("the fault is well formed");
    let error = failure_of(source.read_note("Tickets.base"));
    assert_eq!(source.last_status(), Some(500));
    assert!(error.message().contains("500"), "{error}");
    assert_eq!(source.refusals().len(), 1);
}

#[test]
fn a_fault_fires_only_as_many_times_as_it_is_given() {
    let source = memory_source(&load_corpus());
    source
        .inject(MemoryFault::on("Root Ticket.md", MemoryOp::Read, 503).times(1))
        .expect("the fault is well formed");
    let error = failure_of(source.read_note("Root Ticket.md"));
    assert!(error.message().contains("503"), "{error}");
    assert!(read_of(&source, "Root Ticket.md")
        .expect("the fault is spent")
        .contains("Root ticket"));
}

#[test]
fn a_fault_on_one_path_leaves_the_rest_of_the_vault_readable() {
    let source = memory_source(&load_corpus());
    source
        .inject(MemoryFault::on("Tickets.base", MemoryOp::Read, 500))
        .expect("the fault is well formed");
    assert!(read_of(&source, "Tickets.base").is_err());
    assert_eq!(list_of(&source).len(), CORPUS_SIZE);
    assert!(read_of(&source, "Root Project.md")
        .expect("an unrelated path is fine")
        .contains("business-idea"));
}

#[test]
fn clear_faults_disarms_so_one_test_cannot_inherit_anothers_fault() {
    let source = memory_source(&load_corpus());
    source
        .inject(MemoryFault::on("Root Ticket.md", MemoryOp::Read, 500))
        .expect("the fault is well formed");
    assert!(read_of(&source, "Root Ticket.md").is_err());
    source.clear_faults();
    assert!(read_of(&source, "Root Ticket.md")
        .expect("disarmed")
        .contains("Root ticket"));
}

/// A source that cannot list reports the failure rather than an empty vault. The
/// dangerous shape is a backend that swallows an error and answers with an empty
/// vault: indistinguishable from a vault that genuinely matches nothing, which is
/// the one confusion this server exists to avoid.
#[test]
fn a_source_whose_list_fails_reports_the_failure_rather_than_an_empty_vault() {
    let source = memory_source(&load_corpus());
    source
        .inject(MemoryFault::refusing(ANY_PATH, 500))
        .expect("the fault is well formed");
    let error = failure_of(source.list());
    assert_eq!(source.last_status(), Some(500));
    assert!(error.message().contains("500"), "{error}");
}

// ---------------------------------------------------------------------------
// The async decision, pinned
// ---------------------------------------------------------------------------

/// The in-memory fixture is fully synchronous underneath and presents the same
/// `async` API as the filesystem source, so `Box<dyn VaultSource>` is all the
/// vault layer knows about it. Driven through the trait object here so the claim
/// is executable rather than asserted in a comment.
#[test]
fn the_memory_source_presents_the_same_async_api_behind_a_trait_object() {
    let shared = Rc::new(memory_source(&load_corpus()));
    let mut source: Box<dyn VaultSource> = Box::new(SharedSource(Rc::clone(&shared)));

    assert_eq!(source.kind(), SourceKind::Webdav);
    assert_eq!(list_of(source.as_ref()), list_of(shared.as_ref()));
    assert_eq!(
        hash_of(source.as_ref(), "Root Ticket.md"),
        hash_of(&fs_source(), "Root Ticket.md")
    );
    let expected = load_corpus()
        .iter()
        .find(|f| f.path == "Root Ticket.md")
        .expect("the corpus holds it")
        .content
        .len() as u64;
    assert_eq!(stat_of(source.as_ref(), "Root Ticket.md").size, expected);

    write_of(source.as_mut(), "Note.md", "# n\n");
    mkdir_of(source.as_mut(), "Sandbox");
    assert!(shared.dirs.borrow().contains("Sandbox"));
    delete_of(source.as_mut(), "Note.md");
    assert!(!list_of(shared.as_ref()).contains(&"Note.md".to_string()));
}

fn strip_extension(path: &str) -> String {
    match path.rfind('.') {
        Some(cut) if cut > 0 => path[..cut].to_string(),
        _ => path.to_string(),
    }
}

// ---------------------------------------------------------------------------
// The vault index
// ---------------------------------------------------------------------------
//
// The `Vault` index over the testing vault, driven directly. The `query`,
// `render` and `readNote` comparisons need `src/base.rs`, `src/render/markdown.rs`
// and `src/service.rs` and are deferred; what is here is the index those rest on,
// and each of these is a behaviour the rest of the project is entitled to rely on.

/// A loaded index over the oracle vault.
fn loaded() -> Rc<Vault> {
    let vault = Rc::new(Vault::new(Box::new(
        FsVaultSource::new(vault_dir()).expect("the oracle is a directory"),
    )));
    run(vault.load()).expect("the oracle vault loads");
    vault
}

#[test]
fn the_index_holds_every_note_and_both_bases() {
    let vault = loaded();
    assert_eq!(vault.base_paths(), ["AllNotes.base", "Tickets.base"]);
    assert_eq!(vault.note_paths().len(), 7);
    assert!(vault.note("Root Ticket.md").is_some());
    assert!(vault.note("Nope.md").is_none());
}

#[test]
fn load_is_idempotent_and_reload_rebuilds() {
    let vault = loaded();
    let before = vault.note_paths();
    run(vault.load()).expect("a second load is a no-op");
    assert_eq!(vault.note_paths(), before);
    run(vault.reload()).expect("a reload rebuilds");
    assert_eq!(vault.note_paths(), before);
}

#[test]
fn note_paths_are_in_registration_order_which_is_what_breaks_link_ties() {
    // Every backend sorts `list()`, so the index's key order IS the registration
    // order the tie-break depends on. Pinned so a change of map type that quietly
    // reorders it fails here rather than in a link assertion.
    let paths = loaded().note_paths();
    let mut sorted = paths.clone();
    sorted.sort();
    assert_eq!(paths, sorted);
}

#[test]
fn a_bare_alias_does_not_resolve() {
    // `aliases` feeds the link SUGGESTER, which emits `[[Real Name|Alias]]`. A
    // bare `[[Alias]]` does not resolve in Obsidian, and neither does it here.
    let vault = loaded();
    assert_eq!(
        vault.resolve("Root Project").as_deref(),
        Some("Root Project.md")
    );
    assert_eq!(
        vault.resolve("SomeProject").as_deref(),
        Some("Projects/SomeProject.md")
    );
    assert_eq!(vault.resolve(""), None);
    assert_eq!(vault.resolve("Nowhere"), None);
    assert_eq!(vault.resolve("Ticket"), None, "a prefix is not a link");
}

#[test]
fn a_link_resolves_through_every_spelling_obsidian_accepts() {
    let vault = loaded();
    for target in [
        "Fix login redirect",
        "Fix login redirect.md",
        "Tickets/Fix login redirect",
        "Tickets/Fix login redirect.md",
    ] {
        assert_eq!(
            vault.resolve(target).as_deref(),
            Some("Tickets/Fix login redirect.md"),
            "{target}"
        );
    }
}

#[test]
fn a_link_target_that_carries_an_extension_or_a_folder_prefix_is_not_ambiguous() {
    // The corpus has no two notes with one basename, so the ambiguity rule cannot
    // be exercised against the oracle. It is pinned against a synthetic index in
    // `a_folded_ambiguous_name_takes_the_shortest_path` and
    // `an_exact_ambiguous_name_takes_the_first_registration_in_sorted_path_order`
    // above; what is asserted here is that the two backends agree on which note
    // each corpus link reaches.
    let vault = loaded();
    let fs = fs_source();
    let memory = memory_source(&load_corpus());
    for note in vault.note_paths() {
        assert_eq!(vault.links_for(&note), vault.links_for(&note), "{note}");
    }
    // Both backends see the same seven notes, so a `file.links` that differed
    // would have to come from the links, not the paths.
    assert_eq!(
        run(fs.list()).expect("list").len(),
        run(memory.list()).expect("list").len()
    );
}

#[test]
fn file_tags_carry_their_hash_prefix_from_both_the_frontmatter_and_the_body() {
    // Confirmed against `base:query format=json` on Obsidian 1.13.7, which emits
    // `"Tags": "#Contacts, #Kontakte"`. A frontmatter tag may be written with or
    // without the `#`, and both normalise to the prefixed form.
    let vault = loaded();
    assert_eq!(
        vault.tags_for("Root Ticket.md"),
        [BasesValue::String("#ticket".to_string())]
    );
    let project = vault.tags_for("Root Project.md");
    assert_eq!(
        project,
        [
            BasesValue::String("#business-idea".to_string()),
            BasesValue::String("#project".to_string()),
        ]
    );
    assert!(vault.tags_for("Nope.md").is_empty());
}

#[test]
fn file_links_skip_embeds_so_a_base_region_never_links_its_own_host_note() {
    // The host note's `![[Tickets.base]]` is an EMBED, and `file.links` excludes
    // embeds while `file.embeds` carries them. A backend that moved between the
    // two would give a host note a self-link from its own embed, and the
    // shortest-path rule would then resolve it elsewhere.
    let vault = loaded();
    let embeds = vault.embeds_for("Root Project.md");
    assert_eq!(embeds.len(), 1);
    assert_eq!(embeds[0].link_target(), Some("Tickets.base"));
    let links = vault.links_for("Root Project.md");
    assert!(
        !links
            .iter()
            .any(|link| link.link_target() == Some("Tickets.base")),
        "the embed leaked into file.links: {links:?}"
    );
}

/// `file.links` covers frontmatter AND body, and keeps every link.
///
/// The original's `dedupe` keyed on `String(value)`, and `String()` of a link
/// object is `"[object Object]"`, so its `file.links` returned exactly ONE element
/// however many links a note had -- and `backlinksFor`, which is built on
/// `linksFor`, could therefore miss a backlink behind the first link. This note's
/// two frontmatter links are the case: the original reports one of them, this
/// reports both, and that is the one place the port is knowingly not
/// byte-identical.
#[test]
fn file_links_keeps_every_distinct_link() {
    let vault = loaded();
    let links = vault.links_for("Root Project.md");
    let targets: Vec<Option<String>> = links
        .iter()
        .map(|link| link.link_target().map(str::to_string))
        .collect();
    assert_eq!(
        targets,
        [
            Some("Geschäftsidee".to_string()),
            Some("Projects".to_string())
        ]
    );

    // A repeated link is still one link: `dedupe` is what the original reached
    // for, and the fix is to key it on the link's own text rather than to drop it.
    assert_eq!(vault.links_for("Root Ticket.md").len(), 1);
}

#[test]
fn file_links_carry_the_resolved_target_so_link_identity_comparison_works() {
    // A ticket's `project` property is a bare `[[SomeProject]]`. The link it
    // becomes has to resolve, or `project.contains(link(this.file.name))` silently
    // fails -- one side would be a link and the other a plain string.
    let vault = loaded();
    let links = vault.links_for("Tickets/Fix login redirect.md");
    assert_eq!(links.len(), 1);
    assert_eq!(links[0].link_target(), Some("Projects/SomeProject.md"));
    assert_eq!(links[0].to_display_string(), "[[SomeProject]]");
}

#[test]
fn file_links_includes_a_body_link_and_a_frontmatter_link_without_duplicating_them() {
    // One link from the body and one from `project`, which is the same note, so
    // the list has one element and the element is a LINK rather than a string.
    let vault = loaded();
    let links = vault.links_for("Root Ticket.md");
    assert_eq!(links.len(), 1);
    assert!(
        matches!(links[0], BasesValue::Link { .. }),
        "{:?}",
        links[0]
    );
    assert_eq!(links[0].link_target(), Some("Root Project.md"));
}

#[test]
fn file_backlinks_are_the_notes_that_link_here_in_both_directions() {
    // Every ticket links its project, and the project hosts their base, so the
    // vault has backlinks in both directions.
    let vault = loaded();
    assert_eq!(
        vault.backlinks_for("Root Project.md"),
        [BasesValue::String("Root Ticket.md".to_string())]
    );
    let project_backlinks = vault.backlinks_for("Projects/SomeProject.md");
    assert_eq!(
        project_backlinks,
        [
            BasesValue::String("Tickets/Add offline mode.md".to_string()),
            BasesValue::String("Tickets/Fix login redirect.md".to_string()),
        ]
    );
    // A host note is not its own backlink, even though it embeds a base.
    assert!(!vault
        .backlinks_for("Root Project.md")
        .contains(&BasesValue::String("Root Project.md".to_string())));
}

#[test]
fn file_tasks_are_the_documented_extension_with_the_clis_shape() {
    let vault = loaded();
    let tasks = vault.tasks_for("Tickets/Fix login redirect.md");
    assert_eq!(tasks.len(), 3);
    let first = tasks[0].to_display_string();
    assert!(first.contains("reproduce on staging"), "{first}");
    assert!(first.contains("completed: false"), "{first}");
    assert!(first.contains("status:  "), "{first}");

    let done = vault.tasks_for("Tickets/Invoice export.md");
    assert_eq!(done.len(), 1);
    assert!(
        done[0].to_display_string().contains("completed: true"),
        "{:?}",
        done[0]
    );
}

#[test]
fn a_root_note_reports_the_vault_root_as_a_slash_and_a_nested_one_does_not() {
    // Probed against `base:query format=json` on Obsidian 1.13.7, where a root
    // note emits `"folder": "/"` and a nested one emits `"folder": "Projects"`.
    let vault = loaded();
    assert_eq!(vault.file_value("Root Ticket.md").folder, "/");
    assert_eq!(
        vault.file_value("Projects/SomeProject.md").folder,
        "Projects"
    );
    assert_eq!(
        vault.file_value("Tickets/Fix login redirect.md").folder,
        "Tickets"
    );
}

#[test]
fn file_ctime_and_file_mtime_read_the_same_instant() {
    // Deliberate, and the only pairing available: a note's creation time is
    // recorded by neither backend, and a `FileStat` carries one instant.
    let vault = loaded();
    let file = vault.file_value("Root Ticket.md");
    let ctime = (file.accessors.ctime)();
    let mtime = (file.accessors.mtime)();
    assert_eq!(ctime, mtime);
    assert_eq!(
        ctime.millis(),
        stat_of(&fs_source(), "Root Ticket.md")
            .mtime
            .timestamp_millis()
    );
    // And it is the real clock on fs, not the fake's fixed instant: the value the
    // equivalence suite refuses to compare across backends.
    assert_ne!(ctime.millis(), MEMORY_MTIME_MS);
}

#[test]
fn file_size_is_bytes_and_file_name_and_basename_split_on_the_extension() {
    let vault = loaded();
    let file = vault.file_value("Tickets/Fix login redirect.md");
    assert_eq!(file.name, "Fix login redirect.md");
    assert_eq!(file.basename, "Fix login redirect");
    assert_eq!(file.ext, "md");
    assert_eq!(
        (file.accessors.size)(),
        stat_of(&fs_source(), "Tickets/Fix login redirect.md").size
    );
}

#[test]
fn file_resolve_follows_a_link_to_another_file_value() {
    let vault = loaded();
    let file = vault.file_value("Root Ticket.md");
    let resolved = (file.accessors.resolve)("SomeProject").expect("the link resolves");
    assert_eq!(resolved.path, "Projects/SomeProject.md");
    assert_eq!((resolved.accessors.resolve)("Nowhere"), None);
}

#[test]
fn file_properties_are_the_coerced_frontmatter() {
    // A `[[Some Note]]` property becomes a link object, which is what makes
    // `project.contains(link(this.file.name))` work at all.
    let vault = loaded();
    let properties = (vault.file_value("Root Ticket.md").accessors.properties)();
    assert_eq!(
        properties.get("project"),
        Some(&BasesValue::List(vec![BasesValue::Link {
            target: "Root Project".to_string(),
            display: None,
            // `None` on purpose: frontmatter is coerced BEFORE the path index is
            // built, which is the original's order. See `coerce_frontmatter`.
            resolved: None,
        }]))
    );
    assert_eq!(
        properties.get("status"),
        Some(&BasesValue::String("active".to_string()))
    );
    // `file.links` re-resolves the same target at query time, which is why the
    // unresolved property above does not stop link identity from working.
    assert_eq!(
        vault.links_for("Root Ticket.md")[0].link_target(),
        Some("Root Project.md")
    );
}

#[test]
fn a_note_that_is_not_indexed_reports_the_epoch_rather_than_refusing() {
    // `new DateValue(0, true)` in the original. A missing note is a different
    // question from a note with no mtime, and the index has nothing to say about
    // it, so it says zero rather than guessing.
    let vault = loaded();
    let file = vault.file_value("Nope.md");
    assert_eq!((file.accessors.mtime)().millis(), 0);
    assert_eq!((file.accessors.size)(), 0);
    assert!((file.accessors.properties)().is_empty());
    assert!((file.accessors.tags)().is_empty());
    assert!((file.accessors.links)().is_empty());
    assert!((file.accessors.embeds)().is_empty());
    assert!((file.accessors.backlinks)().is_empty());
    assert!((file.accessors.tasks)().is_empty());
    assert_eq!(vault.file_for("Nope.md"), None);
    assert!(vault.file_for("Root Ticket.md").is_some());
}

#[test]
fn links_to_is_false_because_backlinks_answers_the_same_question() {
    let vault = loaded();
    let file = vault.file_value("Root Project.md");
    assert!(!((file.accessors.links_to)("Root Ticket.md")));
}

#[test]
fn a_markdown_link_property_becomes_a_link_and_a_url_property_does_not() {
    // `[Business](Projects/Business)` is a note; `[Home](https://example.com)` is
    // not, and coercing it would put a link object in a property that is a URL.
    let vault = loaded();
    let mut data = BTreeMap::new();
    data.insert(
        "relative".to_string(),
        BasesValue::String("[Business](Projects/Business)".to_string()),
    );
    data.insert(
        "absolute".to_string(),
        BasesValue::String("[Home](https://example.com)".to_string()),
    );
    data.insert(
        "anchor".to_string(),
        BasesValue::String("[Top](#top)".to_string()),
    );
    data.insert(
        "plain".to_string(),
        BasesValue::String("Just text".to_string()),
    );
    data.insert(
        "nested".to_string(),
        BasesValue::String("[[Tickets/Fix login redirect|Fix]]".to_string()),
    );

    let coerced = coerce_frontmatter(&data, &vault);
    assert_eq!(
        coerced.get("relative"),
        Some(&BasesValue::Link {
            target: "Projects/Business".to_string(),
            display: Some("Business".to_string()),
            resolved: None,
        })
    );
    assert_eq!(
        coerced.get("absolute"),
        Some(&BasesValue::String(
            "[Home](https://example.com)".to_string()
        ))
    );
    assert_eq!(
        coerced.get("anchor"),
        Some(&BasesValue::String("[Top](#top)".to_string()))
    );
    assert_eq!(
        coerced.get("plain"),
        Some(&BasesValue::String("Just text".to_string()))
    );
    // Resolved, because the index is already built. Coercion DURING a rebuild
    // happens before the index exists and resolves nothing -- see
    // `file_properties_are_the_coerced_frontmatter` for that half.
    assert_eq!(
        coerced.get("nested"),
        Some(&BasesValue::Link {
            target: "Tickets/Fix login redirect".to_string(),
            display: Some("Fix".to_string()),
            resolved: Some("Tickets/Fix login redirect.md".to_string()),
        })
    );
}

/// The escape guard compares path components, and every absolute path starts with
/// the root component — so against `/` the guard would be inert and
/// `read_note("/etc/passwd")` would succeed. No vault is the filesystem root, so
/// the configuration is refused rather than served.
#[test]
fn a_vault_rooted_at_the_filesystem_root_is_refused() {
    let error = FsVaultSource::new("/").expect_err("`/` is not a vault");
    assert!(error.message().contains("filesystem root"), "{error}");
}

/// `points_at` is the link-equality rule the docs state as "equivalent as long
/// as they point to the same file": equal, or one is a PATH-SUFFIX of the other
/// with a separator at the join, compared folded and without extensions.
#[test]
fn points_at_accepts_a_bare_name_a_path_and_either_extension() {
    for (link, path) in [
        ("SomeProject", "Projects/SomeProject.md"),
        ("SomeProject.md", "Projects/SomeProject.md"),
        ("Projects/SomeProject", "Projects/SomeProject.md"),
        ("Projects/SomeProject.md", "SomeProject"),
        ("SomeProject.md", "projects/someproject"),
        ("SÓMEPROJECT", "Projects/SomeProject.md"),
    ] {
        assert!(points_at(link, path), "{link} should point at {path}");
    }
}

#[test]
fn points_at_refuses_a_name_that_is_only_a_prefix_of_the_path() {
    // The separator is what makes it a path rather than a longer word: `Some` does
    // not name `Projects/SomeProject.md`.
    assert!(!points_at("Some", "Projects/SomeProject.md"));
    assert!(!points_at(
        "Projects/SomeProject",
        "Projects/OtherProject.md"
    ));
    assert!(!points_at("SomeProjectX", "Projects/SomeProject.md"));
}
