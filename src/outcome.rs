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
#[non_exhaustive]
pub struct Usage {
    /// Input tokens that were **not** served from cache.
    ///
    /// Normalized, because the vendors disagree on what "input" counts. Claude
    /// reports the uncached remainder and Codex reports the whole prompt with
    /// the cached part included, so this field is derived on the Codex side by
    /// subtracting. Reading it as the same quantity on both was the point.
    pub input_tokens: Option<u64>,
    /// Generated tokens.
    pub output_tokens: Option<u64>,
    /// Input tokens served from the prompt cache.
    pub cache_read_tokens: Option<u64>,
    /// Input tokens written into the prompt cache.
    pub cache_write_tokens: Option<u64>,
    /// Every input token the turn was charged for, cached or not.
    ///
    /// The size of the conversation as the model saw it, which makes this the
    /// context tracker: compare it to [`Usage::context_window`]. It is already
    /// a running total, since the cached portion *is* the prior conversation,
    /// so **summing it across turns double counts**. See
    /// [`Usage::accumulate`].
    pub context_tokens: Option<u64>,
    /// The selected model's context window, where the agent reports one.
    ///
    /// Claude and interactive Codex runs do. Without it a host can still show
    /// tokens used, just not a share of the limit.
    pub context_window: Option<u64>,
    /// The most tokens the model may generate in one reply.
    pub max_output_tokens: Option<u64>,
    /// Output tokens spent on reasoning rather than the visible answer, where
    /// the agent separates them. Codex alone does.
    pub reasoning_tokens: Option<u64>,
    /// Cost in USD, when the agent priced the run itself. Never inferred from a
    /// local price table, because a guessed cost is worse than no cost.
    pub cost_usd: Option<f64>,
    /// Copilot's premium-request count, its legacy billing unit.
    pub premium_requests: Option<u64>,
    /// Copilot's AI-credit spend for the session, in nano units, which is the
    /// unit that replaced premium requests. Divide by 1e9 for credits.
    ///
    /// Session-scoped and cumulative within a session, verified by running
    /// Copilot repeatedly: it restarts each run rather than accruing across
    /// them. Not an account balance.
    pub ai_credits_nano: Option<u64>,
    /// Wall-clock time the run took, in milliseconds.
    pub duration_ms: Option<u64>,
    /// Time spent waiting on the provider, in milliseconds.
    pub api_duration_ms: Option<u64>,
}

impl Usage {
    /// Whether the agent reported anything at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        *self == Usage::default()
    }

    /// Fold one turn's usage into a session running total.
    ///
    /// Provided because the obvious loop is wrong. Cost and tokens accumulate,
    /// but `context_tokens` is already cumulative: an agent re-sends the whole
    /// conversation each turn and reports its size. Summing that across turns
    /// counts the same conversation once per turn, and the error grows with the
    /// session.
    ///
    /// So additive fields add, and context-shaped fields take the newer value:
    ///
    /// | field | behaviour |
    /// |---|---|
    /// | `output_tokens`, `reasoning_tokens`, `input_tokens` | summed |
    /// | `cache_read_tokens`, `cache_write_tokens` | summed |
    /// | `cost_usd`, `premium_requests`, `duration_ms`, `api_duration_ms` | summed |
    /// | `context_tokens`, `context_window`, `max_output_tokens` | latest |
    /// | `ai_credits_nano` | latest, being a session total already |
    ///
    /// `input_tokens` sums because it is the uncached remainder, which is new
    /// work each turn.
    ///
    /// The cache figures sum for the same reason, which this once got wrong by
    /// filing them with the context. They are not the conversation's size: a
    /// turn's terminal record already sums the cache reads of every call in
    /// that turn, so 100000 and 102000 arrive as 202000, and every one of those
    /// reads is billed. Taking the latest reported one turn's cache traffic as
    /// the whole session's, which understates a long session badly and leaves a
    /// host's token total unable to explain its own cost figure.
    pub fn accumulate(&mut self, turn: &Usage) {
        fn add(total: &mut Option<u64>, turn: Option<u64>) {
            if let Some(value) = turn {
                *total = Some(total.unwrap_or(0) + value);
            }
        }
        fn latest<T: Copy>(total: &mut Option<T>, turn: Option<T>) {
            if turn.is_some() {
                *total = turn;
            }
        }

        add(&mut self.input_tokens, turn.input_tokens);
        add(&mut self.output_tokens, turn.output_tokens);
        add(&mut self.cache_read_tokens, turn.cache_read_tokens);
        add(&mut self.cache_write_tokens, turn.cache_write_tokens);
        add(&mut self.reasoning_tokens, turn.reasoning_tokens);
        add(&mut self.premium_requests, turn.premium_requests);
        add(&mut self.duration_ms, turn.duration_ms);
        add(&mut self.api_duration_ms, turn.api_duration_ms);
        if let Some(cost) = turn.cost_usd {
            self.cost_usd = Some(self.cost_usd.unwrap_or(0.0) + cost);
        }

        latest(&mut self.context_tokens, turn.context_tokens);
        latest(&mut self.context_window, turn.context_window);
        latest(&mut self.max_output_tokens, turn.max_output_tokens);
        latest(&mut self.ai_credits_nano, turn.ai_credits_nano);
    }

    /// Share of the context window in use, from 0.0 to 1.0.
    ///
    /// `None` unless the agent reported both the tokens and the window. Returns
    /// the ratio rather than a formatted string or a bar, so a host renders it
    /// however it likes.
    #[must_use]
    pub fn context_used(&self) -> Option<f64> {
        let (used, window) = (self.context_tokens?, self.context_window?);
        if window == 0 {
            return None;
        }
        #[expect(
            clippy::cast_precision_loss,
            reason = "token counts are far below the f64 integer limit"
        )]
        Some(used as f64 / window as f64)
    }
}

/// A quota signal the agent emitted mid-run.
///
/// Surfaced rather than acted on: this crate reports what the provider said and
/// leaves backing off to the caller. See `docs/operating-limits.md`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RateLimit {
    /// The provider's status word, e.g. `allowed`, `rejected`.
    pub status: String,
    /// Which window this refers to, e.g. `five_hour`.
    pub window: Option<String>,
    /// Unix epoch seconds at which the window resets.
    pub resets_at: Option<i64>,
    /// The provider's status for overage beyond the plan, e.g. `rejected`.
    pub overage_status: Option<String>,
    /// Whether the run was already drawing on overage rather than the plan.
    pub is_using_overage: Option<bool>,
}

impl RateLimit {
    /// Whether this signal means the request was actually refused, as opposed
    /// to an informational "still allowed" heartbeat.
    ///
    /// Every status the provider prefixes with `allowed` is a heartbeat, not a
    /// refusal. This was an exact match on `allowed`, which made
    /// `allowed_warning` a block: Claude emits that once an account passes a
    /// utilization threshold, which is the *opposite* of being refused, and
    /// the account keeps working for the rest of the window. Observed on
    /// claude 2.1.212, seven-day window, at 77% of quota:
    ///
    /// ```json
    /// {"status": "allowed_warning", "utilization": 0.77,
    ///  "surpassedThreshold": 0.75, "rateLimitType": "seven_day"}
    /// ```
    ///
    /// The consequence was that crossing 75% of a window turned every
    /// finished, successful run into `Error::RateLimited` and discarded its
    /// answer, for as long as the window stayed above the threshold.
    ///
    /// Callers who want to *show* the warning read `status` and `resets_at`,
    /// which carry it. This one method answers only whether the request was
    /// refused.
    #[must_use]
    pub fn is_blocking(&self) -> bool {
        !self.status.to_ascii_lowercase().starts_with("allowed")
    }
}

/// Why the agent stopped.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
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
#[non_exhaustive]
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
    /// Raw stderr, kept for diagnostics and capped at
    /// [`crate::MAX_CAPTURE`].
    pub stderr: String,
    /// How many output lines could not be parsed, with the first as a sample.
    ///
    /// Agents interleave banners with their JSON, so a non-zero count is not
    /// automatically a fault. It becomes one when paired with an empty [`Self::text`],
    /// which is what a vendor changing its output shape looks like from here.
    /// See [`Outcome::looks_like_a_format_change`].
    pub unparsed: usize,
    /// The first line that failed to parse.
    pub first_unparsed: Option<String>,
    /// The answer parsed against the schema given to [`crate::Request::schema`].
    ///
    /// `None` when no schema was asked for, or when the agent's answer did not
    /// parse as JSON. Never a re-interpretation of prose: this is only set from
    /// a value the agent produced under a schema.
    pub structured: Option<serde_json::Value>,
}

impl Outcome {
    /// Whether the run finished cleanly: a zero exit and no error result.
    #[must_use]
    pub fn is_ok(&self) -> bool {
        self.exit_code == 0 && self.stop == Stop::Completed
    }

    /// Whether this run looks like the agent changed its output format.
    ///
    /// The signature is a process that exited successfully while every line it
    /// printed was unreadable: the CLI is healthy and this crate's parser is
    /// not. Worth logging loudly, because the alternative symptom is a
    /// successful run that mysteriously returns nothing.
    #[must_use]
    pub fn looks_like_a_format_change(&self) -> bool {
        self.exit_code == 0 && self.unparsed > 0 && self.text.trim().is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reported from the field, and the reason a healthy account stopped
    /// working: a run that finished and answered was handed back as
    /// `Error::RateLimited`, for every run, once the seven-day window passed
    /// three quarters full.
    ///
    /// The record below is verbatim from claude 2.1.212 at the time. `status`
    /// is `allowed_warning`, which is the provider saying the request went
    /// through and the window is filling, and the old exact match on
    /// `allowed` read every character of that as a refusal.
    #[test]
    fn a_warning_is_not_a_refusal() {
        let warned = RateLimit {
            status: "allowed_warning".into(),
            window: Some("seven_day".into()),
            resets_at: Some(1_785_765_600),
            overage_status: None,
            is_using_overage: Some(false),
        };
        assert!(
            !warned.is_blocking(),
            "a warning that the window is filling blocked a run that succeeded"
        );

        // The plain heartbeat, and the refusals, both still read correctly.
        assert!(
            !RateLimit {
                status: "allowed".into(),
                ..warned.clone()
            }
            .is_blocking()
        );
        for refusal in ["rejected", "blocked", "REJECTED"] {
            assert!(
                RateLimit {
                    status: refusal.into(),
                    ..warned.clone()
                }
                .is_blocking(),
                "{refusal} must still be read as a refusal"
            );
        }
    }

    /// The trap `accumulate` exists to avoid, and the one it once fell into
    /// next door. `context_tokens` is the conversation's size as the agent last
    /// saw it, so summing it counts the same conversation once per turn. The
    /// cache figures look alike and are not: each turn's is what that turn's
    /// calls actually read, and every read is billed.
    ///
    /// Numbers from two real Codex turns on one thread.
    #[test]
    fn a_session_total_does_not_count_the_conversation_twice() {
        let turn1 = Usage {
            input_tokens: Some(2_286),
            output_tokens: Some(5),
            cache_read_tokens: Some(13_056),
            context_tokens: Some(15_342),
            ..Usage::default()
        };
        let turn2 = Usage {
            input_tokens: Some(2_543),
            output_tokens: Some(11),
            cache_read_tokens: Some(28_160),
            context_tokens: Some(30_703),
            ..Usage::default()
        };

        let mut session = Usage::default();
        session.accumulate(&turn1);
        session.accumulate(&turn2);

        // New work each turn, so these add up.
        assert_eq!(session.input_tokens, Some(4_829));
        assert_eq!(session.output_tokens, Some(16));
        // The conversation is one conversation. Summing would claim 46,045.
        assert_eq!(
            session.context_tokens,
            Some(30_703),
            "context is already cumulative and must not be summed"
        );
        // Billed traffic, not a size: the session read 41,216 tokens out of
        // cache across the two turns and was charged for all of them. Taking
        // the latest would report the second turn's 28,160 as the whole
        // session's, and no token total built on it could explain its cost.
        assert_eq!(
            session.cache_read_tokens,
            Some(41_216),
            "cache reads are billed per call and accumulate"
        );
    }

    #[test]
    fn cost_and_duration_accumulate() {
        let mut session = Usage::default();
        for _ in 0..3 {
            session.accumulate(&Usage {
                cost_usd: Some(0.5),
                duration_ms: Some(1_000),
                premium_requests: Some(1),
                ..Usage::default()
            });
        }
        assert!((session.cost_usd.expect("cost") - 1.5).abs() < 1e-9);
        assert_eq!(session.duration_ms, Some(3_000));
        assert_eq!(session.premium_requests, Some(3));
    }

    /// A field the agent stopped reporting must keep its last known value
    /// rather than being wiped by a turn that said nothing.
    #[test]
    fn a_silent_turn_does_not_erase_what_is_known() {
        let mut session = Usage {
            context_window: Some(200_000),
            cost_usd: Some(1.0),
            ..Usage::default()
        };
        session.accumulate(&Usage::default());
        assert_eq!(session.context_window, Some(200_000));
        assert_eq!(session.cost_usd, Some(1.0));
    }

    #[test]
    fn context_used_is_a_ratio_and_absent_without_both_halves() {
        let full = Usage {
            context_tokens: Some(27_645),
            context_window: Some(200_000),
            ..Usage::default()
        };
        let share = full.context_used().expect("both halves present");
        assert!((share - 0.138_225).abs() < 1e-6, "{share}");

        // Codex reports tokens but no window, so a share is not knowable.
        let partial = Usage {
            context_tokens: Some(30_703),
            ..Usage::default()
        };
        assert_eq!(partial.context_used(), None);
    }
}
