//! Spawning an agent and turning its output into events and an outcome.
//!
//! Two entry points over the same machinery:
//! - [`run`] waits and hands back the finished [`Outcome`].
//! - [`stream`] hands back a [`Run`] that yields [`Event`]s as they arrive, for
//!   a UI that shows work in progress.
//!
//! Both read stdout and stderr concurrently. Draining only one would deadlock
//! the moment the other filled its pipe buffer, which for a chatty agent is a
//! matter of seconds.

use std::process::Stdio;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;

use crate::agent::{Continue, EnvPolicy};
use crate::error::{Error, Result};
use crate::event::{Event, Parser, Terminal, append_capped};
use crate::outcome::{Outcome, Stop};
use crate::proc::kill_process_group;
use crate::request::Request;

/// How many events may queue before the producer waits for the consumer. Deep
/// enough that a burst of tool events does not stall the agent, shallow enough
/// that a consumer which stops reading does not grow without bound.
const EVENT_BUFFER: usize = 256;

/// A run in progress.
///
/// Yields events through [`Run::recv`] and settles into an [`Outcome`] through
/// [`Run::finish`].
///
/// **Dropping a `Run` kills the agent.** That is the safe default for the hosts
/// this crate targets: closing a window or cancelling a request should stop the
/// work, not leave an agent running invisibly, spending quota and touching
/// files with nobody watching. Call [`Run::detach`] when background execution is
/// genuinely what you want, or [`Run::cancel`] to stop one deterministically and
/// wait for it to die.
#[derive(Debug)]
pub struct Run {
    events: mpsc::Receiver<Event>,
    /// `None` only after [`Run::finish`], [`Run::cancel`] or [`Run::detach`]
    /// has taken ownership, which is what stops `Drop` from aborting a run that
    /// was already settled deliberately.
    task: Option<tokio::task::JoinHandle<Result<Outcome>>>,
    argv: Vec<String>,
}

impl Run {
    /// The next event, or `None` once the agent has finished producing them.
    pub async fn recv(&mut self) -> Option<Event> {
        self.events.recv().await
    }

    /// The exact command line that was spawned.
    ///
    /// **This contains the prompt and any session id.** Treat it as sensitive:
    /// logging it verbatim puts user content into your logs. Use
    /// [`Run::redacted_argv`] for diagnostics.
    #[must_use]
    pub fn argv(&self) -> &[String] {
        &self.argv
    }

    /// The command line with prompt and session id replaced by placeholders,
    /// safe to log.
    #[must_use]
    pub fn redacted_argv(&self) -> Vec<String> {
        redact(&self.argv)
    }

    /// Wait for the run to finish.
    ///
    /// Drains any events still queued, so a caller that only wants the result
    /// can call this without having consumed the stream.
    ///
    /// # Errors
    /// Whatever the run failed with. See [`Error`].
    pub async fn finish(mut self) -> Result<Outcome> {
        while self.events.recv().await.is_some() {}
        // Taking the handle disarms the `Drop` guard: this run is settling
        // normally, not being abandoned.
        let Some(task) = self.task.take() else {
            unreachable!("the handle is only taken by a consuming method")
        };
        match task.await {
            Ok(result) => result,
            // The driver task panicked or was cancelled. The process itself
            // started fine, so this is not a spawn failure and must not claim
            // to be one.
            Err(join) => Err(Error::Interrupted {
                bin: self.argv.first().cloned().unwrap_or_default(),
                detail: if join.is_panic() {
                    "the driver task panicked".into()
                } else {
                    "the driver task was cancelled".into()
                },
            }),
        }
    }

    /// Stop the run and wait until the agent is actually gone.
    ///
    /// Deterministic, unlike dropping: when this returns, the process and its
    /// children have been signalled and reaped. Prefer it over `drop` when you
    /// need to know the agent has stopped before doing something else, such as
    /// mutating the files it was working on.
    pub async fn cancel(mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
            // Aborting drops the driver's `Child`, whose `kill_on_drop` and
            // process-group teardown do the actual killing. Awaiting the
            // JoinError is what guarantees that has happened.
            let _ = task.await;
        }
    }

    /// Let the run continue after this handle goes away.
    ///
    /// The opposite of the default. Nothing can observe or stop the agent
    /// afterwards, so reach for this only when an unsupervised background run
    /// is genuinely intended.
    pub fn detach(mut self) {
        // Dropping the handle without aborting is what detaches a tokio task.
        drop(self.task.take());
    }
}

impl Drop for Run {
    fn drop(&mut self) {
        // Still holding the handle means the caller abandoned this run rather
        // than finishing, cancelling or detaching it. Abort, which drops the
        // driver's `Child` and triggers the kill path.
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

/// Placeholders substituted for sensitive argv values.
const REDACTED: &str = "<redacted>";

/// Replace prompt and session-id values with a placeholder.
///
/// Flag *names* are kept so a redacted command line is still recognisable; only
/// the values that carry user content or a resumable handle are removed.
fn redact(argv: &[String]) -> Vec<String> {
    /// Flags whose following argument is sensitive.
    const SENSITIVE_FLAGS: &[&str] = &[
        "-p",
        "--prompt",
        "--append-system-prompt",
        "--system",
        "--session-id",
        "--resume",
    ];
    let mut out = Vec::with_capacity(argv.len());
    let mut redact_next = false;
    for (i, arg) in argv.iter().enumerate() {
        if redact_next {
            out.push(REDACTED.to_string());
            redact_next = false;
            continue;
        }
        redact_next = SENSITIVE_FLAGS.contains(&arg.as_str());
        // Codex takes its prompt as the trailing positional rather than behind
        // a flag, so the last argument is redacted unless it is a flag itself.
        let trailing_prompt = i + 1 == argv.len() && !arg.starts_with('-') && i > 1;
        out.push(if trailing_prompt {
            REDACTED.to_string()
        } else {
            arg.clone()
        });
    }
    out
}

/// Run `request` to completion, discarding the intermediate events.
///
/// # Errors
/// See [`Error`]; notably [`Error::NotInstalled`], [`Error::Timeout`],
/// [`Error::RateLimited`] and [`Error::Failed`].
pub async fn run(request: &Request) -> Result<Outcome> {
    stream(request)?.finish().await
}

/// Start `request`, returning a handle that streams its events.
///
/// Returns as soon as the child is spawned; the work proceeds on a task.
///
/// # Errors
/// [`Error::NotInstalled`] if the binary is missing, [`Error::Unsupported`] if
/// the agent cannot honour the request, or [`Error::Spawn`] on an OS failure.
pub fn stream(request: &Request) -> Result<Run> {
    // `tokio::spawn` panics outside a runtime. A fallible signature must not
    // hide that, so the context is checked and reported as an ordinary error.
    let runtime = tokio::runtime::Handle::try_current().map_err(|_| Error::NoRuntime)?;

    let plan = request.plan();
    let argv = request.argv()?;

    let mut command = Command::new(&argv[0]);
    command
        .args(&argv[1..])
        .stdin(if plan.stdin_prompt {
            Stdio::piped()
        } else {
            // Close stdin so an agent that would otherwise wait on it exits
            // instead of hanging forever with nothing to read.
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Without this a killed run can leave the child alive holding the pipes.
        .kill_on_drop(true);
    if let Some(cwd) = &request.cwd {
        command.current_dir(cwd);
    }
    // Narrow the environment first, then apply explicit variables, so an
    // explicit `env()` always wins over the policy.
    match &request.env_policy {
        EnvPolicy::Inherit => {}
        EnvPolicy::Minimal => {
            command.env_clear();
            inherit_named(&mut command, &request.agent.essential_env());
        }
        EnvPolicy::Only(names) => {
            command.env_clear();
            inherit_named(&mut command, names);
        }
    }
    for (key, value) in &request.env {
        command.env(key, value);
    }

    // Put the agent in its own process group so the whole tree can be signalled
    // together. Killing only the CLI leaves the commands *it* spawned running:
    // a build, a test run, a server, still holding files and credentials after
    // the run is supposedly over.
    // 0 means "make this child its own group leader". `tokio::process::Command`
    // exposes this directly on unix.
    #[cfg(unix)]
    command.process_group(0);

    let child = command.spawn().map_err(|source| {
        // A missing binary is the common case and deserves an actionable error
        // with an install hint. Reading it off the spawn avoids resolving PATH
        // twice, and with it the window where the resolved path is replaced
        // between the check and the exec.
        if source.kind() == std::io::ErrorKind::NotFound {
            Error::NotInstalled {
                agent: request.agent,
                bin: plan.bin.clone(),
                hint: request.agent.install_hint(),
            }
        } else {
            Error::Spawn {
                bin: plan.bin.clone(),
                source,
            }
        }
    })?;

    let (tx, rx) = mpsc::channel(EVENT_BUFFER);
    let request = request.clone();
    let task = runtime.spawn(drive(child, request, tx));
    Ok(Run {
        events: rx,
        task: Some(task),
        argv,
    })
}

/// Copy the named variables from this process into `command`, skipping any that
/// are unset so nothing is invented.
fn inherit_named<S: AsRef<str>>(command: &mut Command, names: &[S]) {
    for name in names {
        if let Some(value) = std::env::var_os(name.as_ref()) {
            command.env(name.as_ref(), value);
        }
    }
}

/// Owns the child and tears down its whole process group when dropped.
///
/// `kill_on_drop` alone is not enough: it kills the CLI, leaving the commands
/// *it* spawned running. Since aborting the driver task drops this guard, the
/// same teardown covers cancellation, a dropped [`Run`] and a timeout, without
/// each path having to remember to do it.
struct ChildGuard {
    child: Child,
    /// Cleared once the child has been reaped, so a pid the OS may since have
    /// recycled is never signalled.
    armed: bool,
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if self.armed {
            kill_process_group(&self.child);
        }
    }
}

/// Feed the child, read both its pipes, and assemble the outcome.
#[allow(
    clippy::too_many_lines,
    reason = "one linear lifecycle: feed, read, wait, classify. Splitting it \
              would thread the child, parser, buffers and cancellation state \
              through helpers and obscure the ordering that matters, such as \
              killing the group before reaping."
)]
async fn drive(child: Child, request: Request, events: mpsc::Sender<Event>) -> Result<Outcome> {
    // From here on the child is owned by a guard, so every exit path from this
    // task, including an abort, takes the process group with it.
    let mut child = ChildGuard { child, armed: true };
    let plan = request.plan();
    let bin = plan.bin.clone();

    // An assigned id is known before anything runs, so record it now. This is
    // what makes the session survive a run that times out, crashes, or is
    // cancelled: the binding does not depend on reaching the end.
    if let Some(token) = preassigned_token(&request) {
        persist_session(&request, &token)?;
    }

    // Deliver a piped prompt and close the pipe, or the agent waits on EOF.
    if plan.stdin_prompt {
        if let Some(mut stdin) = child.child.stdin.take() {
            let prompt = request.agent.effective_prompt(&plan);
            stdin
                .write_all(prompt.as_bytes())
                .await
                .map_err(|source| Error::Spawn {
                    bin: bin.clone(),
                    source,
                })?;
            drop(stdin);
        }
    }

    // Drain stderr on its own task: a full stderr pipe blocks the child even
    // while stdout still has room.
    let stderr = child.child.stderr.take();
    let stderr_task = tokio::spawn(async move {
        let mut buf = String::new();
        if let Some(handle) = stderr {
            let mut lines = BufReader::new(handle).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                // Keep draining after the cap is hit: an undrained pipe would
                // block the child even though we no longer want the bytes.
                append_capped(&mut buf, &line);
            }
        }
        buf
    });

    let stdout = child.child.stdout.take();
    let mut parser = Parser::new(request.agent, plan.format);
    // Raw stdout is retained only as a fallback answer for a run that exited
    // cleanly without producing a structured one, and as evidence when
    // classifying a failure. It is capped for the same reason as everything
    // else here: an agent can stream for hours.
    let mut raw = String::new();
    // Tracks the first `Started`, so the binding is written once, and carries a
    // store failure back out instead of discarding it.
    let mut bound = false;
    let mut persist_result: Result<()> = Ok(());

    let read_stdout = async {
        if let Some(handle) = stdout {
            let mut lines = BufReader::new(handle).lines();
            while let Some(line) = lines.next_line().await? {
                append_capped(&mut raw, &line);
                for event in parser.push(&line) {
                    // Bind a printed id the moment it appears rather than at the
                    // end. Codex announces its thread before answering, so a
                    // turn killed mid-answer stays resumable.
                    if let Event::Started { session, .. } = &event
                        && !bound
                    {
                        bound = true;
                        persist_result = persist_session(&request, session);
                    }
                    // A receiver that went away is not a failure: the run should
                    // still finish and produce its outcome.
                    if events.send(event).await.is_err() {
                        break;
                    }
                }
            }
        }
        Ok::<_, std::io::Error>(())
    };

    // Apply the deadline to reading and waiting together, so a child that
    // produces output forever is still bounded.
    let status = match request.timeout {
        Some(limit) => {
            match tokio::time::timeout(limit, async {
                read_stdout.await?;
                child.child.wait().await
            })
            .await
            {
                Ok(result) => result,
                Err(_elapsed) => {
                    // Order matters: signal the group *before* reaping. Reaping
                    // clears the child's pid, and the group kill needs that pid
                    // to target the group, so doing it the other way round
                    // silently leaves every grandchild running.
                    kill_process_group(&child.child);
                    let _ = child.child.kill().await;
                    child.armed = false;
                    return Err(Error::Timeout {
                        bin,
                        timeout: limit,
                        partial: parser.finish().text,
                    });
                }
            }
        }
        None => match read_stdout.await {
            Ok(()) => child.child.wait().await,
            Err(source) => Err(source),
        },
    }
    .map_err(|source| Error::Spawn {
        bin: bin.clone(),
        source,
    })?;

    // The child has been reaped, so its pid must not be signalled again.
    child.armed = false;

    drop(events);
    let stderr = stderr_task.await.unwrap_or_default();
    let terminal = parser.finish();
    let exit_code = status.code().unwrap_or(-1);

    // A run that produced no structured answer still has its raw stdout; better
    // to hand back what the agent printed than an empty string.
    let mut terminal = terminal;
    if terminal.text.is_empty() {
        terminal.text = raw.trim().to_string();
    }

    // A provider refusal is not always an exit code. Claude can report a
    // blocking `rate_limit_event` and still exit 0, and the crate promises that
    // quota refusals surface as `Error::RateLimited`, so the terminal state is
    // checked regardless of how the process exited.
    let quota_blocked = terminal
        .rate_limit
        .as_ref()
        .is_some_and(crate::outcome::RateLimit::is_blocking);
    if exit_code != 0 || quota_blocked {
        return Err(classify(&bin, exit_code, &stderr, &raw, &terminal));
    }

    // A fork lands on a *new* id the agent only reveals at the end, so the name
    // has to be repointed once the run settles. Everything else was bound above.
    persist_result?;
    if let Some(token) = &terminal.session
        && !bound
    {
        persist_session(&request, token)?;
    }
    Ok(Outcome {
        agent: request.agent,
        session: terminal.session,
        text: terminal.text,
        usage: terminal.usage,
        stop: terminal.stop,
        rate_limit: terminal.rate_limit,
        exit_code,
        stderr,
        unparsed: terminal.unparsed,
        first_unparsed: terminal.first_unparsed,
    })
}

/// Turn a non-zero exit into the most specific error available.
fn classify(bin: &str, code: i32, stderr: &str, stdout: &str, terminal: &Terminal) -> Error {
    let quota_signalled = terminal
        .rate_limit
        .as_ref()
        .is_some_and(crate::outcome::RateLimit::is_blocking);
    if quota_signalled || looks_rate_limited(stderr) || looks_rate_limited(stdout) {
        return Error::RateLimited {
            bin: bin.to_string(),
            message: first_meaningful_line(stderr)
                .or_else(|| first_meaningful_line(stdout))
                .unwrap_or_else(|| "usage limit reached".to_string()),
        };
    }
    Error::Failed {
        bin: bin.to_string(),
        code,
        stderr: first_meaningful_line(stderr).unwrap_or_default(),
    }
}

/// Whether text carries a provider quota refusal.
///
/// Deliberately a small set of unambiguous phrases: a false positive here would
/// relabel an ordinary failure as a quota problem and send a caller into a
/// pointless backoff.
fn looks_rate_limited(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    [
        "rate limit",
        "rate_limit",
        "usage limit",
        "quota exceeded",
        "too many requests",
        "429",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

/// The first non-blank line, trimmed. Enough to identify a failure without
/// pasting an entire stack trace into an error message.
fn first_meaningful_line(text: &str) -> Option<String> {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_string)
}

/// Write the session binding back, reporting any store failure.
///
/// Called as soon as an id is known rather than only on a clean exit. Waiting
/// for success would lose the binding for exactly the runs where continuity
/// matters most: a timeout, a crash, or a cancelled turn.
fn persist_session(request: &Request, token: &str) -> Result<()> {
    let Some(binding) = &request.binding else {
        return Ok(());
    };
    binding
        .store
        .bind(request.agent, &binding.project, &binding.name, token)
        .map(|_| ())
}

/// The id this run is already known by before it starts, if any.
///
/// Only a caller-assigned id qualifies: a printed id does not exist yet. This
/// is what makes an assigned session survive a run that never finishes.
fn preassigned_token(request: &Request) -> Option<String> {
    match &request.plan().cont {
        Continue::NewWith(id) => Some(id.clone()),
        _ => None,
    }
}

/// Reported by an agent that exited cleanly but said nothing useful.
impl Outcome {
    /// Whether the agent produced any answer at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.text.trim().is_empty() && self.stop == Stop::Completed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::Agent;

    #[test]
    fn quota_phrases_are_recognized_and_ordinary_errors_are_not() {
        assert!(looks_rate_limited("Error: rate limit exceeded"));
        assert!(looks_rate_limited("HTTP 429 Too Many Requests"));
        assert!(looks_rate_limited("You have hit your usage limit"));
        // A plain failure must not be mistaken for a quota problem.
        assert!(!looks_rate_limited("error: no such file or directory"));
        assert!(!looks_rate_limited("model not found"));
    }

    #[test]
    fn a_blocking_rate_limit_event_classifies_as_rate_limited() {
        let terminal = Terminal {
            rate_limit: Some(crate::outcome::RateLimit {
                status: "rejected".into(),
                window: Some("five_hour".into()),
                resets_at: None,
            }),
            ..Terminal::default()
        };
        assert!(matches!(
            classify("claude", 1, "", "", &terminal),
            Error::RateLimited { .. }
        ));
    }

    #[test]
    fn an_allowed_rate_limit_event_is_not_a_failure_cause() {
        let terminal = Terminal {
            rate_limit: Some(crate::outcome::RateLimit {
                status: "allowed".into(),
                window: None,
                resets_at: None,
            }),
            ..Terminal::default()
        };
        assert!(matches!(
            classify("claude", 1, "boom", "", &terminal),
            Error::Failed { .. }
        ));
    }

    #[test]
    fn failures_report_the_first_useful_line() {
        let err = classify(
            "claude",
            2,
            "\n\n  real problem  \nstack",
            "",
            &Terminal::default(),
        );
        let Error::Failed { code, stderr, .. } = err else {
            panic!("expected a plain failure")
        };
        assert_eq!(code, 2);
        assert_eq!(stderr, "real problem");
    }

    /// Prompts and session ids ride the argv, and `Run::argv` invites logging
    /// it. The redacted form must keep the shape while dropping the content.
    #[test]
    fn redaction_removes_prompts_and_session_ids_but_keeps_flags() {
        let argv = crate::Request::new(Agent::Claude, "my secret prompt")
            .system("secret system")
            .session_id("11111111-2222-3333-4444-555555555555")
            .argv()
            .unwrap();
        let safe = redact(&argv);

        for secret in [
            "my secret prompt",
            "secret system",
            "11111111-2222-3333-4444-555555555555",
        ] {
            assert!(
                !safe.iter().any(|a| a.contains(secret)),
                "{secret:?} survived redaction: {safe:?}"
            );
        }
        // Still recognisable as the same command.
        assert_eq!(safe[0], "claude");
        assert!(safe.contains(&"--permission-mode".to_string()));
        assert!(safe.contains(&"--session-id".to_string()));
    }

    #[test]
    fn codex_trailing_prompt_is_redacted_even_without_a_flag() {
        let argv = crate::Request::new(Agent::Codex, "my secret prompt")
            .argv()
            .unwrap();
        let safe = redact(&argv);
        assert_eq!(safe.last().unwrap(), REDACTED);
        assert_eq!(safe[1], "exec", "the subcommand must survive");
    }

    /// `stream` is synchronous but spawns a task. Outside a runtime that would
    /// panic, which a `Result`-returning function must not do.
    #[test]
    fn stream_outside_a_runtime_errors_instead_of_panicking() {
        let err = stream(&crate::Request::new(Agent::Claude, "hi")).unwrap_err();
        assert!(matches!(err, Error::NoRuntime), "got {err:?}");
    }

    #[tokio::test]
    async fn a_missing_binary_names_the_install_command() {
        let request = Request::new(Agent::Claude, "hi").bin("definitely-not-a-real-binary-xyz");
        let err = run(&request).await.unwrap_err();
        let Error::NotInstalled { hint, agent, .. } = err else {
            panic!("expected NotInstalled, got {err:?}")
        };
        assert_eq!(agent, Agent::Claude);
        assert!(hint.contains("claude-code"));
    }

    #[test]
    fn transient_errors_are_distinguished_from_permanent_ones() {
        assert!(
            Error::RateLimited {
                bin: "claude".into(),
                message: String::new()
            }
            .is_transient()
        );
        assert!(
            !Error::NotInstalled {
                agent: Agent::Claude,
                bin: "claude".into(),
                hint: ""
            }
            .is_transient()
        );
    }
}
