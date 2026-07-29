//! Putting a human in the loop for tool calls the agent wants to make.
//!
//! Every [`crate::Permission`] posture answers the approval question up front,
//! which is what lets a headless run finish unattended: a gated call is
//! pre-approved or auto-denied and the model carries on. That is wrong for a
//! desktop app, where the point is to *ask*.
//!
//! [`crate::Request::approvals`] switches a run to asking. Gated calls arrive as
//! [`crate::Event::ApprovalRequest`] and the run waits, mid-turn, until
//! [`crate::Run::respond`] answers. Deciding stays entirely with the caller;
//! this crate carries the question out and the answer back.
//!
//! # Claude only
//!
//! Verified against claude 2.1.212. Codex `exec` has no approval callback: its
//! sandbox mode *is* the answer, decided before the run starts. Copilot needs
//! `--allow-all-tools` to run headlessly at all, and gates only through
//! `--deny-tool`. Asking either for approvals is
//! [`crate::Error::Unsupported`] rather than a run that quietly never asks.
//!
//! # A run that asks must be streamed
//!
//! [`crate::run`] waits for an outcome and hands back no events, so nobody could
//! answer. Requesting approvals there is [`crate::Error::Unsupported`] too,
//! raised before spawning rather than discovered as a hang.
//!
//! # What "gated" means is the agent's decision, not this crate's
//!
//! Claude decides which calls need asking, and read-only commands are allowed
//! without one: verified on 2.1.212, `whoami` runs unasked while
//! `touch some-file` asks. So a caller must not treat the absence of a request
//! as proof that nothing ran.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A tool call the agent is waiting for permission to make.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Approval {
    /// The agent's own id for this question. Pass it back to
    /// [`crate::Run::respond`]; an answer carrying any other id is ignored by
    /// the agent, which then keeps waiting.
    pub id: String,
    /// The tool being asked about, e.g. `Bash` or `Write`.
    pub tool: String,
    /// The arguments it would be called with, exactly as the agent sent them.
    ///
    /// **Show this to the user before they decide.** For `Bash` it carries the
    /// command; approving on the tool name alone approves an unseen command.
    pub input: Value,
}

/// What to do about an [`Approval`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Decision {
    /// Let the call proceed as asked.
    Allow,
    /// Refuse it. The turn continues: the model is told no and works around it,
    /// so a denial is not a failed run. Claude also lists every refusal in its
    /// terminal record.
    Deny {
        /// Shown to the model as the reason. Worth writing something useful,
        /// since the model reads it and may explain the refusal to the user.
        reason: String,
    },
}

impl Decision {
    /// Refuse with a stock reason, for a caller that has nothing to add.
    #[must_use]
    pub fn deny() -> Decision {
        Decision::Deny {
            reason: "the user declined this action".into(),
        }
    }

    /// The wire form Claude expects on stdin.
    ///
    /// Verified against claude 2.1.212, which answers a `can_use_tool` control
    /// request with a `control_response` carrying `behavior` of `allow` or
    /// `deny`.
    pub(crate) fn wire(&self, id: &str) -> String {
        let response = match self {
            Decision::Allow => serde_json::json!({"behavior": "allow"}),
            Decision::Deny { reason } => {
                serde_json::json!({"behavior": "deny", "message": reason})
            }
        };
        format!(
            "{}\n",
            serde_json::json!({
                "type": "control_response",
                "response": {
                    "request_id": id,
                    "subtype": "success",
                    "response": response,
                },
            })
        )
    }
}

/// The handshake that tells Claude this client will answer approval questions.
///
/// Without it the `can_use_tool` requests never arrive and gated calls resolve
/// on their own, so a caller would see a run that silently never asked.
/// Verified against claude 2.1.212.
pub(crate) fn handshake() -> String {
    format!(
        "{}\n",
        serde_json::json!({
            "type": "control_request",
            "request_id": "agent-abstraction-init",
            "request": {"subtype": "initialize"},
        })
    )
}

/// Wrap a prompt as the stream-json user message Claude reads from stdin.
///
/// Under `--input-format stream-json` the prompt cannot ride the argv, so this
/// is how it is delivered. Verified against claude 2.1.212.
pub(crate) fn user_message(prompt: &str) -> String {
    format!(
        "{}\n",
        serde_json::json!({
            "type": "user",
            "message": {"role": "user", "content": [{"type": "text", "text": prompt}]},
        })
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_decision_serializes_to_the_shape_claude_answers() {
        let allow: Value =
            serde_json::from_str(Decision::Allow.wire("req-1").trim()).expect("json");
        assert_eq!(allow["type"], "control_response");
        assert_eq!(allow["response"]["request_id"], "req-1");
        assert_eq!(allow["response"]["subtype"], "success");
        assert_eq!(allow["response"]["response"]["behavior"], "allow");

        let deny: Value =
            serde_json::from_str(Decision::deny().wire("req-2").trim()).expect("json");
        assert_eq!(deny["response"]["response"]["behavior"], "deny");
        assert!(
            deny["response"]["response"]["message"]
                .as_str()
                .is_some_and(|m| !m.is_empty()),
            "a denial should carry a reason the model can read"
        );
    }

    /// A reason with a quote or a newline must not break the line-delimited
    /// protocol, which is why this is built with serde rather than formatted.
    #[test]
    fn an_awkward_reason_stays_one_json_line() {
        let decision = Decision::Deny {
            reason: "no \"rm -rf\" here\nand no newlines either".into(),
        };
        let wire = decision.wire("req-3");
        assert_eq!(
            wire.matches('\n').count(),
            1,
            "exactly one trailing newline"
        );
        let parsed: Value = serde_json::from_str(wire.trim()).expect("still valid json");
        assert!(
            parsed["response"]["response"]["message"]
                .as_str()
                .expect("message")
                .contains("rm -rf"),
            "the reason survives intact"
        );
    }

    #[test]
    fn a_prompt_with_control_characters_survives_the_wrapper() {
        let wire = user_message("say \"ok\"\nthen stop");
        assert_eq!(wire.matches('\n').count(), 1);
        let parsed: Value = serde_json::from_str(wire.trim()).expect("json");
        assert_eq!(parsed["type"], "user");
        assert_eq!(
            parsed["message"]["content"][0]["text"],
            "say \"ok\"\nthen stop"
        );
    }

    #[test]
    fn the_handshake_is_one_valid_line() {
        let wire = handshake();
        assert_eq!(wire.matches('\n').count(), 1);
        let parsed: Value = serde_json::from_str(wire.trim()).expect("json");
        assert_eq!(parsed["type"], "control_request");
        assert_eq!(parsed["request"]["subtype"], "initialize");
    }
}
