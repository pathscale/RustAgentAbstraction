//! Account-wide usage, for a host that wants to show quota rather than guess.
//!
//! Distinct from [`crate::Usage`], which measures one run. This is the plan
//! behind the runs: how much of a window is spent, when it resets, what credits
//! remain.
//!
//! # Only Codex can answer
//!
//! Of the three, one exposes this without a terminal.
//!
//! - **Codex** answers in full through `codex app-server`, a JSON-RPC interface
//!   over stdio. Percentages, window lengths, reset times, per-day buckets and
//!   lifetime totals.
//! - **Claude** reports quota only *during* a run, as
//!   [`crate::Event::RateLimit`], and the wire carries no utilization figure at
//!   all. Verified against claude 2.1.212: the whole `rate_limit_info` vocabulary
//!   is `status`, `resetsAt`, `rateLimitType`, `overageStatus`,
//!   `overageDisabledReason` and `isUsingOverage`. There is no percentage field
//!   to be absent, so no amount of waiting for the right event produces one.
//!   The percentages on its `/usage` screen are fetched separately by that
//!   screen, and only rendered there.
//! - **Copilot** reports session spend as [`crate::Usage::ai_credits_nano`] and
//!   nothing account-wide. Remaining budget lives in its status footer.
//!
//! [`crate::Agent::reports_account_usage`] answers this up front, so a host can
//! decide whether to build the panel at all rather than discovering it from an
//! error.
//!
//! # Values, not presentation
//!
//! Everything here is a number, a string or a timestamp exactly as the provider
//! gave it. No formatting, no percentages pre-rendered into text, no bars. A
//! host that wants "16% used" or a progress meter builds it from
//! [`UsageWindow::used_percent`].

use std::process::Stdio;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::agent::Agent;
use crate::error::{Error, Result};

/// How long to wait for the whole exchange before giving up.
///
/// Generous for three local round trips, and bounded because a helper that
/// hangs would take a UI thread with it.
const QUERY_TIMEOUT: Duration = Duration::from_secs(30);

/// Cap on a single reply, so a malformed stream cannot exhaust memory.
const MAX_REPLY_BYTES: usize = 1024 * 1024;

/// What an account has spent and what it has left.
///
/// Every field is optional or empty-able: this is assembled from whatever the
/// agent reports, and absent means "it did not say", never zero.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct AccountUsage {
    /// How the account authenticates, e.g. `chatgpt`.
    pub account_kind: Option<String>,
    /// The plan name the provider reports, e.g. `plus`.
    pub plan: Option<String>,
    /// Email on the account, where the agent reports one.
    pub email: Option<String>,
    /// Every quota window the provider is tracking, most constraining first as
    /// the provider ordered them.
    pub windows: Vec<UsageWindow>,
    /// Pay-as-you-go balance, where the plan has one.
    pub credits: Option<Credits>,
    /// All-time counters, where the agent keeps them.
    pub lifetime: Option<Lifetime>,
    /// Per-day token totals, oldest first.
    pub daily: Vec<DailyUsage>,
    /// Whether spend controls have already stopped this account.
    pub spend_control_reached: Option<bool>,
}

/// One quota window and how much of it is gone.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct UsageWindow {
    /// The provider's own name for this window, e.g. `primary`.
    pub id: String,
    /// Share of the window consumed, 0 to 100, as the provider reported it.
    pub used_percent: Option<f64>,
    /// How long the window runs. Codex reports 10080 minutes for a week.
    pub window_minutes: Option<u64>,
    /// Unix epoch seconds at which it resets.
    pub resets_at: Option<i64>,
}

/// A pay-as-you-go balance.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Credits {
    /// Whether any credits are available.
    pub has_credits: bool,
    /// Whether the account is uncapped.
    pub unlimited: bool,
    /// The balance as the provider wrote it. Kept as text because it arrives as
    /// a string and a decimal balance must not be rounded through a float on
    /// its way to a display.
    pub balance: Option<String>,
}

/// All-time counters.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Lifetime {
    /// Tokens used since the account began.
    pub tokens: Option<u64>,
    /// The busiest single day, in tokens.
    pub peak_daily_tokens: Option<u64>,
    /// The longest single turn, in seconds.
    pub longest_turn_secs: Option<u64>,
    /// Consecutive days of use up to now.
    pub current_streak_days: Option<u64>,
    /// The longest such run ever.
    pub longest_streak_days: Option<u64>,
}

/// One day's total.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct DailyUsage {
    /// ISO date, as the provider wrote it.
    pub date: String,
    /// Tokens used that day.
    pub tokens: Option<u64>,
}

impl Agent {
    /// Whether this agent can report account-wide usage without a terminal.
    ///
    /// Worth asking before building a quota panel, since two of the three
    /// cannot and no amount of retrying changes that.
    #[must_use]
    pub fn reports_account_usage(self) -> bool {
        matches!(self, Agent::Codex)
    }

    /// Ask the agent what the account has spent and what remains.
    ///
    /// # Errors
    /// [`Error::Unsupported`] where the agent has no headless way to answer,
    /// which today is Claude and Copilot; check
    /// [`Agent::reports_account_usage`] first to avoid the round trip.
    /// [`Error::NotInstalled`] if the binary is missing, [`Error::Spawn`] if it
    /// cannot be run, [`Error::Timeout`] if it does not reply,
    /// [`Error::AgentError`] if it replies with a refusal, and
    /// [`Error::Parse`] if the reply is not the expected shape.
    pub async fn account_usage(self) -> Result<AccountUsage> {
        match self {
            Agent::Codex => codex_account_usage(self.bin()).await,
            // Deliberately an error rather than a half-answer assembled from a
            // past run's rate-limit event: that would be neither current nor
            // account-wide, and would read as though it were both.
            Agent::Claude | Agent::Copilot => Err(Error::Unsupported {
                agent: self,
                what: "reporting account usage without a terminal",
            }),
        }
    }
}

/// Query `codex app-server`, an experimental JSON-RPC interface over stdio.
///
/// Experimental is the operative word: the method names below are not covered
/// by the live suite's version check, and Codex may rename them. A rename
/// surfaces as [`Error::AgentError`] carrying the server's own complaint, which
/// names the method, rather than as silence.
///
/// Verified against codex-cli 0.145.0 on 2026-07-29.
async fn codex_account_usage(bin: &str) -> Result<AccountUsage> {
    let mut child = tokio::process::Command::new(bin)
        .arg("app-server")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        // Silenced rather than captured: the server logs progress here and none
        // of it belongs in an error about usage.
        .stderr(Stdio::null())
        .spawn()
        .map_err(|source| {
            if source.kind() == std::io::ErrorKind::NotFound {
                Error::NotInstalled {
                    agent: Agent::Codex,
                    bin: bin.to_string(),
                    hint: Agent::Codex.install_hint(),
                }
            } else {
                Error::Spawn {
                    bin: bin.to_string(),
                    source,
                }
            }
        })?;

    let exchange = codex_exchange(&mut child);
    let result = match tokio::time::timeout(QUERY_TIMEOUT, exchange).await {
        Ok(result) => result,
        Err(_) => Err(Error::Timeout {
            bin: bin.to_string(),
            timeout: QUERY_TIMEOUT,
            partial: String::new(),
        }),
    };
    // The server runs until its stdin closes, so it is killed either way rather
    // than left behind holding a pipe.
    let _ = child.kill().await;
    result
}

/// Drive the three requests and collect their replies.
async fn codex_exchange(child: &mut tokio::process::Child) -> Result<AccountUsage> {
    const ACCOUNT: i64 = 2;
    const LIMITS: i64 = 3;
    const USAGE: i64 = 4;

    let Some(stdin) = child.stdin.as_mut() else {
        return Err(Error::Parse {
            agent: Agent::Codex,
            detail: "app-server stdin was not available".into(),
        });
    };
    // `initialize` first: the server rejects everything else until it has been
    // told who is calling.
    let mut batch = String::new();
    batch.push_str(&request(
        1,
        "initialize",
        &serde_json::json!({"clientInfo": {
            "name": "agent-abstraction",
            "title": "agent-abstraction",
            "version": env!("CARGO_PKG_VERSION"),
        }}),
    ));
    for (id, method) in [
        (ACCOUNT, "account/read"),
        (LIMITS, "account/rateLimits/read"),
        (USAGE, "account/usage/read"),
    ] {
        batch.push_str(&request(id, method, &serde_json::json!({})));
    }
    stdin
        .write_all(batch.as_bytes())
        .await
        .map_err(|source| Error::Spawn {
            bin: "codex app-server".into(),
            source,
        })?;
    let _ = stdin.flush().await;

    let Some(stdout) = child.stdout.take() else {
        return Err(Error::Parse {
            agent: Agent::Codex,
            detail: "app-server stdout was not available".into(),
        });
    };

    let mut lines = BufReader::new(stdout).lines();
    let mut usage = AccountUsage::default();
    let mut outstanding = 3;
    while outstanding > 0 {
        // A stream that ends before every reply arrives leaves whatever was
        // collected in place rather than discarding it.
        let Ok(Some(line)) = lines.next_line().await else {
            break;
        };
        if line.len() > MAX_REPLY_BYTES {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let Some(id) = value.get("id").and_then(Value::as_i64) else {
            // A notification, not a reply. The server emits several.
            continue;
        };
        if !matches!(id, ACCOUNT | LIMITS | USAGE) {
            continue;
        }
        outstanding -= 1;
        if let Some(error) = value.get("error") {
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("the app-server refused the request");
            return Err(Error::AgentError {
                agent: Agent::Codex,
                bin: "codex app-server".into(),
                status: None,
                message: message.chars().take(400).collect(),
            });
        }
        let Some(result) = value.get("result") else {
            continue;
        };
        match id {
            ACCOUNT => read_account(&mut usage, result),
            LIMITS => read_limits(&mut usage, result),
            USAGE => read_usage(&mut usage, result),
            _ => unreachable!("filtered above"),
        }
    }

    if usage == AccountUsage::default() {
        return Err(Error::Parse {
            agent: Agent::Codex,
            detail: "app-server reported no account information".into(),
        });
    }
    Ok(usage)
}

/// One JSON-RPC request, newline delimited as the server expects.
fn request(id: i64, method: &str, params: &Value) -> String {
    format!(
        "{}\n",
        serde_json::json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
    )
}

/// Read `account/read`.
fn read_account(usage: &mut AccountUsage, result: &Value) {
    let Some(account) = result.get("account") else {
        return;
    };
    let text = |key: &str| account.get(key).and_then(Value::as_str).map(str::to_string);
    usage.account_kind = text("type");
    usage.plan = text("planType");
    usage.email = text("email");
}

/// Read `account/rateLimits/read`.
fn read_limits(usage: &mut AccountUsage, result: &Value) {
    let Some(limits) = result.get("rateLimits") else {
        return;
    };
    // `primary` and `secondary` are separate keys rather than a list, and
    // `secondary` is null on plans that have only one window.
    for id in ["primary", "secondary"] {
        let Some(window) = limits.get(id).filter(|w| !w.is_null()) else {
            continue;
        };
        usage.windows.push(UsageWindow {
            id: id.to_string(),
            used_percent: window.get("usedPercent").and_then(Value::as_f64),
            window_minutes: window.get("windowDurationMins").and_then(Value::as_u64),
            resets_at: window.get("resetsAt").and_then(Value::as_i64),
        });
    }
    if let Some(credits) = limits.get("credits").filter(|c| !c.is_null()) {
        usage.credits = Some(Credits {
            has_credits: credits
                .get("hasCredits")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            unlimited: credits
                .get("unlimited")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            balance: credits
                .get("balance")
                .and_then(Value::as_str)
                .map(str::to_string),
        });
    }
    usage.spend_control_reached = limits.get("spendControlReached").and_then(Value::as_bool);
    // `account/read` is the better source, but a plan named here still beats
    // nothing if that reply was the one that went missing.
    if usage.plan.is_none() {
        usage.plan = limits
            .get("planType")
            .and_then(Value::as_str)
            .map(str::to_string);
    }
}

/// Read `account/usage/read`.
fn read_usage(usage: &mut AccountUsage, result: &Value) {
    if let Some(summary) = result.get("summary") {
        let get = |key: &str| summary.get(key).and_then(Value::as_u64);
        usage.lifetime = Some(Lifetime {
            tokens: get("lifetimeTokens"),
            peak_daily_tokens: get("peakDailyTokens"),
            longest_turn_secs: get("longestRunningTurnSec"),
            current_streak_days: get("currentStreakDays"),
            longest_streak_days: get("longestStreakDays"),
        });
    }
    if let Some(buckets) = result.get("dailyUsageBuckets").and_then(Value::as_array) {
        usage.daily = buckets
            .iter()
            .filter_map(|bucket| {
                Some(DailyUsage {
                    date: bucket.get("startDate").and_then(Value::as_str)?.to_string(),
                    tokens: bucket.get("tokens").and_then(Value::as_u64),
                })
            })
            .collect();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Trimmed from a real `account/rateLimits/read` reply (codex-cli 0.145.0).
    const LIMITS: &str = r#"{"rateLimits":{"limitId":"codex","primary":{"usedPercent":1,
      "windowDurationMins":10080,"resetsAt":1785925265},"secondary":null,
      "credits":{"hasCredits":false,"unlimited":false,"balance":"0"},
      "spendControlReached":false,"planType":"plus"}}"#;

    #[test]
    fn rate_limits_carry_the_window_and_its_reset() {
        let mut usage = AccountUsage::default();
        read_limits(&mut usage, &serde_json::from_str(LIMITS).expect("json"));
        assert_eq!(usage.windows.len(), 1, "secondary is null on this plan");
        let window = &usage.windows[0];
        assert_eq!(window.id, "primary");
        assert_eq!(window.used_percent, Some(1.0));
        // A week, which a host can render however it likes.
        assert_eq!(window.window_minutes, Some(10080));
        assert_eq!(window.resets_at, Some(1_785_925_265));
        assert_eq!(usage.spend_control_reached, Some(false));
    }

    /// A decimal balance must reach a display without passing through a float.
    #[test]
    fn a_credit_balance_stays_exactly_as_the_provider_wrote_it() {
        let mut usage = AccountUsage::default();
        read_limits(&mut usage, &serde_json::from_str(LIMITS).expect("json"));
        let credits = usage.credits.expect("credits");
        assert_eq!(credits.balance.as_deref(), Some("0"));
        assert!(!credits.has_credits);
        assert!(!credits.unlimited);
    }

    /// `secondary` is a key rather than a list entry, and is null on plans with
    /// one window. A null must not become an empty window.
    #[test]
    fn a_null_window_is_skipped_not_invented() {
        let both =
            r#"{"rateLimits":{"primary":{"usedPercent":12},"secondary":{"usedPercent":40}}}"#;
        let mut usage = AccountUsage::default();
        read_limits(&mut usage, &serde_json::from_str(both).expect("json"));
        assert_eq!(usage.windows.len(), 2);
        assert_eq!(usage.windows[1].id, "secondary");
        assert_eq!(usage.windows[1].used_percent, Some(40.0));
    }

    #[test]
    fn lifetime_and_daily_totals_are_read() {
        let reply = r#"{"summary":{"lifetimeTokens":1243297,"peakDailyTokens":1060227,
          "longestRunningTurnSec":11,"currentStreakDays":2,"longestStreakDays":2},
          "dailyUsageBuckets":[{"startDate":"2026-07-28","tokens":1060227},
          {"startDate":"2026-07-29","tokens":183070}]}"#;
        let mut usage = AccountUsage::default();
        read_usage(&mut usage, &serde_json::from_str(reply).expect("json"));
        let lifetime = usage.lifetime.expect("lifetime");
        assert_eq!(lifetime.tokens, Some(1_243_297));
        assert_eq!(lifetime.current_streak_days, Some(2));
        assert_eq!(usage.daily.len(), 2);
        assert_eq!(usage.daily[1].date, "2026-07-29");
        assert_eq!(usage.daily[1].tokens, Some(183_070));
    }

    /// The capability is answerable without spawning anything, so a host can
    /// decide whether to build the panel at all.
    #[tokio::test]
    async fn agents_that_cannot_report_say_so_without_being_asked_twice() {
        for agent in [Agent::Claude, Agent::Copilot] {
            assert!(!agent.reports_account_usage(), "{agent}");
            assert!(
                matches!(agent.account_usage().await, Err(Error::Unsupported { .. })),
                "{agent} should refuse rather than assemble a partial answer"
            );
        }
        assert!(Agent::Codex.reports_account_usage());
    }
}
