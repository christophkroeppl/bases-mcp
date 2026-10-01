//! The corpus: the testing vault, as `{ path, content }`.
//!
//! One definition, shared by every vault-related suite. Two suites that each
//! globbed `test/vault` would agree today and diverge the first time someone adds
//! a file, and the symptom would be an equivalence failure pointing at a backend
//! that had not changed.
//!
//! The enumeration is delegated to `FsVaultSource::list()` rather than repeated
//! here. That is the whole point: the corpus is then exactly the set of files the
//! filesystem backend is willing to show an agent, so a fake can never be seeded
//! with something no backend could have produced. A hand-rolled recursive read
//! would be a second definition of the same rule, and would eventually disagree
//! with the one `Vault` actually consumes.
//!
//! Read-only by construction. Nothing here writes, and `test/vault` must stay at
//! exactly [`CORPUS_SIZE`] files: it is the parity oracle, and a file added to it
//! silently changes what `obsidian base:query` returns.

use std::path::{Path, PathBuf};

use bases_mcp::vault::{FsVaultSource, VaultSource};

pub mod fake_dav;
pub mod memory;

/// One file, as the fakes hold it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VaultFile {
    pub path: String,
    pub content: String,
}

/// The testing vault on disk.
pub fn vault_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("test").join("vault")
}

/// How many files the testing vault holds, `.obsidian` aside.
///
/// Pinned as a number rather than re-derived, because a corpus that has quietly
/// grown means either an unrecorded change to the oracle or a test that stopped
/// covering what it claims to. Both are worth failing over.
pub const CORPUS_SIZE: usize = 9;

/// Read the testing vault into memory.
pub fn load_corpus() -> Vec<VaultFile> {
    load_corpus_from(&vault_dir())
}

pub fn load_corpus_from(dir: &Path) -> Vec<VaultFile> {
    let source = FsVaultSource::new(dir).expect("a directory is a vault root");
    futures_block_on(async {
        let mut files = Vec::new();
        for path in source.list().await.expect("the testing vault is readable") {
            let content = source.read_text(&path).await.expect("the testing vault is readable");
            files.push(VaultFile { path, content });
        }
        files
    })
}

/// The runtime the synchronous fixture setup uses.
///
/// `tokio` is already a dependency because the sources are `async`, and a test
/// that built a second runtime to read nine files would be a second thing to
/// keep working.
pub fn futures_block_on<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a current-thread runtime builds")
        .block_on(future)
}

/// Count the files in a directory tree, ignoring dot-directories.
///
/// Used only to prove the oracle's size has not changed. It reads the tree
/// itself rather than reusing the loader, because the loader's answer is the
/// thing under test: asking it how many files there are would be circular.
pub fn count_files(dir: &Path) -> usize {
    let mut total = 0;
    let Ok(entries) = std::fs::read_dir(dir) else { return 0 };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        let Ok(file_type) = entry.file_type() else { continue };
        if file_type.is_dir() {
            total += count_files(&entry.path());
        } else if file_type.is_file() {
            total += 1;
        }
    }
    total
}

/// Write a corpus to a directory, so a temp vault matches the in-memory one
/// exactly.
pub fn seed_dir(dir: &Path, corpus: &[VaultFile]) {
    for file in corpus {
        let abs = dir.join(&file.path);
        if let Some(parent) = abs.parent() {
            std::fs::create_dir_all(parent).expect("a sandbox directory is writable");
        }
        std::fs::write(&abs, file.content.as_bytes()).expect("a sandbox file is writable");
    }
}

/// The tree the equivalence suite pins `isIndexable` against.
///
/// The testing vault cannot carry this case: it must stay at nine files. The rule
/// is pinned against a tree built in a temp directory, compared three ways — fs,
/// the in-memory source, and `is_indexable` itself — so a backend cannot pass by
/// agreeing with a wrong rule that two implementations happen to share.
pub fn dotfile_tree() -> Vec<VaultFile> {
    [
        (".obsidian/app.json", "{}"),
        (".hidden/Note.md", "# hidden"),
        ("Notes/Alpha.md", "# alpha"),
        ("Notes/Deep/Beta.md", "# beta"),
        ("Notes/Deep/Notes.base", "views: []"),
        ("Notes/scratch.txt", "not a note"),
        ("Templates/template.md", "# template"),
    ]
    .into_iter()
    .map(|(path, content)| VaultFile { path: path.to_string(), content: content.to_string() })
    .collect()
}
