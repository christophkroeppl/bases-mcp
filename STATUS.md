# STATUS

Handoff for the next agent. Rewritten 2026-10-20 on branch `develop` at `cdcba59`.

## Read these first

| File | Why |
|---|---|
| `CONTEXT.md` | The domain vocabulary. Use its words. `Base`, `View`, `Host note`, `Base region`, `Projection`, `Property ID`, `Link value`, `Divergence`, `Divergence registry`, `Fixture vault`, `Testing vault`. Do not write "embed" for a **Base region**. |
| `docs/divergences.md` | The Divergence registry, and the single place a Divergence may be recorded. D1-D9 plus a Bugs section. Consult before calling anything a bug. |
| `docs/tooling.md` | Every gate, the four transport/backend combinations, and how to run the container. |
| `PLAN-security-backlog-and-gate-repair.md` | What is still outstanding, and why. Phases 2, 3 and 4 are open. |

## Repo state

```
develop             cdcba59  [origin/develop]   Rust, shipping
ts-implementation   9a86bd5  [origin/ts-impl]   TypeScript + 4 ported data-loss fixes
main                0fbef40  [origin/main]      frozen
```

`develop` is 18 commits ahead of `origin/develop`. Nothing has been pushed.

## Verified green

| Gate | Result |
|---|---|
| `just check` | **487 tests pass**, fmt clean, clippy 0 warnings, versions agree |
| `cargo test --test cross_product` | 5 passed — all four transport x backend combinations, with writes |
| `podman build -f Containerfile` | image builds; `localhost/bases-mcp:latest` |
| the built image | HTTP handshake + `tools/list` answer; a `write_note` and an `add_note_to_base` land on the HOST filesystem; the stdio transport answers over the container's stdin |
| `test/vault` | 9 files, pure LF, untouched |
| working tree | clean except two untracked files (this file and the plan) |

## What changed most recently

**All four transport x backend combinations now work, and the container is
buildable and usable** (`7478d89`, `c7873fa`, `cdcba59`).

The server had two independent axes — the transport (stdio or Streamable HTTP)
and the backend (a directory or a WebDAV collection) — and only two of the four
combinations were tested: `(stdio, fs)` and `(http, fs)`. The two a deployment
actually uses, `(stdio, webdav)` and `(http, webdav)`, were untested.
`tests/cross_product.rs` drives all four end to end: it spawns the real
binary, speaks MCP to it the way a client does, and checks every write **at
the far end** — the file read back off disk, or the bytes read back from the
WebDAV server's own store. A write that reports success without landing
anywhere fails the suite.

Three defects in the `Containerfile` made the image unbuildable, and all three
were silent until a build ran:

1. The build image was pinned to `rust:1.85`, but the **lockfile** needs rustc
   1.88 — `rmcp 3.5.0` and the `icu` crates refuse to compile on 1.85, so
   cargo failed the whole graph before a crate of this project was built.
2. `Cargo.toml` declares a `[[test]]` target at `tests/parity.rs`, and a
   declared target whose file is missing is a **manifest error** — cargo could
   not parse the manifest, so it never compiled anything.
3. The image's user was left to `useradd --system`, which picks 999 on Debian.
   The uid is now pinned to **1000**, because a host operator can only arrange
   for the container to write the vault if they know which uid to arrange it
   for. See `docs/tooling.md` for the `chown` this needs.

**The `fmt` gate was red on committed code** (`7478d89`). It was clean at
`dd80ca0` and dirty at `60b89d8`: the transport branch was committed
unformatted, and CI runs `cargo fmt --check` as a required step, so `develop`
was red in CI. The fix is three wrapped lines in two files, proved
whitespace-only by comparing the whitespace-stripped SHA of each file against
`HEAD` — **not** by `git diff -w`, which reports rustfmt's line *splits* as
changes.

## The todo queue

### Blocking — the security backlog is not landed

1. **The security backlog document does not exist on `develop`.** It lives on
   `feat/security-backlog` (`48cf2cb`) and, in a richer form, on
   `feat/guarden-deploy` (`8ae0f4c`). Both forked at `dd80ca0` and neither was
   merged. `develop` has **dangling references** to the file it names:
   `docs/divergences.md:265` and `.guarden/service.yml:10`. Plan Phase 2
   lands one document at `docs/BACKLOG-network-security.md` — the path the
   ticket names and both references point at — carrying the guarden branch's
   content with its stale "HTTP does not exist yet" claim corrected.

### Bugs known, not fixed

2. **`just record` and `just corpus-check` are broken.** Both call
   `cargo run --release --bin parity-record`, and no such bin target exists.
   `justfile:82,90` and the `precommit` recipe all reference it. `STATUS.md`
   item 2 of the previous revision offered the alternative: build it, or make
   the recipes fail honestly. Plan Phase 3 does the latter, and removes
   `corpus-check` from `precommit` — a gate that cannot pass is not a gate.
3. **`get_note` never returns `base_hash`, but `write_note` documents it as
   coming from `get_note`** (`src/tools.rs:233` omits `note.base_hash`). The
   conditional-write flow is impossible for an agent as documented. Small fix.
4. **`tags == "project"` returns true, Obsidian says false.** Tags is a distinct
   type storing `#project`, so only `tags.contains("project")` matches. The last
   measured equality divergence: cli=0, ours=1.
5. **`contains()` on a null property** raises a hard error; the CLI silently drops
   the row.
6. **Frontmatter coercion chain missing.** A quoted `YYYY-MM-DD` becomes a Date,
   `[[x]]` a Link, a URL a URL — Obsidian does this before typing.
7. **Date equality must fall back to date-only** when either side lacks a time:
   `due: 2026-01-05T17:30 == today()` is true all day on the 5th. Needs a stable
   clock fixture.
8. **`contains()` is case-insensitive and `contains("")` is always false**, while
   `==` is case-sensitive. `startsWith`/`endsWith` case behaviour unconfirmed.

### Medium

9. **Filter object must have exactly one of `and`/`or`/`not`.** Two keys, zero
   keys, or another key is a hard parse failure. `not:` takes a list.
10. **CLI parity traps to encode as fixture guards:** `limit:` truncates
    silently; columns come only from `order`; headers are display names and can
    collide, silently dropping a JSON key; all CLI values are strings; nulls sort
    last in **both** directions.
11. **Pin the clean-round-trip win.** A no-op write must not rewrite the file's
    bytes: measured 48→48 with normalisation, 48→49 mixed without. Recorded in
    D8 as measured-not-pinned.
12. **Release-please.** `release-type: rust`, `include-component-in-tag: false`,
    `bump-minor-pre-major: true`. GitHub Releases only, **never crates.io**. The
    load-bearing claim is that `extra-files` reaches the Rust strategy; it is
    verified in release-please's source but its own fixture uses
    `release-type: simple`, so **the first release PR is the proof**.
13. **`Line::content_end`'s `\r` trim** (`src/note.rs`) is a measured no-op —
    removing it passed the whole suite. A 6-call-site parser refactor for no
    behavioural gain. Decide and delete or document why it stays.

### Deferred, needs a human in the Obsidian UI

14. **UI validation.** 41 items: A1–A7 (dates/YAML), B1–B10 (coercion, null,
    absence), C1–C6 (`this`), D0–D10 (CLI parity), E1–E7 (low-risk). Sequence
    **C2, C5, C6** first, then **A1, A2, B1, B2, B4, B7**. D0 gates all CLI work
    (the installer is 1.12.4, below the documented 1.12.7 floor). **D2 and D7
    silently corrupt a comparison rather than failing loudly** — treat a
    surprising row count as one of those before treating it as our bug.
    Items 4–8 above are inferred from the binary, not confirmed; this is what
    would settle them.

### Out of repo

15. **Link-layer performance.** `file.links`/`file.backlinks` are
    quadratic-to-cubic: 55 s for one query on a 150-note vault. Needs a resolve
    cache keyed on folded target plus a by-path backlink index built once per
    rebuild. Belongs in `/media/data/projects/ACTIVE/bases-mcp-*.md`, not here.

### Dropped

- **Body-text search.** No body search exists in Obsidian Bases at all — the
  entire function registry was enumerated from the binary (16 globals, 65
  methods, every `FileValue` key). Not expressible in Obsidian, so not in us.
  Matching a Host note's title against child *titles* and *properties* works;
  against child *bodies* it does not.

## The abandoned work in `feat/guarden-deploy`

That worktree holds **13 uncommitted modified files and an untracked 371-line
`src/http.rs`** — a hand-rolled SSE server that does not compile (63 cargo
errors) and is not wired into `src/lib.rs`. Its mtimes are *before* develop's
transport commit, so it is a superseded earlier attempt at the same problem,
not work in progress. Its premise is also now known to be wrong: it says
rmcp's `local` feature blocks its SSE service, and develop dropped `local`
and used `StreamableHttpService` directly. Plan Phase 4 disposes of it.

## How to work here

- `just check` is the gate and needs no Obsidian. `just parity` needs Obsidian
  running with `test/vault` registered; run `just dev-setup` to check.
- `test/vault` is the Testing vault, a parity oracle. Nine files. Never write to
  it. **Running any `obsidian` command rewrites it** — the CLI launches the
  desktop app, which rewrites the `.base` files. Snapshot before touching the
  CLI.
- Commit messages are conventional commits; CI lints them on the PR.
- Work in a worktree off `develop`, one branch per change, merge with `--no-ff`.
- Never run the suite with a stale `target/release` binary. Two defects this
  session were wrongly dismissed because the binary predated its own fix.
- Verify assertions can fail. `lone_lf` passed straight through a `\r\r\n`
  corruption because `\r\r\n` contains no lone `\n`. `stray_cr` replaced it.
- On this host, `podman` needs `--runtime /home/linuxbrew/.linuxbrew/bin/crun`;
  the system crun (0.17) is too old for this kernel and fails with "crun:
  unknown version specified".
