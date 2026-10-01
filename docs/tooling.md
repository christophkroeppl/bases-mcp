# Tooling

Rust crate, one toolchain. Every gate runs from `just`.

```
just check           fmt, clippy, tests without Obsidian. What CI runs.
just check-all       everything, requiring the live Obsidian CLI.
just parity          the Obsidian CLI gate on its own.
just dev-setup       what a new machine needs, once.
just --list          everything else.
```

| | Tool | Gate |
|---|---|---|
| Format | rustfmt | `just fmt` |
| Lint | clippy | `just lint` |
| Types | rust-analyzer / `cargo check` | `rust-analyzer diagnostics .` |

## Obsidian CLI gates

`just check` does not need Obsidian. `just parity` and `just check-all` do.

`tests/parity.rs` **panics** when the CLI cannot answer rather than skipping, so
a green suite always compared something. `BASES_MCP_ALLOW_SKIP=1` relaxes it, and
that is a deliberate choice: the alternative is a parity suite that passes
because it tested nothing, which is the failure this project has already made
once.

`just dev-setup` prints whether the CLI is on `PATH`, its version, the registered
vaults, and — if the testing vault is missing — the exact path to add. The CLI
cannot register a vault itself, so that last step is manual by necessity rather
than by omission.

## opencode.jsonc

Project-scoped, plain JSON, no comments.

```
formatter  rustfmt   rustfmt --edition 2021 $FILE     .rs
formatter  prettier  disabled
lsp        rust      rust-analyzer                   .rs
```

There is **no `linter` key** in opencode's config schema; it sets
`additionalProperties: false`, so writing one is a validation error. Linting is
wired in as a language server, which is also what opencode's own guidance
recommends.

### Three things that validated and then did nothing

Each passed a schema check and was still wrong. Recorded because the schema is
not the specification.

1. **An empty LSP entry is not "no LSP".** `{"rust": {}}` is permitted by the
   schema and rejected by opencode at load: `Missing key lsp.rust.command`. An
   entry needs an explicit command.
2. **The Rust LSP id is `rust`, not `rust-analyzer`.** The class is called
   `RustAnalyzer`; the id is `rust`. The wrong name validated and attached
   nothing.
3. **An empty diagnostics result is not "no problems".**
   `opencode debug lsp diagnostics` disposes the server about 100 ms after
   touching the file, and rust-analyzer needs roughly 45 s to index this crate.
   Every `.rs` result is `[]` whether or not the code compiles.

So the honest check for Rust types is `rust-analyzer diagnostics .`, plus
`cargo check` in CI. Do not treat `opencode debug lsp diagnostics` as evidence
about a `.rs` file.

## Verifying a change

`opencode debug config` shows what opencode actually resolved, including entries
inherited from a global config. To prove a formatter fires, write an unformatted
file and confirm it is reformatted on disk. To prove an LSP is attached,
introduce a real type error and confirm it is reported — `[]` reads as clean
whether or not the server is working.

## Corpus

`test/vault` is the Testing vault: the parity oracle, identical on every branch,
nine files, and read-only to every tool that is not explicitly a writer.
`test/fixtures` holds one Fixture vault per spec construct, each isolating one
construct and never registered in Obsidian.

`.obsidian/` inside `test/vault` is Obsidian's own state and is gitignored.

Recorded CLI answers live under `test/parity/data/`, keyed by a digest of the
results rather than by version, so several Obsidian versions that agree share one
directory. `just record` refreshes them; `just corpus-check` reports whether the
Testing vault still matches the recording.
