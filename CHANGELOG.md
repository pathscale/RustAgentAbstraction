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
  `TRUNCATION_MARK` when shortened. The channel bounded how many events queued but not how
  large they were, leaving roughly 130 MiB reachable in flight; it is now about 16 MiB.
  Identifiers are exempt, because a truncated session id resumes nothing and a truncated
  tool id matches no call. Oversized tool arguments are replaced rather than cut, since
  truncated JSON no longer parses.

### Fixed

- **Dropping a `Run` now reliably kills the process group.** It signalled the driver and
  aborted it, which left the kill waiting on the runtime to poll the aborted task. On Linux
  that did not reliably happen and grandchildren survived, while `cancel` and timeouts were
  unaffected because both kill directly. `Run` now holds the pid and kills synchronously in
  `Drop`.
- Output lines are bounded at `MAX_LINE`. A line that never ended could exhaust memory
  before any total cap applied, because the reader accumulated until a newline.

### Added

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
