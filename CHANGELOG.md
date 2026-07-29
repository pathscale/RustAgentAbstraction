# Changelog

Notable changes per release, following [semantic versioning](https://semver.org).

The 0.x caveat: a minor bump is the only signal Cargo treats as incompatible, so behaviour
changes belong there once anything depends on this crate. While nothing does, they may
appear in a patch rather than inflating the version toward 1.0 on a crate still finding its
shape. **Where that happens the entry says so at the top**, because a version number that
under-signals is only acceptable if the changelog over-signals to compensate.

## 0.3.0

A minor bump because it adds API rather than because anything broke: existing code compiles
and behaves identically, and a run that sets no effort sends no flag.

### Added

- **`Request::effort`** sets the reasoning effort level. Previously the crate could not send
  one at all, so a host had no way to act on a level a user picked. Delivered as `--effort`
  on Claude and Copilot, and as `-c model_reasoning_effort=<level>` on Codex, which has no
  flag for it. The key was confirmed with `--strict-config` rather than assumed. Passed
  through verbatim, like the model.
- **`Model::efforts` is now populated for Claude and Copilot.** Both shipped empty while both
  CLIs document a `--effort` flag, so a picker had nothing to offer. Claude's five levels come
  from `claude --help` (2.1.212) and Copilot's seven from `copilot --help` (1.0.75). Codex was
  never empty: it reports levels per model, and they genuinely differ.

### Fixed

- **Copilot's `auto` no longer advertises effort levels it refuses.** Applying the documented
  set uniformly was wrong, and a live test caught it: `auto` exits 1 with
  `Model "auto" does not support reasoning effort configuration` rather than ignoring the
  flag, so offering a level there would have produced a failed run instead of a slower one.
  Effort support is not uniform within an agent even where `--help` lists one set.

## 0.2.2

Additive: nothing changes for existing code. A patch rather than a minor bump because no
behaviour moved, only new API arrived.

### Added

- **A model catalogue per agent**, so a host can render a picker without hard-coding three
  vendors' worth of ids. `Agent::models()` returns them best first, `Model::kind`
  distinguishes an alias from a pinned id, and `Model::is_default` marks the safe
  pre-selection. `Agent::models_verified()` records how each list was established and against
  which release, because the three were gathered three different ways and the weakest is the
  one worth distrusting.

  The list is **advisory and never enforced**. `Request::model` still takes any string and
  nothing checks it, so a model released this morning is not blocked by a list compiled last
  month, and one the account cannot reach fails as `Error::AgentError` with the provider's
  own status. Two findings from building it are why:

  - **A catalogue is not an entitlement.** Copilot's picker lists twenty-three models and a
    Free plan permits exactly one, refusing every other id before a request is made,
    including `gpt-5.4` from Copilot's own `--help`.
  - **An alias and a pinned id do not always agree.** On claude 2.1.212, `--model opus`
    reported `claude-opus-4-8` while `--model claude-opus-5` reported `claude-opus-5`, even
    though that release's notes call Opus 5 the default Opus model. Both forms are carried
    for that reason.
- **`Model`, `Kind`, `Source` and `Verified`** are re-exported at the crate root, all
  `serde`-serializable so a host can hand them straight to a UI layer.
- **`Agent::discover_models()`** asks the CLI itself, which reflects the installed binary
  rather than the one this crate was written against. Codex answers through
  `codex debug models`, including per-model reasoning levels, and its hidden internal model
  is filtered out. Claude and Copilot return `Error::Unsupported`: both enumerate models only
  in an interactive picker, and a caller asking for discovery is asking for freshness, so
  quietly returning the compiled list would answer a question they did not ask.

### Changed

- **The Claude version these flags are verified against is now 2.1.212**, up from 2.1.205.
  Only `Probe` output changes: an installed 2.1.205 now reports `VersionStatus::Older`
  where it previously reported `Verified`. Nothing about how requests are built changed. The
  bump was made after the full live suite passed against 2.1.212 and the permission-mode
  choices were re-read from its `--help`, rather than on the assumption that a patch release
  of the CLI changed nothing.

## 0.2.1

A patch by version number, but **read the behaviour changes**: they are the kind that would
normally earn a minor bump, and are here only because nothing depends on this crate yet.
`agent-abstraction = "0.2"` picks this up on an ordinary `cargo update`, so if you are
already rendering events, check the first item.

### Behaviour changes

- **Streaming is now the default, and text arrives token by token on Claude.** `Format`
  defaulted to `Json`, under which nothing is observable until the turn ends: a twenty-minute
  run reported nothing for twenty minutes. Worse, `Request::session` *pinned* that format, so
  the multi-turn path a chat UI always uses could not stream at all.

  Three consequences for a caller who does not pin a format:
  - `stream()` now yields events during a run that previously yielded none.
  - `Event::Text` is token-level on Claude, not message-level. A transcript that appended one
    Text event per line now receives `"po"`, `"ng"` where it received `"pong"`.
  - `run()` parses a JSONL stream rather than one document. The `Outcome` is unchanged.

  Pin `Format::Json` to keep the old behaviour.
- **A turn the agent says failed is now an error, not an answer.** All three exit `0` for
  some failures and put the explanation where the answer belongs: ask Claude for a model
  that does not exist and it exits cleanly, reports `subtype: "success"`, and returns
  "There's an issue with the selected model" as its reply. A caller checking `Result::is_ok`
  rendered that as the model's answer. These are now `Error::AgentError`, carrying the
  agent's own wording and the provider's status where one was given (typically 404 for an
  unknown model). `NotAuthenticated` and `RateLimited` keep their own variants, since the
  remedy differs; this covers the rest of that family. Codex forwards the upstream body as a
  JSON *string*, so its message is unwrapped to the sentence and the status pulled out of it.
- **An unauthenticated Copilot run is now reported as one.** Its wording shares no phrase with
  Claude's or Codex's, so it previously fell through to a generic `Error::Failed` with no
  login hint. Callers matching on `Error` should expect `NotAuthenticated` from Copilot now.

### Added

- **`Request::schema`** constrains an answer to a JSON Schema, with the conforming value on
  `Outcome::structured` already parsed. For answers that are data rather than prose, such as
  review findings, this replaces guessing at formatting the model never promised. Claude takes
  the schema inline, Codex reads it from a file this crate writes and removes, and Copilot
  1.0.75 has no schema support so asking is `Error::Unsupported`.

  Write schemas strictly: Codex sends yours to a provider that requires
  `"additionalProperties": false` on every object and rejects the request with a 400 before
  the model runs otherwise.
- **`AuthStatus::check(agent)`** answers whether an agent is logged in without spending a
  request. Claude reports JSON, Codex reports prose, Copilot offers neither and is reported as
  `AuthState::Unknown` rather than a logout, since telling someone to re-authenticate a
  working setup is worse than admitting the question cannot be answered.

### Fixed

- **A healthy quota heartbeat is no longer read as a refusal.** Every Claude `stream-json`
  run prints a `rate_limit_event` record, and on a healthy run its status is `allowed`.
  Classification scanned the raw stream for the substring `rate_limit`, which that record
  contains, so any Claude failure came back as `Error::RateLimited`: a caller was told to
  back off when the real cause was one they could fix. The parsed signal and the agent's own
  prose now decide, and the raw scan is only the fallback for output that produced neither.
- **A failure reports its cause rather than a status line.** The first line of stderr was taken
  as the explanation, but CLIs open with progress chatter: a rejected Codex schema reported
  "Reading additional input from stdin...", which is not what went wrong. A line that looks
  like an error now wins, and stdout is consulted when stderr only narrates, since Codex
  reports errors as JSON events on stdout.

## 0.2.0

Nothing was removed or renamed, so this compiles against any 0.1 code. It is a minor bump
rather than a patch because some changes alter behaviour, and `"0.1"` would have delivered
them through an ordinary `cargo update`.

*Corrected after release: this entry originally also credited 0.2.0 with streaming by default
and token-by-token Claude text. Those landed three hours after 0.2.0 was published and ship in
0.2.1, where they are now listed. The published 0.2.0 does not have them.*

### Behaviour changes

- **An unauthenticated run is now an error.** Claude exits `0` and puts
  "Not logged in · Please run /login" in its result text, so a missing login previously came
  back as a successful `Outcome` whose answer was a login prompt. Auth is now checked
  regardless of exit code, as quota refusals already were, and reported as
  `Error::NotAuthenticated` carrying the command that fixes it. Callers matching on `Ok`
  should expect this variant. `Error::is_auth_failure` distinguishes it.
- **Event payloads are bounded at `MAX_EVENT_BYTES`** (64 KiB), marked with
  `TRUNCATION_MARK` when shortened. Oversized tool arguments are replaced rather than cut,
  since truncated JSON no longer parses.
- **Identifiers are validated rather than truncated.** A session id, tool-call id or tool
  name over `MAX_IDENTIFIER_BYTES` (4 KiB) is rejected: an oversized session id is not
  captured or persisted, and an oversized tool id is dropped while its event is still
  reported. Shortening one would be worse than losing it, since a truncated session id
  resumes nothing and a truncated tool id matches the wrong call.
- **The pending-tool map is bounded by bytes**, not only by entry count. Counting entries
  bounded nothing while the entries themselves were unbounded.

  Together these give a real ceiling on queued memory. Each event is at most 64 KiB of
  payload plus a few identifiers of 4 KiB, so a full 256-deep channel holds under about
  21 MiB, with a further 256 KiB of pending-tool state. Before this release the figure was
  unbounded in practice: identifiers were exempt from every limit, so a single line could
  carry a 512 KiB id and 256 queued events could reach roughly 128 MiB.

### Fixed

- **`COPILOT_GITHUB_TOKEN` reaches Copilot under `EnvPolicy::Minimal`.** It is the
  *highest-precedence* credential variable Copilot accepts and it was missing from the
  list, so a host authenticating that way would have failed to authenticate at all once
  `Minimal` became the default.
- **A failure now reports its cause rather than a status line.** The first line of stderr
  was taken as the explanation, but CLIs open with progress chatter: a rejected Codex schema
  reported "Reading additional input from stdin...", which is not what went wrong. A line
  that looks like an error now wins, and stdout is consulted when stderr only narrates, since
  Codex reports errors as JSON events on stdout.
- **Dropping a `Run` now reliably kills the process group.** It signalled the driver and
  aborted it, which left the kill waiting on the runtime to poll the aborted task. On Linux
  that did not reliably happen and grandchildren survived, while `cancel` and timeouts were
  unaffected because both kill directly. `Run` now holds the pid and kills synchronously in
  `Drop`.
- Output lines are bounded at `MAX_LINE`. A line that never ended could exhaust memory
  before any total cap applied, because the reader accumulated until a newline.

### Added

- **`Request::schema`** constrains an answer to a JSON Schema, with the conforming value on
  `Outcome::structured` already parsed. For answers that are data rather than prose, such as
  a set of review findings, this replaces guessing at formatting the model never promised.
  Delivery differs and is hidden: Claude takes the schema inline and reports the value in its
  own field, Codex reads it from a file this crate writes and removes. Copilot 1.0.75 has no
  schema support, so asking is `Error::Unsupported` rather than prose presented as data.

  Write schemas strictly: Codex sends yours to a provider that requires
  `"additionalProperties": false` on every object, and rejects the request with a 400 before
  the model runs otherwise. Claude is more forgiving, so a schema that works there can still
  fail on Codex.
- **`AuthStatus::check(agent)`** answers whether an agent is logged in without spending a
  request, which a missing login otherwise only revealed by running a turn and failing.
  Claude reports JSON, Codex reports prose, and Copilot offers neither: that case is
  `AuthState::Unknown` rather than a logout, since telling someone to re-authenticate a
  working setup is worse than admitting the question cannot be answered.
- **`Probe`** reads a CLI's `--version` and compares it against
  `Agent::verified_version`, the release its flag mappings were checked against, reporting
  `Verified` / `Newer` / `Older` / `Unrecognized` with an `advisory()` written to be shown to
  a person. Not automatic: probing spawns a process, so a host calls it at startup.
- **`Error::FlagRejected`**, separated from `Error::Failed` when a CLI refuses an argument.
  Usually version drift, where nothing about the request is wrong and the wrapper and the CLI
  simply disagree about what is accepted.
- `Agent::login_hint`, the per-agent command that resolves a missing login.
- `MAX_LINE`, `MAX_EVENT_BYTES` and `TRUNCATION_MARK` are public, so a consumer can size its
  own buffers against the same numbers.

## 0.1.0

First release. Drives Claude Code, Codex and GitHub Copilot headlessly behind one request
type, one event vocabulary and one session model, with resume and fork.

Every flag mapping and output shape was verified against the installed CLIs rather than
taken from their documentation, against `claude 2.1.205`, `codex-cli 0.145.0` and
`GitHub Copilot CLI 1.0.75`.
