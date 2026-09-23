//! Drive Claude Code, Codex, GitHub Copilot and Grok headlessly from Rust.
//!
//! One request type, one event vocabulary and one session model across four
//! agent CLIs that agree on none of those things. This is a **library**: your
//! program links it and spawns the agent itself, with no intermediate CLI
//! marshalling a request through stdout and back.
//!
//! # Running a prompt
//!
//! Every entry point that spawns a CLI takes a handle to a
//! [`nagoya::reactor::Reactor`], the I/O driver the child's pipes are
//! registered with. The caller starts it, keeps it alive for as long as any run
//! it served is in flight, and passes `&reactor.handle()`. This crate never
//! starts one itself.
//!
//! ```no_run
//! use agent_abstraction::nagoya::reactor::Reactor;
//! use agent_abstraction::{Agent, Permission, Request, run};
//!
//! # async fn example() -> agent_abstraction::Result<()> {
//! let reactor = Reactor::start().expect("an I/O reactor");
//! let outcome = run(
//!     &Request::new(Agent::Claude, "Reply with the single word: pong")
//!         .model("haiku")
//!         .permission(Permission::ReadOnly),
//!     &reactor.handle(),
//! )
//! .await?;
//!
//! println!("{}", outcome.text);
//! # Ok(())
//! # }
//! ```
//!
//! # Watching one as it works
//!
//! ```no_run
//! use agent_abstraction::nagoya::reactor::Handle;
//! use agent_abstraction::{Agent, Event, Request, stream};
//!
//! # async fn example(reactor: &Handle) -> agent_abstraction::Result<()> {
//! let mut running = stream(&Request::new(Agent::Claude, "audit this repo"), reactor)?;
//! while let Some(event) = running.recv().await {
//!     match event {
//!         Event::Text(text) => print!("{text}"),
//!         Event::ToolCall { name, .. } => println!("[{name}]"),
//!         _ => {}
//!     }
//! }
//! let outcome = running.finish().await?;
//! # Ok(())
//! # }
//! ```
//!
//! # Multi-turn conversations
//!
//! Thread one stable name across turns and let [`SessionStore`] map it to
//! whatever handle the agent understands:
//!
//! ```no_run
//! use agent_abstraction::nagoya::reactor::Handle;
//! use agent_abstraction::{Agent, Request, SessionStore, run};
//!
//! # async fn example(reactor: &Handle) -> agent_abstraction::Result<()> {
//! let store = SessionStore::open("/var/lib/myapp/sessions");
//!
//! // First turn creates the session; later turns continue it.
//! let first = Request::new(Agent::Claude, "remember the number 7")
//!     .session(&store, ".", "thread-42", false)?;
//! run(&first, reactor).await?;
//!
//! let second = Request::new(Agent::Claude, "what number did I say?")
//!     .session(&store, ".", "thread-42", false)?;
//! println!("{}", run(&second, reactor).await?.text);
//! # Ok(())
//! # }
//! ```
//!
//! # What each agent can do
//!
//! | | session id | fork | events | system prompt |
//! |---|---|---|---|---|
//! | Claude Code | caller-minted (`--session-id`) | yes | yes | native flag |
//! | Codex | agent-printed (`thread_id`) | no | yes | prepended |
//! | Copilot | caller-minted (`--session-id`) | no | yes | prepended |
//! | Grok | agent-printed (`session/new`) | yes | yes | native (`--rules`) |
//!
//! Asking for something an agent cannot do is always an [`Error::Unsupported`],
//! never a silent downgrade. A caller that asked to fork and got a linear
//! resume would corrupt the conversation it meant to branch.
//!
//! # Operating within the agents' terms
//!
//! This crate drives each vendor's own supported headless interface with the
//! credentials that CLI already uses. It does not reimplement a provider API,
//! multiplex accounts, or retry around a quota: a refusal surfaces as
//! [`Error::RateLimited`], carrying the provider's own wording, and backing off
//! is the caller's decision. See `docs/operating-limits.md`.

mod account;
mod agent;
mod approval;
mod auth;
mod codex_app_server;
mod command;
mod error;
mod event;
mod grok_acp;
mod model;
mod outcome;
mod probe;
mod proc;
mod request;
mod run;
mod session;

pub use account::{AccountUsage, Credits, DailyUsage, Lifetime, UsageWindow};
pub use agent::{Agent, Caps, EnvPolicy, Format, NETWORK_ENV, Permission, SessionSupport};
pub use approval::{Approval, Decision};
pub use auth::{AuthState, AuthStatus};
pub use command::{Command, Commands, Compaction};
pub use error::{Error, Result};
pub use event::{Event, MAX_CAPTURE, MAX_EVENT_BYTES, MAX_LINE, TRUNCATION_MARK};
pub use model::{Kind, Model, Retired, Source, Verified};
pub use outcome::{Outcome, RateLimit, Stop, Usage};
pub use probe::{Probe, Version, VersionStatus};
pub use request::Request;
pub use run::{Run, RunControl, interrupt, run, stream};
pub use session::{Phase, SessionRecord, SessionStore};

/// The runtime this crate is built on, re-exported so a caller can start the
/// [`nagoya::reactor::Reactor`] every spawning entry point takes a handle to
/// without naming a second, possibly mismatched, copy of the dependency.
pub use nagoya;
