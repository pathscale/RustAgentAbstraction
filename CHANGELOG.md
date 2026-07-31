# Changelog

Notable changes per release, following [semantic versioning](https://semver.org).

The 0.x caveat: a minor bump is the only signal Cargo treats as incompatible, so behaviour
changes belong there once anything depends on this crate. While nothing does, they may
appear in a patch rather than inflating the version toward 1.0 on a crate still finding its
shape. **Where that happens the entry says so at the top**, because a version number that
under-signals is only acceptable if the changelog over-signals to compensate.

## 0.4.1

### Added

- **Slash commands as values, not strings.** `Request::command(Agent::Claude, &Command::Compact
  { instructions: None })` runs the CLI's own `/compact`, the answer to a conversation that has
  grown too long for the model to think in. `Command::Clear` discards one, and `Command::Other`
  reaches anything else this install offers.

  A host should not build `"/compact"` by hand. The literal is indistinguishable from a user who
  meant to type the word, and on an agent with no command vocabulary it is worse than useless: the
  model reads it as prose and answers a question *about* compaction, which looks from the outside
  exactly like the command having run. Codex and Copilot therefore refuse before spawning.

  **A command is its own turn, not an interruption**, and this is a constructor rather than
  something `Run::send` can deliver mid-turn for a reason found by running it. Injected into a
  live turn, the compaction emits its own `result` record *after* the turn's own, overwriting the
  outcome: the answer's text becomes the compaction's empty string and the turn's usage becomes
  the compaction's zeroes. As its own run resuming the session, it produces one clean terminal.

- **`Event::Compaction`**, reporting a compaction starting and settling. A conversation too short
  to summarise is refused with `Not enough messages to compact.`, which arrives here as
  `Finished { ok: false, error: Some(..) }` on a completed run rather than as an error: the
  command ran and answered. The outcome's text is empty and its `num_turns` is zero, because a
  compaction generates no answer, and neither is a failure.

- **`Event::Commands`**, the agent's own command catalogue, emitted once as a run starts. Claude
  publishes two lists at init and the split is the one a user sees: skills are capabilities
  someone installed, utilities are part of the tool, and `Commands::utilities()` is the other half
  of `Commands::skills`. Read from the agent rather than compiled in, because plugins, skills and
  user commands make the set per-install: a hardcoded list would describe the developer's machine
  instead of the user's.

- **`Caps::commands`**, so a host can ask before offering the button.

Verified against claude 2.1.212, including a live test that drives a real compaction end to end.
The assertion is on the lifecycle records rather than on the run succeeding, since an agent that
read the slash as prose would also succeed.

## 0.4.0

### Changed

- **`Usage::accumulate` sums the cache figures instead of taking the latest.** A behaviour
  change to a public method, and a minor bump for the reason at the top of this file:
  something depends on this crate now.

  `cache_read_tokens` and `cache_write_tokens` were filed with the context figures, on the
  reasoning that an agent re-sends the whole conversation each turn and reports it mostly as
  cache reads, so summing would count one conversation many times. That reasoning is right
  about `context_tokens`, which still takes the latest, and wrong about these. A turn's cache
  figure is not the conversation's size: the terminal record already sums the cache traffic
  of every call in the turn, which this crate's own parser test shows with calls of 100000
  and 102000 arriving as 202000. Every one of those reads is billed.

  The visible cost was a host whose token total could not explain its own cost figure:
  54.6k tokens beside $9.409, with six figures of billed cache reads missing from the count
  on a long conversation.

  **If you fold usage with `accumulate`, your session cache totals will rise.** They were
  the last turn's traffic and are now the session's. Per-turn `Outcome::usage` is untouched;
  only the folding rule moved.

## 0.3.8

### Added

- **`Event::Usage`, a token counter while the turn is still running**, for a status line
  like `7m 8s · 8.5k tokens`. One event per model call, foldable into a running total with
  `Usage::accumulate`, whose per-field rules were written for this.

  Deduplicated on `message.id`: Claude sends one `assistant` record per content block and
  each repeats the same usage, so a four-call turn arrives as eight records and reporting
  every one would count each call twice. Verified against claude 2.1.212 that the per-call
  figures accumulate to exactly the terminal record's totals, and a live test asserts it
  end to end so a counter cannot drift from the number that replaces it.

  **`output_tokens` is withheld on these events.** Mid-turn the agent reports the count as
  it stood when the message began, which understates the finished figure badly: one run
  whose per-call reports summed to 9 had generated 497 by the end. Reporting it would let a
  host build a counter that is simply wrong. The context and cache figures are exact.

  Claude reports throughout a turn. Copilot reports once, near the end, carrying its AI
  credit spend. Codex reports nothing until the turn completes and never emits this.

### Changed

- **The per-request context figure has one source now.** 0.3.7 computed it inline while
  parsing assistant records; the live counter needed the same arithmetic, and two copies of
  it are how a live figure and a final one drift apart. Folded into one place, with the
  event deduplicated on `message.id` and the context figure deliberately not, so a record
  carrying no id still keeps the context correct as it did before.

## 0.3.7

### Fixed

- **`Usage::context_tokens` could exceed the context window.** The figure was derived from
  the terminal record's usage, which Claude sums across every API request in the turn — a
  tool-heavy turn re-counts the conversation once per round trip, and a host displayed
  195% of a 1M window. Each `assistant` record carries its own request's usage; the last
  one's prompt side (input plus both cache figures) is the conversation as the model
  actually saw it, and is now the figure reported, with the terminal sum kept only as the
  fallback for streams whose assistant records carry no usage.

## 0.3.6

### Added

- **`Request::interactive` and `Run::send`**, so a user who types a correction mid-turn does
  not have to wait for the turn to end. The message is delivered into a run already under
  way and the agent takes it at its next step boundary, verified against claude 2.1.212: a
  three-command task told to stop after the first ran only that one, with the later two
  never executing.

  **Rendering rule for a host: append the message to the transcript below the user's previous
  one, immediately.** That is the whole contract. Claude can echo messages back with
  `--replay-user-messages`, and this crate deliberately does not pass it, because the caller
  already knows what it sent and waiting for an echo would delay the very thing this makes
  immediate.

  `send` returns `Error::Cancelled` once the turn has settled, so a message typed a moment
  too late is reported rather than dropped; it belongs in a new run resuming the session.

  Claude only: neither other agent reads a structured message stream on stdin, so both are
  `Error::Unsupported` before spawning. `approvals` now implies `interactive`, since both
  ride the same open stdin, and that implication is enforced where the command line is built
  rather than only in the builder, so a hand-made `Plan` cannot be inconsistent.

## 0.3.5

### Fixed

- **A wrong claim about Claude's `/usage` screen, in three places.** The docs said its figures
  are approximate and cover only one machine's local sessions. That disclaimer belongs to the
  *attribution* breakdown further down that screen (the per-MCP-server and per-skill shares),
  not to the three top-line percentages, which carry no such caveat.

  Replaced with what is actually verifiable: the run stream carries no utilization figure at
  all. Against claude 2.1.212 the entire `rate_limit_info` vocabulary is `status`, `resetsAt`,
  `rateLimitType`, `overageStatus`, `overageDisabledReason` and `isUsingOverage`, checked with
  an account at 32% of its session window. There is no percentage field to be missing, so
  waiting for a different event cannot produce one.

## 0.3.4

### Added

- **A human in the loop.** `Request::approvals` routes gated tool calls to the caller instead
  of letting the posture answer them: the call arrives as `Event::ApprovalRequest` carrying
  the tool and its arguments, the run blocks mid-turn, and `Run::respond` answers with
  `Decision::Allow` or `Decision::Deny`. A denial is not a failed run; the model is told no
  and carries on.

  Claude only, over its `--permission-prompt-tool stdio` control channel, verified end to end
  against 2.1.212 by denying a `touch` and confirming the file was never created.

  Four combinations are refused up front rather than met as a hang or a silence: Codex and
  Copilot, which have no headless approval channel; `run()`, which discards events so nobody
  could answer; `Permission::ReadOnly`, which removes the mutating tools outright so there
  would be nothing to ask about; and `respond` on a run that did not opt in.

  Two findings worth knowing. Claude decides what needs asking, and read-only commands run
  without a question, so the absence of a request is not proof that nothing ran. And under
  `--input-format stream-json` Claude keeps the session open for another message, so stdin is
  closed once the turn settles; without that the run only ended at its timeout even though
  the answer had already arrived.

  A note on scope: this is not the general duplex/background support deferred earlier. Stdin
  stays open only for the life of one run, and only when approvals were asked for.

## 0.3.3

### Added

- **`claude-opus-5[1m]` is catalogued**, and the Claude entries now state their context
  window. Running every id on claude 2.1.212 showed `claude-opus-5` is the odd one out:
  `claude-sonnet-5` and `claude-fable-5` are 1M natively, while it defaults to 200k and
  takes the `[1m]` suffix to widen. A picker can now offer both forms explicitly rather
  than leaving the suffix to be discovered.

## 0.3.2

### Fixed

- **A 1M-context session no longer reports a 200k window.** On claude, a run on any
  non-Haiku model lists a Haiku helper in its per-model usage, and lists it first. The
  window binding took the first entry, so `Usage::context_window` reported the helper's
  200,000 for every such run, which presented as sessions capped at 200k regardless of the
  `[1m]` variant in use. The binding now keys on the resolved model name the run announced
  at start (`claude-sonnet-5[1m]` both names the model and keys the usage map, verified on
  2.1.212). With several entries and no name to match, the window is now reported as
  unknown rather than guessed, since guessing is how the bug happened. `Terminal::model`
  records the resolved name.

### Verified, no code change needed

- The `[1m]` aliases already in the catalogue work headlessly, confirmed by running each on
  claude 2.1.212: `sonnet[1m]` and `opus[1m]` report `contextWindow: 1000000`, and the
  suffix composes with pinned ids (`claude-opus-5[1m]` runs). `fable[1m]` is accepted and
  resolves to plain `claude-fable-5`, which is 1M natively, so no `fable[1m]` entry was
  added: it would duplicate `fable` under a second name.

## 0.3.1

**Read the behaviour change below before upgrading.** It is the kind that would normally earn
a minor bump and is here only because nothing yet depends on the affected field being wrong.
`agent-abstraction = "0.3"` picks this up on an ordinary `cargo update`.

### Behaviour changes

- **`Usage::input_tokens` now means the same thing on every agent.** The vendors count
  oppositely: Claude reports the uncached remainder, Codex reports the whole prompt with the
  cached part inside it. Both were passed through into one field, so summing or comparing
  across agents was wrong, and a context tracker built on it was wrong for one of them.

  `input_tokens` is now the uncached remainder everywhere, derived on the Codex side by
  subtracting the cached count. **Codex figures will drop**, by exactly the cached portion.
  The number Codex reports is preserved as `context_tokens`.

  Verified across two turns of one Codex thread: input rose 15342 to 30703 while cached rose
  13056 to 28160. Had cached been separate, the second turn would have meant 30k *new* tokens
  for a four-word question.

### Added

- **A context tracker that needs no bookkeeping.** `Usage::context_tokens` is every input
  token a turn was charged for, which is the conversation as the model saw it, and
  `Usage::context_window` is the limit where the agent reports one. `Usage::context_used()`
  returns the share as an `f64` from 0.0 to 1.0. Claude reports both halves; Codex reports
  tokens without a window, so it gives tokens but no share.
- **`Usage::accumulate`** folds a turn into a session running total. Provided because the
  obvious loop is wrong: an agent re-sends the whole conversation each turn and reports it,
  mostly as cache reads, so summing the context figures counts the same conversation once per
  turn and the error grows with the session. Cost and generated tokens add; context-shaped
  figures take the latest value.
- **`Agent::account_usage()`** reports plan-wide quota: windows with a used percentage, their
  length and reset time, credit balance, per-day token buckets and lifetime totals. Backed by
  `codex app-server`, an experimental JSON-RPC interface. Claude and Copilot return
  `Error::Unsupported`, and `Agent::reports_account_usage()` answers that up front so a host
  can decide whether to build the panel rather than learning it from an error.

  Claude is not scraped for this deliberately. It reports quota only during a run, as
  `Event::RateLimit`, and the percentages on its `/usage` screen are not on the wire: the
  whole `rate_limit_info` vocabulary is `status`, `resetsAt`, `rateLimitType` and the overage
  fields, with no utilization figure anywhere in it.
- **Usage fields that were already on the wire and being discarded**: `reasoning_tokens`
  (Codex separates them), `max_output_tokens`, `duration_ms` and `api_duration_ms`, and
  `ai_credits_nano`, which is the Copilot billing unit that replaced premium requests. The
  crate had been capturing only the legacy counter.
- **`RateLimit::overage_status` and `RateLimit::is_using_overage`**, so a caller can tell a
  plan limit from an overage one rather than seeing only `allowed` or `rejected`.

Values throughout, never presentation: percentages are numbers, durations are minutes or
milliseconds, and a credit balance stays the string the provider sent so a decimal is not
rounded through a float on its way to a display.

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

- **A UUID containing `401` is no longer read as an authentication failure.** The status code
  was matched as a bare substring, so any Copilot failure became `Error::NotAuthenticated`
  whenever one of the ids it prints happened to contain those three digits, as in
  `"id":"1b0b1401-cb86-..."`. A run emits several ids, so the misdiagnosis was intermittent
  and sent the user to re-login over an unrelated failure. It now has to appear as a
  standalone token, which keeps `HTTP 401` and rejects every hex blob. The auth check also
  reads stderr and the agent's own prose rather than the raw stream, matching what the quota
  check already did.
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
