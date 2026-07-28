//! Asking an agent whether it is logged in, without spending a request.
//!
//! A missing login is otherwise only discoverable by running a turn and
//! catching [`crate::Error::NotAuthenticated`], which costs quota and is a poor
//! way to populate a settings screen. Two of the three CLIs expose a status
//! command; the third does not, and this says so rather than guessing.
//!
//! ```no_run
//! # use agent_abstraction::{Agent, AuthStatus};
//! # async fn example() -> agent_abstraction::Result<()> {
//! for agent in Agent::ALL {
//!     let status = AuthStatus::check(agent).await?;
//!     println!("{agent}: {}", status.summary());
//! }
//! # Ok(())
//! # }
//! ```

use serde_json::Value;

use crate::agent::Agent;
use crate::error::{Error, Result};

/// Whether an agent has usable credentials.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum AuthState {
    /// The CLI confirmed it is logged in.
    LoggedIn,
    /// The CLI confirmed it is not.
    LoggedOut,
    /// Could not be determined. Either the agent exposes no way to ask, or it
    /// answered something unrecognized.
    ///
    /// Deliberately distinct from [`AuthState::LoggedOut`]: reporting "not
    /// logged in" for an agent that simply cannot be asked would send someone
    /// to re-authenticate a working setup.
    Unknown,
}

/// What an agent reported about its credentials.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct AuthStatus {
    /// The agent asked.
    pub agent: Agent,
    /// What it said.
    pub state: AuthState,
    /// How it is authenticated, in its own words: `claude.ai`, `ChatGPT`, an
    /// API key. `None` when it did not say.
    pub method: Option<String>,
    /// The account, where the agent reports one. Claude gives an email.
    pub account: Option<String>,
    /// The plan or subscription, where reported.
    pub plan: Option<String>,
    /// The CLI's own output, or an explanation when it could not be asked.
    pub detail: String,
    /// The command that resolves a missing login.
    pub login_hint: &'static str,
}

impl AuthStatus {
    /// Ask `agent`'s default binary.
    ///
    /// # Errors
    /// [`Error::NotInstalled`] if the binary is missing, [`Error::Spawn`] if it
    /// cannot be run. A CLI that answers "logged out" is a successful check,
    /// not an error.
    pub async fn check(agent: Agent) -> Result<AuthStatus> {
        AuthStatus::check_bin(agent, agent.bin()).await
    }

    /// Ask a specific binary, for a caller overriding the path with
    /// [`crate::Request::bin`].
    ///
    /// # Errors
    /// [`Error::NotInstalled`] if the binary is missing, [`Error::Spawn`] if it
    /// cannot be run.
    pub async fn check_bin(agent: Agent, bin: &str) -> Result<AuthStatus> {
        let Some(args) = agent.auth_status_argv() else {
            return Ok(AuthStatus::uncheckable(agent));
        };

        let output = tokio::process::Command::new(bin)
            .args(args)
            .output()
            .await
            .map_err(|source| {
                if source.kind() == std::io::ErrorKind::NotFound {
                    Error::NotInstalled {
                        agent,
                        bin: bin.to_string(),
                        hint: agent.install_hint(),
                    }
                } else {
                    Error::Spawn {
                        bin: bin.to_string(),
                        source,
                    }
                }
            })?;

        let mut text = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if text.is_empty() {
            text = String::from_utf8_lossy(&output.stderr).trim().to_string();
        }
        Ok(AuthStatus::read(agent, &text, output.status.success()))
    }

    /// Interpret a status command's output.
    ///
    /// Split out from the spawn so every agent's parsing is unit-testable
    /// against its real output without needing the CLI installed.
    #[must_use]
    pub(crate) fn read(agent: Agent, text: &str, exit_ok: bool) -> AuthStatus {
        let mut status = AuthStatus {
            agent,
            state: AuthState::Unknown,
            method: None,
            account: None,
            plan: None,
            detail: text.to_string(),
            login_hint: agent.login_hint(),
        };

        match agent {
            // Claude answers JSON by default, which is the one machine-readable
            // status of the three.
            Agent::Claude => {
                if let Ok(Value::Object(map)) = serde_json::from_str::<Value>(text) {
                    status.state = match map.get("loggedIn").and_then(Value::as_bool) {
                        Some(true) => AuthState::LoggedIn,
                        Some(false) => AuthState::LoggedOut,
                        None => AuthState::Unknown,
                    };
                    let field = |key: &str| {
                        map.get(key)
                            .and_then(Value::as_str)
                            .map(str::to_string)
                            .filter(|v| !v.is_empty())
                    };
                    status.method = field("authMethod");
                    status.account = field("email");
                    status.plan = field("subscriptionType");
                }
            }
            // Codex answers prose, so this reads the phrases it actually uses.
            // The negative is checked first: "not logged in" contains "logged
            // in".
            Agent::Codex => {
                let lower = text.to_ascii_lowercase();
                status.state = if lower.contains("not logged in") || lower.contains("logged out") {
                    AuthState::LoggedOut
                } else if exit_ok && lower.contains("logged in") {
                    // e.g. "Logged in using ChatGPT"
                    status.method = text
                        .rsplit_once(" using ")
                        .map(|(_, method)| method.trim().to_string());
                    AuthState::LoggedIn
                } else {
                    AuthState::Unknown
                };
            }
            // Unreachable: `auth_status_argv` returns None, so `check_bin`
            // never gets here for Copilot.
            Agent::Copilot => {}
        }
        status
    }

    /// The status for an agent that offers no way to ask.
    fn uncheckable(agent: Agent) -> AuthStatus {
        // A token in the environment is worth reporting, but its presence is
        // not proof it is valid, so this stays Unknown rather than claiming a
        // login it has not verified.
        let env_token = agent
            .auth_env_vars()
            .iter()
            .find(|name| std::env::var_os(name).is_some_and(|v| !v.is_empty()));

        AuthStatus {
            agent,
            state: AuthState::Unknown,
            method: env_token.map(|name| format!("token in {name}")),
            account: None,
            plan: None,
            detail: match env_token {
                Some(name) => format!(
                    "{agent} exposes no status command, so this cannot be confirmed without \
                     spending a request. {name} is set, but its validity is unverified."
                ),
                None => format!(
                    "{agent} exposes no status command, so this cannot be confirmed without \
                     spending a request, and no credential environment variable is set."
                ),
            },
            login_hint: agent.login_hint(),
        }
    }

    /// Whether the agent confirmed it is logged in.
    ///
    /// False for [`AuthState::Unknown`], so a caller that gates on this is
    /// conservative. Check `state` directly to distinguish "no" from "cannot
    /// tell".
    #[must_use]
    pub fn is_logged_in(&self) -> bool {
        self.state == AuthState::LoggedIn
    }

    /// Whether the answer means someone has to log in.
    ///
    /// Only a confirmed logout. An agent that cannot be asked is not evidence
    /// of a problem.
    #[must_use]
    pub fn needs_login(&self) -> bool {
        self.state == AuthState::LoggedOut
    }

    /// One line fit to show in a settings screen.
    #[must_use]
    pub fn summary(&self) -> String {
        match self.state {
            AuthState::LoggedIn => {
                let who = self
                    .account
                    .as_deref()
                    .or(self.method.as_deref())
                    .unwrap_or("logged in");
                match &self.plan {
                    Some(plan) => format!("logged in as {who} ({plan})"),
                    None => format!("logged in as {who}"),
                }
            }
            AuthState::LoggedOut => format!("not logged in: {}", self.login_hint),
            AuthState::Unknown => format!("unknown: {}", self.detail),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verbatim from `claude auth status`, which answers JSON by default.
    #[test]
    fn claude_json_is_read_into_a_status() {
        let text = r#"{
            "loggedIn": true,
            "authMethod": "claude.ai",
            "apiProvider": "firstParty",
            "email": "claude@pathscale.com",
            "orgId": "18b5d0a6",
            "subscriptionType": "max"
        }"#;
        let status = AuthStatus::read(Agent::Claude, text, true);
        assert_eq!(status.state, AuthState::LoggedIn);
        assert!(status.is_logged_in());
        assert!(!status.needs_login());
        assert_eq!(status.account.as_deref(), Some("claude@pathscale.com"));
        assert_eq!(status.method.as_deref(), Some("claude.ai"));
        assert_eq!(status.plan.as_deref(), Some("max"));
        assert!(status.summary().contains("claude@pathscale.com"));
    }

    #[test]
    fn claude_reports_a_logout_as_one() {
        let status = AuthStatus::read(Agent::Claude, r#"{"loggedIn": false}"#, true);
        assert_eq!(status.state, AuthState::LoggedOut);
        assert!(status.needs_login());
        assert!(status.summary().contains("/login"), "{}", status.summary());
    }

    /// Verbatim from `codex login status`.
    #[test]
    fn codex_prose_is_read_into_a_status() {
        let status = AuthStatus::read(Agent::Codex, "Logged in using ChatGPT", true);
        assert_eq!(status.state, AuthState::LoggedIn);
        assert_eq!(status.method.as_deref(), Some("ChatGPT"));
    }

    /// "not logged in" contains "logged in", so order of checks decides this.
    #[test]
    fn codex_negatives_are_not_read_as_positives() {
        for text in ["Not logged in", "You are not logged in.", "Logged out"] {
            let status = AuthStatus::read(Agent::Codex, text, true);
            assert_eq!(status.state, AuthState::LoggedOut, "{text:?}");
            assert!(status.summary().contains("codex login"));
        }
    }

    #[test]
    fn unrecognized_output_is_unknown_rather_than_a_guess() {
        for (agent, text) in [
            (Agent::Claude, "not json at all"),
            (Agent::Codex, "something else entirely"),
        ] {
            let status = AuthStatus::read(agent, text, true);
            assert_eq!(status.state, AuthState::Unknown, "{agent}");
            assert!(!status.is_logged_in());
            // Crucially not `needs_login`: an unreadable answer is not evidence
            // that someone has to log in.
            assert!(!status.needs_login(), "{agent}");
        }
    }

    /// Copilot exposes no status command, and saying "logged out" for an agent
    /// that cannot be asked would send someone to fix a working setup.
    #[tokio::test]
    async fn copilot_reports_that_it_cannot_be_checked() {
        let status = AuthStatus::check_bin(Agent::Copilot, "copilot")
            .await
            .expect("an uncheckable agent is not an error");
        assert_eq!(status.state, AuthState::Unknown);
        assert!(!status.needs_login());
        assert!(
            status.detail.contains("no status command"),
            "{}",
            status.detail
        );
    }

    #[test]
    fn only_claude_and_codex_can_be_asked() {
        assert!(Agent::Claude.auth_status_argv().is_some());
        assert!(Agent::Codex.auth_status_argv().is_some());
        assert!(Agent::Copilot.auth_status_argv().is_none());
    }
}
