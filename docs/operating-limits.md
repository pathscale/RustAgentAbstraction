# Operating within the agents' terms

This crate exists to let a program drive Claude Code, Codex and GitHub Copilot
programmatically. That is a supported thing to do: all three ship a documented
non-interactive mode (`claude -p`, `codex exec`, `copilot -p`) intended for scripting and
automation. This document records the design choices that keep a wrapper on the right side
of that line, so a future change does not quietly cross it.

## What this crate does

- **Drives each vendor's own CLI**, as a child process, using whatever credentials that CLI
  already holds (its stored login, `ANTHROPIC_API_KEY`, `CODEX_HOME`, and so on). It never
  handles, stores, or forwards credentials itself.
- **Reports quota refusals** as [`Error::RateLimited`], carrying the provider's own wording
  unedited, and surfaces Claude's `rate_limit_event` as an ordinary `Event::RateLimit`
  during a run.
- **Passes model names through verbatim.** An unknown model is the provider's error to
  raise, not this crate's to guess around. It is reported as [`Error::AgentError`] with the
  provider's status, since the agents answer an unknown model with a clean exit and an
  explanation where the answer belongs.

## What this crate deliberately does not do

- **No automatic retry around a quota.** `Error::RateLimited` is returned to the caller,
  never absorbed. Burying a retry loop here would turn a limit the provider deliberately set
  into something the library quietly works around, and it would do so invisibly, in a
  dependency, where nobody reviewing the calling code would see it. `Error::is_transient()`
  classifies the failure so a caller can decide; deciding is the caller's job.
- **No account multiplexing.** There is no facility for rotating between credentials,
  config directories, or logins to widen an effective rate limit. The `env` and `bin`
  builders exist so a caller can point at a specific installation, not so a scheduler can
  cycle identities.
- **No reimplementation of a provider API.** This crate spawns the vendor's CLI. It does not
  reconstruct the underlying HTTP API, forge client headers, or impersonate an interactive
  session.
- **No permission bypass by default.** [`Permission::ReadOnly`] is the default and
  `Bypass` must be asked for by name.

## For the caller

If you are building on this crate and you hit `Error::RateLimited`:

- Back off. The [`RateLimit`] attached to an `Outcome` carries `resets_at` as Unix epoch
  seconds when the provider supplied it, which is a real answer to "how long".
- Back off *per identity*, not per process. Spawning more workers against the same account
  does not create more quota.
- Surface it to the human. A GUI that silently stalls for five hours is worse than one that
  says the limit was reached and when it lifts.

Rate limits are a pricing and capacity signal from the provider, not an obstacle for the
integration layer to route around.
