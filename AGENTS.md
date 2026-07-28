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
- **This crate is a library. It has no binary and no CLI.** If something seems to need a
  command-line entry point, it belongs in the consumer, not here.
- **No shell.** Arguments are built as a `Vec<String>` and handed to `exec`. Never
  interpolate a prompt into a shell string; that is how a prompt containing `$(...)` becomes
  a command.

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

## Architecture

Pure logic and I/O are kept apart so the mappings are testable without spawning anything.

| File | Role |
|---|---|
| `src/agent.rs` | The three agents, their capabilities, and argv building. **Pure.** |
| `src/request.rs` | The fluent request builder and its resolution into a `Plan`. **Pure.** |
| `src/event.rs` | Normalizing three JSON dialects into one event vocabulary. **Pure.** |
| `src/session.rs` | Name → native-id bindings on disk. |
| `src/run.rs` | Spawning, streaming, timeouts, failure classification. |
| `src/outcome.rs` | What a finished run produced. |
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
