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

use crate::agent::Continue;
use crate::error::{Error, Result};
use crate::event::{Event, Parser, Terminal};
use crate::outcome::{Outcome, Stop};
use crate::request::Request;

/// How many events may queue before the producer waits for the consumer. Deep
/// enough that a burst of tool events does not stall the agent, shallow enough
/// that a consumer which stops reading does not grow without bound.
const EVENT_BUFFER: usize = 256;

/// A run in progress.
///
/// Yields events through [`Run::recv`] and settles into an [`Outcome`] through
/// [`Run::finish`]. Dropping it detaches the run; the child is not killed.
#[derive(Debug)]
pub struct Run {
    events: mpsc::Receiver<Event>,
    task: tokio::task::JoinHandle<Result<Outcome>>,
    argv: Vec<String>,
}

impl Run {
    /// The next event, or `None` once the agent has finished producing them.
    pub async fn recv(&mut self) -> Option<Event> {
        self.events.recv().await
    }

    /// The exact command line that was spawned.
    #[must_use]
    pub fn argv(&self) -> &[String] {
        &self.argv
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
        match self.task.await {
            Ok(result) => result,
            // The driver task itself panicked or was cancelled; there is no
            // outcome to report and no useful exit code to invent.
            Err(join) => Err(Error::Spawn {
                bin: self.argv.first().cloned().unwrap_or_default(),
                source: std::io::Error::other(join),
            }),
        }
    }
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
    let plan = request.plan();
    let argv = request.argv()?;

    // Resolve on PATH first, so a missing agent is an actionable error with an
    // install hint rather than a bare ENOENT out of the spawn.
    which::which(&plan.bin).map_err(|_| Error::NotInstalled {
        agent: request.agent,
        bin: plan.bin.clone(),
        hint: request.agent.install_hint(),
    })?;

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
    for (key, value) in &request.env {
        command.env(key, value);
    }

    let child = command.spawn().map_err(|source| Error::Spawn {
        bin: plan.bin.clone(),
        source,
    })?;

    let (tx, rx) = mpsc::channel(EVENT_BUFFER);
    let request = request.clone();
    let task = tokio::spawn(drive(child, request, tx));
    Ok(Run {
        events: rx,
        task,
        argv,
    })
}

/// Feed the child, read both its pipes, and assemble the outcome.
async fn drive(mut child: Child, request: Request, events: mpsc::Sender<Event>) -> Result<Outcome> {
    let plan = request.plan();
    let bin = plan.bin.clone();

    // Deliver a piped prompt and close the pipe, or the agent waits on EOF.
    if plan.stdin_prompt {
        if let Some(mut stdin) = child.stdin.take() {
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
    let stderr = child.stderr.take();
    let stderr_task = tokio::spawn(async move {
        let mut buf = String::new();
        if let Some(handle) = stderr {
            let mut lines = BufReader::new(handle).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                buf.push_str(&line);
                buf.push('\n');
            }
        }
        buf
    });

    let stdout = child.stdout.take();
    let mut parser = Parser::new(request.agent, plan.format);
    let mut raw = String::new();

    let read_stdout = async {
        if let Some(handle) = stdout {
            let mut lines = BufReader::new(handle).lines();
            while let Some(line) = lines.next_line().await? {
                raw.push_str(&line);
                raw.push('\n');
                for event in parser.push(&line) {
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
                child.wait().await
            })
            .await
            {
                Ok(result) => result,
                Err(_elapsed) => {
                    // Kill, then reap, so no zombie is left behind.
                    let _ = child.kill().await;
                    return Err(Error::Timeout {
                        bin,
                        timeout: limit,
                        partial: parser.finish().text,
                    });
                }
            }
        }
        None => match read_stdout.await {
            Ok(()) => child.wait().await,
            Err(source) => Err(source),
        },
    }
    .map_err(|source| Error::Spawn {
        bin: bin.clone(),
        source,
    })?;

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

    if exit_code != 0 {
        return Err(classify(&bin, exit_code, &stderr, &raw, &terminal));
    }

    persist_session(&request, &terminal);
    Ok(Outcome {
        agent: request.agent,
        session: terminal.session,
        text: terminal.text,
        usage: terminal.usage,
        stop: terminal.stop,
        rate_limit: terminal.rate_limit,
        exit_code,
        stderr,
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

/// Write the session binding back, if this run was attached to a name.
///
/// Best-effort: a store that cannot be written must not discard a completed
/// run's result. The next turn simply starts a new conversation.
fn persist_session(request: &Request, terminal: &Terminal) {
    let Some(binding) = &request.binding else {
        return;
    };
    // Prefer the id the agent reported. For a minted session it is the one we
    // assigned, so the two agree; for a forked one the agent reports the *new*
    // branch, which is what the name should now follow.
    let token = terminal
        .session
        .clone()
        .or_else(|| match &request.plan().cont {
            Continue::NewWith(id) => Some(id.clone()),
            _ => None,
        });
    if let Some(token) = token {
        let _ = binding
            .store
            .bind(request.agent, &binding.project, &binding.name, &token);
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
