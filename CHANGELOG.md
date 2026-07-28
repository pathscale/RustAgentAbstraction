# Changelog

Notable changes per release. Versions follow [semantic versioning](https://semver.org),
with the 0.x caveat that a minor bump is the only signal Cargo treats as incompatible, so
behaviour changes go there rather than into a patch.

## 0.2.0

Nothing was removed or renamed, so this compiles against any 0.1 code. It is a minor bump
rather than a patch because two changes alter behaviour, and `"0.1"` would have delivered
them through an ordinary `cargo update`.

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
- **Dropping a `Run` now reliably kills the process group.** It signalled the driver and
  aborted it, which left the kill waiting on the runtime to poll the aborted task. On Linux
  that did not reliably happen and grandchildren survived, while `cancel` and timeouts were
  unaffected because both kill directly. `Run` now holds the pid and kills synchronously in
  `Drop`.
- Output lines are bounded at `MAX_LINE`. A line that never ended could exhaust memory
  before any total cap applied, because the reader accumulated until a newline.

### Added

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
