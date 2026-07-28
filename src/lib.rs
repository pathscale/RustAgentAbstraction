//! Drive Claude Code, Codex and GitHub Copilot headlessly from Rust.
//!
//! One request type, one event vocabulary and one session model across three
//! agent CLIs that agree on none of those things. This is a **library**: your
//! program links it and spawns the agent itself, with no intermediate CLI
//! marshalling a request through stdout and back.
//!
//! # Running a prompt
//!
//! ```no_run
//! use agent_abstraction::{Agent, Permission, Request, run};
//!
//! # async fn example() -> agent_abstraction::Result<()> {
//! let outcome = run(
//!     &Request::new(Agent::Claude, "Reply with the single word: pong")
//!         .model("haiku")
//!         .permission(Permission::ReadOnly),
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
//! use agent_abstraction::{Agent, Event, Request, stream};
//!
//! # async fn example() -> agent_abstraction::Result<()> {
//! let mut running = stream(&Request::new(Agent::Claude, "audit this repo"))?;
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
//! use agent_abstraction::{Agent, Request, SessionStore, run};
//!
//! # async fn example() -> agent_abstraction::Result<()> {
//! let store = SessionStore::open("/var/lib/myapp/sessions");
//!
//! // First turn creates the session; later turns continue it.
//! let first = Request::new(Agent::Claude, "remember the number 7")
//!     .session(&store, ".", "thread-42", false)?;
//! run(&first).await?;
//!
//! let second = Request::new(Agent::Claude, "what number did I say?")
//!     .session(&store, ".", "thread-42", false)?;
//! println!("{}", run(&second).await?.text);
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

mod agent;
mod error;
mod event;
mod outcome;
mod probe;
mod proc;
mod request;
mod run;
mod session;

pub use agent::{Agent, Caps, EnvPolicy, Format, NETWORK_ENV, Permission, SessionSupport};
pub use error::{Error, Result};
pub use event::{Event, MAX_CAPTURE, MAX_EVENT_BYTES, MAX_LINE, TRUNCATION_MARK};
pub use outcome::{Outcome, RateLimit, Stop, Usage};
pub use probe::{Probe, Version, VersionStatus};
pub use request::Request;
pub use run::{Run, run, stream};
pub use session::{Phase, SessionRecord, SessionStore};
