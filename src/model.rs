//! Which models each agent offers, so a host can render a picker.
//!
//! The catalogue is **advisory and never enforced**. [`crate::Request::model`]
//! takes any string and this crate does not check it against anything here. A
//! model that shipped this morning must not be blocked by a list compiled last
//! month, and a picked model the account cannot reach fails as
//! [`crate::Error::AgentError`] carrying the provider's own status and wording.
//! Enforcing the list would trade a clear runtime error for a wrong compile-time
//! one.
//!
//! # A catalogue is not an entitlement
//!
//! What an agent *offers* and what an account may *use* are different sets, and
//! only the account knows the second one. On a Copilot Free plan the picker
//! lists twenty-three models and permits exactly one:
//!
//! ```text
//! Your Copilot Free plan currently includes only Auto, which automatically
//! selects the best available model for each task.
//! ```
//!
//! Every other id there is rejected before a request is made, including
//! `gpt-5.4`, the example in Copilot's own `--help`. So a host should present
//! this list as choices to try, not as promises, and let the run report what the
//! account actually allows. [`Model::is_default`] marks the one an agent falls
//! back to, which is the safe pre-selection.
//!
//! # Where the entries come from
//!
//! [`Agent::models`] is a compiled-in list with its provenance recorded in
//! [`Agent::models_verified`], because two of the three agents cannot be asked.
//! [`Agent::discover_models`] asks the CLI itself where that is possible, and
//! returns [`crate::Error::Unsupported`] where it is not, rather than quietly
//! handing back the compiled list under a name that promises freshness.

use std::borrow::Cow;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::agent::Agent;
use crate::error::{Error, Result};

/// Whether an id names a specific model or points at whichever is current.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Kind {
    /// Resolves to whatever is newest in a family, so it survives a release.
    /// Claude's `opus` and Copilot's `auto` are both this.
    Alias,
    /// Names one model. Reproducible, and goes stale on its own schedule.
    Pinned,
}

/// One model a caller can choose.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Model {
    /// Exactly what goes to `--model`. Passed through verbatim.
    pub id: Cow<'static, str>,
    /// The vendor's own display name, for a picker.
    pub name: Cow<'static, str>,
    /// One line on what it is for. Empty when the vendor offers none.
    pub note: Cow<'static, str>,
    /// Whether the id tracks a family or names one model.
    pub kind: Kind,
    /// Reasoning levels this model accepts, in the vendor's order, for
    /// [`crate::Request::effort`].
    ///
    /// Kept as strings for the same reason ids are, and the three agents make
    /// the case on their own: Claude documents five levels, Copilot seven, and
    /// Codex varies them per model, offering `ultra` on its two frontier models
    /// and not on the rest. A shared enum would have to be edited before a new
    /// level could even be named.
    ///
    /// Empty means a picker has nothing to offer for this model. That covers
    /// two cases, and the catalogue comments say which applies: the model
    /// genuinely accepts no level, as Copilot's `auto` does, or the levels are
    /// simply not established here. Neither is a promise that a level would be
    /// refused, since nothing in this crate validates against it.
    pub efforts: Vec<Cow<'static, str>>,
    /// Whether the agent uses this when the caller names no model.
    pub is_default: bool,
}

impl Model {
    /// Build a catalogue entry from static parts.
    fn new(
        id: &'static str,
        name: &'static str,
        note: &'static str,
        kind: Kind,
        efforts: &[&'static str],
        is_default: bool,
    ) -> Model {
        Model {
            id: Cow::Borrowed(id),
            name: Cow::Borrowed(name),
            note: Cow::Borrowed(note),
            kind,
            efforts: efforts.iter().map(|e| Cow::Borrowed(*e)).collect(),
            is_default,
        }
    }
}

/// How a catalogue was established, so a stale one can be recognised as stale.
///
/// Recorded rather than described in prose because the entries below were
/// gathered three different ways, and the weakest of them is the one a reader
/// most needs to know about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Verified {
    /// Where the list came from.
    pub source: Source,
    /// ISO date it was last checked.
    pub checked: &'static str,
    /// The CLI release it was checked against.
    pub against: &'static str,
}

/// The kind of evidence behind a catalogue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Source {
    /// The CLI itself reported it, and can be asked again at runtime. The
    /// strongest of the three: it cannot drift without the CLI changing.
    Cli,
    /// Read out of the CLI's interactive picker. Accurate when taken, but there
    /// is no way to re-read it without a terminal, so it ages silently.
    Picker,
    /// Taken from vendor documentation. Weakest: it describes the product
    /// rather than the installed binary, and says nothing about entitlement.
    Docs,
}

impl Agent {
    /// The models this agent offers, best first.
    ///
    /// Advisory: this is not enforced, and it does not tell you what an account
    /// may actually use. See [`Model`] and [`Agent::models_verified`].
    #[must_use]
    pub fn models(&self) -> Vec<Model> {
        match self {
            Agent::Claude => claude_models(),
            Agent::Codex => codex_models(),
            Agent::Copilot => copilot_models(),
        }
    }

    /// How this agent's compiled-in catalogue was established, and when.
    #[must_use]
    pub fn models_verified(&self) -> Verified {
        match self {
            // Mixed: the five aliases were read from the `/model` picker, but
            // the pinned ids and the `best` / `opusplan` / `[1m]` entries come
            // from documentation. `source` records the weakest evidence behind
            // any entry, since that is the one a reader needs to distrust.
            Agent::Claude => Verified {
                source: Source::Docs,
                checked: "2026-07-30",
                against: "claude 2.1.212",
            },
            Agent::Codex => Verified {
                source: Source::Cli,
                checked: "2026-07-29",
                against: "codex-cli 0.145.0",
            },
            // Read from the `/model` picker. Copilot has no headless list; see
            // `discover_models`.
            Agent::Copilot => Verified {
                source: Source::Picker,
                checked: "2026-07-29",
                against: "Copilot CLI 1.0.75",
            },
        }
    }

    /// Ask the installed CLI what models it has, rather than trusting the
    /// compiled-in list.
    ///
    /// Worth preferring wherever it works: it reflects the binary actually
    /// present instead of the one this crate was written against.
    ///
    /// # Errors
    /// [`Error::Unsupported`] on an agent with no headless way to answer, which
    /// today is Claude and Copilot. That is deliberately an error rather than a
    /// silent fall back to [`Agent::models`]: a caller asking for discovery is
    /// asking for freshness, and handing back a compiled list without saying so
    /// answers a question they did not ask. [`Error::NotInstalled`] if the
    /// binary is missing, [`Error::Spawn`] if it cannot be run, and
    /// [`Error::Parse`] if its output is not the expected shape.
    pub async fn discover_models(&self) -> Result<Vec<Model>> {
        match self {
            Agent::Codex => discover_codex(self.bin()).await,
            // Neither can be asked without a terminal, verified against
            // Copilot CLI 1.0.75 and claude 2.1.212. Copilot has no `models`
            // subcommand, rejects an unknown `--model` without listing the valid
            // ones, and its ACP `session/new` reply carries session modes and
            // permissions but no models. Claude documents its aliases in
            // `--help` but has no subcommand that enumerates them. In both the
            // interactive `/model` picker is the only listing.
            Agent::Claude | Agent::Copilot => Err(Error::Unsupported {
                agent: *self,
                what: "listing models without a terminal",
            }),
        }
    }
}

/// Claude, aliases first.
///
/// The aliases are the better picker entries and are listed first for that
/// reason: they resolve to whatever is current, so they survive a model release
/// and respect what the account is entitled to, which a pinned id does neither
/// of. Pinned ids follow for a caller who needs one exact model.
///
/// Verified against claude 2.1.212 (`--help`) and the published model list,
/// 2026-07-29.
/// Claude's effort levels, verified from `claude --help` on 2.1.212:
/// `--effort <level>` (low, medium, high, xhigh, max).
///
/// Session-level rather than per-model, so every entry carries the same set:
/// `--help` does not vary the choices by model, and a picker reading
/// [`Model::efforts`] for the selected model gets the right answer either way.
const CLAUDE_EFFORTS: &[&str] = &["low", "medium", "high", "xhigh", "max"];

fn claude_models() -> Vec<Model> {
    let mut models = claude_aliases();
    models.extend(claude_pinned());
    models
}

/// The aliases, which are what the `/model` picker offers.
fn claude_aliases() -> Vec<Model> {
    vec![
        Model::new(
            "default",
            "Default",
            "Whatever is recommended for this account, or the organization default",
            Kind::Alias,
            CLAUDE_EFFORTS,
            true,
        ),
        Model::new(
            "opus",
            "Opus",
            "Latest Opus, for complex reasoning",
            Kind::Alias,
            CLAUDE_EFFORTS,
            false,
        ),
        Model::new(
            "sonnet",
            "Sonnet",
            "Latest Sonnet, for daily coding",
            Kind::Alias,
            CLAUDE_EFFORTS,
            false,
        ),
        Model::new(
            "haiku",
            "Haiku",
            "Fast and efficient, for simple tasks",
            Kind::Alias,
            CLAUDE_EFFORTS,
            false,
        ),
        Model::new(
            "fable",
            "Fable",
            "For the hardest and longest-running tasks (1M context)",
            Kind::Alias,
            CLAUDE_EFFORTS,
            false,
        ),
        Model::new(
            "best",
            "Best available",
            "Fable where the organization has it, otherwise the latest Opus",
            Kind::Alias,
            CLAUDE_EFFORTS,
            false,
        ),
        // Not models. Accepted by `--model` and offered here for that reason,
        // but a picker that shows them beside the rest will mislead: one swaps
        // model mid-session and the others only widen the context window.
        Model::new(
            "opusplan",
            "Opus, then Sonnet",
            "Opus while planning, Sonnet to execute (a mode, not a model)",
            Kind::Alias,
            CLAUDE_EFFORTS,
            false,
        ),
        // The `[1m]` suffix widens the context window without changing the
        // model. Verified by running each on claude 2.1.212 (2026-07-30): the
        // terminal record reports `contextWindow: 1000000`, keyed by the
        // suffixed id (`claude-sonnet-5[1m]`). The suffix also composes with a
        // pinned id: `claude-opus-5[1m]` ran and reported 1M. `fable[1m]` is
        // accepted too, resolving to plain `claude-fable-5` at 1M, since Fable
        // is 1M natively and needs no suffix.
        Model::new(
            "opus[1m]",
            "Opus (1M context)",
            "Opus with a 1M token context window (a variant, not a model)",
            Kind::Alias,
            CLAUDE_EFFORTS,
            false,
        ),
        Model::new(
            "sonnet[1m]",
            "Sonnet (1M context)",
            "Sonnet with a 1M token context window (a variant, not a model)",
            Kind::Alias,
            CLAUDE_EFFORTS,
            false,
        ),
    ]
}

/// Pinned ids, which the picker does not list at all.
///
/// Its own subtitle says so: "For other/previous model names, specify with
/// `--model`". They are still worth carrying, because an alias and a pinned id
/// do not always agree. Verified on 2026-07-29 against claude 2.1.212 by running
/// both: `--model opus` reported `claude-opus-4-8` in its usage while
/// `--model claude-opus-5` reported `claude-opus-5`, even though that release's
/// own notes call Opus 5 "now the default Opus model". An alias is whatever the
/// account resolves it to, which is not always the newest model.
fn claude_pinned() -> Vec<Model> {
    vec![
        // Windows verified by running each id on claude 2.1.212 (2026-07-30).
        // `claude-opus-5` is the odd one out: every other 5-series model is 1M
        // natively, while it defaults to 200k and needs the suffix. Both forms
        // are catalogued so a picker can offer the choice explicitly.
        Model::new(
            "claude-opus-5",
            "Claude Opus 5",
            "For complex agentic coding and enterprise work (200k context)",
            Kind::Pinned,
            CLAUDE_EFFORTS,
            false,
        ),
        Model::new(
            "claude-opus-5[1m]",
            "Claude Opus 5 (1M context)",
            "Opus 5 with a 1M token context window",
            Kind::Pinned,
            CLAUDE_EFFORTS,
            false,
        ),
        Model::new(
            "claude-sonnet-5",
            "Claude Sonnet 5",
            "The best combination of speed and intelligence (1M context)",
            Kind::Pinned,
            CLAUDE_EFFORTS,
            false,
        ),
        Model::new(
            "claude-fable-5",
            "Claude Fable 5",
            "Next-generation intelligence for long-running agents (1M context)",
            Kind::Pinned,
            CLAUDE_EFFORTS,
            false,
        ),
        Model::new(
            "claude-haiku-4-5",
            "Claude Haiku 4.5",
            "The fastest model with near-frontier intelligence",
            Kind::Pinned,
            CLAUDE_EFFORTS,
            false,
        ),
    ]
}

/// Codex, in the priority order the CLI itself reports.
///
/// Verified by running `codex debug models` against codex-cli 0.145.0 on
/// 2026-07-29. `codex-auto-review` is reported with `visibility: "hide"` and is
/// left out for that reason; [`discover_codex`] applies the same filter.
fn codex_models() -> Vec<Model> {
    const FULL: &[&str] = &["low", "medium", "high", "xhigh", "max", "ultra"];
    const TO_MAX: &[&str] = &["low", "medium", "high", "xhigh", "max"];
    const TO_XHIGH: &[&str] = &["low", "medium", "high", "xhigh"];
    vec![
        Model::new(
            "gpt-5.6-sol",
            "GPT-5.6-Sol",
            "Latest frontier agentic coding model.",
            Kind::Pinned,
            FULL,
            true,
        ),
        Model::new(
            "gpt-5.6-terra",
            "GPT-5.6-Terra",
            "Balanced agentic coding model for everyday work.",
            Kind::Pinned,
            FULL,
            false,
        ),
        Model::new(
            "gpt-5.6-luna",
            "GPT-5.6-Luna",
            "Fast and affordable agentic coding model.",
            Kind::Pinned,
            TO_MAX,
            false,
        ),
        Model::new(
            "gpt-5.5",
            "GPT-5.5",
            "Frontier model for complex coding, research, and real-world tasks.",
            Kind::Pinned,
            TO_XHIGH,
            false,
        ),
        Model::new(
            "gpt-5.4",
            "GPT-5.4",
            "Strong model for everyday coding.",
            Kind::Pinned,
            TO_XHIGH,
            false,
        ),
        Model::new(
            "gpt-5.4-mini",
            "GPT-5.4-Mini",
            "Small, fast, and cost-efficient model for simpler coding tasks.",
            Kind::Pinned,
            TO_XHIGH,
            false,
        ),
    ]
}

/// Copilot, in the order its `/model` picker lists them.
///
/// Read from the interactive picker on Copilot CLI 1.0.75, 2026-07-29, because
/// nothing else enumerates them. Note that the picker lists every model the
/// product has, not every model the account may use: the same screen carried
/// "Your Copilot Free plan currently includes only Auto", and on that plan every
/// id below except `auto` is refused before a request is made.
/// Copilot's effort levels, verified from `copilot --help` on 1.0.75:
/// `--effort, --reasoning-effort <level>` (none, minimal, low, medium, high,
/// xhigh, max).
///
/// Two levels wider than Claude's at the bottom, which is why levels are passed
/// through rather than mapped onto a shared enum.
///
/// Applied to the pinned models only. `auto` rejects the flag outright, so
/// support is not uniform across an agent even when `--help` lists one set, and
/// these came from `--help` rather than from running each model: a Free plan
/// permits only `auto`, so there was no way to confirm the rest.
const COPILOT_EFFORTS: &[&str] = &["none", "minimal", "low", "medium", "high", "xhigh", "max"];

fn copilot_models() -> Vec<Model> {
    vec![
        Model::new(
            "auto",
            "Auto",
            "Copilot picks the best available model for each task",
            Kind::Alias,
            // Deliberately none. Verified on Copilot CLI 1.0.75 by running it:
            //   Error: Model "auto" does not support reasoning effort
            //   configuration (requested: "low").
            // It exits 1 rather than ignoring the flag, so offering a level for
            // `auto` in a picker produces a failed run, not a slower one.
            &[],
            true,
        ),
        pinned("claude-sonnet-5", "Claude Sonnet 5"),
        pinned("claude-sonnet-4.6", "Claude Sonnet 4.6"),
        pinned("claude-sonnet-4.5", "Claude Sonnet 4.5"),
        pinned("claude-haiku-4.5", "Claude Haiku 4.5"),
        pinned("claude-fable-5", "Claude Fable 5"),
        pinned("claude-opus-5", "Claude Opus 5"),
        pinned("claude-opus-4.8", "Claude Opus 4.8"),
        pinned("claude-opus-4.8-fast", "Claude Opus 4.8 (fast)"),
        pinned("claude-opus-4.7", "Claude Opus 4.7"),
        pinned("claude-opus-4.6", "Claude Opus 4.6"),
        pinned("claude-opus-4.5", "Claude Opus 4.5"),
        pinned("gpt-5.6-sol", "GPT-5.6-Sol"),
        pinned("gpt-5.6-terra", "GPT-5.6-Terra"),
        pinned("gpt-5.6-luna", "GPT-5.6-Luna"),
        pinned("gpt-5.5", "GPT-5.5"),
        pinned("gpt-5.4", "GPT-5.4"),
        pinned("gpt-5.3-codex", "GPT-5.3-Codex"),
        pinned("gpt-5.4-mini", "GPT-5.4-Mini"),
        pinned("gpt-5-mini", "GPT-5 mini"),
        pinned("gemini-3.1-pro-preview", "Gemini 3.1 Pro (preview)"),
        pinned("gemini-3.6-flash", "Gemini 3.6 Flash"),
        pinned("gemini-3.5-flash", "Gemini 3.5 Flash"),
        pinned("kimi-k2.7-code", "Kimi K2.7 Code"),
    ]
}

/// A pinned entry with no vendor description, which is every Copilot model: its
/// picker shows ids and nothing else.
fn pinned(id: &'static str, name: &'static str) -> Model {
    Model::new(id, name, "", Kind::Pinned, COPILOT_EFFORTS, false)
}

/// Read Codex's own model list.
///
/// `codex debug models` prints one JSON document carrying every model plus each
/// one's full system prompt, so the reply runs to hundreds of kilobytes. Only
/// the descriptive fields are kept.
async fn discover_codex(bin: &str) -> Result<Vec<Model>> {
    let output = tokio::process::Command::new(bin)
        .args(["debug", "models"])
        .output()
        .await
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

    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_codex_models(&stdout)
}

/// Turn `codex debug models` output into catalogue entries.
///
/// Split from the spawn so the shape can be tested without a subprocess.
fn parse_codex_models(stdout: &str) -> Result<Vec<Model>> {
    let value: Value = serde_json::from_str(stdout.trim()).map_err(|e| Error::Parse {
        agent: Agent::Codex,
        detail: format!("`codex debug models` did not return JSON: {e}"),
    })?;
    let listed = value
        .get("models")
        .and_then(Value::as_array)
        .ok_or_else(|| Error::Parse {
            agent: Agent::Codex,
            detail: "`codex debug models` returned no `models` array".into(),
        })?;

    // `priority` is the vendor's own display order and is not the array order,
    // so it is read rather than assumed.
    let mut ranked: Vec<(u64, Model)> = listed
        .iter()
        // `visibility` is how Codex marks its internal models, and
        // `codex-auto-review` is one. Offering it in a picker hands a user a
        // model the vendor deliberately withheld.
        .filter(|m| m.get("visibility").and_then(Value::as_str) != Some("hide"))
        .filter_map(|m| {
            let id = m.get("slug").and_then(Value::as_str)?;
            let model = Model {
                id: id.to_string().into(),
                name: m
                    .get("display_name")
                    .and_then(Value::as_str)
                    .unwrap_or(id)
                    .to_string()
                    .into(),
                note: m
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string()
                    .into(),
                kind: Kind::Pinned,
                efforts: m
                    .get("supported_reasoning_levels")
                    .and_then(Value::as_array)
                    .map(|levels| {
                        levels
                            .iter()
                            .filter_map(|l| l.get("effort").and_then(Value::as_str))
                            .map(|e| Cow::Owned(e.to_string()))
                            .collect()
                    })
                    .unwrap_or_default(),
                // Codex names a default reasoning level per model but never a
                // default model, so the top of its own ordering stands in.
                is_default: false,
            };
            let priority = m
                .get("priority")
                .and_then(Value::as_u64)
                .unwrap_or(u64::MAX);
            Some((priority, model))
        })
        .collect();

    if ranked.is_empty() {
        return Err(Error::Parse {
            agent: Agent::Codex,
            detail: "`codex debug models` listed no visible models".into(),
        });
    }
    ranked.sort_by_key(|(priority, _)| *priority);

    let mut models: Vec<Model> = ranked.into_iter().map(|(_, model)| model).collect();
    if let Some(first) = models.first_mut() {
        first.is_default = true;
    }
    Ok(models)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Trimmed from real `codex debug models` output (codex-cli 0.145.0). The
    /// hidden entry and the out-of-order priorities are both as reported.
    const CODEX_OUTPUT: &str = r#"{"models":[
      {"slug":"gpt-5.5","display_name":"GPT-5.5","description":"Frontier model.",
       "default_reasoning_level":"medium","visibility":"list","priority":7,
       "supported_reasoning_levels":[{"effort":"low"},{"effort":"medium"},{"effort":"high"}]},
      {"slug":"codex-auto-review","display_name":"Codex Auto Review","description":"Internal.",
       "visibility":"hide","priority":43,"supported_reasoning_levels":[{"effort":"low"}]},
      {"slug":"gpt-5.6-sol","display_name":"GPT-5.6-Sol","description":"Latest frontier model.",
       "default_reasoning_level":"low","visibility":"list","priority":1,
       "supported_reasoning_levels":[{"effort":"low"},{"effort":"ultra"}]}
    ]}"#;

    #[test]
    fn codex_discovery_reads_the_fields_a_picker_needs() {
        let models = parse_codex_models(CODEX_OUTPUT).expect("should parse");
        let sol = &models[0];
        assert_eq!(sol.id, "gpt-5.6-sol");
        assert_eq!(sol.name, "GPT-5.6-Sol");
        assert_eq!(sol.note, "Latest frontier model.");
        assert_eq!(sol.efforts, vec!["low", "ultra"]);
    }

    /// The array order is not the display order: `gpt-5.5` is listed first and
    /// carries priority 7, while `gpt-5.6-sol` is listed last at priority 1.
    #[test]
    fn codex_discovery_uses_the_vendors_ordering_not_the_array_order() {
        let models = parse_codex_models(CODEX_OUTPUT).expect("should parse");
        let ids: Vec<&str> = models.iter().map(|m| m.id.as_ref()).collect();
        assert_eq!(ids, ["gpt-5.6-sol", "gpt-5.5"]);
        assert!(
            models[0].is_default,
            "the top-priority model is the default"
        );
    }

    /// Codex marks its internal models `hide`. Offering one in a picker hands a
    /// user a model the vendor deliberately withheld.
    #[test]
    fn codex_discovery_drops_models_the_vendor_hides() {
        let models = parse_codex_models(CODEX_OUTPUT).expect("should parse");
        assert!(
            !models.iter().any(|m| m.id == "codex-auto-review"),
            "a hidden model must not reach a picker"
        );
    }

    #[test]
    fn unparseable_output_is_an_error_not_an_empty_list() {
        assert!(matches!(
            parse_codex_models("Reading additional input from stdin..."),
            Err(Error::Parse { .. })
        ));
        assert!(
            matches!(
                parse_codex_models(r#"{"models":[]}"#),
                Err(Error::Parse { .. })
            ),
            "an empty list means the shape changed, not that Codex has no models"
        );
    }

    /// Discovery must not quietly answer with the compiled-in list: a caller
    /// asking for it is asking for freshness, and a silent fallback answers a
    /// different question.
    #[tokio::test]
    async fn agents_that_cannot_be_asked_say_so() {
        for agent in [Agent::Claude, Agent::Copilot] {
            assert!(
                matches!(
                    agent.discover_models().await,
                    Err(Error::Unsupported { .. })
                ),
                "{agent} should report that it cannot enumerate models"
            );
        }
    }

    /// The gap this closes: every Claude and Copilot entry shipped with an
    /// empty `efforts` while both CLIs document a `--effort` flag, so a picker
    /// had nothing to offer.
    #[test]
    fn every_model_reports_the_levels_its_agent_accepts() {
        for agent in [Agent::Claude, Agent::Codex, Agent::Copilot] {
            for model in agent.models() {
                // `auto` is the documented exception, asserted below.
                if agent == Agent::Copilot && model.id == "auto" {
                    continue;
                }
                assert!(
                    !model.efforts.is_empty(),
                    "{agent} model {} reports no effort levels",
                    model.id
                );
            }
        }
    }

    /// Support is not uniform across an agent even where `--help` lists one set.
    /// Copilot exits 1 for an effort on `auto` rather than ignoring it, so
    /// offering a level there would produce a failed run.
    #[test]
    fn copilot_auto_offers_no_levels_because_it_refuses_them() {
        let models = Agent::Copilot.models();
        let auto = models.iter().find(|m| m.id == "auto").expect("auto");
        assert!(
            auto.efforts.is_empty(),
            "auto rejects the effort flag outright"
        );
        let pinned = models.iter().find(|m| m.id == "gpt-5.5").expect("gpt-5.5");
        assert!(
            !pinned.efforts.is_empty(),
            "pinned models do document levels"
        );
    }

    /// Verified from `claude --help` (2.1.212) and `copilot --help` (1.0.75).
    /// Copilot's set is two wider at the bottom, which is the whole reason
    /// levels are strings rather than a shared enum.
    #[test]
    fn the_documented_level_sets_are_not_interchangeable() {
        let claude = &Agent::Claude.models()[0].efforts;
        let copilot_models = Agent::Copilot.models();
        let copilot = &copilot_models
            .iter()
            .find(|m| m.id == "gpt-5.5")
            .expect("gpt-5.5")
            .efforts;
        assert_eq!(claude, &["low", "medium", "high", "xhigh", "max"]);
        assert_eq!(
            copilot,
            &["none", "minimal", "low", "medium", "high", "xhigh", "max"]
        );
        assert_ne!(claude, copilot, "a shared enum would have to cover both");
    }

    /// Codex is the one agent whose levels differ per model, which is why they
    /// live on the model rather than on the agent.
    #[test]
    fn codex_levels_differ_between_its_own_models() {
        let models = Agent::Codex.models();
        let by_id = |id: &str| -> Vec<String> {
            models
                .iter()
                .find(|m| m.id == id)
                .unwrap_or_else(|| panic!("{id} should be catalogued"))
                .efforts
                .iter()
                .map(ToString::to_string)
                .collect()
        };
        assert!(
            by_id("gpt-5.6-sol").contains(&"ultra".to_string()),
            "its frontier model offers ultra"
        );
        assert!(
            !by_id("gpt-5.6-luna").contains(&"ultra".to_string()),
            "its fast model does not"
        );
    }

    #[test]
    fn every_agent_offers_exactly_one_default() {
        for agent in [Agent::Claude, Agent::Codex, Agent::Copilot] {
            let defaults = agent.models().iter().filter(|m| m.is_default).count();
            assert_eq!(defaults, 1, "{agent} should mark exactly one default");
        }
    }

    #[test]
    fn no_catalogue_repeats_an_id() {
        for agent in [Agent::Claude, Agent::Codex, Agent::Copilot] {
            let models = agent.models();
            let mut ids: Vec<&str> = models.iter().map(|m| m.id.as_ref()).collect();
            ids.sort_unstable();
            let count = ids.len();
            ids.dedup();
            assert_eq!(ids.len(), count, "{agent} has a duplicate model id");
        }
    }

    /// The catalogue is a suggestion, not a gate. A model released after this
    /// list was compiled has to reach the command line untouched.
    #[test]
    fn an_unlisted_model_is_still_accepted() {
        let request = crate::Request::new(Agent::Claude, "hi").model("some-model-from-next-year");
        let argv = request
            .argv()
            .expect("an unlisted model must not be rejected");
        assert!(
            argv.windows(2)
                .any(|w| w[0] == "--model" && w[1] == "some-model-from-next-year"),
            "the model should reach the command line verbatim: {argv:?}"
        );
    }
}
