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

use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;

use crate::agent::{Continue, EnvPolicy};
use crate::error::{Error, Result};
use crate::event::{Event, MAX_LINE, Parser, Terminal, append_capped};
use crate::outcome::{Outcome, Stop};
use crate::proc::kill_process_group;
use crate::request::Request;

/// Read one line, giving up on a line that never ends.
///
/// `AsyncBufReadExt::lines` buffers until a newline arrives, so a stream that
/// emits megabytes without one exhausts memory before any total cap applies.
/// This reads a bounded amount and, past the limit, returns what it has and
/// discards the remainder of that line. Returns `None` at end of input.
async fn read_bounded_line<R>(reader: &mut R, buf: &mut String) -> std::io::Result<Option<bool>>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    buf.clear();
    let mut bytes = Vec::new();
    let mut truncated = false;
    loop {
        let mut byte = [0u8; 1];
        match reader.read(&mut byte).await? {
            // End of input: a trailing fragment still counts as a line.
            0 => {
                if bytes.is_empty() {
                    return Ok(None);
                }
                break;
            }
            _ if byte[0] == b'\n' => break,
            _ => {
                if bytes.len() < MAX_LINE {
                    bytes.push(byte[0]);
                } else {
                    // Keep draining to the newline so the pipe does not block,
                    // but stop accumulating.
                    truncated = true;
                }
            }
        }
    }
    // Output is not guaranteed to be valid UTF-8, and one bad byte should not
    // end a run.
    buf.push_str(&String::from_utf8_lossy(&bytes));
    Ok(Some(truncated))
}

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
/// genuinely what you want.
///
/// Dropping is prompt but not synchronous: `Drop` cannot await, so it asks the
/// driver to stop and aborts it, and the teardown runs when the runtime next
/// polls that task. If you need to *know* the tree is gone before doing
/// something else, such as touching the files it was working on, use
/// [`Run::cancel`], which waits for exactly that.
#[derive(Debug)]
pub struct Run {
    events: mpsc::Receiver<Event>,
    /// The typed command line, kept so both the plain and redacted views come
    /// from the same source.
    typed: Vec<crate::agent::Arg>,
    /// Dropping or firing this asks the driver to tear down in order. Held as
    /// an `Option` so `detach` can discard it without signalling.
    cancel: Option<tokio::sync::oneshot::Sender<()>>,
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

    /// The command line with every non-public value replaced by a placeholder.
    ///
    /// Prompts, system prompts, session ids and anything from
    /// [`crate::Request::unchecked_args`] are removed; flag names are kept so
    /// the command stays recognisable. Sensitivity is recorded where each
    /// argument is built rather than inferred from the finished line, so a
    /// bare positional prompt or an opaque raw argument is covered too.
    #[must_use]
    pub fn redacted_argv(&self) -> Vec<String> {
        redact(&self.typed)
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
    /// Cooperative rather than an abort: the driver is asked to stop, signals
    /// the process group, reaps the child and joins its readers, and only then
    /// does this return. So when it returns the tree really has exited, which
    /// matters if the next thing you do touches the files it was working on.
    ///
    /// Returns the partial [`Outcome`] if the run happened to finish first,
    /// otherwise [`Error::Cancelled`].
    ///
    /// # Errors
    /// [`Error::Cancelled`] in the normal case, or whatever the run failed with
    /// if it failed before the request arrived.
    pub async fn cancel(mut self) -> Result<Outcome> {
        // Dropping the sender is itself the signal, so this cannot fail in a
        // way that leaves the driver waiting.
        drop(self.cancel.take());
        let Some(task) = self.task.take() else {
            unreachable!("the handle is only taken by a consuming method")
        };
        match task.await {
            Ok(result) => result,
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

    /// Let the run continue after this handle goes away.
    ///
    /// The opposite of the default. Nothing can observe or stop the agent
    /// afterwards, so reach for this only when an unsupervised background run
    /// is genuinely intended.
    pub fn detach(mut self) {
        // Leak the cancel signal rather than dropping it: a dropped sender is
        // read by the driver as "stop", which is the opposite of detaching.
        if let Some(cancel) = self.cancel.take() {
            std::mem::forget(cancel);
        }
        // Dropping the handle without aborting is what detaches a tokio task.
        drop(self.task.take());
    }
}

impl Drop for Run {
    fn drop(&mut self) {
        // Abandoned rather than finished, cancelled or detached. Signal the
        // driver so it tears down in order if it gets the chance, then abort so
        // the teardown happens even if nothing polls it again. `Drop` cannot
        // await, so abort remains the backstop: it drops the driver's
        // `ChildGuard`, which kills the process group synchronously.
        drop(self.cancel.take());
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

/// Placeholder substituted for a sensitive argv value.
const REDACTED: &str = "<redacted>";

/// Render a typed command line for logging, keeping flag names and replacing
/// every value that is not `Public`.
///
/// Derived from the sensitivity recorded where each argument was built, so it
/// cannot miss a case the way matching on flag names and positions can.
fn redact(argv: &[crate::agent::Arg]) -> Vec<String> {
    use crate::agent::Sensitivity;

    argv.iter()
        .map(|arg| match arg.sensitivity {
            Sensitivity::Public => arg.value.clone(),
            _ => REDACTED.to_string(),
        })
        .collect()
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
    let typed = request.typed_argv()?;
    let argv: Vec<String> = typed.iter().map(|a| a.value.clone()).collect();

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

    // Reserve an assigned session id before the child exists. Doing it inside
    // the driver leaves a window where a spawn that half-succeeds loses the
    // binding, and this is the id the caller may already be showing in a UI.
    if let Some(token) = preassigned_token(request) {
        persist_session(request, &token)?;
    }

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
    let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel();
    let request = request.clone();
    let task = runtime.spawn(drive(child, request, tx, cancel_rx));
    Ok(Run {
        events: rx,
        typed,
        cancel: Some(cancel_tx),
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
async fn drive(
    child: Child,
    request: Request,
    events: mpsc::Sender<Event>,
    cancel: tokio::sync::oneshot::Receiver<()>,
) -> Result<Outcome> {
    // From here on the child is owned by a guard, so every exit path from this
    // task, including an abort, takes the process group with it.
    let mut child = ChildGuard { child, armed: true };
    let plan = request.plan();
    let bin = plan.bin.clone();

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
            let mut reader = BufReader::new(handle);
            let mut line = String::new();
            // Keep draining after the cap is hit: an undrained pipe blocks the
            // child even though we no longer want the bytes.
            while let Ok(Some(_)) = read_bounded_line(&mut reader, &mut line).await {
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
            let mut reader = BufReader::new(handle);
            let mut line = String::new();
            while read_bounded_line(&mut reader, &mut line).await?.is_some() {
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

    // Race three outcomes: the run finishing, the deadline, and a cancellation
    // request. Reading and waiting are one future so a child that produces
    // output forever is still bounded by the timeout.
    let work = async {
        read_stdout.await?;
        child.child.wait().await
    };
    // A timeout is optional; `pending()` makes the un-timed case the same shape
    // rather than duplicating the whole select.
    let deadline = async {
        match request.timeout {
            Some(limit) => tokio::time::sleep(limit).await,
            None => std::future::pending().await,
        }
    };

    let status = tokio::select! {
        // Biased so a finished run is reported as finished even if a deadline
        // or cancellation lands in the same tick.
        biased;
        result = work => result,
        () = deadline => {
            // Order matters: signal the group *before* reaping. Reaping clears
            // the child's pid, and the group kill needs that pid to target the
            // group, so the other order silently leaves grandchildren running.
            let partial = shut_down(&mut child, stderr_task).await;
            return Err(Error::Timeout {
                bin,
                timeout: request.timeout.unwrap_or_default(),
                partial: parser.finish().text,
            })
            .inspect_err(|_| drop(partial));
        }
        _ = cancel => {
            // Cooperative teardown: the caller is waiting on this, so the tree
            // is signalled, reaped and joined before returning.
            shut_down(&mut child, stderr_task).await;
            return Err(Error::Cancelled { bin });
        }
    }
    .map_err(|source| Error::Spawn {
        bin: bin.clone(),
        source,
    })?;

    // The child has been reaped, so its pid must not be signalled again.
    child.armed = false;

    drop(events);
    let stderr = stderr_task.await.unwrap_or_default();
    let saw_structured = parser.saw_structured_record();
    let saw_terminal = parser.saw_terminal_record();
    let terminal = parser.finish();
    let exit_code = status.code().unwrap_or(-1);

    // Under a structured format, silently handing back raw stdout would turn a
    // protocol failure into a plausible-looking answer. A run that recognized
    // nothing, or never reached its terminal record, did not produce a result
    // this crate can vouch for, so it is reported rather than papered over.
    let structured = plan.format != crate::Format::Text;
    if structured && exit_code == 0 {
        if !saw_structured {
            return Err(Error::Parse {
                agent: request.agent,
                detail: format!(
                    "no recognizable {} records in {} lines of output;                      the CLI's output shape has probably changed",
                    request.agent,
                    raw.lines().count()
                ),
            });
        }
        if !saw_terminal {
            return Err(Error::Parse {
                agent: request.agent,
                detail: "the stream ended without its terminal record, so the turn                          did not complete"
                    .into(),
            });
        }
    }

    // Plain text has no structure to validate: the stream is the answer.
    let mut terminal = terminal;
    if terminal.text.is_empty() && !structured {
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

/// Kill the process group, reap the child, and join the stderr reader.
///
/// The orderly teardown both cancellation and timeout share. Returns whatever
/// stderr had been captured, so a caller can still report why a run was stopped.
async fn shut_down(child: &mut ChildGuard, stderr_task: tokio::task::JoinHandle<String>) -> String {
    kill_process_group(&child.child);
    // Reap, so the caller is not left with a zombie once this returns.
    let _ = child.child.kill().await;
    child.armed = false;
    // The pipes are closed now that the child is gone, so this finishes
    // promptly rather than hanging the cancellation.
    stderr_task.await.unwrap_or_default()
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
    // A rejected flag is not a failed request, it is this crate and the CLI
    // disagreeing about what the CLI accepts. Naming that is the difference
    // between "the run failed" and "your codex is a different version".
    if let Some(detail) = rejected_flag(stderr).or_else(|| rejected_flag(stdout)) {
        return Error::FlagRejected {
            bin: bin.to_string(),
            detail,
        };
    }
    Error::Failed {
        bin: bin.to_string(),
        code,
        stderr: first_meaningful_line(stderr).unwrap_or_default(),
    }
}

/// The CLI's complaint, if it refused an argument.
///
/// The phrasings are clap's and commander's, which is what all three CLIs are
/// built on. Matched narrowly: a false positive would relabel a genuine failure
/// as a version problem and send someone chasing the wrong thing.
fn rejected_flag(text: &str) -> Option<String> {
    const REJECTIONS: &[&str] = &[
        "unexpected argument",
        "unknown option",
        "unrecognized option",
        "unknown flag",
        "invalid option",
        "unexpected option",
    ];
    let lower = text.to_ascii_lowercase();
    REJECTIONS
        .iter()
        .any(|needle| lower.contains(needle))
        .then(|| first_meaningful_line(text).unwrap_or_default())
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

    /// The exact failure that cost a round of debugging: `codex exec resume`
    /// rejects `--sandbox`, which `Error::Failed` reported as a generic
    /// non-zero exit naming a flag rather than a version mismatch.
    #[test]
    fn a_rejected_flag_is_named_as_a_version_mismatch() {
        let err = classify(
            "codex",
            2,
            "error: unexpected argument '--sandbox' found",
            "",
            &Terminal::default(),
        );
        let Error::FlagRejected { bin, detail } = err else {
            panic!("expected FlagRejected, got {err:?}")
        };
        assert_eq!(bin, "codex");
        assert!(detail.contains("--sandbox"), "{detail}");
    }

    #[test]
    fn ordinary_failures_are_not_mistaken_for_version_drift() {
        for stderr in [
            "error: no such file or directory",
            "model not found",
            "permission denied",
        ] {
            assert!(
                matches!(
                    classify("codex", 1, stderr, "", &Terminal::default()),
                    Error::Failed { .. }
                ),
                "{stderr:?} should stay a plain failure"
            );
        }
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
        let request = crate::Request::new(Agent::Claude, "my secret prompt")
            .system("secret system")
            .session_id("11111111-2222-3333-4444-555555555555");
        let safe = redact(&request.typed_argv().unwrap());

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
        let request = crate::Request::new(Agent::Codex, "my secret prompt");
        let safe = redact(&request.typed_argv().unwrap());
        assert_eq!(safe.last().unwrap(), REDACTED);
        assert_eq!(safe[1], "exec", "the subcommand must survive");
    }

    /// Redaction must cover the two shapes positional guesswork misses: Codex's
    /// bare trailing prompt, and raw arguments whose contents are unknowable.
    #[test]
    fn redaction_covers_positional_prompts_and_unchecked_arguments() {
        let request = crate::Request::new(Agent::Codex, "my secret prompt")
            .unchecked_args(["-c", "api_key=hunter2"]);
        let safe = redact(&request.typed_argv().unwrap());
        assert!(!safe.iter().any(|a| a.contains("my secret prompt")));
        assert!(
            !safe.iter().any(|a| a.contains("hunter2")),
            "unchecked arguments may hold secrets: {safe:?}"
        );
        assert_eq!(safe[1], "exec", "the subcommand must survive");
    }

    /// A resume id is a capability: it continues someone's conversation.
    #[test]
    fn redaction_covers_the_codex_positional_resume_id() {
        let request = crate::Request::new(Agent::Codex, "hi").resume("thread-secret-9");
        let safe = redact(&request.typed_argv().unwrap());
        assert!(
            !safe.iter().any(|a| a.contains("thread-secret-9")),
            "{safe:?}"
        );
        assert!(safe.contains(&"resume".to_string()));
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
