# STATUS

Handoff for the next agent. Rewritten 2026-10-20 on branch `develop` at `cf69857`.

## Read these first

| File | Why |
|---|---|
| `CONTEXT.md` | The domain vocabulary. Use its words. `Base`, `View`, `Host note`, `Base region`, `Projection`, `Property ID`, `Link value`, `Divergence`, `Divergence registry`, `Fixture vault`, `Testing vault`. Do not write "embed" for a **Base region**. |
| `docs/divergences.md` | The Divergence registry, and the single place a Divergence may be recorded. D1-D9 plus a Bugs section. Consult before calling anything a bug. |
| `docs/tooling.md` | Every gate, the four transport/backend combinations, and how to run the container. |
| `PLAN-security-backlog-and-gate-repair.md` | What is still outstanding, and why. Phase 4 is open. |

## Repo state

```
develop             cf69857  [origin/develop]   Rust, shipping
ts-implementation   9a86bd5  [origin/ts-impl]   TypeScript + 4 ported data-loss fixes
main                0fbef40  [origin/main]      frozen
```

`develop` is 19 commits ahead of `origin/develop`. Nothing has been pushed.

## Verified green

| Gate | Result |
|---|---|
| `just check` | **489 tests pass**, fmt clean, clippy 0 warnings, versions agree |
| `cargo test --test cross_product` | 7 passed — all four transport x backend combinations, with writes; a note whose name needs percent-encoding; `write_note` refusing a missing path |
| `podman build -f Containerfile` | image builds; `localhost/bases-mcp:latest` |
| the built image | with the DOCUMENTED command (`--userns=keep-id`): HTTP handshake + `tools/list` answer; a `write_note` and an `add_note_to_base` land on the HOST filesystem; the stdio transport answers over the container's stdin |
| `test/vault` | 9 files, pure LF, untouched |
| working tree | clean except two untracked files (this file and the plan) |
| `just record` / `just corpus-check` | exit 1 with a message naming the missing recorder — a deliberate honest failure, not cargo's `no bin target named` |

## What changed most recently

**All four transport x backend combinations now work, and the container is
buildable and usable** (`7478d89`, `c7873fa`, `cdcba59`).

The server had two independent axes — the transport (stdio or Streamable HTTP)
and the backend (a directory or a WebDAV collection) — and only two of the four
combinations were tested: `(stdio, fs)` and `(http, fs)`. The two a deployment
actually uses, `(stdio, webdav)` and `(http, webdav)`, were untested.
`tests/cross_product.rs` drives all four end to end: it spawns the real
binary, speaks MCP to it the way a client does, and checks every write **at
the far end** — the file read back off disk, or the note READ BACK over HTTP
from the WebDAV server, not peeked at in its map. A write that reports success
without landing anywhere fails the suite.

The WebDAV fixture is a real HTTP server speaking the real protocol, and two
of its properties are **mutation-checked** rather than assumed: with the hrefs
emitted decoded again the encoding test FAILS, and with the server storing
different bytes than it received both WebDAV cells FAIL.

Three defects in the `Containerfile` made the image unbuildable, and all three
were silent until a build ran:

1. The build image was pinned to `rust:1.85`, but the **lockfile** needs rustc
   1.88 — `rmcp 3.5.0` and the `icu` crates refuse to compile on 1.85, so
   cargo failed the whole graph before a crate of this project was built.
2. `Cargo.toml` declares a `[[test]]` target at `tests/parity.rs`, and a
   declared target whose file is missing is a **manifest error** — cargo could
   not parse the manifest, so it never compiled anything.
3. The image's user was left to `useradd --system`, which picks 999 on Debian.
   The uid is now pinned to **1000** — and pinning it is not decoration:
   guarden reads the image's user out of the image and renders
   `Userns=keep-id:uid=…,gid=…` from it (ADR-0019), so it can only pin a uid
   it can read.

And a fourth defect that only a review found, because the symptom looked
right: **the container could read the vault but not write it.** Under rootless
podman the host uid is mapped to CONTAINER ROOT, so a host file owned by uid
1000 appears inside as owned by uid 0, and the server runs as uid 1000 — not
the owner. The fix is `--userns=keep-id`, which maps the host uid to the same
uid inside. The first remedy (a `chown` to uid 1000) was wrong in a way that
made it look right: it makes the files appear as uid 0, which uid 1000 still
does not own. See `docs/tooling.md`.

**The `fmt` gate was red on committed code** (`7478d89`). It was clean at
`dd80ca0` and dirty at `60b89d8`: the transport branch was committed
unformatted, and CI runs `cargo fmt --check` as a required step, so `develop`
was red in CI. The fix is three wrapped lines in two files, proved
whitespace-only by comparing the whitespace-stripped SHA of each file against
`HEAD` — **not** by `git diff -w`, which reports rustfmt's line *splits* as
changes.

## The todo queue

### The security backlog document has LANDED (todo item 1 — resolved)

`docs/BACKLOG-network-security.md` (plan Phase 2) is now on `develop`, at the
path the ticket names — which is also the path both references point at
(`docs/divergences.md:265`, `.guarden/service.yml:10`). Neither is dangling
any more. It carries the seven-item hardening list with the OWASP API Security
Top 10 (2023) mapping, and every claim in it was checked against the tree
before it was written — the tool count, the `MAX_DEPTH = 96` guard, the
parser's measured 471-level edge, the path assertions, the config redaction.

The one claim that had to change is the one the stale branch got wrong: the
transport **ships**. The old draft said "an HTTP transport does not exist on
this branch yet", which was true when the branch forked and false today. The
document now describes the transport as it is, and adds the deployment posture
that the container work established — the loopback default, the deliberate
`0.0.0.0` override, the non-root uid, and the fact that `--userns=keep-id` is
a *containment* control and says nothing about who may call the tools.

`docs/divergences.md` no longer points at a branch that does not exist.

### Bugs known, not fixed (todo items 2-10; item 1 is the backlog, landed above)

2. **`just record` and `just corpus-check` now fail HONESTLY** (was: they
   failed with cargo's `no bin target named 'parity-record'`). Both recipes
   print what is missing and exit 1. `precommit` no longer chains
   `corpus-check` — a gate that cannot pass is not a gate, and leaving it in
   made every pre-commit run red for a reason that had nothing to do with the
   change being committed. The recorder itself is still **not built**: the
   threshold for building it (a live Obsidian CLI in CI, a snapshot corpus
   under `test/parity/data/`, and a comparison target that reads that corpus)
   is not met — none of the three hold today.
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
