//! The three agents, what each can do, and how a request becomes an argv.
//!
//! Everything here is pure: [`Agent::argv`] builds a command line from a
//! [`Plan`] without touching the filesystem, the clock, or a process, so every
//! flag mapping is covered by an ordinary unit test. Spawning lives in
//! [`crate::run`].

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// A coding agent this crate can drive headlessly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Agent {
    /// Anthropic's Claude Code (`claude`).
    Claude,
    /// The `OpenAI` Codex CLI (`codex`).
    Codex,
    /// GitHub Copilot CLI (`copilot`).
    Copilot,
}

/// How an agent's native session id is obtained. This is the axis deciding whether
/// a caller-owned session name can be bound to it at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionSupport {
    /// The caller assigns the id up front (`claude --session-id <uuid>`), so the
    /// binding is known before the process starts and survives a crashed run.
    Minted,
    /// The agent prints an id we read back out of its output (Codex's
    /// `thread_id`). The binding only exists once the run produced output.
    Printed,
    /// No id is exposed headlessly. Named sessions are refused for this agent.
    None,
}

/// What an agent supports. Used to reject an impossible request before spawning
/// rather than silently doing something weaker than asked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Caps {
    /// How a native session id is obtained, if at all.
    pub session: SessionSupport,
    /// Whether resuming can branch a new session instead of appending in place.
    pub fork: bool,
    /// Whether the agent emits a structured event stream this crate normalizes.
    pub events: bool,
    /// Whether the agent takes a real system-prompt flag. When false the system
    /// text is prepended to the prompt so it still reaches the model.
    pub native_system: bool,
}

/// Permission posture for a run, mapped onto each agent's own vocabulary.
///
/// # What these do and do not guarantee
///
/// These postures constrain each CLI's **built-in** tools: its shell, its file
/// writes, its sandbox. They do **not** constrain MCP servers, plugins or custom
/// tools the agent is configured with. An MCP tool that files an issue, writes
/// to a database or calls a deployment API is a separate tool category in all
/// three CLIs and can still act during a nominally restricted run.
///
/// If a run must not cause remote side effects, the containment has to come from
/// the agent's own configuration (which MCP servers are enabled at all), not
/// from this enum. What is selected here is enforced by the CLI, and what the
/// CLI does not model cannot be enforced from out here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Permission {
    /// No writes to the local filesystem, and no shell where the CLI can gate
    /// one.
    ///
    /// The strongest posture this crate can express, and still not a guarantee
    /// of "no side effects": see the type-level note about MCP tools. Codex
    /// enforces it with a read-only sandbox, which blocks writes but still
    /// permits command execution.
    #[default]
    ReadOnly,
    /// Ask the agent to plan rather than act.
    ///
    /// Claude and Copilot have a real plan mode. **Codex has none**, so this
    /// maps to its read-only sandbox: writes are blocked, but the model is not
    /// instructed to withhold execution the way a true plan mode would.
    Plan,
    /// Allow file edits, while still gating shell commands where the CLI can.
    Edit,
    /// Allow the agent's own default automation.
    Auto,
    /// Skip every permission check. For sandboxes.
    Bypass,
}

/// Environment variables that route an agent's traffic through a corporate
/// proxy or a custom certificate authority.
///
/// None of the three vendors documents proxy support, and none exposes a proxy
/// flag, so this is a convenience list of names a host may want to forward, not
/// a claim that forwarding them works. (The names do appear in all three
/// shipped binaries, but that shows they are referenced, not that provider
/// traffic honours them.) Verify against your own proxy before relying on it.
///
/// Not included in [`EnvPolicy::Minimal`]: they are situational, and the proxy
/// URLs frequently carry credentials. Offered here so a host can present them
/// as an explicit setting and forward the ones it wants with
/// [`crate::Request::env`], rather than every caller rediscovering the names.
///
/// Excluding them from `Minimal` does not block them. Under the default
/// [`EnvPolicy::Inherit`] they flow exactly as they would for the CLI run from a
/// shell; the only thing `Minimal` changes is that forwarding becomes a
/// decision rather than an accident.
///
/// ```no_run
/// # use agent_abstraction::{Agent, EnvPolicy, NETWORK_ENV, Request};
/// let mut request = Request::new(Agent::Claude, "hi").env_policy(EnvPolicy::Minimal);
/// // Forward only the proxy settings this host actually has.
/// for name in NETWORK_ENV {
///     if let Ok(value) = std::env::var(name) {
///         request = request.env(*name, value);
///     }
/// }
/// ```
pub const NETWORK_ENV: &[&str] = &[
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
    "NO_PROXY",
    "http_proxy",
    "https_proxy",
    "all_proxy",
    "no_proxy",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    "NODE_EXTRA_CA_CERTS",
];

/// Which of the host's environment variables reach the agent.
///
/// The default is [`EnvPolicy::Inherit`], matching how a CLI behaves when run
/// from a shell. In an embedded host that also hands the agent, and every
/// command it runs, whatever unrelated secrets the process happens to hold:
/// cloud credentials, CI tokens, database URLs.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum EnvPolicy {
    /// Pass the whole parent environment through.
    #[default]
    Inherit,
    /// Pass through only what the selected agent needs, per
    /// [`Agent::essential_env`], plus anything set with [`crate::Request::env`].
    ///
    /// The crate owns this list rather than the caller, because "what does this
    /// CLI need to work" is knowledge about the agent, and an incomplete
    /// hand-written list produces a run that fails in a way that looks like an
    /// auth problem.
    Minimal,
    /// Pass through only these names, plus anything set with
    /// [`crate::Request::env`]. Names unset in the parent are skipped.
    Only(Vec<String>),
}

/// Output shape requested from the agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Format {
    /// Plain prose on stdout. Carries no session id and no events.
    Text,
    /// One JSON result document.
    #[default]
    Json,
    /// A JSONL event stream, normalized into [`crate::Event`]s.
    Stream,
}

/// How a run continues an earlier conversation.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Continue {
    /// Start a fresh conversation.
    #[default]
    New,
    /// Start a fresh conversation under an id the caller chose. Only valid for
    /// [`SessionSupport::Minted`] agents.
    NewWith(String),
    /// Append to an existing conversation in place.
    Resume(String),
    /// Branch a new conversation off an existing one, leaving it untouched.
    Fork(String),
}

/// A fully resolved run request, ready to become an argv. Built by
/// [`crate::Request::plan`]; consumed by [`Agent::argv`].
#[derive(Debug, Clone)]
pub struct Plan {
    /// The binary to invoke.
    pub bin: String,
    /// The user prompt.
    pub prompt: String,
    /// System prompt, if any.
    pub system: Option<String>,
    /// Model id or alias, if pinned.
    pub model: Option<String>,
    /// Permission posture.
    pub permission: Permission,
    /// Requested output shape.
    pub format: Format,
    /// How this run continues an earlier one.
    pub cont: Continue,
    /// True when the prompt is piped on stdin instead of riding the argv.
    pub stdin_prompt: bool,
}

/// Prompts at or above this many bytes are piped on stdin rather than placed on
/// the argv. Well under the ~1 MiB `ARG_MAX` floor on macOS, with room for the
/// rest of the command line and the inherited environment.
pub(crate) const STDIN_THRESHOLD: usize = 128 * 1024;

impl Agent {
    /// Every agent, in a stable order.
    pub const ALL: [Agent; 3] = [Agent::Claude, Agent::Codex, Agent::Copilot];

    /// The stable identifier used in session records and logs.
    #[must_use]
    pub fn id(self) -> &'static str {
        match self {
            Agent::Claude => "claude-code",
            Agent::Codex => "codex",
            Agent::Copilot => "copilot",
        }
    }

    /// The default binary name looked up on `PATH`.
    #[must_use]
    pub fn bin(self) -> &'static str {
        match self {
            Agent::Claude => "claude",
            Agent::Codex => "codex",
            Agent::Copilot => "copilot",
        }
    }

    /// The documented install command, surfaced by [`Error::NotInstalled`].
    #[must_use]
    pub fn install_hint(self) -> &'static str {
        match self {
            Agent::Claude => "npm install -g @anthropic-ai/claude-code",
            Agent::Codex => "npm install -g @openai/codex",
            Agent::Copilot => "npm install -g @github/copilot",
        }
    }

    /// The environment variables this agent needs to function, used by
    /// [`EnvPolicy::Minimal`].
    ///
    /// Two groups: what any process needs to start, and this agent's own
    /// credential and config variables. A name absent from the parent
    /// environment is skipped, so nothing here is fabricated.
    ///
    /// Proxy and custom-CA variables are deliberately **not** here. They are
    /// environment-specific rather than required, and `HTTP_PROXY` /
    /// `HTTPS_PROXY` routinely embed credentials (`http://user:pass@proxy`), so
    /// passing them automatically would leak one through the very policy meant
    /// to withhold secrets. A host that needs them should offer them as a
    /// setting and pass them with [`crate::Request::env`]; [`NETWORK_ENV`] names
    /// them so a settings screen does not have to hardcode the list.
    ///
    /// `PATH`, `HOME` and `USER` are the verified floor on macOS: all three CLIs
    /// answer correctly with exactly those set, and Claude reports "Not logged
    /// in" without `USER`, since its keychain lookup is keyed on it. The Windows
    /// names are included on the same reasoning but are **not** verified, as
    /// this crate has not been run there.
    #[must_use]
    pub fn essential_env(self) -> Vec<&'static str> {
        // Needed by any child process, plus the locale and temp dir the CLIs
        // use for scratch files.
        const BASE: &[&str] = &[
            "PATH", "HOME", "USER", "LOGNAME", "SHELL", "TMPDIR", "LANG", "LC_ALL",
        ];
        // Unverified: this crate has not been exercised on Windows.
        const WINDOWS: &[&str] = &[
            "USERPROFILE",
            "APPDATA",
            "LOCALAPPDATA",
            "SystemRoot",
            "SystemDrive",
            "TEMP",
            "TMP",
            "PATHEXT",
            "ComSpec",
        ];
        let agent: &[&str] = match self {
            Agent::Claude => &[
                "ANTHROPIC_API_KEY",
                "ANTHROPIC_AUTH_TOKEN",
                "ANTHROPIC_BASE_URL",
                "CLAUDE_CONFIG_DIR",
            ],
            Agent::Codex => &[
                "CODEX_HOME",
                "CODEX_API_KEY",
                "OPENAI_API_KEY",
                "OPENAI_BASE_URL",
            ],
            Agent::Copilot => &[
                "GH_TOKEN",
                "GITHUB_TOKEN",
                "COPILOT_ALLOW_ALL",
                "XDG_CONFIG_HOME",
            ],
        };
        BASE.iter().chain(WINDOWS).chain(agent).copied().collect()
    }

    /// What this agent supports.
    #[must_use]
    pub fn caps(self) -> Caps {
        match self {
            // Verified against claude 2.1.205: `--session-id <uuid>` assigns the
            // id, `--fork-session` branches, `--output-format stream-json`
            // streams (and demands `--verbose`), `--append-system-prompt` is a
            // real flag.
            Agent::Claude => Caps {
                session: SessionSupport::Minted,
                fork: true,
                events: true,
                native_system: true,
            },
            // `codex exec --json` emits `thread_id`; continuation is the
            // `resume` subcommand and is linear (`codex fork` is TUI-only).
            Agent::Codex => Caps {
                session: SessionSupport::Printed,
                fork: false,
                events: true,
                native_system: false,
            },
            // Verified against Copilot CLI 1.0.75: `--session-id <uuid>` both
            // mints a new session and resumes an existing one (one flag, both
            // directions), and `--output-format json` is a JSONL event stream.
            // There is no headless fork.
            Agent::Copilot => Caps {
                session: SessionSupport::Minted,
                fork: false,
                events: true,
                native_system: false,
            },
        }
    }

    /// The format that can carry this agent's session id, if any. A named
    /// session upgrades to this when the caller did not pin a format.
    #[must_use]
    pub fn session_format(self) -> Option<Format> {
        match self.caps().session {
            // Claude reports the id in both structured formats; `Json` is the
            // cheaper default when the caller did not ask to stream.
            SessionSupport::Minted | SessionSupport::Printed => Some(match self {
                Agent::Claude => Format::Json,
                // `--json` IS Codex's stream and Copilot's `json` is JSONL;
                // neither has a single-document form.
                Agent::Codex | Agent::Copilot => Format::Stream,
            }),
            SessionSupport::None => None,
        }
    }

    /// Whether `format` can carry this agent's session id.
    ///
    /// Distinct from [`Agent::session_format`], which names the *preferred* one:
    /// Claude reports its id under both `Json` and `Stream`, and only plain text
    /// loses it. A named session needs this, not equality with the preferred
    /// format, or streaming a named Claude session would be refused for no
    /// reason.
    #[must_use]
    pub fn format_carries_session(self, format: Format) -> bool {
        self.session_format().is_some() && format != Format::Text
    }

    /// Reject a plan this agent cannot honour, before anything is spawned.
    fn check(self, plan: &Plan) -> Result<()> {
        let caps = self.caps();
        if matches!(plan.cont, Continue::Fork(_)) && !caps.fork {
            return Err(Error::Unsupported {
                agent: self,
                what: "forking a session headlessly",
            });
        }
        if matches!(plan.cont, Continue::NewWith(_)) && caps.session != SessionSupport::Minted {
            return Err(Error::Unsupported {
                agent: self,
                what: "assigning a session id up front",
            });
        }
        if plan.format == Format::Stream && !caps.events {
            return Err(Error::Unsupported {
                agent: self,
                what: "a structured event stream",
            });
        }
        Ok(())
    }

    /// Build the command line for `plan`.
    ///
    /// The first element is the binary; the rest are its arguments. Returns
    /// [`Error::Unsupported`] when the plan asks for a capability this agent
    /// lacks, never a quiet downgrade.
    ///
    /// # Errors
    /// [`Error::Unsupported`] if the plan needs a capability this agent lacks.
    pub fn argv(self, plan: &Plan) -> Result<Vec<String>> {
        self.check(plan)?;
        Ok(match self {
            Agent::Claude => argv_claude(plan),
            Agent::Codex => argv_codex(plan),
            Agent::Copilot => argv_copilot(plan),
        })
    }

    /// The prompt text actually delivered, with the system prompt folded in for
    /// agents that have no flag for it. Never dropped silently.
    #[must_use]
    pub fn effective_prompt(self, plan: &Plan) -> String {
        match (&plan.system, self.caps().native_system) {
            (Some(system), false) => format!("{system}\n\n{}", plan.prompt),
            _ => plan.prompt.clone(),
        }
    }
}

impl fmt::Display for Agent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.id())
    }
}

/// Builds an argv, keeping every flag name literal at its call site so the flag
/// list for an agent stays greppable and auditable against `--help`.
struct Argv(Vec<String>);

impl Argv {
    /// Start with the binary.
    fn new(bin: &str) -> Self {
        Self(vec![bin.to_string()])
    }

    /// A bare flag with no value.
    fn bare(&mut self, flag: &str) -> &mut Self {
        self.0.push(flag.to_string());
        self
    }

    /// A flag and its value, as two arguments.
    fn pair(&mut self, flag: &str, value: impl AsRef<str>) -> &mut Self {
        self.0.push(flag.to_string());
        self.0.push(value.as_ref().to_string());
        self
    }

    /// A flag and its value, only when the value is present.
    fn opt(&mut self, flag: &str, value: Option<&String>) -> &mut Self {
        if let Some(value) = value {
            self.pair(flag, value);
        }
        self
    }

    /// A positional argument.
    fn arg(&mut self, value: impl Into<String>) -> &mut Self {
        self.0.push(value.into());
        self
    }

    fn done(&mut self) -> Vec<String> {
        std::mem::take(&mut self.0)
    }
}

/// Claude Code's permission-mode token for each posture. Choices verified from
/// `claude --help` (2.1.205): acceptEdits, auto, bypassPermissions, manual,
/// dontAsk, plan.
fn claude_mode(p: Permission) -> &'static str {
    match p {
        // `dontAsk` auto-denies gated tools and keeps going rather than
        // blocking on a prompt no one can answer headlessly. The read-only
        // guarantee comes from `--disallowedTools`, below.
        Permission::ReadOnly => "dontAsk",
        Permission::Plan => "plan",
        Permission::Edit => "acceptEdits",
        Permission::Auto => "auto",
        Permission::Bypass => "bypassPermissions",
    }
}

/// `claude -p <prompt> --permission-mode M --output-format F [...]`
fn argv_claude(plan: &Plan) -> Vec<String> {
    let mut a = Argv::new(&plan.bin);
    a.bare("-p");
    if plan.stdin_prompt {
        // With `--input-format text` claude reads the prompt from stdin, so a
        // large prompt never has to fit on the argv.
        a.pair("--input-format", "text");
    } else {
        a.arg(Agent::Claude.effective_prompt(plan));
    }

    a.pair("--permission-mode", claude_mode(plan.permission));
    if plan.permission == Permission::ReadOnly {
        // Remove the mutating built-ins outright. Reads still run via
        // Read/Grep/Glob. `mcp__*` covers every MCP tool: denying only the
        // built-in writers would leave an MCP server free to mutate remote
        // state during a run the caller asked to be read-only.
        a.bare("--disallowedTools");
        for tool in ["Bash", "Edit", "Write", "NotebookEdit", "mcp__*"] {
            a.arg(tool);
        }
    }

    a.opt("--model", plan.model.as_ref());
    a.opt("--append-system-prompt", plan.system.as_ref());

    match &plan.cont {
        Continue::New => {}
        Continue::NewWith(id) => {
            a.pair("--session-id", id);
        }
        Continue::Resume(id) => {
            a.pair("--resume", id);
        }
        Continue::Fork(id) => {
            // Mints a new id off `id`, leaving the original and its cached
            // prefix untouched. The new id comes back in the output.
            a.pair("--resume", id).bare("--fork-session");
        }
    }

    a.pair(
        "--output-format",
        match plan.format {
            Format::Text => "text",
            Format::Json => "json",
            Format::Stream => "stream-json",
        },
    );
    if plan.format == Format::Stream {
        // Claude refuses `-p --output-format stream-json` without it:
        // "--print with --output-format=stream-json requires --verbose".
        a.bare("--verbose");
    }
    a.done()
}

/// `codex exec [resume <id>] --skip-git-repo-check [sandbox flags] [--model M]
/// [--json] <prompt>`
fn argv_codex(plan: &Plan) -> Vec<String> {
    let mut a = Argv::new(&plan.bin);
    a.bare("exec");
    if let Continue::Resume(id) = &plan.cont {
        // Continuation is a subcommand, not a flag.
        a.bare("resume").arg(id.clone());
    }

    // `codex exec` aborts outside a git repository unless told not to. That
    // check guards against an agent editing files with no way to undo them, but
    // this crate is embedded in hosts that legitimately run against scratch
    // directories, worktrees and review checkouts, and a hard abort there is
    // useless to them. The real containment is the sandbox below, which is
    // `read-only` by default, so nothing is unrecoverable regardless.
    a.bare("--skip-git-repo-check");

    // `codex exec` takes `--sandbox`, but `codex exec resume` does **not**: it
    // rejects the flag outright and takes the same setting as a `-c` config
    // override instead. Verified against codex-cli 0.145.0, where passing
    // `--sandbox` to a resume fails with "unexpected argument '--sandbox'".
    // Dropping the sandbox on resume would silently run a continued turn under a
    // different posture than the caller asked for.
    let resuming = matches!(plan.cont, Continue::Resume(_));
    let sandbox = match plan.permission {
        Permission::Bypass => None,
        Permission::ReadOnly | Permission::Plan => Some("read-only"),
        Permission::Edit | Permission::Auto => Some("workspace-write"),
    };
    match (sandbox, resuming) {
        (None, _) => a.bare("--dangerously-bypass-approvals-and-sandbox"),
        (Some(mode), false) => a.pair("--sandbox", mode),
        // The value is TOML-parsed, falling back to a raw string, so the bare
        // token is read as the mode name.
        (Some(mode), true) => a.pair("-c", format!("sandbox_mode={mode}")),
    };

    a.opt("--model", plan.model.as_ref());
    // `--json` is Codex's event stream and the only place `thread_id` appears.
    if plan.format != Format::Text {
        a.bare("--json");
    }
    // Codex has no system flag, so the system text rides the prompt. A literal
    // `-` makes it read the prompt from stdin instead, keeping a large one off
    // the argv.
    a.arg(if plan.stdin_prompt {
        "-".to_string()
    } else {
        Agent::Codex.effective_prompt(plan)
    });
    a.done()
}

/// `copilot -p <prompt> --allow-all-tools [...] [--session-id <uuid>]`
///
/// Flags verified against Copilot CLI 1.0.75. Two of its conventions matter:
/// `--allow-all-tools` is *required* for non-interactive mode, and the
/// repeatable tool filters are declared `--allow-tool[=tools...]`, an optional
/// value, which only binds with `=`, never across a space.
fn argv_copilot(plan: &Plan) -> Vec<String> {
    // Copilot reads stdin as the prompt only when `-p` is absent: a `-p` value
    // makes the pipe be ignored. So a piped prompt drops the flag entirely.
    let mut a = Argv::new(&plan.bin);
    if !plan.stdin_prompt {
        a.pair("-p", Agent::Copilot.effective_prompt(plan));
    }

    // Without this, a headless run stops at the first tool confirmation.
    a.bare("--allow-all-tools").bare("--no-ask-user");
    match plan.permission {
        Permission::Bypass | Permission::Auto => a.bare("--allow-all-paths"),
        // Deny beats allow, so this is allow-all minus the mutating tools.
        // `--allow-all-paths` is deliberately NOT set: it disables path
        // verification entirely, which would widen filesystem reach in the one
        // posture that exists to narrow it.
        Permission::ReadOnly => a.bare("--deny-tool=shell").bare("--deny-tool=write"),
        // Edits run; shell stays denied so commands cannot.
        Permission::Edit => a.bare("--deny-tool=shell"),
        Permission::Plan => a.pair("--mode", "plan"),
    };

    a.opt("--model", plan.model.as_ref());
    // One flag serves both directions: it sets the UUID for a new session and
    // resumes an existing one by id.
    match &plan.cont {
        Continue::NewWith(id) | Continue::Resume(id) => {
            a.pair("--session-id", id);
        }
        // `Fork` is rejected by `Agent::check` before reaching here.
        Continue::New | Continue::Fork(_) => {}
    }

    a.pair(
        "--output-format",
        if plan.format == Format::Text {
            "text"
        } else {
            // Copilot's `json` is JSONL, so it serves both structured formats.
            "json"
        },
    );
    a.done()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(bin: &str) -> Plan {
        Plan {
            bin: bin.into(),
            prompt: "hi".into(),
            system: None,
            model: None,
            permission: Permission::ReadOnly,
            format: Format::Json,
            cont: Continue::New,
            stdin_prompt: false,
        }
    }

    fn argv(agent: Agent, plan: &Plan) -> Vec<String> {
        agent.argv(plan).expect("plan is supported")
    }

    fn pos(a: &[String], needle: &str) -> Option<usize> {
        a.iter().position(|s| s == needle)
    }

    #[test]
    fn claude_builds_print_mode_with_format_and_permission() {
        let a = argv(Agent::Claude, &plan("claude"));
        assert_eq!(a[0..3], ["claude", "-p", "hi"]);
        assert!(pos(&a, "--permission-mode").is_some());
        assert!(a.contains(&"dontAsk".to_string()));
        assert_eq!(a[pos(&a, "--output-format").unwrap() + 1], "json");
    }

    #[test]
    fn claude_read_only_removes_the_mutating_tools() {
        let a = argv(Agent::Claude, &plan("claude"));
        let at = pos(&a, "--disallowedTools").expect("read-only denies tools");
        assert_eq!(
            &a[at + 1..at + 5],
            ["Bash", "Edit", "Write", "NotebookEdit"]
        );
    }

    #[test]
    fn claude_bypass_does_not_deny_tools() {
        let mut p = plan("claude");
        p.permission = Permission::Bypass;
        let a = argv(Agent::Claude, &p);
        assert!(a.contains(&"bypassPermissions".to_string()));
        assert!(pos(&a, "--disallowedTools").is_none());
    }

    #[test]
    fn claude_stream_format_adds_verbose_but_json_does_not() {
        let mut p = plan("claude");
        p.format = Format::Stream;
        assert!(argv(Agent::Claude, &p).contains(&"--verbose".to_string()));
        p.format = Format::Json;
        assert!(!argv(Agent::Claude, &p).contains(&"--verbose".to_string()));
    }

    #[test]
    fn claude_mints_an_id_for_a_new_session_and_resumes_an_old_one() {
        let mut p = plan("claude");
        p.cont = Continue::NewWith("11111111-2222-3333-4444-555555555555".into());
        let a = argv(Agent::Claude, &p);
        assert_eq!(
            a[pos(&a, "--session-id").unwrap() + 1],
            "11111111-2222-3333-4444-555555555555"
        );
        assert!(pos(&a, "--resume").is_none());

        p.cont = Continue::Resume("sess-1".into());
        let a = argv(Agent::Claude, &p);
        assert_eq!(a[pos(&a, "--resume").unwrap() + 1], "sess-1");
        assert!(!a.contains(&"--fork-session".to_string()));
    }

    #[test]
    fn claude_fork_resumes_and_branches() {
        let mut p = plan("claude");
        p.cont = Continue::Fork("sess-1".into());
        let a = argv(Agent::Claude, &p);
        assert_eq!(a[pos(&a, "--resume").unwrap() + 1], "sess-1");
        assert!(a.contains(&"--fork-session".to_string()));
    }

    #[test]
    fn claude_keeps_the_system_prompt_on_its_own_flag() {
        let mut p = plan("claude");
        p.system = Some("be terse".into());
        let a = argv(Agent::Claude, &p);
        assert_eq!(
            a[pos(&a, "--append-system-prompt").unwrap() + 1],
            "be terse"
        );
        // The prompt itself stays clean.
        assert!(a.contains(&"hi".to_string()));
    }

    #[test]
    fn claude_stdin_prompt_leaves_the_argv() {
        let mut p = plan("claude");
        p.stdin_prompt = true;
        let a = argv(Agent::Claude, &p);
        assert_eq!(a[pos(&a, "--input-format").unwrap() + 1], "text");
        assert!(!a.contains(&"hi".to_string()), "prompt must not ride argv");
    }

    #[test]
    fn codex_resume_is_a_subcommand_and_prompt_is_last() {
        let mut p = plan("codex");
        p.cont = Continue::Resume("thread-9".into());
        let a = argv(Agent::Codex, &p);
        assert_eq!(a[0..4], ["codex", "exec", "resume", "thread-9"]);
        assert_eq!(a.last().unwrap(), "hi");
    }

    /// `Minimal` exists to withhold secrets, so nothing it passes through may
    /// be a credential carrier. Proxy URLs in particular routinely embed
    /// `user:pass`, which is why they are offered separately instead.
    #[test]
    fn the_minimal_environment_carries_no_proxy_variables() {
        for agent in Agent::ALL {
            let essential = agent.essential_env();
            for name in NETWORK_ENV {
                assert!(
                    !essential.contains(name),
                    "{agent} would pass {name} through EnvPolicy::Minimal"
                );
            }
        }
    }

    /// The floor verified live on macOS: with exactly these set, all three CLIs
    /// authenticate and answer. Claude reports "Not logged in" without `USER`.
    #[test]
    fn every_agent_asks_for_the_verified_floor() {
        for agent in Agent::ALL {
            let essential = agent.essential_env();
            for name in ["PATH", "HOME", "USER"] {
                assert!(essential.contains(&name), "{agent} omits {name}");
            }
        }
    }

    /// Each agent's own credentials, and nobody else's.
    #[test]
    fn agents_do_not_request_each_others_credentials() {
        let claude = Agent::Claude.essential_env();
        assert!(claude.contains(&"ANTHROPIC_API_KEY"));
        assert!(!claude.contains(&"OPENAI_API_KEY"));
        assert!(!claude.contains(&"GH_TOKEN"));

        let codex = Agent::Codex.essential_env();
        assert!(codex.contains(&"OPENAI_API_KEY"));
        assert!(!codex.contains(&"ANTHROPIC_API_KEY"));
    }

    /// The model is the caller's choice on every agent. It is forwarded
    /// verbatim and never defaulted, normalized, or validated here: a host with
    /// a model picker owns that list, and an unknown name must surface as the
    /// agent's own error rather than something this crate guessed at.
    #[test]
    fn every_agent_forwards_the_callers_model_verbatim() {
        for agent in Agent::ALL {
            let mut p = plan(agent.bin());
            // Deliberately not a real model id: nothing here may interpret it.
            p.model = Some("some-model-9".into());
            let a = argv(agent, &p);
            let at = pos(&a, "--model").unwrap_or_else(|| panic!("{agent} dropped --model: {a:?}"));
            assert_eq!(a[at + 1], "some-model-9", "{agent} rewrote the model");
        }
    }

    /// No model means the agent picks its own, so a host can offer a "default"
    /// entry without this crate inventing one.
    #[test]
    fn no_model_means_no_model_flag() {
        for agent in Agent::ALL {
            let p = plan(agent.bin());
            assert!(p.model.is_none());
            let a = argv(agent, &p);
            assert!(
                pos(&a, "--model").is_none(),
                "{agent} invented a model: {a:?}"
            );
        }
    }

    /// `codex exec` aborts outside a git repository. A host embedding this
    /// crate runs against scratch dirs and review checkouts, so the check is
    /// waived on every invocation; the sandbox is what actually contains a run.
    #[test]
    fn codex_always_waives_the_git_repo_check() {
        for cont in [Continue::New, Continue::Resume("t-1".into())] {
            let mut p = plan("codex");
            p.cont = cont.clone();
            assert!(
                argv(Agent::Codex, &p).contains(&"--skip-git-repo-check".to_string()),
                "{cont:?} must still run outside a repo"
            );
        }
    }

    /// `codex exec resume` rejects `--sandbox` and takes `-c sandbox_mode=`
    /// instead. Getting this wrong makes every second turn fail with an
    /// "unexpected argument" error, which only a multi-turn run reveals.
    #[test]
    fn codex_sets_the_sandbox_by_flag_when_fresh_and_by_config_when_resuming() {
        let mut fresh = plan("codex");
        fresh.permission = Permission::ReadOnly;
        let a = argv(Agent::Codex, &fresh);
        assert_eq!(a[pos(&a, "--sandbox").unwrap() + 1], "read-only");
        assert!(pos(&a, "-c").is_none());

        let mut resumed = fresh.clone();
        resumed.cont = Continue::Resume("thread-9".into());
        let a = argv(Agent::Codex, &resumed);
        assert!(
            pos(&a, "--sandbox").is_none(),
            "resume rejects --sandbox: {a:?}"
        );
        assert_eq!(a[pos(&a, "-c").unwrap() + 1], "sandbox_mode=read-only");
    }

    #[test]
    fn codex_bypass_uses_the_same_flag_on_both_paths() {
        for cont in [Continue::New, Continue::Resume("t".into())] {
            let mut p = plan("codex");
            p.permission = Permission::Bypass;
            p.cont = cont.clone();
            let a = argv(Agent::Codex, &p);
            assert!(a.contains(&"--dangerously-bypass-approvals-and-sandbox".to_string()));
            assert!(pos(&a, "--sandbox").is_none(), "{cont:?}: {a:?}");
        }
    }

    #[test]
    fn codex_without_a_system_flag_prepends_it_to_the_prompt() {
        let mut p = plan("codex");
        p.system = Some("be terse".into());
        let a = argv(Agent::Codex, &p);
        assert_eq!(a.last().unwrap(), "be terse\n\nhi");
    }

    #[test]
    fn codex_maps_each_posture_to_a_sandbox() {
        for (perm, expect) in [
            (Permission::ReadOnly, "read-only"),
            (Permission::Plan, "read-only"),
            (Permission::Edit, "workspace-write"),
            (Permission::Auto, "workspace-write"),
        ] {
            let mut p = plan("codex");
            p.permission = perm;
            let a = argv(Agent::Codex, &p);
            assert_eq!(a[pos(&a, "--sandbox").unwrap() + 1], expect, "{perm:?}");
        }
        let mut p = plan("codex");
        p.permission = Permission::Bypass;
        let a = argv(Agent::Codex, &p);
        assert!(a.contains(&"--dangerously-bypass-approvals-and-sandbox".to_string()));
        assert!(pos(&a, "--sandbox").is_none());
    }

    #[test]
    fn copilot_drops_dash_p_when_the_prompt_is_piped() {
        let mut p = plan("copilot");
        p.stdin_prompt = true;
        let a = argv(Agent::Copilot, &p);
        assert!(
            !a.contains(&"-p".to_string()),
            "a -p value shadows the pipe"
        );
        assert!(!a.contains(&"hi".to_string()));
    }

    /// Copilot declares its tool filters as `--deny-tool[=tools...]`, an
    /// optional value, which binds only with `=`. Passed across a space the
    /// value is silently read as a positional instead, so the deny is lost.
    #[test]
    fn copilot_read_only_denies_shell_and_write_with_the_combined_form() {
        let a = argv(Agent::Copilot, &plan("copilot"));
        assert!(a.contains(&"--deny-tool=shell".to_string()));
        assert!(a.contains(&"--deny-tool=write".to_string()));
        assert!(
            !a.iter().any(|s| s == "--deny-tool"),
            "a bare --deny-tool would drop its value: {a:?}"
        );
    }

    /// A headless Copilot run stalls at the first tool confirmation without it.
    #[test]
    fn copilot_always_allows_tools_and_silences_the_ask_tool() {
        for permission in [Permission::ReadOnly, Permission::Plan, Permission::Bypass] {
            let mut p = plan("copilot");
            p.permission = permission;
            let a = argv(Agent::Copilot, &p);
            assert!(
                a.contains(&"--allow-all-tools".to_string()),
                "{permission:?}"
            );
            assert!(a.contains(&"--no-ask-user".to_string()), "{permission:?}");
        }
    }

    /// Copilot uses one flag in both directions: it sets the id for a new
    /// session and resumes an existing one.
    #[test]
    fn copilot_uses_session_id_for_both_new_and_resumed_sessions() {
        for cont in [
            Continue::NewWith("11111111-2222-3333-4444-555555555555".into()),
            Continue::Resume("11111111-2222-3333-4444-555555555555".into()),
        ] {
            let mut p = plan("copilot");
            p.cont = cont.clone();
            let a = argv(Agent::Copilot, &p);
            assert_eq!(
                a[pos(&a, "--session-id").unwrap() + 1],
                "11111111-2222-3333-4444-555555555555",
                "{cont:?}"
            );
        }
    }

    #[test]
    fn unsupported_capabilities_are_refused_not_downgraded() {
        // Forking headlessly is Claude-only.
        for agent in [Agent::Codex, Agent::Copilot] {
            let mut p = plan(agent.bin());
            p.cont = Continue::Fork("s".into());
            assert!(
                matches!(agent.argv(&p), Err(Error::Unsupported { .. })),
                "{agent} must refuse a fork rather than resume linearly"
            );
        }
        // Codex's id is printed, not assigned, so it cannot be chosen up front.
        let mut p = plan("codex");
        p.cont = Continue::NewWith("id".into());
        assert!(matches!(
            Agent::Codex.argv(&p),
            Err(Error::Unsupported { .. })
        ));
    }

    /// All three expose an id, so all three can back a named session, but only
    /// through a format that actually carries one.
    #[test]
    fn every_agent_has_a_format_that_carries_its_session_id() {
        assert_eq!(Agent::Claude.session_format(), Some(Format::Json));
        assert_eq!(Agent::Codex.session_format(), Some(Format::Stream));
        assert_eq!(Agent::Copilot.session_format(), Some(Format::Stream));
    }

    /// Claude and Copilot let the caller assign the id, so a run that dies
    /// mid-turn still leaves a resumable session.
    #[test]
    fn the_minting_agents_are_claude_and_copilot() {
        let minting: Vec<_> = Agent::ALL
            .into_iter()
            .filter(|a| a.caps().session == SessionSupport::Minted)
            .collect();
        assert_eq!(minting, [Agent::Claude, Agent::Copilot]);
    }
}
