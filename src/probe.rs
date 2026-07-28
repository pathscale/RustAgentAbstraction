//! Asking a CLI what version it is, and whether this crate was built for it.
//!
//! Every flag mapping here was verified against a specific release. Those
//! releases move: `codex exec resume` accepts `--sandbox` in some versions and
//! rejects it outright in 0.145.0, and Copilot gained a headless session id and
//! an event stream that an older wrapper still models as absent. Without a
//! version check, that drift is discovered by a run failing halfway through with
//! "unexpected argument", which names a flag rather than the cause.
//!
//! This module makes the check available up front. It is **not** run
//! automatically: probing spawns a process, and paying that on every request to
//! guard against an occasional upstream change is the wrong trade. A host
//! should probe once at startup, or when a run fails with
//! [`crate::Error::FlagRejected`], and surface the result to whoever can act on
//! it.

use std::fmt;

use crate::agent::Agent;
use crate::error::{Error, Result};

/// A three-part version, compared numerically.
///
/// Deliberately not a `semver` dependency: these CLIs report a plain dotted
/// triple inside prose ("codex-cli 0.145.0", "2.1.205 (Claude Code)"), none of
/// them publishes pre-release or build metadata here, and a whole crate to
/// compare three integers is not worth the graph.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    /// Breaking-change component.
    pub major: u32,
    /// Feature component.
    pub minor: u32,
    /// Fix component.
    pub patch: u32,
}

impl Version {
    /// Find the first dotted triple anywhere in `text`.
    ///
    /// Each CLI wraps its version in different prose, so this scans rather than
    /// parsing a fixed shape: `2.1.205 (Claude Code)`, `codex-cli 0.145.0`, and
    /// `GitHub Copilot CLI 1.0.75.` all yield their number.
    #[must_use]
    pub fn find(text: &str) -> Option<Version> {
        let bytes = text.as_bytes();
        let mut start = 0;
        while start < bytes.len() {
            if bytes[start].is_ascii_digit()
                // Only start at a boundary, so `1.0.75` inside `abc1.0.75` is
                // not read from the middle of a token.
                && (start == 0 || !bytes[start - 1].is_ascii_digit() && bytes[start - 1] != b'.')
                && let Some(version) = Version::parse_at(&text[start..])
            {
                return Some(version);
            }
            start += 1;
        }
        None
    }

    /// Parse a triple anchored at the start of `text`, ignoring any trailing
    /// prose.
    fn parse_at(text: &str) -> Option<Version> {
        let mut parts = [0u32; 3];
        let mut rest = text;
        for (index, part) in parts.iter_mut().enumerate() {
            if index > 0 {
                rest = rest.strip_prefix('.')?;
            }
            let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
            if digits.is_empty() {
                return None;
            }
            *part = digits.parse().ok()?;
            rest = &rest[digits.len()..];
        }
        Some(Version {
            major: parts[0],
            minor: parts[1],
            patch: parts[2],
        })
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// How an installed CLI relates to the release this crate was verified against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VersionStatus {
    /// Exactly the verified release. Mappings are known good.
    Verified,
    /// Newer. The likely direction of drift: flags may have been renamed,
    /// replaced, or moved between subcommands.
    Newer,
    /// Older. Flags this crate relies on may not exist yet.
    Older,
    /// The CLI answered, but no version could be read out of it.
    Unrecognized,
}

impl VersionStatus {
    /// Whether mappings are known good for this version.
    #[must_use]
    pub fn is_verified(self) -> bool {
        self == VersionStatus::Verified
    }
}

/// What an installed agent CLI reported about itself.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct Probe {
    /// The agent probed.
    pub agent: Agent,
    /// The binary that answered.
    pub bin: String,
    /// Its `--version` output, trimmed.
    pub reported: String,
    /// The version read out of that output, if one could be.
    pub version: Option<Version>,
    /// The release this crate's mappings were verified against.
    pub verified: Version,
    /// How the two relate.
    pub status: VersionStatus,
}

impl Probe {
    /// Ask `agent`'s default binary for its version.
    ///
    /// # Errors
    /// [`Error::NotInstalled`] if the binary is missing, [`Error::Spawn`] if it
    /// cannot be run.
    pub async fn run(agent: Agent) -> Result<Probe> {
        Probe::run_bin(agent, agent.bin()).await
    }

    /// Ask a specific binary for its version, for a caller that overrides the
    /// path with [`crate::Request::bin`].
    ///
    /// # Errors
    /// [`Error::NotInstalled`] if the binary is missing, [`Error::Spawn`] if it
    /// cannot be run.
    pub async fn run_bin(agent: Agent, bin: &str) -> Result<Probe> {
        let output = tokio::process::Command::new(bin)
            .arg("--version")
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

        // Some CLIs print their version to stderr; take whichever answered.
        let mut reported = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if reported.is_empty() {
            reported = String::from_utf8_lossy(&output.stderr).trim().to_string();
        }

        let verified = agent.verified_version();
        let version = Version::find(&reported);
        let status = match version {
            None => VersionStatus::Unrecognized,
            Some(found) if found == verified => VersionStatus::Verified,
            Some(found) if found > verified => VersionStatus::Newer,
            Some(_) => VersionStatus::Older,
        };

        Ok(Probe {
            agent,
            bin: bin.to_string(),
            reported,
            version,
            verified,
            status,
        })
    }

    /// A sentence explaining a non-verified version, or `None` when it matches.
    ///
    /// Written to be shown to a person: it says what was found, what was
    /// expected, and what that means for them.
    #[must_use]
    pub fn advisory(&self) -> Option<String> {
        let agent = self.agent;
        let verified = self.verified;
        match self.status {
            VersionStatus::Verified => None,
            VersionStatus::Newer => Some(format!(
                "{agent} is newer than the {verified} this crate's flags were verified \
                 against ({}). Flags may have been renamed or moved between subcommands; \
                 a run failing with an unexpected-argument error is the likely symptom.",
                self.version
                    .map_or_else(|| "unknown".into(), |v| v.to_string()),
            )),
            VersionStatus::Older => Some(format!(
                "{agent} is older than the {verified} this crate's flags were verified \
                 against ({}). Flags this crate relies on may not exist in it yet.",
                self.version
                    .map_or_else(|| "unknown".into(), |v| v.to_string()),
            )),
            VersionStatus::Unrecognized => Some(format!(
                "could not read a version out of what {agent} reported ({:?}), so its flags \
                 cannot be checked against the verified {verified}.",
                self.reported,
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The real strings, as each CLI actually prints them.
    #[test]
    fn versions_are_found_inside_each_cli_s_own_prose() {
        assert_eq!(
            Version::find("2.1.205 (Claude Code)"),
            Some(Version {
                major: 2,
                minor: 1,
                patch: 205
            })
        );
        assert_eq!(
            Version::find("codex-cli 0.145.0"),
            Some(Version {
                major: 0,
                minor: 145,
                patch: 0
            })
        );
        // Note the trailing period, which must not be read as another part.
        assert_eq!(
            Version::find("GitHub Copilot CLI 1.0.75."),
            Some(Version {
                major: 1,
                minor: 0,
                patch: 75
            })
        );
    }

    #[test]
    fn text_without_a_triple_yields_nothing() {
        for text in ["", "no version here", "1.2", "v1", "beta"] {
            assert_eq!(Version::find(text), None, "{text:?}");
        }
    }

    /// A version embedded in a longer token must not be read from its middle.
    #[test]
    fn a_triple_is_only_read_from_a_token_boundary() {
        assert_eq!(
            Version::find("build20251.0.75"),
            Some(Version {
                major: 20251,
                minor: 0,
                patch: 75
            }),
            "the whole leading number belongs to the version"
        );
    }

    #[test]
    fn versions_order_numerically_not_lexically() {
        let v = |major, minor, patch| Version {
            major,
            minor,
            patch,
        };
        // The case a string comparison gets wrong: 205 > 99.
        assert!(v(2, 1, 205) > v(2, 1, 99));
        assert!(v(0, 145, 0) > v(0, 99, 9));
        assert!(v(1, 0, 0) > v(0, 999, 999));
    }

    #[test]
    fn every_agent_declares_a_parseable_verified_version() {
        for agent in Agent::ALL {
            let verified = agent.verified_version();
            assert!(verified.major > 0 || verified.minor > 0, "{agent}");
        }
    }

    #[tokio::test]
    async fn probing_a_missing_binary_says_how_to_install_it() {
        let err = Probe::run_bin(Agent::Claude, "agent-abstraction-no-such-binary")
            .await
            .unwrap_err();
        assert!(matches!(err, Error::NotInstalled { .. }), "{err:?}");
    }
}
