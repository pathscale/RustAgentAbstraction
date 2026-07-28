# agent-abstraction

[![crates.io](https://img.shields.io/crates/v/agent-abstraction.svg)](https://crates.io/crates/agent-abstraction)
[![docs.rs](https://img.shields.io/docsrs/agent-abstraction)](https://docs.rs/agent-abstraction)
[![license](https://img.shields.io/crates/l/agent-abstraction.svg)](LICENSE)

Drive **Claude Code**, **Codex** and **GitHub Copilot** headlessly from Rust: one request
type, one event vocabulary, one session model across three CLIs that agree on none of those
things.

```toml
[dependencies]
agent-abstraction = "0.1"
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

`drop` cannot await, so it signals and aborts as a backstop. That is prompt but **not
synchronous**: teardown runs when the runtime next polls the aborted task. Both kill the
tree; only `cancel` tells you when.

Each run gets its own process group on Unix, and cancellation, drop and timeout all tear
down the whole group. Killing only the CLI would orphan the commands *it* started, which
keep holding files and credentials afterwards. Windows has no equivalent here yet: only the
direct child is killed, since containing a tree there needs a Job Object.

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

Under a structured format there is no silent fallback to raw stdout: a run that produced no
recognizable records, or never reached its terminal record, returns `Error::Parse` rather
than a plausible-looking answer assembled from whatever was printed.

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
