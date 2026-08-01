//! Describing a run before it happens.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::agent::{
    Agent, Continue, EnvPolicy, Format, MAX_COMMAND_LINE, Permission, Plan, STDIN_THRESHOLD,
};
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
    pub(crate) effort: Option<String>,
    pub(crate) duplex: bool,
    pub(crate) approvals: bool,
    pub(crate) permission: Permission,
    pub(crate) format: Option<Format>,
    pub(crate) cont: Continue,
    pub(crate) cwd: Option<PathBuf>,
    /// Additional working roots the agent may write beside `cwd`.
    pub(crate) extra_dirs: Vec<PathBuf>,
    pub(crate) env: Vec<(String, String)>,
    pub(crate) extra_args: Vec<String>,
    pub(crate) env_policy: EnvPolicy,
    pub(crate) schema: Option<String>,
    /// Set by the runner for agents that read the schema from a file.
    pub(crate) schema_file: Option<String>,
    pub(crate) timeout: Option<Duration>,
    /// Set when [`Request::session`] resolved a named session, so the runner
    /// knows to write the binding back.
    pub(crate) binding: Option<Binding>,
    /// Set by [`Request::command`]: the prompt is a slash command, so the
    /// capability check refuses agents that have no command vocabulary.
    pub(crate) is_command: bool,
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
    /// Defaults are deliberately conservative: [`Permission::ReadOnly`],
    /// [`EnvPolicy::Minimal`], and the agent's structured output format. Widen
    /// them explicitly.
    pub fn new(agent: Agent, prompt: impl Into<String>) -> Self {
        Self {
            agent,
            bin: None,
            prompt: prompt.into(),
            system: None,
            model: None,
            effort: None,
            duplex: false,
            approvals: false,
            permission: Permission::ReadOnly,
            format: None,
            cont: Continue::New,
            cwd: None,
            extra_dirs: Vec::new(),
            env: Vec::new(),
            extra_args: Vec::new(),
            env_policy: EnvPolicy::Minimal,
            schema: None,
            schema_file: None,
            timeout: None,
            binding: None,
            is_command: false,
        }
    }

    /// A run that carries a slash command instead of a prompt.
    ///
    /// The agent's own verbs, addressed as values: `/compact` summarises a
    /// conversation that has grown too long to think in, `/clear` discards it.
    /// See [`crate::Command`].
    ///
    /// Pair it with [`Request::session`] or [`Request::resume`]. A command with
    /// no conversation behind it has nothing to act on: `/compact` on a fresh
    /// session is refused, and says so.
    ///
    /// # A command is a turn, not an interruption
    ///
    /// Deliberately a constructor rather than something [`crate::Run::send`]
    /// delivers mid-turn. Verified against claude 2.1.212: a command injected
    /// into a running turn emits its own `result` record *after* the turn's,
    /// which overwrites the outcome — the answer's text becomes the
    /// compaction's empty string and the turn's usage becomes the compaction's
    /// zeroes. As its own run the same command produces one clean terminal.
    ///
    /// # Reading the result
    ///
    /// The outcome's text is empty and `num_turns` is zero, because a
    /// compaction generates no answer. Neither is a failure, and neither is a
    /// refusal: [`crate::Event::Compaction`] carries whether it worked, so this
    /// wants [`crate::stream`] rather than [`crate::run`].
    ///
    /// Claude only. No other agent has a command vocabulary, so both refuse
    /// before spawning.
    #[must_use]
    pub fn command(agent: Agent, command: &crate::Command) -> Self {
        let mut request = Self::new(agent, command.wire());
        request.is_command = true;
        request
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

    /// Set the reasoning effort level.
    ///
    /// Passed through verbatim, exactly like [`Request::model`] and for the same
    /// reason: the accepted set belongs to the provider, differs between agents,
    /// and has already grown once. [`crate::Model::efforts`] lists what each
    /// model is known to take, and nothing here validates against it.
    ///
    /// Delivered as `--effort` on Claude and Copilot, and as
    /// `-c model_reasoning_effort=<level>` on Codex, which has no flag for it.
    #[must_use]
    pub fn effort(mut self, effort: impl Into<String>) -> Self {
        self.effort = Some(effort.into());
        self
    }

    /// Keep the input channel open for the turn, so the caller can send more.
    ///
    /// Without this a run takes one prompt and that is the whole conversation.
    /// With it, [`crate::Run::send`] delivers another message while the agent is
    /// still working, which is what lets a chat UI accept a correction the
    /// moment a user types it rather than making them wait for the turn to end.
    ///
    /// The agent takes the message at its next step boundary, not mid-token.
    /// Verified against claude 2.1.212 and codex-cli 0.145.0.
    ///
    /// Claude and Codex support this. Codex switches from `exec` to app-server
    /// for the interactive turn. Copilot is [`crate::Error::Unsupported`].
    #[must_use]
    pub fn interactive(mut self) -> Self {
        self.duplex = true;
        self
    }

    /// Route gated tool calls to the caller for a decision, instead of letting
    /// the posture answer them.
    ///
    /// Every [`Permission`] resolves the approval question up front, which is
    /// what lets a headless run finish unattended. This asks instead: a gated
    /// call arrives as [`crate::Event::ApprovalRequest`] and the run waits,
    /// mid-turn, until [`crate::Run::respond`] answers it.
    ///
    /// Two constraints, both raised before spawning rather than met as a hang:
    /// this needs [`crate::stream`], since [`crate::run`] yields no events for
    /// anyone to answer. Claude and Codex expose approval callbacks; Copilot
    /// does not and is [`crate::Error::Unsupported`].
    ///
    /// [`Permission`] still applies to everything the agent does not ask about.
    /// Agents may allow read-only commands without asking, so the absence of a
    /// question is not proof that nothing ran.
    #[must_use]
    pub fn approvals(mut self) -> Self {
        self.approvals = true;
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

    /// Add another working root beside [`Request::cwd`].
    ///
    /// Claude and `codex exec` receive `--add-dir`. Interactive Codex runs use
    /// the same path as an app-server runtime root and workspace-write root, so
    /// the access described here survives transport changes without a caller
    /// assembling provider-specific arguments.
    #[must_use]
    pub fn add_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.extra_dirs.push(dir.into());
        self
    }

    /// Set an environment variable for the child. Repeatable.
    #[must_use]
    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }

    /// Choose which of the host's environment variables reach the agent.
    ///
    /// Defaults to [`EnvPolicy::Minimal`], which passes through only what the
    /// selected agent needs. Reach for [`EnvPolicy::Inherit`] when the host
    /// holds nothing the agent should not see, or when something this crate
    /// does not know about has to reach the CLI.
    ///
    /// ```no_run
    /// # use agent_abstraction::{Agent, EnvPolicy, Request};
    /// let request = Request::new(Agent::Claude, "review this")
    ///     .env_policy(EnvPolicy::Inherit);
    /// ```
    #[must_use]
    pub fn env_policy(mut self, policy: EnvPolicy) -> Self {
        self.env_policy = policy;
        self
    }

    /// Kill the run if it has not finished within `timeout`.
    #[must_use]
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// Append raw arguments after everything this crate builds.
    ///
    /// The escape hatch for agent-specific flags with no unified spelling.
    ///
    /// **This voids the crate's guarantees.** Arguments land after the generated
    /// ones, so they can contradict [`Request::permission`], redirect the output
    /// format the parser expects, or point the run at a different session.
    /// Codex's `-c key=value` in particular can rewrite sandbox and approval
    /// policy for the invocation. Nothing here is validated, and a security
    /// review of the permission posture means little without also reviewing
    /// whatever is passed here.
    ///
    /// Arguments are passed straight to the binary without a shell.
    #[must_use]
    pub fn unchecked_args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.extra_args.extend(args.into_iter().map(Into::into));
        self
    }

    /// Constrain the answer to a JSON Schema.
    ///
    /// The agent is asked to return a value conforming to `schema`, which
    /// [`Outcome::structured`] then carries already parsed. Useful when the
    /// answer is data rather than prose: a set of review findings, an
    /// extraction, a classification. Reading it beats parsing prose, which is
    /// a guess about formatting the model never promised.
    ///
    /// The two CLIs that support this take it differently, and the difference
    /// is hidden: Claude accepts the schema inline, Codex reads it from a file
    /// this crate writes for the run and removes afterwards. **Copilot 1.0.75
    /// has no schema support**, so asking is [`crate::Error::Unsupported`]
    /// rather than a prose answer presented as data.
    ///
    /// The schema is passed through unvalidated; a malformed one surfaces as
    /// the agent's own error.
    ///
    /// # Write the schema strictly
    ///
    /// Codex sends it to `OpenAI`'s structured-output API, which rejects anything
    /// permissive. Every object needs `"additionalProperties": false` and every
    /// property listed in `required`, or the request fails with a 400 before
    /// the model runs:
    ///
    /// ```text
    /// 'additionalProperties' is required to be supplied and to be false
    /// ```
    ///
    /// Claude is more forgiving, so a schema that works there can still fail on
    /// Codex. Writing to the stricter rule keeps one schema usable for both.
    ///
    /// [`Outcome::structured`]: crate::Outcome::structured
    #[must_use]
    pub fn schema(mut self, schema: impl Into<String>) -> Self {
        self.schema = Some(schema.into());
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
        // A named session needs an id back. The default format carries one, so
        // this only has to refuse a format the caller pinned that cannot:
        // otherwise the run would succeed and then silently fail to bind.
        //
        // Deliberately no longer *sets* the format. Doing so overrode the
        // caller's streaming intent, which is how a named session, the case a
        // chat UI always uses, ended up unable to stream.
        if let Some(format) = self.format
            && !self.agent.format_carries_session(format)
        {
            return Err(crate::Error::Unsupported {
                agent: self.agent,
                what: "a named session under an output format that carries no session id",
            });
        }
        Ok(self)
    }

    /// Roughly how many bytes of command line this request needs.
    ///
    /// Only the caller-supplied text is counted; the flags themselves are a
    /// bounded handful of short literals. Used to decide whether the prompt has
    /// to move to stdin.
    fn argv_weight(&self) -> usize {
        self.prompt.len()
            + self.system.as_ref().map_or(0, String::len)
            + self.extra_args.iter().map(String::len).sum::<usize>()
            // Claude's schema rides the command line too.
            + self.schema.as_ref().map_or(0, String::len)
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
    ///
    /// Crate-internal: `Plan` is how the crate works, not what it promises, and
    /// a caller that wants to see the command line should use
    /// [`Request::argv`].
    #[must_use]
    pub(crate) fn plan(&self) -> Plan {
        Plan {
            bin: self
                .bin
                .clone()
                .unwrap_or_else(|| self.agent.bin().to_string()),
            prompt: self.prompt.clone(),
            system: self.system.clone(),
            model: self.model.clone(),
            effort: self.effort.clone(),
            duplex: self.duplex || self.approvals,
            approvals: self.approvals,
            permission: self.permission,
            format: self.effective_format(),
            cont: self.cont.clone(),
            extra_dirs: self
                .extra_dirs
                .iter()
                .map(|path| path.to_string_lossy().into_owned())
                .collect(),
            // Measure the whole command line, not just the prompt: for Codex
            // and Copilot the system text is prepended to it, and for Claude the
            // system prompt rides its own argument. A small prompt with a large
            // system prompt would otherwise still hit E2BIG.
            stdin_prompt: self.argv_weight() >= STDIN_THRESHOLD,
            schema: self.schema.clone(),
            schema_file: self.schema_file.clone(),
            is_command: self.is_command,
        }
    }

    /// The full command line, for logging or for showing a user exactly what
    /// will run before they approve it.
    ///
    /// # Errors
    /// [`crate::Error::Unsupported`] if the agent cannot honour this request.
    pub fn argv(&self) -> Result<Vec<String>> {
        Ok(self
            .typed_argv()?
            .into_iter()
            .map(|arg| arg.value)
            .collect())
    }

    /// The command line with per-argument sensitivity, the single source both
    /// the executable and the redacted forms are derived from.
    pub(crate) fn typed_argv(&self) -> Result<Vec<crate::agent::Arg>> {
        use crate::agent::{Arg, Sensitivity};

        let plan = self.plan();
        let mut argv = self.agent.typed_argv(&plan)?;
        // Raw arguments have no known shape, so they are assumed to carry
        // secrets rather than assumed not to.
        argv.extend(self.extra_args.iter().map(|value| Arg {
            value: value.clone(),
            sensitivity: Sensitivity::Unchecked,
        }));

        // Moving the prompt to stdin does not move anything else: Claude keeps
        // the system prompt on its own argument, and raw arguments are always on
        // the line, so a small prompt with a large system prompt still
        // overflows. Name the culprit rather than letting the OS answer E2BIG.
        let total: usize = argv.iter().map(|a| a.value.len()).sum();
        if total > MAX_COMMAND_LINE {
            let system = self.system.as_ref().map_or(0, String::len);
            let extra: usize = self.extra_args.iter().map(String::len).sum();
            let prompt = if plan.stdin_prompt {
                0
            } else {
                self.prompt.len()
            };
            let (what, size) = if system >= extra && system >= prompt {
                ("the system prompt", system)
            } else if extra >= prompt {
                ("the unchecked arguments", extra)
            } else {
                ("the prompt", prompt)
            };
            return Err(crate::Error::CommandLineTooLarge {
                agent: self.agent,
                what,
                size,
                limit: MAX_COMMAND_LINE,
            });
        }
        Ok(argv)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The defaults are the safe posture, so a caller who configures nothing
    /// does not get the permissive one by accident.
    #[test]
    fn defaults_are_read_only_isolated_and_structured() {
        let request = Request::new(Agent::Claude, "hi");
        assert_eq!(request.permission, Permission::ReadOnly);
        assert_eq!(
            request.env_policy,
            EnvPolicy::Minimal,
            "full environment inheritance must be an explicit decision"
        );
        assert_eq!(
            request.effective_format(),
            Format::Stream,
            "the default must be watchable: Json reports nothing until the turn ends"
        );
        let argv = request.argv().unwrap();
        assert!(argv.contains(&"--disallowedTools".to_string()));
    }

    #[test]
    fn extra_args_land_after_everything_the_crate_builds() {
        let argv = Request::new(Agent::Claude, "hi")
            .unchecked_args(["--add-dir", "/tmp/extra"])
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

    /// Moving the prompt to stdin does not move the system prompt, so a small
    /// prompt with a huge system prompt still overflows the command line. The
    /// OS would answer `E2BIG` naming nothing; this names the culprit.
    #[test]
    fn an_oversized_system_prompt_is_reported_rather_than_left_to_e2big() {
        let err = Request::new(Agent::Claude, "tiny")
            .system("s".repeat(MAX_COMMAND_LINE + 1))
            .argv()
            .unwrap_err();
        let crate::Error::CommandLineTooLarge { what, .. } = err else {
            panic!("expected CommandLineTooLarge, got {err:?}")
        };
        assert_eq!(what, "the system prompt");
    }

    #[test]
    fn oversized_unchecked_arguments_are_named_too() {
        let err = Request::new(Agent::Claude, "tiny")
            .unchecked_args([format!("--x={}", "y".repeat(MAX_COMMAND_LINE))])
            .argv()
            .unwrap_err();
        assert!(matches!(
            err,
            crate::Error::CommandLineTooLarge {
                what: "the unchecked arguments",
                ..
            }
        ));
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
        assert_eq!(
            request.effective_format(),
            Format::Stream,
            "the default must be watchable: Json reports nothing until the turn ends"
        );

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
