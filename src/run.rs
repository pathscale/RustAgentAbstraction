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
use crate::proc::{kill_group_by_pid, kill_process_group};
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

/// Aborts a task when dropped.
///
/// The decision forwarder holds the child's stdin, so leaving it running past
/// the run would keep a pipe open to a process that is gone.
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// How many decisions may queue on the way back to the agent.
///
/// Small on purpose: the agent asks one question at a time and waits, so a deep
/// queue here would only mean answers piling up for questions nobody asked.
const APPROVAL_BUFFER: usize = 8;

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
/// On Unix, dropping **synchronously signals** the run's process group and then
/// aborts the driver task. What it cannot do is *wait*: `Drop` cannot await, so
/// it does not block until the child has exited or its readers have been
/// joined. Use [`Run::cancel`] when you need to know the tree has actually gone
/// before continuing, such as before touching the files it was working on. On
/// Windows only the direct child is signalled.
#[derive(Debug)]
pub struct Run {
    events: mpsc::Receiver<Event>,
    /// Which agent this is, so `respond` can name it in an error.
    agent: crate::Agent,
    /// The typed command line, kept so both the plain and redacted views come
    /// from the same source.
    typed: Vec<crate::agent::Arg>,
    /// The child's pid, so `Drop` can tear the group down itself rather than
    /// depending on an aborted task being polled.
    pid: Option<u32>,
    /// Set by the driver once the child has been reaped, so `Drop` never
    /// signals a pid the OS may since have handed to someone else.
    reaped: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Decisions on the way back to the agent. `None` unless the request asked
    /// for approvals, which is what makes [`Run::respond`] refuse rather than
    /// silently do nothing on an ordinary run.
    decisions: Option<mpsc::Sender<String>>,
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

    /// Answer an [`Event::ApprovalRequest`].
    ///
    /// The agent is blocked until this is called, so a consumer that receives an
    /// approval request and never responds stalls the run until its timeout.
    ///
    /// The id must be the one from the request. The agent ignores an answer
    /// carrying any other id and keeps waiting, so a mismatch presents as a
    /// hang rather than an error; this passes the id straight through and does
    /// not invent one.
    ///
    /// # Errors
    /// [`Error::Unsupported`] on a run that did not ask for approvals, since
    /// there is no channel to answer on. [`Error::Cancelled`] if the run has
    /// already finished or been torn down, which is the same reason a decision
    /// can no longer be delivered.
    pub async fn respond(&self, id: &str, decision: &crate::Decision) -> Result<()> {
        let Some(channel) = &self.decisions else {
            return Err(Error::Unsupported {
                agent: self.agent,
                what: "answering an approval on a run that did not request them",
            });
        };
        channel
            .send(decision.wire(id))
            .await
            .map_err(|_| Error::Cancelled {
                bin: self.argv.first().cloned().unwrap_or_default(),
            })
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
        // The driver owns teardown from here; `Drop` must not also fire.
        self.pid = None;
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
        // The driver tears down cooperatively and this awaits it, so `Drop`
        // must not race that with a kill of its own.
        self.pid = None;
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
        // Disarm `Drop` before it runs, or detaching would immediately kill the
        // run it exists to keep alive.
        self.pid = None;
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
        // Abandoned rather than finished, cancelled or detached.
        //
        // Kill the group here, directly. Signalling the driver and aborting it
        // is not enough on its own: that leaves teardown waiting on the runtime
        // to poll the aborted task so its guard runs, and a dropped `Run` was
        // observed leaving grandchildren alive and sleeping on Linux while the
        // same teardown worked from `cancel`. `Drop` cannot await, so it does
        // the one thing it can do synchronously.
        if let Some(pid) = self.pid
            && !self.reaped.load(std::sync::atomic::Ordering::SeqCst)
        {
            kill_group_by_pid(pid);
        }
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
///
/// [`Error::Unsupported`] for a request that asked for approvals: this entry
/// point discards events, so an approval request would reach nobody and the run
/// would sit blocked until its timeout. Use [`stream`] instead.
pub async fn run(request: &Request) -> Result<Outcome> {
    if request.plan().approvals {
        return Err(Error::Unsupported {
            agent: request.agent,
            what: "approvals on a run whose events are discarded; use `stream`",
        });
    }
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

    // Written before the argv is built, because the argv has to name it.
    let schema_file = match (&request.schema, request.agent.caps().schema) {
        (Some(schema), crate::agent::SchemaSupport::File) => {
            Some(SchemaFile::write(schema).map_err(|source| Error::Spawn {
                bin: request.agent.bin().to_string(),
                source,
            })?)
        }
        _ => None,
    };
    let mut request = request.clone();
    if let Some(file) = &schema_file {
        request.schema_file = Some(file.0.display().to_string());
    }
    let request = &request;

    let plan = request.plan();
    let typed = request.typed_argv()?;
    let argv: Vec<String> = typed.iter().map(|a| a.value.clone()).collect();

    let mut command = Command::new(&argv[0]);
    command
        .args(&argv[1..])
        .stdin(if plan.stdin_prompt || plan.approvals {
            // An approvals run needs stdin for the whole turn, not just to
            // deliver a prompt: it is the channel every decision travels back
            // on.
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

    let request_agent = request.agent;
    let pid = child.id();
    let (tx, rx) = mpsc::channel(EVENT_BUFFER);
    // Only created for an approvals run, so `respond` can tell "no channel" from
    // "channel closed" and refuse the first rather than hanging on it.
    let (decisions_tx, decisions_rx) = if plan.approvals {
        let (tx, rx) = mpsc::channel::<String>(APPROVAL_BUFFER);
        (Some(tx), Some(rx))
    } else {
        (None, None)
    };
    let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel();
    let reaped = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let reaped_for_task = std::sync::Arc::clone(&reaped);
    let request = request.clone();
    let task = runtime.spawn(async move {
        // Moved in so the file outlives the run and is removed with it.
        let _schema_file = schema_file;
        drive(child, request, tx, cancel_rx, reaped_for_task, decisions_rx).await
    });
    Ok(Run {
        events: rx,
        agent: request_agent,
        typed,
        pid,
        reaped,
        cancel: Some(cancel_tx),
        decisions: decisions_tx,
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

/// A schema file written for one run, removed when the run ends.
///
/// Codex reads its schema from disk, so the file has to outlive the spawn and
/// not outlive the process. Tying it to a guard means every exit path removes
/// it, including a cancel or a timeout, without each one remembering.
struct SchemaFile(std::path::PathBuf);

impl SchemaFile {
    /// Write `schema` somewhere the agent can read it.
    fn write(schema: &str) -> std::io::Result<SchemaFile> {
        use std::io::Write as _;
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);

        let path = std::env::temp_dir().join(format!(
            "agent-abstraction-schema-{}-{}.json",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        // A schema can encode what a caller is looking for, so it is no more
        // public than the prompt.
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        options.open(&path)?.write_all(schema.as_bytes())?;
        Ok(SchemaFile(path))
    }
}

impl Drop for SchemaFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
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
    reaped: std::sync::Arc<std::sync::atomic::AtomicBool>,
    decisions: Option<mpsc::Receiver<String>>,
) -> Result<Outcome> {
    // From here on the child is owned by a guard, so every exit path from this
    // task, including an abort, takes the process group with it.
    let mut child = ChildGuard { child, armed: true };
    let plan = request.plan();
    let bin = plan.bin.clone();

    // An approvals run owns stdin for the whole turn: the handshake and the
    // prompt go out first, then it stays open carrying decisions until the run
    // ends. Closing it after the prompt, as the plain piped path does, would
    // take the answer channel with it.
    let mut decision_task = None;
    let mut close_stdin = None;
    if plan.approvals {
        let Some(mut stdin) = child.child.stdin.take() else {
            return Err(Error::Spawn {
                bin: bin.clone(),
                source: std::io::Error::other("stdin was not piped for an approvals run"),
            });
        };
        let opening = format!(
            "{}{}",
            crate::approval::handshake(),
            crate::approval::user_message(&request.agent.effective_prompt(&plan)),
        );
        stdin
            .write_all(opening.as_bytes())
            .await
            .map_err(|source| Error::Spawn {
                bin: bin.clone(),
                source,
            })?;
        let _ = stdin.flush().await;
        // Forwarding runs on its own task so a decision can be written while
        // stdout is being read. It ends on whichever comes first: the channel
        // closing, or the turn settling.
        let (close_tx, mut close_rx) = tokio::sync::oneshot::channel::<()>();
        close_stdin = Some(close_tx);
        decision_task = decisions.map(|mut rx| {
            tokio::spawn(async move {
                loop {
                    tokio::select! {
                        reply = rx.recv() => {
                            let Some(reply) = reply else { break };
                            if stdin.write_all(reply.as_bytes()).await.is_err() {
                                break;
                            }
                            let _ = stdin.flush().await;
                        }
                        // The turn is over. Dropping stdin is what lets claude
                        // exit rather than wait for another message.
                        _ = &mut close_rx => break,
                    }
                }
                drop(stdin);
            })
        });
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
    // Aborted on every exit path from here, so a forwarder never survives the
    // run it belongs to.
    let _decision_guard = decision_task.map(AbortOnDrop);

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
                let parsed = parser.push(&line);
                // Close stdin as soon as the turn settles. Under stream-json
                // input claude waits for another message otherwise, so the run
                // would only end at its timeout even though the answer already
                // arrived.
                if parser.saw_terminal()
                    && let Some(close) = close_stdin.take()
                {
                    let _ = close.send(());
                }
                for event in parsed {
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
            reaped.store(true, std::sync::atomic::Ordering::SeqCst);
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
            reaped.store(true, std::sync::atomic::Ordering::SeqCst);
            return Err(Error::Cancelled { bin });
        }
    }
    .map_err(|source| Error::Spawn {
        bin: bin.clone(),
        source,
    })?;

    // The child has been reaped, so its pid must not be signalled again, by the
    // guard here or by `Run::drop` racing this.
    child.armed = false;
    reaped.store(true, std::sync::atomic::Ordering::SeqCst);

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
    // An unauthenticated Claude run exits 0 and reports the problem in its
    // result text, so checking only the exit code would hand back a successful
    // Outcome whose answer is "Please run /login".
    //
    // Read from stderr and the agent's own prose rather than the raw stream, for
    // the reason `classify` does the same with quota wording: a phrase hunted
    // through structured output matches ids and field names, not statements.
    let unauthenticated = looks_unauthenticated(&terminal.text) || looks_unauthenticated(&stderr);
    // The agent saying its turn failed is as much a failure as a non-zero exit,
    // and Claude reports an unknown model exactly this way: exit 0, `is_error`
    // true, and the explanation where the answer would be.
    let turn_failed = terminal.stop == Stop::Error;
    if exit_code != 0 || quota_blocked || unauthenticated || turn_failed {
        return Err(classify_run(
            request.agent,
            &bin,
            exit_code,
            &stderr,
            &raw,
            &terminal,
        ));
    }

    // A fork lands on a *new* id the agent only reveals at the end, so the name
    // has to be repointed once the run settles. Everything else was bound above.
    persist_result?;
    // Resolved before the terminal is consumed by the Outcome below.
    let structured = terminal.structured.clone().or_else(|| {
        request
            .schema
            .as_ref()
            .and_then(|_| serde_json::from_str(&terminal.text).ok())
    });
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
        // Claude reports the conforming value separately; Codex returns it as
        // the answer text, so that is parsed only when a schema was asked for.
        // Prose is never reinterpreted as data.
        structured,
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

/// Turn a failure into the most specific error available, agent included so an
/// auth failure can carry the right login command.
fn classify_run(
    agent: crate::Agent,
    bin: &str,
    code: i32,
    stderr: &str,
    stdout: &str,
    terminal: &Terminal,
) -> Error {
    // Checked before quota and before a plain failure: a login problem is the
    // most specific reading of the output, and the only one a user can act on
    // directly.
    for source in [terminal.text.as_str(), stderr, stdout] {
        if looks_unauthenticated(source) {
            return Error::NotAuthenticated {
                agent,
                bin: bin.to_string(),
                message: first_meaningful_line(source).unwrap_or_default(),
                hint: agent.login_hint(),
            };
        }
    }
    classify(agent, bin, code, stderr, stdout, terminal)
}

/// Whether text is an agent saying it has no usable credentials.
///
/// Narrow on purpose. Mislabelling an ordinary failure as an auth problem sends
/// someone to re-login over something unrelated, so these are phrases the CLIs
/// actually emit rather than every string containing "auth".
fn looks_unauthenticated(text: &str) -> bool {
    const PHRASES: &[&str] = &[
        // Claude, verified: an unauthenticated run answers exactly this.
        "not logged in",
        "please run /login",
        // Copilot, verified: it exits 1 with plain text, and none of the other
        // phrases here appear in it. Its wording shares no vocabulary with the
        // other two, which is why this had to be observed rather than guessed.
        "no authentication information",
        "invalid api key",
        "authentication_error",
        "unauthorized",
        "not authenticated",
        "no credentials",
        "credentials not found",
        "please log in",
    ];
    let lower = text.to_ascii_lowercase();
    PHRASES.iter().any(|needle| lower.contains(needle)) || mentions_status(&lower, "401")
}

/// Whether `code` appears as a standalone token rather than inside a longer run
/// of characters.
///
/// `401` was previously matched as a bare substring, which made any Copilot
/// failure an auth failure whenever one of the UUIDs it prints happened to
/// contain those three digits: `"id":"1b0b1401-cb86-..."` was enough. That is
/// not rare, since a run emits several ids, so the misdiagnosis was
/// intermittent and told someone to re-login over an unrelated failure.
///
/// A status code is a word. Requiring non-alphanumeric neighbours keeps
/// `HTTP 401` and `(status 401)` while rejecting every hex blob, and a UUID
/// cannot produce a standalone `401` at all because its groups are four, eight
/// or twelve characters long.
fn mentions_status(haystack: &str, code: &str) -> bool {
    haystack.match_indices(code).any(|(at, _)| {
        let before = haystack[..at].chars().next_back();
        let after = haystack[at + code.len()..].chars().next();
        let free = |c: Option<char>| c.is_none_or(|c| !c.is_alphanumeric());
        free(before) && free(after)
    })
}

/// Turn a non-zero exit into the most specific error available.
fn classify(
    agent: crate::Agent,
    bin: &str,
    code: i32,
    stderr: &str,
    stdout: &str,
    terminal: &Terminal,
) -> Error {
    let quota_signalled = terminal
        .rate_limit
        .as_ref()
        .is_some_and(crate::outcome::RateLimit::is_blocking);
    // Scanning the *raw* stream for quota wording is a false-positive machine:
    // under `stream-json` Claude prints a `rate_limit_event` record on every
    // run, including one whose status is `allowed`, so the substring
    // `rate_limit` is present in perfectly healthy output. Where the stream
    // parsed, the parsed signal and the agent's own prose decide; the raw scan
    // is only the fallback for output that produced neither.
    let prose = match (&terminal.error_message, terminal.text.as_str()) {
        (Some(message), text) => format!("{message}\n{text}"),
        (None, text) if !text.is_empty() => text.to_string(),
        _ => stdout.to_string(),
    };
    if quota_signalled || looks_rate_limited(stderr) || looks_rate_limited(&prose) {
        return Error::RateLimited {
            bin: bin.to_string(),
            message: first_meaningful_line(stderr)
                .or_else(|| first_meaningful_line(&prose))
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
    // Checked before the generic failure but after quota and a rejected flag,
    // which are more specific readings of the same output.
    if terminal.stop == Stop::Error {
        return Error::AgentError {
            agent,
            bin: bin.to_string(),
            status: terminal.error_status,
            // Codex reports the reason apart from the answer; Claude puts it
            // where the answer would be.
            message: terminal
                .error_message
                .clone()
                .or_else(|| first_meaningful_line(&terminal.text))
                .or_else(|| first_meaningful_line(stderr))
                .unwrap_or_else(|| "the agent reported a failure without explaining it".into()),
        };
    }

    Error::Failed {
        bin: bin.to_string(),
        code,
        // Fall back to stdout when stderr explains nothing. Codex reports a
        // rejected schema as an `{"type":"error"}` event on *stdout* while
        // stderr carries only "Reading additional input from stdin...", so
        // reporting stderr alone describes the failure as a status message.
        stderr: first_meaningful_line(stderr)
            .filter(|line| looks_explanatory(line))
            .or_else(|| first_meaningful_line(stdout))
            .or_else(|| first_meaningful_line(stderr))
            .unwrap_or_default(),
    }
}

/// Whether a line plausibly explains a failure rather than narrating progress.
fn looks_explanatory(line: &str) -> bool {
    const NOISE: &[&str] = &[
        "reading additional input",
        "reading prompt",
        "waiting",
        "connecting",
        "loading",
    ];
    let lower = line.to_ascii_lowercase();
    !NOISE.iter().any(|noise| lower.contains(noise))
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

/// The most useful line of a CLI's output for an error message.
///
/// Not simply the first non-blank one. CLIs open with progress and status
/// chatter, so the first line is often "Reading additional input from stdin..."
/// while the actual cause is further down. That turns a report into a
/// misdirection: it looks like an explanation and is not one.
///
/// So a line that looks like an error wins, and the first non-blank line is the
/// fallback when nothing does.
fn first_meaningful_line(text: &str) -> Option<String> {
    const ERROR_MARKERS: &[&str] = &[
        "error",
        "failed",
        "fatal",
        "panic",
        "denied",
        "invalid",
        "unexpected",
        "cannot",
        "unable",
    ];
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();

    lines
        .iter()
        .find(|line| {
            let lower = line.to_ascii_lowercase();
            ERROR_MARKERS.iter().any(|marker| lower.contains(marker))
        })
        .or_else(|| lines.first())
        .map(|line| (*line).to_string())
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
                overage_status: None,
                is_using_overage: None,
            }),
            ..Terminal::default()
        };
        assert!(matches!(
            classify(Agent::Claude, "claude", 1, "", "", &terminal),
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
                overage_status: None,
                is_using_overage: None,
            }),
            ..Terminal::default()
        };
        assert!(matches!(
            classify(Agent::Claude, "claude", 1, "boom", "", &terminal),
            Error::Failed { .. }
        ));
    }

    /// The exact shape that made a Copilot run look unauthenticated: a UUID
    /// carrying the digits 401. Copilot prints several ids per run, so this
    /// misfired intermittently and told the user to re-login over a failure
    /// that had nothing to do with credentials.
    #[test]
    fn an_id_containing_401_is_not_an_auth_failure() {
        let line = r#"{"type":"session.mcp_server_status_changed","id":"1b0b1401-cb86-4276-9874-e84b94c96499"}"#;
        assert!(
            !looks_unauthenticated(line),
            "a hex blob is not a status code"
        );
    }

    /// The needle still has to work where it was meant to. A status code is a
    /// word, and these are the forms an agent actually prints.
    #[test]
    fn a_real_401_is_still_recognized() {
        for text in [
            "HTTP 401",
            "request failed (status 401)",
            "401: unauthorized",
            "got a 401 from the API",
        ] {
            assert!(looks_unauthenticated(text), "should match: {text}");
        }
    }

    /// Neighbouring digits mean it is part of some longer number, not a status.
    #[test]
    fn digits_around_401_keep_it_from_matching() {
        for text in ["error 4010", "code 1401", "seq 24019"] {
            assert!(!looks_unauthenticated(text), "should not match: {text}");
        }
    }

    /// Verbatim from a healthy claude 2.1.205 run. Every `stream-json` run
    /// carries this record, and its status is `allowed`: nothing is refused.
    /// Scanning the raw stream for `rate_limit` matched it anyway, so any
    /// Claude failure was reported as a quota refusal, sending a caller to back
    /// off when the real cause was something they could fix.
    #[test]
    fn a_healthy_rate_limit_heartbeat_is_not_a_refusal() {
        let stdout = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed","resetsAt":1785331800,"rateLimitType":"five_hour","overageStatus":"rejected","isUsingOverage":false}}"#;
        let terminal = Terminal {
            stop: Stop::Error,
            error_status: Some(404),
            text: "There's an issue with the selected model (bogus-model-xyz).".into(),
            rate_limit: Some(crate::outcome::RateLimit {
                status: "allowed".into(),
                window: Some("five_hour".into()),
                resets_at: Some(1_785_331_800),
                overage_status: None,
                is_using_overage: None,
            }),
            ..Terminal::default()
        };
        let err = classify_run(Agent::Claude, "claude", 0, "", stdout, &terminal);
        assert!(
            matches!(err, Error::AgentError { .. }),
            "the heartbeat must not mask the real cause: {err:?}"
        );
    }

    /// The counterpart: a refusal the parser did read must still be one, even
    /// though it arrives with the same zero exit code.
    #[test]
    fn a_rejected_quota_signal_is_still_a_refusal() {
        let terminal = Terminal {
            rate_limit: Some(crate::outcome::RateLimit {
                status: "rejected".into(),
                window: Some("five_hour".into()),
                resets_at: None,
                overage_status: None,
                is_using_overage: None,
            }),
            ..Terminal::default()
        };
        assert!(matches!(
            classify_run(Agent::Claude, "claude", 0, "", "", &terminal),
            Error::RateLimited { .. }
        ));
    }

    /// Verbatim from a real run with an unknown model. Claude exits **0** with
    /// `subtype: "success"` while `is_error` is true and the explanation sits
    /// where the answer would be, so a caller checking only `Result::is_ok`
    /// renders "There's an issue with the selected model" as the answer.
    #[test]
    fn a_failed_turn_is_an_error_even_though_the_process_exited_cleanly() {
        let terminal = Terminal {
            stop: Stop::Error,
            error_status: Some(404),
            text: "There's an issue with the selected model (bogus-model-xyz). \
                   It may not exist or you may not have access to it."
                .into(),
            ..Terminal::default()
        };
        let err = classify_run(Agent::Claude, "claude", 0, "", "", &terminal);
        let Error::AgentError {
            agent,
            status,
            message,
            ..
        } = &err
        else {
            panic!("expected AgentError, got {err:?}")
        };
        assert_eq!(*agent, Agent::Claude);
        assert_eq!(*status, Some(404), "the provider status must survive");
        assert!(message.contains("selected model"), "{message}");
    }

    /// A quota refusal and a missing login are more specific readings of the
    /// same shape, so they must not be swallowed by the general case.
    #[test]
    fn a_failed_turn_does_not_mask_a_more_specific_cause() {
        let auth = Terminal {
            stop: Stop::Error,
            text: "Not logged in · Please run /login".into(),
            ..Terminal::default()
        };
        assert!(
            classify_run(Agent::Claude, "claude", 0, "", "", &auth).is_auth_failure(),
            "an unauthenticated failed turn must stay an auth failure"
        );

        let quota = Terminal {
            stop: Stop::Error,
            rate_limit: Some(crate::outcome::RateLimit {
                status: "rejected".into(),
                window: None,
                resets_at: None,
                overage_status: None,
                is_using_overage: None,
            }),
            ..Terminal::default()
        };
        assert!(
            matches!(
                classify_run(Agent::Claude, "claude", 0, "", "", &quota),
                Error::RateLimited { .. }
            ),
            "a quota-blocked failed turn must stay a rate limit"
        );
    }

    /// Verified against the real CLI: with `USER` withheld, claude answers
    /// "Not logged in · Please run /login" and exits **0**. Checking only the
    /// exit code hands back a successful Outcome whose answer is a login
    /// prompt.
    #[test]
    fn an_unauthenticated_run_is_named_even_though_it_exits_zero() {
        let terminal = Terminal {
            text: "Not logged in · Please run /login".into(),
            ..Terminal::default()
        };
        let err = classify_run(Agent::Claude, "claude", 0, "", "", &terminal);
        let Error::NotAuthenticated { agent, hint, .. } = &err else {
            panic!("expected NotAuthenticated, got {err:?}")
        };
        assert_eq!(*agent, Agent::Claude);
        assert!(hint.contains("/login"), "{hint}");
        assert!(err.is_auth_failure());
    }

    /// Verbatim from an unauthenticated Copilot run, captured by pointing it at
    /// an empty HOME. Its wording shares no phrase with Claude's or Codex's, so
    /// before this was observed the phrase list did not match it at all and a
    /// missing Copilot login was reported as a generic failure.
    #[test]
    fn copilots_own_unauthenticated_wording_is_recognized() {
        let stderr = "Error: No authentication information found.\n\n\
                      Copilot can be authenticated with GitHub using an OAuth Token or a \
                      Fine-Grained Personal Access Token.\n\n\
                      To authenticate, you can use any of the following methods:\n\
                      \u{2022} Start 'copilot' and run the '/login' command\n\
                      \u{2022} Set the COPILOT_GITHUB_TOKEN, GH_TOKEN, or GITHUB_TOKEN \
                      environment variable";
        let err = classify_run(
            Agent::Copilot,
            "copilot",
            1,
            stderr,
            "",
            &Terminal::default(),
        );
        let Error::NotAuthenticated { agent, hint, .. } = &err else {
            panic!("expected NotAuthenticated, got {err:?}")
        };
        assert_eq!(*agent, Agent::Copilot);
        assert!(hint.contains("copilot login"), "{hint}");
    }

    /// Each agent's hint has to name its own login route, since they differ:
    /// Codex and Copilot have `login` subcommands, Claude does not.
    #[test]
    fn every_agent_offers_its_own_login_route() {
        for (agent, expected) in [
            (Agent::Claude, "setup-token"),
            (Agent::Codex, "codex login"),
            (Agent::Copilot, "copilot login"),
        ] {
            let err = classify_run(
                agent,
                agent.bin(),
                1,
                "error: unauthorized",
                "",
                &Terminal::default(),
            );
            let Error::NotAuthenticated { hint, .. } = &err else {
                panic!("{agent}: expected NotAuthenticated, got {err:?}")
            };
            assert!(hint.contains(expected), "{agent}: {hint}");
        }
    }

    /// Auth is the most specific reading, so it wins over a generic failure,
    /// but must not swallow unrelated errors.
    #[test]
    fn ordinary_failures_are_not_mistaken_for_auth_problems() {
        for stderr in [
            "error: no such file or directory",
            "model not found",
            "rate limit exceeded",
            "error: unexpected argument '--sandbox' found",
        ] {
            let err = classify_run(Agent::Codex, "codex", 1, stderr, "", &Terminal::default());
            assert!(
                !err.is_auth_failure(),
                "{stderr:?} was misread as an auth failure: {err:?}"
            );
        }
    }

    /// The exact failure that cost a round of debugging: `codex exec resume`
    /// rejects `--sandbox`, which `Error::Failed` reported as a generic
    /// non-zero exit naming a flag rather than a version mismatch.
    #[test]
    fn a_rejected_flag_is_named_as_a_version_mismatch() {
        let err = classify(
            Agent::Codex,
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
                    classify(Agent::Codex, "codex", 1, stderr, "", &Terminal::default()),
                    Error::Failed { .. }
                ),
                "{stderr:?} should stay a plain failure"
            );
        }
    }

    /// Real output from a failing codex run: the first line is status, the
    /// cause is below it. Reporting the first line looks like an explanation
    /// while pointing at the wrong thing.
    #[test]
    fn a_status_line_does_not_masquerade_as_the_cause() {
        let stderr = "Reading additional input from stdin...\n\
                      error: invalid value 'nope' for '--sandbox <SANDBOX_MODE>'";
        let err = classify_run(Agent::Codex, "codex", 1, stderr, "", &Terminal::default());
        let Error::Failed {
            stderr: reported, ..
        } = err
        else {
            panic!("expected Failed, got {err:?}")
        };
        assert!(reported.contains("invalid value"), "reported {reported:?}");
    }

    /// Codex reports a rejected schema as a JSON error event on **stdout**
    /// while stderr carries only a status line. Reporting stderr alone
    /// described the failure as "Reading additional input from stdin...",
    /// which is not what went wrong.
    #[test]
    fn a_cause_on_stdout_is_reported_when_stderr_only_narrates() {
        let stdout = r#"{"type":"error","message":"invalid_json_schema: 'additionalProperties' is required to be supplied and to be false."}"#;
        let err = classify_run(
            Agent::Codex,
            "codex",
            1,
            "Reading additional input from stdin...",
            stdout,
            &Terminal::default(),
        );
        let Error::Failed {
            stderr: reported, ..
        } = err
        else {
            panic!("expected Failed, got {err:?}")
        };
        assert!(
            reported.contains("additionalProperties"),
            "reported {reported:?}, which explains nothing"
        );
    }

    #[test]
    fn failures_report_the_first_useful_line() {
        let err = classify(
            Agent::Claude,
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
