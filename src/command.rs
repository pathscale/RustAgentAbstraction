//! Slash commands: the CLI's own verbs, addressed as values rather than text.
//!
//! Claude Code answers a set of commands that are not prompts. `/compact`
//! summarises the conversation and continues from the summary; `/clear` throws
//! it away. They travel the same channel as a prompt, which is exactly why a
//! host should not have to build one by hand: `"/compact"` typed into a string
//! literal is indistinguishable from a user who meant to say the word, and a
//! typo produces a turn where the model earnestly discusses the command it was
//! sent rather than the command running.
//!
//! # A command is its own turn
//!
//! Verified against claude 2.1.212. Sending `/compact` produces a complete,
//! self-contained turn:
//!
//! ```text
//! system/status           status=compacting
//! system/status           compact_result=success
//! system/init             the session re-initialises
//! system/compact_boundary where the summary begins
//! result                  is_error=false, num_turns=0, result=""
//! ```
//!
//! So a command belongs in a run of its own that resumes the session, not
//! injected into a turn already in flight with [`crate::Run::send`]. Injected,
//! its `result` record arrives *after* the turn's own and overwrites the
//! outcome: the answer's text becomes the compaction's empty string and the
//! turn's usage becomes the compaction's zeroes. The empty `result` is not a
//! failure, and neither is `num_turns: 0` — a compaction generates no answer.
//! [`crate::Event::Compaction`] is what reports whether it worked.

use serde::{Deserialize, Serialize};

/// A slash command a run can carry instead of a prompt.
///
/// `#[non_exhaustive]`: the CLI's vocabulary grows, and this names only the
/// commands whose behaviour has been verified. Anything else in the catalogue
/// reaches the agent through [`Command::Other`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum Command {
    /// Summarise the conversation so far and continue from the summary.
    ///
    /// The answer to a context window filling up. The optional instructions are
    /// passed to the summariser, for steering what survives: `Some("keep the
    /// API surface and the failing test")`.
    ///
    /// Refused with `Not enough messages to compact.` on a conversation too
    /// short to be worth summarising. That arrives as a completed run carrying
    /// [`crate::Event::Compaction`] with the reason, not as an error: the
    /// command ran and answered.
    Compact {
        /// What the summary should preserve. `None` leaves it to the agent.
        instructions: Option<String>,
    },
    /// Discard the conversation and start fresh, keeping the session.
    Clear,
    /// Any other command this install offers, named without its leading slash.
    ///
    /// The catalogue is per-install — skills, plugins and user commands all
    /// land in it — so it cannot be enumerated here honestly.
    /// [`crate::Event::Commands`] reports what the running agent actually has.
    Other(String),
}

impl Command {
    /// The text the agent reads, leading slash included.
    #[must_use]
    pub fn wire(&self) -> String {
        match self {
            Command::Compact {
                instructions: Some(how),
            } => format!("/compact {how}"),
            Command::Compact { instructions: None } => "/compact".to_string(),
            Command::Clear => "/clear".to_string(),
            // Trimmed of a slash the caller may have included, rather than
            // sending `//skill`, which the CLI reads as prose.
            Command::Other(name) => format!("/{}", name.trim_start_matches('/')),
        }
    }
}

/// What the running agent reports it can do, from its own catalogue.
///
/// Emitted as [`crate::Event::Commands`] when a run starts. Read from the
/// agent rather than compiled in, because the set is per-install: skills,
/// plugins and user-defined commands all appear, and a hardcoded list would
/// describe the developer's machine instead of the user's.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Commands {
    /// Every command, without leading slashes. Includes the skills below.
    pub all: Vec<String>,
    /// The subset that are skills rather than built-in utilities.
    ///
    /// Claude Code reports these separately, and the split is the one a user
    /// sees: a skill is a capability someone installed, a utility is part of
    /// the tool. [`Commands::utilities`] is the other half.
    pub skills: Vec<String>,
}

impl Commands {
    /// The built-in half: everything that is not a skill.
    #[must_use]
    pub fn utilities(&self) -> Vec<&str> {
        self.all
            .iter()
            .filter(|name| !self.skills.iter().any(|skill| skill == *name))
            .map(String::as_str)
            .collect()
    }

    /// Whether the agent offers a command, by name or with its slash.
    #[must_use]
    pub fn has(&self, name: &str) -> bool {
        let wanted = name.trim_start_matches('/');
        self.all.iter().any(|known| known == wanted)
    }
}

/// How far a `/compact` got.
///
/// Both arms arrive on a run that completed: a refused compaction is an answer,
/// not an error, so it is reported rather than raised.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum Compaction {
    /// The agent has begun summarising. A UI can say so; nothing else follows
    /// until it finishes.
    Started,
    /// The summary is in place, or was refused with a reason.
    Finished {
        /// Whether the conversation was actually compacted.
        ok: bool,
        /// Why not, when the agent said. `Not enough messages to compact.` is
        /// the common one.
        error: Option<String>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_carries_its_instructions_and_nothing_more() {
        assert_eq!(Command::Compact { instructions: None }.wire(), "/compact");
        assert_eq!(
            Command::Compact {
                instructions: Some("keep the failing test".into())
            }
            .wire(),
            "/compact keep the failing test"
        );
    }

    /// A caller who writes the slash should not send two.
    #[test]
    fn a_named_command_gets_exactly_one_slash() {
        assert_eq!(Command::Other("context".into()).wire(), "/context");
        assert_eq!(Command::Other("/context".into()).wire(), "/context");
    }

    /// The split the user sees, from the two lists the agent reports.
    #[test]
    fn utilities_are_the_commands_that_are_not_skills() {
        let commands = Commands {
            all: vec![
                "code-review".into(),
                "compact".into(),
                "context".into(),
                "verify".into(),
            ],
            skills: vec!["code-review".into(), "verify".into()],
        };
        assert_eq!(commands.utilities(), vec!["compact", "context"]);
        assert!(commands.has("compact"));
        assert!(commands.has("/compact"));
        assert!(!commands.has("nonesuch"));
    }
}
