# Handover: `agent-abstraction` → AgencyZero

**Crate:** `agent-abstraction = "0.4"` (0.4.0 on crates.io, MIT). Source: `~/code/RustAgentAbstraction`, repo `pathscale/RustAgentAbstraction`. Read `README.md` and `docs/host-integration.md` in that repo before writing UI code.

Drives Claude Code, Codex and GitHub Copilot CLIs headlessly behind one API. Library only — no CLI, no binary.

## Shape

```rust
use agent_abstraction::{Agent, Decision, Event, Permission, Request, Usage, run, stream};

let request = Request::new(Agent::Claude, prompt)
    .model("opus[1m]")
    .effort("high")
    .permission(Permission::Edit)
    .session(&store, &project, "chat")?   // multi-turn continuity
    .interactive()                         // enables Run::send
    .approvals();                          // enables Event::ApprovalRequest

let mut run = stream(&request)?;           // stream() for UI; run() discards events
while let Some(event) = run.recv().await { /* … */ }
let outcome = run.finish().await?;
```

Exports: `Agent, Request, Run, run, stream, Event, Outcome, Usage, Stop, RateLimit, Permission, Format, EnvPolicy, Error, Result, Approval, Decision, Model, Kind, Source, Verified, AccountUsage, UsageWindow, Credits, Lifetime, DailyUsage, SessionStore, SessionRecord, Phase, Probe, Version, VersionStatus, AuthStatus, AuthState, Caps`.

`Event` is `#[non_exhaustive]` — always include a `_ => {}` arm.

## Traps the compiler will not catch

1. **Render a follow-up on send, never on echo.** `run.send(text)` delivers mid-turn; append it to your transcript below the user's previous message *immediately*. Do not build echo ordering — `--replay-user-messages` exists and the crate deliberately does not use it.
2. **`send` lands at the next step boundary, not instantly.** Say "sent", never "stopped". A hard stop is `run.cancel()`, which kills the run.
3. **`send` after the turn settles returns `Error::Cancelled`.** Not a bug — handle it by starting a new run resuming the session, or the user's message is silently lost.
4. **Show `approval.input`, not just `approval.tool`.** For `Bash` the command lives in the input. "Allow Bash?" approves an unseen command.
5. **An unanswered `ApprovalRequest` stalls the run to its timeout.** If a dialog can be dismissed, ensure some path still answers or cancels.
6. **`approvals()` + `Permission::ReadOnly` is refused.** ReadOnly strips mutating tools, so nothing would ever be asked and silence would read as "the agent wanted nothing". Use `Permission::Edit`.
7. **Silence ≠ nothing ran.** Claude allows read-only commands unasked: `whoami` runs, `touch f` asks.
8. **Never sum usage across turns in a loop.** `context_tokens` is already cumulative; summing double-counts and the error grows. Use `session_usage.accumulate(&turn_usage)`. The cache figures are the exception that looks like the rule: they are billed traffic, not a size, and 0.4 sums them. A total that omits them cannot explain its own cost, which is how AgencyZero showed 54.6k tokens beside $9.409.
9. **`output_tokens` is `None` on `Event::Usage`.** Mid-turn counts understate badly (9 reported vs 497 actual). Build live counters on `context_tokens`; the real output arrives with the `Outcome`.
10. **`claude-opus-5` defaults to 200k context.** Every other 5-series model is 1M natively. Use `claude-opus-5[1m]` or `opus[1m]`.
11. **The model catalogue is advisory, not entitlement.** `Agent::models()` lists what the product offers; the account may permit far less (a Copilot Free plan permits only `auto`). Let the run report the truth.

## Capability matrix

| | Claude | Codex | Copilot |
|---|---|---|---|
| `interactive()` / `send` | yes | `Unsupported` | `Unsupported` |
| `approvals()` | yes | `Unsupported` | `Unsupported` |
| `Event::Usage` live counter | throughout | never | once, near end |
| `discover_models()` | `Unsupported` | yes | `Unsupported` |
| `account_usage()` | `Unsupported` | yes (percentages, windows, credits, daily) | `Unsupported` |
| `context_window` reported | yes | no | no |
| `cost_usd` | yes | no | no (AI credits) |

Check capability *before* building UI: `agent.reports_account_usage()`. The `Unsupported` errors are raised before spawning, so one startup check is enough.

## Deliberately not possible

- **Claude's usage percentages** (session / weekly / Fable). Not on the wire at any usage level — verified the entire `rate_limit_info` vocabulary is `status`, `resetsAt`, `rateLimitType` and overage fields, with no utilization field to be missing. Reachable only via the OAuth-token route, which `docs/operating-limits.md` forbids. Don't add it without amending that document first.
- What you *do* get from Claude per-run: window identity, reset time, allowed/rejected, overage flags, via `Event::RateLimit`.

## Error handling

```rust
match run(&request).await {
    Err(Error::AgentError { status: Some(404), message, .. }) => /* bad model */,
    Err(e) if e.is_auth_failure() => /* prompt login; carries the fix command */,
    Err(e) if e.is_transient() => /* back off */,
    Err(e) if e.is_cancelled() => /* user asked; not a failure */,
    _ => {}
}
```

Agents report failures with **exit code 0** and the explanation where the answer goes — `Error::AgentError` catches that family so `Ok` never contains an error message.

## Open items

- Parsing `~/.claude/projects/*/*.jsonl` for token/cost history — no credentials needed, undocumented format. Deferred, not started.
- Inline image/file attachment. Researched and proven working via stream-json content blocks (base64, no path); Codex takes `-i FILE`, Copilot `--attachment PATH`. Not built.
- `Permission::Ask` as a real posture instead of the orthogonal `approvals()` builder. Breaking change, needs 0.5.
- ~~Do not release 0.4 before 2026-08-06~~. Lifted on the owner's say-so and **0.4.0 shipped 2026-07-31**: `Usage::accumulate` sums the cache figures rather than taking the latest. AgencyZero is on it as of 0.1.18 and no longer carries its own accumulator.

## Conventions

Releases are automatic: merging a `Cargo.toml` version bump to `master` publishes and tags. A bump *is* a release. Never `runs-on: ubuntu-latest` — Ubicloud only. No AI attribution anywhere. Default branch `master`. No em dashes in prose.

## Verification status

Everything above was verified against claude 2.1.212, codex-cli 0.145.0 and Copilot CLI 1.0.75. The capability matrix is the part most likely to drift when those CLIs update. `Probe` compares an installed version against what the flags were verified against, and the live suite (`cargo test --test live -- --ignored --test-threads 1`, 26 tests) is what proves the CLIs still agree.
