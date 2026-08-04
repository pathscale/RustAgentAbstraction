# agent-abstraction

[![crates.io](https://img.shields.io/crates/v/agent-abstraction.svg)](https://crates.io/crates/agent-abstraction)
[![docs.rs](https://img.shields.io/docsrs/agent-abstraction)](https://docs.rs/agent-abstraction)
[![license](https://img.shields.io/crates/l/agent-abstraction.svg)](LICENSE)

Drive **Claude Code**, **Codex** and **GitHub Copilot** headlessly from Rust: one request
type, one event vocabulary, one session model across three CLIs that agree on none of those
things.

```toml
[dependencies]
agent-abstraction = "0.2"
```

Every flag mapping and output shape here was verified against the installed CLIs rather than
taken from documentation, which is the part that keeps being wrong.

It is a **library, not a CLI**. Your program links it and spawns the agent directly, so
nothing marshals a request through a command line and back out of stdout twice.

```rust
use agent_abstraction::{Agent, Permission, Request, run};

let outcome = run(
    &Request::new(Agent::Claude, "Reply with the single word: pong")
        .model("haiku")
        .permission(Permission::ReadOnly),
)
.await?;

println!("{}", outcome.text);          // "pong"
println!("{:?}", outcome.usage.cost_usd);
```

## What each agent can actually do

Verified live, against `claude 2.1.205`, `codex-cli 0.145.0` and `GitHub Copilot CLI 1.0.75`
and not inferred from documentation.

| | session id | fork | events | system prompt | resume flag |
|---|---|---|---|---|---|
| **Claude Code** | caller-minted (`--session-id`) | yes (`--fork-session`) | `stream-json` | native (`--append-system-prompt`) | `--resume` |
| **Codex** | agent-printed (`thread_id`) | no | `--json` | prepended to prompt | `exec resume <id>` |
| **Copilot** | caller-minted (`--session-id`) | no | `--output-format json` | prepended to prompt | `--session-id` |

### Can I choose the session id, or do I have to read it back?

Both, depending on the agent. Verified by round-trip, not from `--help`:

| | assign it up front | read it back |
|---|---|---|
| **Claude Code** | yes, `.session_id(uuid)` | also reported |
| **Copilot** | yes, `.session_id(uuid)` | also reported |
| **Codex** | **no** | `thread_id`, before it answers |

```rust
// Claude and Copilot: the id is yours to pick, so it can match a thread id
// your app already has, with no mapping table in between.
let mine = uuid::Uuid::new_v4().to_string();
let outcome = run(&Request::new(Agent::Claude, "hi").session_id(&mine)).await?;
assert_eq!(outcome.session.as_deref(), Some(mine.as_str()));
```

Both CLIs require a valid UUID. Asking Codex for an assigned id is an
`Error::Unsupported` raised before spawning, never a silently unrelated conversation.

**Assigning beats reading back**, where you get the choice: the binding exists *before* the
process starts, so a run that dies mid-turn still leaves a resumable session. Codex's
`thread_id` can only be recorded once it has been printed.

That said, Codex prints it early. It arrives in `thread.started`, the first record of the
stream, **before any answer text**, so a host can persist the binding the moment the stream
opens rather than waiting for the turn to finish. `Event::Started` is the normalized form of
that moment for all three agents.

Asking an agent for something it cannot do is always an `Error::Unsupported`, never a quiet
downgrade. A caller that asked to fork and silently got a linear resume would corrupt the
conversation it meant to branch.

## Streaming

```rust
let mut running = stream(&Request::new(Agent::Claude, "audit this repo"))?;
while let Some(event) = running.recv().await {
    match event {
        Event::Text(text) => print!("{text}"),
        Event::ToolCall { name, .. } => println!("[{name}]"),
        Event::RateLimit(limit) => eprintln!("quota: {}", limit.status),
        _ => {}
    }
}
let outcome = running.finish().await?;
```

**Streaming is the default.** `Format::Stream` is what you get without asking, because the
alternative is silence: under `Format::Json` a run that takes twenty minutes reports nothing
for twenty minutes, which is indistinguishable from a hang. `Stream` carries everything
`Json` does, session id and schema-conforming value included, so the default costs only
parsing.

Text arrives token by token on Claude (via `--include-partial-messages`) and Copilot, and
message by message on Codex, which has no finer granularity. Claude sends deltas *and* the
completed message they build up to; only the deltas are emitted, or a transcript would show
every answer twice.

`Event::Text` is the *incremental display stream*. `Outcome::text` is the agent's own
*authoritative answer*. They are deliberately separate: concatenating the deltas is not
guaranteed to equal the final text (Copilot emits both; Claude emits only the latter), so
read `Outcome::text` for the answer and never sum the events.

## Sessions

Thread one stable name across turns; the store maps it to whatever handle the agent
understands.

```rust
let store = SessionStore::open("/var/lib/myapp/sessions");

let turn = Request::new(Agent::Claude, "what did I ask you to remember?")
    .session(&store, ".", "thread-42", /* fork */ false)?;

assert_eq!(turn.session_phase(), Some(Phase::Continue));
let outcome = run(&turn).await?;
```

Records live at `<dir>/<project-slug>/<name>.json`, partitioned by project so the same name
in two checkouts never collides, and written through a temp file and a rename so a
concurrent reader never sees a half-written record. A corrupt record reads as absent: the
next turn opens a fresh conversation rather than failing over a cache nobody asked about.

Names are percent-encoded into a single path segment, which is **injective**: two different
names can never land on the same file. That matters more than it sounds, because the failure
mode of a lossy scheme is silent, not loud. Folding unsafe characters to `-` would map
`café` and `cafe-` together, and the second session to use that name would quietly resume
the first one's conversation. Uppercase is encoded too, since macOS and Windows are
case-insensitive and would otherwise collide `Chat` with `chat`. The record keeps your
original name, so `list()` hands back what you passed, not a mangled segment.

## Choosing a model

`Agent::models()` returns what each agent offers, so a host can render a picker without
hard-coding three vendors' worth of ids:

```rust
for model in Agent::Claude.models() {
    println!("{:24} {}", model.id, model.name);   // "opus", "Opus"
}
```

The list is **advisory and never enforced**. `Request::model` takes any string and nothing
here checks it, so a model released this morning is not blocked by a list compiled last
month. A model the account cannot reach comes back as `Error::AgentError` with the
provider's own status and wording.

That distinction matters more than it sounds, because **a catalogue is not an entitlement**.
Copilot's picker lists twenty-three models and a Free plan permits exactly one:

```text
Your Copilot Free plan currently includes only Auto, which automatically selects the
best available model for each task.
```

Every other id there is refused before a request is made, including `gpt-5.4`, the example
in Copilot's own `--help`. Treat the list as choices to try, and let the run report what
the account actually allows. `Model::is_default` marks the safe pre-selection.

Aliases and pinned ids are both carried, and `Model::kind` tells them apart, because they
do not always agree. On claude 2.1.212, `--model opus` reported `claude-opus-4-8` while
`--model claude-opus-5` reported `claude-opus-5`, even though that release's own notes call
Opus 5 the default Opus model. An alias is whatever the account resolves it to.

### Reasoning effort

`Model::efforts` lists the levels each model takes, and `Request::effort` sends one:

```rust
let request = Request::new(Agent::Codex, prompt).model("gpt-5.6-sol").effort("ultra");
```

Passed through verbatim, like the model, because the sets are not interchangeable: Claude
documents five levels, Copilot seven, and Codex varies them per model, offering `ultra` on
its two frontier models and not on the rest. Delivered as `--effort` on Claude and Copilot,
and as `-c model_reasoning_effort=<level>` on Codex, which has no flag for it.

Support is not uniform even within one agent. Copilot's `auto` **exits 1** rather than
ignoring the flag:

```text
Error: Model "auto" does not support reasoning effort configuration (requested: "low")
```

so its catalogue entry carries no levels. An empty `efforts` means a picker has nothing to
offer for that model, whether because it accepts none or because the levels are not
established here; the entry says which.

Where a CLI can be asked directly, prefer that:

```rust
let models = Agent::Codex.discover_models().await?;   // reflects the installed binary
```

`discover_models` returns `Error::Unsupported` on Claude and Copilot rather than silently
returning the compiled list: both enumerate models only in an interactive picker, and a
caller asking for discovery is asking for freshness. `Agent::models_verified()` records how
each compiled list was established and against which release.

### Disabling thinking

`Request::thinking(false)` turns the model's reasoning off for a run; left unset the agent
keeps its own default, which for Claude is adaptive thinking:

```rust
let request = Request::new(Agent::Claude, prompt).thinking(false);
```

Only Claude has a lever, and it is not a flag. The `claude` CLI builds the API `thinking`
block itself and gates it on `MAX_THINKING_TOKENS` (verified against claude 2.1.212: the
block is sent only while that value is above zero), so `thinking(false)` sets
`MAX_THINKING_TOKENS=0` in the child environment, which wins over `EnvPolicy` the same way an
explicit `env()` does. Codex and Copilot have no equivalent off switch, so `thinking(false)`
is a no-op for them and their reasoning is steered by `effort` instead. `effort` and
`thinking` are independent: effort sets how hard the model reasons, thinking whether it
reasons at all.

## Structured answers

When the answer is data rather than prose, constrain it with a JSON Schema and read it
back parsed instead of guessing at formatting the model never promised:

```rust
let outcome = run(&Request::new(Agent::Codex, "Alice is 30 years old.")
    .schema(r#"{"type":"object",
                "properties":{"name":{"type":"string"},"age":{"type":"integer"}},
                "required":["name","age"],"additionalProperties":false}"#))
    .await?;

assert_eq!(outcome.structured.unwrap()["name"], "Alice");
```

The delivery differs and is hidden: Claude takes the schema inline and reports the value in
its own field, Codex reads it from a file this crate writes and removes, and returns the
value as its answer text. **Copilot 1.0.75 has no schema support at all**, so asking is an
`Error::Unsupported` rather than prose dressed up as data.

**Write schemas strictly.** Codex sends yours to OpenAI's structured-output API, which
requires `"additionalProperties": false` on every object and every property in `required`.
Without it the request fails with a 400 before the model runs. Claude is more forgiving, so
a schema that works there can still fail on Codex; writing to the stricter rule keeps one
schema usable for both.

## Cancellation

**Dropping a `Run` kills the agent, and everything it spawned.** Closing a window or
cancelling a request should stop the work, not leave an agent running invisibly, spending
quota and writing files with nobody watching.

```rust
let running = stream(&request)?;
drop(running);                  // agent and its children are killed
running.cancel().await?;        // cooperative: returns only once the tree has exited
running.detach();               // opt out: keep running unsupervised
```

`cancel` is cooperative rather than an abort: the driver signals the process group, reaps
the child and joins its readers before returning `Error::Cancelled`. So when it returns the
tree really is gone, which matters if the next thing you do touches the files it was working
on.

`drop` **synchronously signals** the process group on Unix, then aborts the driver. What it
cannot do is wait: `Drop` cannot await, so it does not block until the child has exited and
its readers are joined. Both kill the tree; only `cancel` tells you when it is gone. On
Windows only the direct child is signalled.

Each run gets its own process group on Unix, and cancellation, drop and timeout all tear
down the whole group. Killing only the CLI would orphan the commands *it* started, which
keep holding files and credentials afterwards. Windows has no equivalent here yet: only the
direct child is killed, since containing a tree there needs a Job Object.

## Sending while the agent is working

A user who types a correction mid-turn should not have to wait for the turn to end.
`Request::interactive` keeps the input channel open and `Run::send` delivers a message into
a run already under way:

```rust
let request = Request::new(Agent::Claude, prompt).interactive();
let mut run = stream(&request)?;

// Keep input independent from the task continuously draining run.recv().
let control = run.control();

// ... from the UI thread, the moment the user hits enter:
control.send("actually, skip the tests and just fix the parser").await?;
```

The agent takes it at its **next step boundary**, not mid-token. Verified against claude
2.1.212: a three-command task told to stop after the first ran only that one.

### How a host should render this

Append the message to the transcript below the user's previous one, immediately, and carry
on. That is the whole rule.

There is deliberately no message echo to wait for. The CLI can echo messages back with
`--replay-user-messages` for a host that wants the agent to sequence its transcript, and
this crate does not use it: the caller already knows what it sent, so an echo would only
report something it knew. `Run::send` does wait for transport acceptance before its future
resolves. Codex must acknowledge `turn/steer`; Claude's input must be written and flushed.
Render immediately on send, then recover the same visible message if that receipt fails.

### One boundary worth handling

`send` returns `Error::Cancelled` once the turn has settled. A message typed a moment too
late is not silently dropped, it is reported, and it belongs in a **new run resuming the
session** rather than in the finished one:

```rust
if let Err(e) = run.send(&text).await {
    if e.is_cancelled() {
        // the turn ended first: start a new run with .session(...) and send it there
    }
}
```

`Caps::live_follow_up` and `Caps::approvals` let a host decide whether to offer these
controls before building a request. Claude and Codex support both; interactive Codex runs
use app-server while ordinary Codex runs keep using `exec`. Copilot returns
`Error::Unsupported` before spawning. `approvals` implies `interactive`, since both need
the same open control channel.

## Slash commands

A conversation that outgrows its context window makes the model measurably worse at the thing
it was doing. `/compact` is the CLI's answer: it summarises the conversation and continues from
the summary. `Request::command` runs it as a value rather than a string:

```rust
use agent_abstraction::{Agent, Command, Compaction, Event, Request, stream};

let request = Request::command(Agent::Claude, &Command::Compact { instructions: None })
    .resume(&session_id);

let mut run = stream(&request)?;
while let Some(event) = run.recv().await {
    if let Event::Compaction(Compaction::Finished { ok, error }) = event {
        // `ok: false` with a reason is an answer, not an error.
    }
}
```

**Do not build the literal yourself, and do not send it with `Run::send`.** Two reasons, both
found by running it:

- On Codex and Copilot there is no command vocabulary, so `/compact` arrives as prose and the
  model answers a question *about* compaction. That is indistinguishable from success for a
  caller checking `is_ok`, which is why both refuse before spawning.
- Injected into a live turn, the compaction emits its own `result` record **after** the turn's,
  overwriting the outcome: the answer's text becomes the compaction's empty string and the
  turn's usage becomes its zeroes. A command is its own turn, resuming the session.

A compaction writes no answer, so the outcome's text is empty and `num_turns` is zero. Neither
is a failure, and a conversation too short to summarise is refused with a reason on a run that
completed. `Event::Compaction` is what says whether it worked.

The catalogue is the agent's own. Claude reports it at init and `Event::Commands` carries it,
split the way a user sees it: skills are capabilities someone installed, utilities are part of
the tool.

```rust
Event::Commands(commands) => {
    commands.has("compact");   // offer the button only if it exists
    commands.utilities();      // the built-in half
    commands.skills;           // the installed half
}
```

Read rather than compiled in, because plugins, skills and user commands make the set
per-install: a hardcoded list would describe the developer's machine instead of the user's.
`Caps::commands` says whether an agent has one at all.

## Human in the loop

Every `Permission` answers the approval question up front, which is what lets a headless run
finish unattended. For a desktop app the point is to *ask*. `Request::approvals` routes
gated tool calls to the caller instead:

```rust
let request = Request::new(Agent::Claude, prompt)
    .permission(Permission::Edit)
    .approvals();

let mut run = stream(&request)?;
while let Some(event) = run.recv().await {
    if let Event::ApprovalRequest(approval) = event {
        // approval.tool is "Bash"; approval.input carries the actual command
        let decision = if user_said_yes(&approval) {
            Decision::Allow
        } else {
            Decision::deny()
        };
        run.respond(&approval.id, &decision).await?;
    }
}
let outcome = run.finish().await?;
```

**Show `approval.input` before deciding.** For `Bash` it carries the command; approving on the
tool name alone approves an unseen command.

Four things are refused up front rather than met as a hang or a silence:

| combination | why |
|---|---|
| Copilot | it has no headless approval channel and needs `--allow-all-tools` to run headlessly at all |
| `run()` instead of `stream()` | `run` discards events, so nobody could answer |
| `Permission::ReadOnly` | it removes the mutating tools outright, so there is nothing left to be asked about, and a caller would never be asked |
| `respond` on a run that did not opt in | there is no channel to answer on |

A denial is not a failure: the model is told no, works around it, and the turn completes.
Claude also lists every refusal in its terminal record.

**Claude decides what needs asking, not this crate.** Read-only commands run without a
question: verified on 2.1.212, `whoami` runs unasked while `touch some-file` asks. So the
absence of a request is not proof that nothing ran.

The user's own settings stay loaded. Suppressing them with `--setting-sources ""` would
discard their CLAUDE.md, and it is not needed: a mutating command still asks with those
settings in place.

## Permissions

`Permission` maps one posture onto each agent's own vocabulary:

| | Claude | Codex | Copilot |
|---|---|---|---|
| `ReadOnly` | `dontAsk` + `--disallowedTools` | `--sandbox read-only` | `--deny-tool=shell,write` |
| `Plan` | `--permission-mode plan` | `--sandbox read-only` | `--mode plan` |
| `Edit` | `acceptEdits` | `--sandbox workspace-write` | `--deny-tool=shell` |
| `Auto` | `--permission-mode auto` | `--sandbox workspace-write` | `--allow-all-paths` |
| `Bypass` | `bypassPermissions` | `--dangerously-bypass-approvals-and-sandbox` | `--allow-all-paths` |

The default is `ReadOnly`. Widen it explicitly.

Use `Request::cwd` for the primary workspace and repeat `Request::add_dir` for repositories
or folders the agent also needs to edit. The mapping stays provider-neutral: Claude and
ordinary Codex runs receive `--add-dir`, while interactive Codex runs receive the same paths
as app-server runtime and workspace-write roots.

```rust
let request = Request::new(agent, prompt)
    .cwd("/work/task")
    .add_dir("/work/repository")
    .permission(Permission::Auto);
```

**What this does not cover.** These postures constrain each CLI's *built-in* tools: its
shell, its file writes, its sandbox. They do **not** constrain MCP servers, plugins or
custom tools, which are a separate tool category in all three CLIs. An MCP tool that files
an issue, writes to a database or calls a deploy API can still act during a nominally
read-only run. Claude's mapping denies `mcp__*` as well, but the other two have no
equivalent switch, so if a run must not cause remote side effects the containment has to be
which MCP servers are enabled at all.

Two more honest limits: Codex has no true plan mode, so `Plan` maps to its read-only
sandbox (writes blocked, execution still permitted), and `unchecked_args` can contradict any
of this by design.

## Is each agent logged in?

Without spending a request:

```rust
for agent in Agent::ALL {
    let status = AuthStatus::check(agent).await?;
    println!("{agent}: {}", status.summary());
}
```

```text
claude-code: logged in as you@example.com (max)
codex:       logged in as ChatGPT
copilot:     unknown: copilot exposes no status command, so this cannot be
             confirmed without spending a request
```

Claude answers JSON (`claude auth status`), Codex answers prose
(`codex login status`), and **Copilot offers neither**. That third case reports `Unknown`
rather than "logged out", because telling someone to re-authenticate a working setup is
worse than admitting the question cannot be answered. `needs_login()` is true only for a
*confirmed* logout, so gating on it never nags about an agent that simply cannot be asked.

## Environment isolation

**`EnvPolicy::Minimal` is the default.** Inheriting the whole environment is what a CLI gets
from a shell, but this crate runs inside processes that hold unrelated secrets, and full
inheritance hands every one of them to the agent and to every command the agent runs. That
is worth deciding deliberately, so it is the opt-in:

```rust
Request::new(Agent::Claude, "review this")     // Minimal, nothing to configure
Request::new(Agent::Claude, "review this").env_policy(EnvPolicy::Inherit)   // opt in
```

`Minimal` passes through only what the selected agent needs. The crate owns that list per
agent rather than the caller, because an incomplete hand-written one fails as an
authentication error rather than as an obvious config mistake.

The list was derived by experiment, not assumption: `PATH` + `HOME` alone is **not** enough,
because Claude's keychain lookup is keyed on `USER` and returns "Not logged in" without it.
`PATH` + `HOME` + `USER` is the verified floor for all three on macOS. Windows names are
included on the same reasoning but are unverified.

Proxy and custom-CA variables are deliberately **excluded**. They are situational rather
than required, and `HTTPS_PROXY` routinely embeds credentials (`http://user:pass@proxy`),
so forwarding them automatically would leak one through the policy meant to withhold
secrets. A host that needs them should surface them as a setting; `NETWORK_ENV` names them
so a settings screen does not have to hardcode the list:

```rust
for name in NETWORK_ENV {
    if let Ok(value) = std::env::var(name) {
        request = request.env(*name, value);
    }
}
```

Two tests keep it honest: a live one asserting every agent still authenticates under
`Minimal`, so an incomplete list fails loudly, and a deterministic one asserting the host's
own variables do not reach the child.

## Gotchas worth knowing

- **`codex exec` refuses to run outside a git repository.** This crate always passes
  `--skip-git-repo-check`, so it runs anywhere. That check exists to stop an agent editing
  files with no way to undo them; the sandbox is the real containment here, and it defaults
  to `read-only`.
- **`codex exec resume` does not accept `--sandbox`.** It is a different option set from
  `codex exec` and rejects the flag outright, so the permission posture is applied as
  `-c sandbox_mode=...` on the resume path. Only a multi-turn run reveals this: every
  single-turn test passes either way.
- **Copilot's tool filters need `=`.** They are declared `--deny-tool[=tools...]`, an
  optional value, which binds only as `--deny-tool=shell`. Across a space the value is read
  as a positional and the deny is silently lost. This crate always emits the combined form.
- **Copilot needs `--allow-all-tools` to run headlessly at all**, or it stalls at the first
  tool confirmation. It is always emitted; `Permission` then narrows via denies.
- **A failed turn still exits 0.** Ask Claude for a model that does not exist and it exits
  cleanly, reports `subtype: "success"`, and puts "There's an issue with the selected model"
  where the answer belongs. Codex does the same and wraps the upstream body in a JSON string.
  Both come back as `Error::AgentError`; see below.
- **`claude-opus-5` defaults to a 200k window while every other 5-series model is 1M.**
  Verified by running each id: `claude-sonnet-5` and `claude-fable-5` report a 1,000,000
  token window natively, `claude-opus-5` reports 200,000 and needs the `[1m]` suffix
  (`claude-opus-5[1m]`) to widen. The catalogue carries both forms.
- **Claude's `stream-json` requires `--verbose`**, or it refuses to start. Handled.
- **Large prompts move to stdin automatically** above 128 KiB, so a long prompt never fails
  with `E2BIG`.

## When a vendor changes its output

The CLIs move. A format change shows up here as a run that exits `0` and returns nothing,
which is a miserable thing to debug from the outside, so `Outcome` carries the evidence:

```rust
if outcome.looks_like_a_format_change() {
    tracing::error!(
        unparsed = outcome.unparsed,
        sample = ?outcome.first_unparsed,
        "the CLI is healthy; this crate's parser is not",
    );
}
```

Unparseable lines are counted rather than discarded. A non-zero count on its own is normal
(agents interleave banners with their JSON); a non-zero count *with an empty answer* is the
signature worth alerting on.

Captured buffers (`text`, raw stdout, stderr) are bounded at `MAX_CAPTURE`, 1 MiB, keeping
the earliest output. An agent can stream for hours, and an unbounded capture turns a long
run into an OOM instead of an answer. Individual lines are bounded separately at `MAX_LINE`,
because a reader that accumulates until a newline can exhaust memory on one line that never
ends, long before any total cap applies.

Individual event payloads are bounded at `MAX_EVENT_BYTES` (64 KiB) and marked with
`TRUNCATION_MARK` when shortened. The channel bounds how many events queue, not how large
they are, so without this a stalled consumer could hold roughly 130 MiB; with it, about
16 MiB. Identifiers are exempt: a shortened session id cannot resume anything and a
shortened tool id cannot be matched to its call.

Under a structured format there is no silent fallback to raw stdout: a run that produced no
recognizable records, or never reached its terminal record, returns `Error::Parse` rather
than a plausible-looking answer assembled from whatever was printed.

## A clean exit is not a successful turn

All three agents report some failures with exit code 0, with the explanation where the answer
should be. An unknown model, a rejected schema and an upstream outage all arrive this way, so
a caller checking only `Result::is_ok` renders the error as the model's reply.

These are `Error::AgentError`, carrying the agent's own wording and the provider's status
where one was reported:

```rust
match run(&request).await {
    Err(Error::AgentError { status: Some(404), message, .. }) => {
        // Typically a model the account cannot reach. `message` is the agent's wording.
        eprintln!("{message}");
    }
    Err(e) if e.is_auth_failure() => { /* prompt for login */ }
    Err(e) if e.is_transient() => { /* back off */ }
    other => { /* ... */ }
}
```

`NotAuthenticated` and `RateLimited` are the two members of this family that predate it and
keep their own variants, because the remedy differs. Everything else in it lands here.

Codex needs one extra step: it forwards the provider's response body as a *string*, so its
`turn.failed` message is JSON containing the sentence and the status. This crate unwraps it,
so `message` is the sentence and `status` is the code. A message that is not that shape is
passed through untouched.

## Usage and quota

Two questions with two answers. `Outcome::usage` measures the run; `Agent::account_usage()`
measures the plan behind it. Everything is a value, never a formatted string or a rendered
bar, so a host presents it however it likes.

### Per run, and per session

```rust
let outcome = run(&request).await?;
let used = outcome.usage.context_used();   // Option<f64>, 0.0 to 1.0
```

**Do not sum usage across turns in a loop.** An agent re-sends the whole conversation each
turn and reports its size, so adding `context_tokens` counts the same conversation once per
turn and the error grows with the session. `Usage::accumulate` applies the right rule per
field:

```rust
let mut session = Usage::default();
session.accumulate(&outcome.usage);   // tokens and cost add; context takes the latest
```

The cache figures add, and that is worth saying out loud because they look like context and
are not. A turn's `cache_read_tokens` is what that turn's calls actually read, already summed
across them by the agent's terminal record, and every read is billed. Taking the latest
instead reports one turn's cache traffic as the whole session's, which is how a host ends up
with a token total that cannot explain its own cost figure.

The vendors also disagree on what "input" counts: Claude reports the uncached remainder,
Codex reports the whole prompt with the cached part inside it. `input_tokens` is normalized
to the first meaning on both, and `context_tokens` carries the whole prompt.

| | Claude | Codex | Copilot |
|---|---|---|---|
| tokens in / out | yes | yes | no |
| cache read / write | yes | yes | no |
| `context_tokens` | yes | yes | no |
| `context_window`, `max_output_tokens` | yes | no | no |
| `reasoning_tokens` | no | yes | no |
| `cost_usd` | yes | no | no |
| `ai_credits_nano`, `premium_requests` | no | no | yes |
| `duration_ms`, `api_duration_ms` | yes | no | yes |

`context_used()` needs both the tokens and the window, so today it answers only on Claude.

### A live counter while the agent works

`Event::Usage` reports token usage as the turn runs, for a status line like
`7m 8s · 8.5k tokens`:

```rust
let mut live = Usage::default();
while let Some(event) = run.recv().await {
    if let Event::Usage(usage) = event {
        live.accumulate(&usage);
        ui.set_counter(live.context_tokens, live.context_used());
    }
}
```

One event per model call, deduplicated: Claude sends several records per call, one per
content block, each repeating the same usage, and reporting all of them would count a call
three times.

**`output_tokens` is absent on these events, deliberately.** Mid-turn the agent reports the
count as it stood when the message began, which understates the finished figure badly: one
run whose per-call reports summed to 9 had generated 497 by the end. The context and cache
figures are exact, so a counter built on `context_tokens` is honest and one built on output
would not be. The real output count arrives with the `Outcome`.

Claude reports throughout a turn. Copilot reports once, near the end. Codex reports nothing
until the turn completes, so it never emits this.

### Account-wide

```rust
if agent.reports_account_usage() {
    let account = agent.account_usage().await?;
    for window in &account.windows {
        // window.used_percent, window.window_minutes, window.resets_at
    }
}
```

Only Codex can answer, through its `codex app-server` JSON-RPC interface: quota windows with
percentages and reset times, credit balance, per-day token buckets and lifetime totals.

Claude and Copilot return `Error::Unsupported`. Claude reports quota only *during* a run, as
`Event::RateLimit`, and **the wire carries no utilization figure at all**: verified against
2.1.212, the entire `rate_limit_info` vocabulary is `status`, `resetsAt`, `rateLimitType`,
`overageStatus`, `overageDisabledReason` and `isUsingOverage`. There is no percentage field
to be missing, so waiting for a different event will not produce one. The percentages on its
`/usage` screen are fetched separately by that screen. Copilot reports session spend and
nothing account-wide. Ask `reports_account_usage()` first rather than discovering this from
an error.

## No shell, ever

Arguments are built as a `Vec<String>` and passed straight to `exec`. There is no shell in
the path, so a prompt containing `;`, backticks or `$(...)` is data, not syntax, with no quoting
or escaping to get wrong.

## Operating within the agents' terms

This crate drives each vendor's own supported headless interface using the credentials that
CLI already holds. It does not reimplement a provider API, multiplex accounts, or retry
around a quota. A refusal surfaces as `Error::RateLimited` carrying the provider's own
wording, and backing off is the caller's decision. See
[`docs/operating-limits.md`](docs/operating-limits.md).

## Testing

```bash
cargo test
```

```bash
cargo test --test live -- --ignored --test-threads 1
```

The live suite drives the installed agents end to end (answer, usage, streaming, multi-turn
memory, forking) and skips any agent whose binary is absent rather than failing on it.
It spawns real agents and consumes real quota, which is why it is ignored by default.

## Relationship to oneharness

A Rust port of [nickderobertis/oneharness](https://github.com/nickderobertis/oneharness)
(MIT), reduced to three agents and rebuilt as an embeddable library. What changed:

- **The Python and TypeScript SDKs are gone**, along with the JSON-Schema codegen that fed
  them. They existed only so non-Rust callers could shell out to the `oneharness` binary and
  re-validate its JSON. In a Rust consumer that entire layer collapses into the public API:
  the type system *is* the contract.
- **The CLI is gone.** A GUI embedding this crate should not pay for a process boundary and
  two JSON round-trips to ask a question.
- **The shell scripts are gone**, 39 of them, mostly CI gates and per-harness e2e drivers.
- **Five harnesses are gone** (OpenCode, Goose, Qwen, Crush, Cursor).
- **Async throughout.** oneharness runs blocking; this streams over tokio, which is what a
  Tauri front end needs to render a run as it happens.

Some findings did not survive re-verification against the current CLIs. oneharness models
Copilot as having no headless session id and no event stream (`session_formats: &[]`,
`events_format: None`); Copilot 1.0.75 has both. Where this crate and oneharness disagree,
this crate matches what the CLI does today.

## License

MIT. See [LICENSE](LICENSE); the original oneharness copyright is retained alongside ours.
