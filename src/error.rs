//! The one error type every fallible call in this crate returns.
//!
//! Each variant is a case a caller has to branch on differently. A GUI shows
//! [`Error::NotInstalled`] as an install prompt, [`Error::RateLimited`] as
//! "wait", and [`Error::Unsupported`] as a programming mistake. Failures that
//! need no branch collapse into [`Error::Spawn`] / [`Error::Store`].

use std::time::Duration;

use crate::agent::Agent;

/// Result alias for this crate.
pub type Result<T> = std::result::Result<T, Error>;

/// Everything that can go wrong driving an agent CLI.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The agent's binary is not on `PATH`. Carries the install command so a UI
    /// can offer it directly instead of making the user go find it.
    #[error("`{bin}` not found on PATH; install it: {hint}")]
    NotInstalled {
        /// The agent whose binary is missing.
        agent: Agent,
        /// The binary name that was looked up.
        bin: String,
        /// The documented install command.
        hint: &'static str,
    },

    /// The child process could not be started, or its stdio could not be read.
    #[error("failed to spawn `{bin}`: {source}")]
    Spawn {
        /// The binary that failed to start.
        bin: String,
        /// The underlying OS error.
        #[source]
        source: std::io::Error,
    },

    /// The run exceeded its deadline and the child was killed. Any output
    /// captured before the kill is preserved so a caller can still show it.
    #[error("`{bin}` exceeded its {} s timeout and was killed", timeout.as_secs())]
    Timeout {
        /// The binary that overran.
        bin: String,
        /// The deadline that was hit.
        timeout: Duration,
        /// Whatever the agent had printed before it was killed.
        partial: String,
    },

    /// The agent ran to completion but exited non-zero.
    #[error("`{bin}` exited with status {code}: {stderr}")]
    Failed {
        /// The binary that failed.
        bin: String,
        /// Its exit code, or `-1` when it died to a signal.
        code: i32,
        /// Its stderr, trimmed, for the message.
        stderr: String,
    },

    /// The provider refused the request for quota reasons: a usage limit, a
    /// rate limit, or an exhausted budget.
    ///
    /// This is deliberately its own variant and this crate never retries it
    /// automatically. Backing off is the caller's decision and burying a
    /// retry loop in here would turn a limit the provider set into something
    /// the library quietly works around. See `docs/operating-limits.md`.
    #[error("`{bin}` was rate limited or hit a usage limit: {message}")]
    RateLimited {
        /// The binary that was limited.
        bin: String,
        /// The provider's own wording, passed through unedited.
        message: String,
    },

    /// The request asked an agent for something it cannot do headlessly:
    /// forking on Codex, a named session on Copilot, an event stream on an
    /// agent that only prints text.
    ///
    /// Always an error, never a silent downgrade: a caller that asked to fork
    /// and got a linear resume would corrupt the conversation it meant to
    /// branch.
    #[error("{agent} does not support {what}")]
    Unsupported {
        /// The agent that was asked.
        agent: Agent,
        /// The capability it lacks.
        what: &'static str,
    },

    /// A named session already belongs to a different agent. Sessions cannot
    /// migrate: the stored handle is only meaningful to the CLI that minted it.
    #[error("session `{name}` belongs to {bound}, cannot resume it on {requested}")]
    SessionConflict {
        /// The caller's session name.
        name: String,
        /// The agent that created the session.
        bound: Agent,
        /// The agent the caller tried to use.
        requested: Agent,
    },

    /// The session store could not be read or written.
    #[error("session store I/O failed at {path}: {source}")]
    Store {
        /// The file or directory involved.
        path: String,
        /// The underlying OS error.
        #[source]
        source: std::io::Error,
    },

    /// The agent produced output this crate could not interpret: a missing
    /// session id under a format that promises one, or unparseable JSON where
    /// the contract requires it.
    #[error("could not parse {agent} output: {detail}")]
    Parse {
        /// The agent whose output was unreadable.
        agent: Agent,
        /// What specifically was wrong.
        detail: String,
    },

    /// The run was stopped by [`crate::Run::cancel`] or by dropping its handle.
    ///
    /// Not a fault: the caller asked for this. Distinguished from
    /// [`Error::Interrupted`], which means the driver died unexpectedly, and
    /// from [`Error::Timeout`], which is a deadline rather than a request.
    #[error("the run of `{bin}` was cancelled")]
    Cancelled {
        /// The binary that was stopped.
        bin: String,
    },

    /// A prompt, system prompt or raw argument too large for the command line,
    /// on an agent with no way to deliver it off the argv.
    ///
    /// Returned rather than letting the OS reject the spawn with a bare
    /// `E2BIG`, which says nothing about which input was the problem.
    #[error(
        "{what} is {size} bytes, over the {limit} byte command-line budget for {agent}, \
         and it has no way to take it off the command line"
    )]
    CommandLineTooLarge {
        /// The agent the request targeted.
        agent: Agent,
        /// Which input overflowed.
        what: &'static str,
        /// Its size in bytes.
        size: usize,
        /// The budget it exceeded.
        limit: usize,
    },

    /// [`crate::stream`] was called outside a Tokio runtime.
    ///
    /// Spawning the driver task needs a runtime context. Reporting this rather
    /// than letting `tokio::spawn` panic keeps the fallible signature honest.
    #[error("no Tokio runtime is running; call this from within one")]
    NoRuntime,

    /// The task driving the run panicked or was cancelled, so there is no
    /// outcome to report.
    ///
    /// Distinct from [`Error::Spawn`] on purpose: the process started fine, and
    /// reporting this as a spawn failure would name the wrong cause. It is also
    /// why this is not squeezed into an [`std::io::Error`], which a dropped
    /// runtime task is not.
    #[error("the run of `{bin}` was interrupted: {detail}")]
    Interrupted {
        /// The binary that was running.
        bin: String,
        /// Whether the task panicked or was cancelled.
        detail: String,
    },
}

impl Error {
    /// Whether retrying this exact request later could plausibly succeed.
    ///
    /// True for quota and timeout failures; false for a missing binary, an
    /// unsupported capability, or a session conflict, which need the caller to
    /// change something first. This classifies; it does not retry.
    #[must_use]
    pub fn is_transient(&self) -> bool {
        matches!(self, Error::RateLimited { .. } | Error::Timeout { .. })
    }

    /// Whether this run was stopped because the caller asked, rather than
    /// because anything went wrong. A UI should not show it as a failure.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        matches!(self, Error::Cancelled { .. })
    }
}
