# RustAgentAbstraction

`agent-abstraction` drives **Claude Code**, **Codex** and **GitHub Copilot** headlessly from
Rust: one request type, one event vocabulary, one session model across three CLIs that agree
on none of those things.

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

**Minted beats printed.** Where the caller can assign the session id up front (Claude,
Copilot), the binding is written *before* the process starts, so a run that crashes
mid-turn still leaves a resumable session. Codex only reveals its `thread_id` in its output,
so its binding can only be recorded after a run produces one.

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

Session names are reduced to a single safe path segment, so a name like `../../etc/passwd`
cannot escape the store.

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

## Gotchas worth knowing

- **`codex exec` refuses to run outside a git repository.** This crate always passes
  `--skip-git-repo-check`, so it runs anywhere. That check exists to stop an agent editing
  files with no way to undo them; the sandbox is the real containment here, and it defaults
  to `read-only`.
- **Copilot's tool filters need `=`.** They are declared `--deny-tool[=tools...]`, an
  optional value, which binds only as `--deny-tool=shell`. Across a space the value is read
  as a positional and the deny is silently lost. This crate always emits the combined form.
- **Copilot needs `--allow-all-tools` to run headlessly at all**, or it stalls at the first
  tool confirmation. It is always emitted; `Permission` then narrows via denies.
- **Claude's `stream-json` requires `--verbose`**, or it refuses to start. Handled.
- **Large prompts move to stdin automatically** above 128 KiB, so a long prompt never fails
  with `E2BIG`.

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
