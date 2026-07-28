//! What a finished run produced.

use serde::{Deserialize, Serialize};

use crate::agent::Agent;

/// Token and cost accounting for a run.
///
/// Every field is optional because the three agents report different subsets:
/// Claude reports full token counts and a dollar cost, Codex reports tokens,
/// Copilot reports premium requests and no tokens at all. An absent field means
/// "this agent did not say", never zero.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    /// Non-cached input tokens.
    pub input_tokens: Option<u64>,
    /// Generated tokens.
    pub output_tokens: Option<u64>,
    /// Input tokens served from the prompt cache.
    pub cache_read_tokens: Option<u64>,
    /// Input tokens written into the prompt cache.
    pub cache_write_tokens: Option<u64>,
    /// Cost in USD, when the agent priced the run itself. Never inferred from a
    /// local price table, because a guessed cost is worse than no cost.
    pub cost_usd: Option<f64>,
    /// Copilot's premium-request count, its only usage unit.
    pub premium_requests: Option<u64>,
}

impl Usage {
    /// Whether the agent reported anything at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        *self == Usage::default()
    }
}

/// A quota signal the agent emitted mid-run.
///
/// Surfaced rather than acted on: this crate reports what the provider said and
/// leaves backing off to the caller. See `docs/operating-limits.md`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RateLimit {
    /// The provider's status word, e.g. `allowed`, `rejected`.
    pub status: String,
    /// Which window this refers to, e.g. `five_hour`.
    pub window: Option<String>,
    /// Unix epoch seconds at which the window resets.
    pub resets_at: Option<i64>,
}

impl RateLimit {
    /// Whether this signal means the request was actually refused, as opposed
    /// to an informational "still allowed" heartbeat.
    #[must_use]
    pub fn is_blocking(&self) -> bool {
        !self.status.eq_ignore_ascii_case("allowed")
    }
}

/// Why the agent stopped.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stop {
    /// Completed normally.
    #[default]
    Completed,
    /// The agent reported an error result.
    Error,
    /// The agent stopped for a reason it named but this crate does not model.
    Other(String),
}

/// The result of one completed run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Outcome {
    /// Which agent produced it.
    pub agent: Agent,
    /// The native session id, when the run had or produced one. This is the
    /// handle a later turn resumes with.
    pub session: Option<String>,
    /// The assistant's final text.
    pub text: String,
    /// Token and cost accounting.
    pub usage: Usage,
    /// Why it stopped.
    pub stop: Stop,
    /// The last quota signal seen, if any.
    pub rate_limit: Option<RateLimit>,
    /// The process exit code.
    pub exit_code: i32,
    /// Raw stderr, kept for diagnostics.
    pub stderr: String,
}

impl Outcome {
    /// Whether the run finished cleanly: a zero exit and no error result.
    #[must_use]
    pub fn is_ok(&self) -> bool {
        self.exit_code == 0 && self.stop == Stop::Completed
    }
}
