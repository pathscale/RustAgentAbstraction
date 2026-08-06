# Working agreement: RustAgentAbstraction

The operating contract for **any** coding agent working in this repository. This file is
the single source of truth for the rules: Codex, Cursor and Gemini CLI read `AGENTS.md`
natively, and Claude Code loads it through the `@AGENTS.md` import in
[`CLAUDE.md`](CLAUDE.md). **Never fork these rules into a per-vendor file.**

A single Rust library crate, `agent-abstraction`: drives the Claude Code, Codex and GitHub
Copilot CLIs headlessly behind one API. Consumed as a direct dependency by the
`pathscale/agencyzero` Tauri app. See [README.md](README.md) for the API and the layout.

## Invariants (don't break these)

- **Every flag mapping must be verified against the real CLI, and the version recorded in a
  comment next to it.** These CLIs change flags between releases: Copilot 1.0.75 gained a
  headless session id and a JSONL event stream that oneharness (our upstream) still models as
  absent. A mapping copied from documentation, from upstream, or from memory is a guess.
  Check `--help`, or run the thing.
- **Never silently downgrade a capability.** If an agent cannot fork, cannot stream, or
  cannot take a session id, that is an `Error::Unsupported`. A caller who asked to fork and
  got a linear resume has just corrupted the conversation they meant to branch, and a loud
  error is always better than a quiet wrong answer.
- **Never invent usage numbers.** `Usage` fields are `Option` because the three agents
  report different subsets. Absent means "the agent did not say", never zero, and never a
  figure derived from a local price table.
- **Context-shaped usage figures are already cumulative and must never be summed.** An agent
  re-sends the whole conversation each turn and reports it, mostly as cache reads, so adding
  `context_tokens` or `cache_read_tokens` across turns counts the same conversation once per
  turn. `Usage::accumulate` holds the per-field rule; extend it rather than hand-rolling a
  total. The vendors also disagree on what "input" counts, so a new field must say which
  convention it follows and normalize to one.
- **A follow-up message is rendered on send, never on echo.** `Run::send` delivers a message
  into a running turn and the host appends it to its transcript immediately, below the user's
  previous one. Claude can echo messages back with `--replay-user-messages` for a host that
  wants the agent to sequence its transcript; this crate deliberately does not pass it,
  because the caller already knows what it sent and waiting for an echo would delay the one
  thing the feature exists to make immediate. Do not add echo-based ordering.
- **This crate is a library. It has no binary and no CLI.** If something seems to need a
  command-line entry point, it belongs in the consumer, not here.
- **No shell.** Arguments are built as a `Vec<String>` and handed to `exec`. Never
  interpolate a prompt into a shell string; that is how a prompt containing `$(...)` becomes
  a command.
- **Cancellation is complete on Unix and incomplete on Windows.** Dropping, cancelling or
  timing out a run tears down the whole process group, so the commands an agent started die
  with it. On Windows only the direct child is killed: containing a tree there needs a Job
  Object, which this crate does not set up. Do not describe cancellation as cross-platform,
  and keep `tests/process.rs` honest by leaving it `#![cfg(unix)]` rather than making it
  pass vacuously elsewhere.
- **`src/proc.rs` holds the crate's only `unsafe`.** `Cargo.toml` sets
  `unsafe_code = "deny"` rather than `forbid` so that one audited call can be excepted. A
  second `unsafe` anywhere is a design question, not a local decision.

## CI runners

Use the explicitly pinned GitHub-hosted `ubuntu-24.04` image. Keep the complete CI
decision in one job so checkout, toolchain setup, dependency resolution, and runner
startup happen once per pull request. Do not add an external runner dependency without
an operational fallback: unavailable third-party capacity must not leave releases queued
indefinitely.

## Build & run

```bash
cargo build
cargo test
cargo fmt && cargo clippy --all-targets    # run after every change
```

### Live tests

```bash
cargo test --test live -- --ignored --test-threads 1
```

Spawns the real agents and consumes real quota, so it is `#[ignore]`d by default. Each test
skips itself when its binary is absent. **Run it after touching any argv mapping or output
parser**. The unit tests prove the code does what it says, only the live suite proves the
CLI agrees.

## Releasing

Publishing is automatic: merging a version bump in `Cargo.toml` to `master` publishes that
version to crates.io and tags the commit. Nothing else triggers it, and a version already on
the registry is a no-op, so a rerun or a revert cannot double-publish.

Two consequences worth holding on to:

- **A version bump is a release.** Required PR CI is the verification gate, then
  `cargo publish` performs the final package build. There is no staging step between merging
  one and it being permanent on crates.io, where a version number can never be reused. Bump
  the version in the commit you intend to ship, not ahead of it.
- **Do not repeat the full check suite in the publish job.** The required PR job already ran
  formatting, clippy, tests, docs, package validation, and audit against that commit. Running
  the same work again after merge costs another runner without testing different code.

Requires the `CARGO_REGISTRY_TOKEN` repository secret. Without it the job fails with that
name in the message rather than an opaque auth error from cargo.

## Architecture

Pure logic and I/O are kept apart so the mappings are testable without spawning anything.

| File | Role |
|---|---|
| `src/agent.rs` | The three agents, their capabilities, and argv building. **Pure.** |
| `src/request.rs` | The fluent request builder and its resolution into a `Plan`. **Pure.** |
| `src/event.rs` | Normalizing three JSON dialects into one event vocabulary. **Pure.** |
| `src/model.rs` | The per-agent model catalogue, and discovery where a CLI supports it. |
| `src/session.rs` | Name → native-id bindings on disk. |
| `src/run.rs` | Spawning, streaming, timeouts, failure classification. |
| `src/proc.rs` | Process-group teardown. The crate's only `unsafe`. |
| `src/outcome.rs` | What a finished run produced. |
| `src/account.rs` | Account-wide quota and usage, where a CLI exposes it. |
| `src/approval.rs` | The human-in-the-loop approval channel. **Pure.** |
| `src/error.rs` | One error type; one variant per case a caller must branch on. |

Anything pure gets ordinary unit tests in the same file. Keep it that way: a mapping that
needs a subprocess to test is a mapping in the wrong module.

## Verification

Run what you build before reporting it done. Type-checks and tests verify code correctness,
not feature correctness. **If you can't run it, say so explicitly** rather than implying
success. If an agent CLI isn't installed and you mapped its flags from a document, say that
plainly and mark it in the code.

- Compare against the base branch rather than asserting: a pre-existing failing test or
  clippy warning is not something you introduced, and saying so requires checking.
- `cargo build` finishing in under a second means it was cached, not that it rebuilt. Touch
  the sources when a rebuild is the thing you're verifying.

## PR discipline

**Always paste the full PR URL**
(`https://github.com/pathscale/RustAgentAbstraction/pull/<n>`), not just the number, so it's
clickable.

## Keeping docs honest

Hit a factual error here, such as a stale flag, a wrong version or a moved status? Fix
it in the same change. Don't open cosmetic rewording PRs.

Learned something durable, such as a CLI gotcha, a flag that changed, or a shape that
differs from the docs? It belongs **in this repo** (a comment next to the mapping, or
the README's gotchas
section), not in your agent's private memory. Repo docs are versioned, reviewable, and
visible to every agent and human; private memory dies with your machine.

## Git workflow

- **Always specify the branch when pushing**: `git push origin branch-name`
- **Branch naming**: `fix/issue-description` or `feat/issue-description`
- **Default branch is `master`**, not `main`.
- **Force-push your own branch freely.** Rebasing a feature branch onto a moved base, or
  amending before review, is normal and correct. Use `--force-with-lease` so you don't
  clobber someone else's push.
- **Never force-push the default branch.** That is the history everyone else builds on.

## No AI attribution

Never add AI attribution to anything in this repo or leaving it: no "Generated with
Claude Code" / robot-emoji footers, no `Co-Authored-By: Claude` (or any AI) trailers,
and no AI credit in commit messages, PR or issue titles/bodies, changelogs, release
notes, or code comments. Applies to every agent and every vendor. Work product should
be indistinguishable from a human teammate's.

## Writing style

No em dashes in prose or documentation. Restructure the sentence instead.
