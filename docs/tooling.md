# Tooling

TypeScript on Bun, formatted and linted by Biome. The Rust port lives on the
`rust-port` branch and carries its own rustfmt/clippy configuration.

## What is verified, and how

Configuration that parses is not configuration that works. Every claim below was
checked by running the tool, and three of them were wrong on the first attempt.

| Branch | Formatter | Linter | Type check |
|---|---|---|---|
| `main` | Biome | Biome | `tsc --noEmit` |

Gate: `bun run verify` (typecheck, lint, test).

## opencode.jsonc

One project-scoped config, plain JSON, no comments. It registers:

```
formatter  biome     bunx biome check --write $FILE     .ts .tsx .js .json
formatter  prettier  disabled
lsp        biome     biome lsp-proxy --stdio
lsp        typescript ./node_modules/.bin/typescript-language-server --stdio
```

There is **no `linter` key** in opencode's config schema; it sets
`additionalProperties: false`, so writing one is a validation error. Linting is
wired in as a language server, which is also what opencode's own guidance
recommends.

### Three things that validated and then did nothing

Each of these passed a schema check and was still wrong. They are recorded
because the schema is not the specification.

1. **`{"biome": {}}` for an LSP entry.** Permitted by the schema, rejected by
   opencode at load: `Missing key lsp.biome.command`. An LSP entry needs an
   explicit command.
2. **The Rust LSP is registered as `rust`, not `rust-analyzer`.** The class is
   called `RustAnalyzer` but the id is `rust`. The wrong name validated and
   attached nothing.
3. **A configuration error and a correct configuration look identical until
   something breaks.** Proving an entry works means introducing a real error and
   seeing it reported, not observing an empty result — `[]` is what a
   misconfigured server looks like, and it is indistinguishable from a clean file
   unless you test it with a broken one.

## Verifying a change

`opencode debug config` shows what opencode actually resolved — including
entries it inherited from the global config. To prove a formatter fires, write an
unformatted file and confirm it is reformatted on disk; to prove an LSP is
attached, introduce a real type error and confirm it is reported.

## Corpus

`test/vault` is the parity oracle and is identical on both branches: nine files,
read-only to every tool that is not explicitly a writer. `test/fixtures` holds
one mini-vault per spec construct.

`.obsidian/` inside `test/vault` is Obsidian's own state and is gitignored.

## Obsidian CLI

Parity is a live gate against `obsidian base:query`, not a recording. It skips
visibly when the CLI cannot answer and never passes vacuously — see the note in
`docs/divergences.md` about the parity suite reporting green with zero
assertions, which is the failure mode to avoid.
