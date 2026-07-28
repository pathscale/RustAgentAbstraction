//! Describing a run before it happens.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::agent::{Agent, Continue, Format, Permission, Plan, STDIN_THRESHOLD};
use crate::error::Result;
use crate::session::{Phase, SessionStore};

/// A run, described but not yet started.
///
/// Built fluently and then handed to [`crate::run`] or [`crate::stream`]:
///
/// ```no_run
/// use agent_abstraction::{Agent, Permission, Request};
///
/// let request = Request::new(Agent::Claude, "summarize this repo")
///     .model("sonnet")
///     .permission(Permission::ReadOnly);
/// ```
#[derive(Debug, Clone)]
pub struct Request {
    pub(crate) agent: Agent,
    pub(crate) bin: Option<String>,
    pub(crate) prompt: String,
    pub(crate) system: Option<String>,
    pub(crate) model: Option<String>,
    pub(crate) permission: Permission,
    pub(crate) format: Option<Format>,
    pub(crate) cont: Continue,
    pub(crate) cwd: Option<PathBuf>,
    pub(crate) env: Vec<(String, String)>,
    pub(crate) extra_args: Vec<String>,
    pub(crate) timeout: Option<Duration>,
    /// Set when [`Request::session`] resolved a named session, so the runner
    /// knows to write the binding back.
    pub(crate) binding: Option<Binding>,
}

/// A named session this run is attached to.
#[derive(Debug, Clone)]
pub(crate) struct Binding {
    pub(crate) store: SessionStore,
    pub(crate) project: PathBuf,
    pub(crate) name: String,
    pub(crate) phase: Phase,
}

impl Request {
    /// A request for `agent` with `prompt`.
    ///
    /// Defaults are deliberately conservative: [`Permission::ReadOnly`], and the
    /// agent's structured output format. Widen them explicitly.
    pub fn new(agent: Agent, prompt: impl Into<String>) -> Self {
        Self {
            agent,
            bin: None,
            prompt: prompt.into(),
            system: None,
            model: None,
            permission: Permission::ReadOnly,
            format: None,
            cont: Continue::New,
            cwd: None,
            env: Vec::new(),
            extra_args: Vec::new(),
            timeout: None,
            binding: None,
        }
    }

    /// Override the binary. Defaults to the agent's own name on `PATH`.
    #[must_use]
    pub fn bin(mut self, bin: impl Into<String>) -> Self {
        self.bin = Some(bin.into());
        self
    }

    /// A system prompt. Delivered by flag where the agent has one and prepended
    /// to the prompt where it does not. It is never dropped.
    #[must_use]
    pub fn system(mut self, system: impl Into<String>) -> Self {
        self.system = Some(system.into());
        self
    }

    /// Pin the model. Passed through verbatim; this crate does not validate
    /// model names, so an unknown one surfaces as the agent's own error.
    #[must_use]
    pub fn model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    /// Set the permission posture.
    #[must_use]
    pub fn permission(mut self, permission: Permission) -> Self {
        self.permission = permission;
        self
    }

    /// Pin the output format. Left unset, a run picks the agent's structured
    /// format, which is also the one that carries a session id.
    #[must_use]
    pub fn format(mut self, format: Format) -> Self {
        self.format = Some(format);
        self
    }

    /// The working directory the agent runs in.
    #[must_use]
    pub fn cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    /// Set an environment variable for the child. Repeatable.
    #[must_use]
    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }

    /// Kill the run if it has not finished within `timeout`.
    #[must_use]
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// Append raw arguments, after everything this crate builds.
    ///
    /// The escape hatch for agent-specific flags with no unified spelling, such
    /// as `--add-dir` to widen file access or Codex's `-c key=value` config
    /// overrides. Arguments are passed straight to the binary without a shell.
    #[must_use]
    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.extra_args.extend(args.into_iter().map(Into::into));
        self
    }

    /// Continue an earlier conversation by its native id, bypassing the session
    /// store. Prefer [`Request::session`] unless you are tracking ids yourself.
    #[must_use]
    pub fn resume(mut self, id: impl Into<String>) -> Self {
        self.cont = Continue::Resume(id.into());
        self
    }

    /// Start a **new** conversation under an id you choose, rather than one the
    /// agent picks.
    ///
    /// Useful when a host already has its own identifier for a thread and wants
    /// the agent's session to match it, with no mapping table in between. The
    /// id is known before the process starts, so the association survives a run
    /// that dies mid-turn.
    ///
    /// Only Claude and Copilot accept an assigned id
    /// ([`SessionSupport::Minted`]). Codex reveals its `thread_id` only in its
    /// own output, so this is [`crate::Error::Unsupported`] for it, raised when
    /// the argv is built rather than silently starting an unrelated session.
    ///
    /// Both CLIs require a valid UUID here; this crate passes the string through
    /// without checking, so a non-UUID surfaces as the agent's own error.
    ///
    /// [`SessionSupport::Minted`]: crate::SessionSupport::Minted
    #[must_use]
    pub fn session_id(mut self, id: impl Into<String>) -> Self {
        self.cont = Continue::NewWith(id.into());
        self
    }

    /// Attach this run to a caller-owned session name.
    ///
    /// The store decides whether this turn creates, continues, or forks, and the
    /// binding is written back once the run yields an id. `fork` branches a new
    /// conversation off the stored one instead of appending to it.
    ///
    /// The store is cloned into the request so the run can write the binding
    /// back without borrowing it. That clone is a [`PathBuf`], not the sessions
    /// themselves: records are read and written on demand and never held in
    /// memory, so this stays cheap however many sessions exist.
    ///
    /// # Errors
    /// [`crate::Error::SessionConflict`] if the name belongs to another agent,
    /// or [`crate::Error::Unsupported`] if this agent cannot fork or has no
    /// session id at all.
    pub fn session(
        mut self,
        store: &SessionStore,
        project: impl AsRef<Path>,
        name: impl Into<String>,
        fork: bool,
    ) -> Result<Self> {
        let project = project.as_ref().to_path_buf();
        let name = name.into();
        let (phase, cont) = store.plan(self.agent, &project, &name, fork)?;
        self.cont = cont;
        self.binding = Some(Binding {
            store: store.clone(),
            project,
            name,
            phase,
        });
        // A named session needs an id back, so it selects the format that
        // carries one unless the caller pinned a format explicitly.
        if self.format.is_none() {
            self.format = self.agent.session_format();
        }
        Ok(self)
    }

    /// The format this request will actually use.
    #[must_use]
    pub fn effective_format(&self) -> Format {
        self.format.unwrap_or_default()
    }

    /// Whether this turn opens, continues, or branches its named session.
    /// `None` when the request is not attached to one.
    ///
    /// Known before the run starts, so a UI can label the turn up front.
    #[must_use]
    pub fn session_phase(&self) -> Option<Phase> {
        self.binding.as_ref().map(|b| b.phase)
    }

    /// Freeze the request into the [`Plan`] an argv is built from.
    #[must_use]
    pub fn plan(&self) -> Plan {
        Plan {
            bin: self
                .bin
                .clone()
                .unwrap_or_else(|| self.agent.bin().to_string()),
            prompt: self.prompt.clone(),
            system: self.system.clone(),
            model: self.model.clone(),
            permission: self.permission,
            format: self.effective_format(),
            cont: self.cont.clone(),
            // A prompt too large for the argv is piped instead, so a long one
            // never fails with E2BIG.
            stdin_prompt: self.prompt.len() >= STDIN_THRESHOLD,
        }
    }

    /// The full command line, for logging or for showing a user exactly what
    /// will run before they approve it.
    ///
    /// # Errors
    /// [`crate::Error::Unsupported`] if the agent cannot honour this request.
    pub fn argv(&self) -> Result<Vec<String>> {
        let mut argv = self.agent.argv(&self.plan())?;
        argv.extend(self.extra_args.iter().cloned());
        Ok(argv)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_read_only_and_structured() {
        let request = Request::new(Agent::Claude, "hi");
        assert_eq!(request.permission, Permission::ReadOnly);
        assert_eq!(request.effective_format(), Format::Json);
        let argv = request.argv().unwrap();
        assert!(argv.contains(&"--disallowedTools".to_string()));
    }

    #[test]
    fn extra_args_land_after_everything_the_crate_builds() {
        let argv = Request::new(Agent::Claude, "hi")
            .args(["--add-dir", "/tmp/extra"])
            .argv()
            .unwrap();
        assert_eq!(argv[argv.len() - 2..], ["--add-dir", "/tmp/extra"]);
    }

    #[test]
    fn a_large_prompt_moves_to_stdin() {
        let big = "x".repeat(STDIN_THRESHOLD + 1);
        let plan = Request::new(Agent::Claude, big.clone()).plan();
        assert!(plan.stdin_prompt);
        let argv = Request::new(Agent::Claude, big).argv().unwrap();
        assert!(
            !argv.iter().any(|a| a.len() > STDIN_THRESHOLD),
            "a large prompt must not ride the argv"
        );
    }

    #[test]
    fn a_small_prompt_stays_on_the_argv() {
        assert!(!Request::new(Agent::Claude, "hi").plan().stdin_prompt);
    }

    #[test]
    fn a_named_session_selects_a_format_that_carries_an_id() {
        let dir = std::env::temp_dir().join(format!("aa-req-{}", std::process::id()));
        let store = SessionStore::open(&dir);
        let request = Request::new(Agent::Claude, "hi")
            .session(&store, "/proj", "chat", false)
            .unwrap();
        assert_eq!(request.effective_format(), Format::Json);

        // An explicit format is respected over the automatic upgrade.
        let pinned = Request::new(Agent::Claude, "hi")
            .format(Format::Stream)
            .session(&store, "/proj", "chat2", false)
            .unwrap();
        assert_eq!(pinned.effective_format(), Format::Stream);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn resume_bypasses_the_store() {
        let argv = Request::new(Agent::Claude, "hi")
            .resume("sess-9")
            .argv()
            .unwrap();
        let at = argv.iter().position(|a| a == "--resume").unwrap();
        assert_eq!(argv[at + 1], "sess-9");
    }
}
