//! Normalizing three different JSON streams into one event vocabulary.
//!
//! Each agent narrates a run in its own shape. [`Parser`] is fed one output line
//! at a time and yields [`Event`]s a consumer can render without knowing which
//! agent produced them, while accumulating the terminal facts (session id, final
//! text, usage) into a [`Terminal`].
//!
//! Two distinct notions of text are kept apart on purpose:
//! - [`Event::Text`] is the *incremental display stream*, what a GUI appends to
//!   a transcript as it arrives.
//! - [`Terminal::text`] is the agent's own *authoritative final answer*, taken
//!   from its terminal record.
//!
//! Concatenating the deltas is not guaranteed to equal the final text (Copilot
//! emits both; Claude emits only the latter), so a caller that needs the answer
//! reads `Terminal::text` and never sums the events.
//!
//! Every shape here was captured from the live CLIs, except where a comment says
//! otherwise.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::agent::{Agent, Format};
use crate::outcome::{RateLimit, Stop, Usage};

/// One normalized thing an agent did, agent-agnostic so a single renderer works
/// across all three.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Event {
    /// The session is live. Emitted once, as early as the agent reveals it.
    Started {
        /// The native session id.
        session: String,
        /// The model actually selected, when named.
        model: Option<String>,
    },
    /// Reasoning text, where the agent exposes it.
    Thinking(String),
    /// Assistant text as it arrives.
    Text(String),
    /// The agent invoked a tool.
    ToolCall {
        /// Correlates with the matching [`Event::ToolResult`], when the agent
        /// provides an id.
        id: Option<String>,
        /// The tool's name.
        name: String,
        /// Its arguments, in the agent's own shape.
        input: Value,
    },
    /// A tool returned.
    ToolResult {
        /// Correlates with the originating [`Event::ToolCall`].
        id: Option<String>,
        /// Whether the tool reported success. `None` when the agent does not say.
        ok: Option<bool>,
        /// The observation the model saw.
        output: String,
    },
    /// A quota signal. Reported, never acted on.
    RateLimit(RateLimit),
}

/// The ceiling on any single captured buffer.
///
/// An agent can stream for hours; `text`, raw stdout and stderr would otherwise
/// grow without bound and a long run would end in an OOM rather than an answer.
/// A megabyte is far more prose than any consumer displays, and the fields this
/// bounds are for reading and diagnosis, never for reconstructing the stream.
pub const MAX_CAPTURE: usize = 1024 * 1024;

/// Append `line` and a newline to `buf`, stopping once [`MAX_CAPTURE`] is
/// reached. Returns whether anything was written.
///
/// Truncation keeps the *earliest* output, which is where a banner, a usage
/// error, or the start of an answer lives. Later output from a runaway agent is
/// the part worth dropping.
pub(crate) fn append_capped(buf: &mut String, line: &str) -> bool {
    let remaining = MAX_CAPTURE.saturating_sub(buf.len());
    if remaining == 0 {
        return false;
    }
    // `<` rather than `<=`, because the newline also has to fit.
    if line.len() < remaining {
        buf.push_str(line);
        buf.push('\n');
    } else {
        // Cut on a character boundary; a truncated buffer must stay valid UTF-8.
        let mut cut = remaining - 1;
        while cut > 0 && !line.is_char_boundary(cut) {
            cut -= 1;
        }
        buf.push_str(&line[..cut]);
        buf.push('\n');
    }
    true
}

/// Facts that are only known once the stream ends.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Terminal {
    /// The native session id.
    pub session: Option<String>,
    /// The agent's authoritative final answer.
    pub text: String,
    /// Token and cost accounting.
    pub usage: Usage,
    /// Why the agent stopped.
    pub stop: Stop,
    /// The last quota signal seen.
    pub rate_limit: Option<RateLimit>,
    /// How many output lines could not be parsed.
    ///
    /// Non-zero is not automatically a fault: agents interleave banners and
    /// warnings with their JSON. It matters when a run *also* came back empty,
    /// which is what a vendor changing its output shape looks like from here.
    pub unparsed: usize,
    /// The first line that failed to parse, as evidence for the above.
    pub first_unparsed: Option<String>,
}

/// Incrementally turns one agent's output into [`Event`]s and a [`Terminal`].
#[derive(Debug)]
pub(crate) struct Parser {
    agent: Agent,
    format: Format,
    term: Terminal,
    /// Tool names by call id, so a result can be attributed to its call.
    tools: HashMap<String, String>,
    /// True once a [`Event::Started`] has been emitted, so it fires only once.
    started: bool,
}

impl Parser {
    /// A parser for `agent` reading output in `format`.
    #[must_use]
    pub fn new(agent: Agent, format: Format) -> Self {
        Self {
            agent,
            format,
            term: Terminal::default(),
            tools: HashMap::new(),
            started: false,
        }
    }

    /// Feed one line of stdout, returning the events it produced.
    ///
    /// Unparseable lines yield nothing rather than failing the run: agents
    /// interleave banners and warnings with their JSON, and a stray line is not
    /// a reason to lose a completed turn. They are counted in
    /// [`Terminal::unparsed`] so that a silent vendor format change is
    /// diagnosable instead of merely producing an empty answer.
    pub fn push(&mut self, line: &str) -> Vec<Event> {
        let line = line.trim();
        if line.is_empty() {
            return Vec::new();
        }
        // Under a plain-text format there is nothing to parse: the whole stream
        // is the answer.
        if self.format == Format::Text {
            append_capped(&mut self.term.text, line);
            return vec![Event::Text(line.to_string())];
        }
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            self.term.unparsed += 1;
            if self.term.first_unparsed.is_none() {
                // One short sample is enough to identify a shape change; keeping
                // every stray line would reintroduce the unbounded growth this
                // parser just capped.
                let mut cut = line.len().min(512);
                while cut > 0 && !line.is_char_boundary(cut) {
                    cut -= 1;
                }
                self.term.first_unparsed = Some(line[..cut].to_string());
            }
            return Vec::new();
        };
        let mut out = match self.agent {
            Agent::Claude => self.claude(&value),
            Agent::Codex => self.codex(&value),
            Agent::Copilot => self.copilot(&value),
        };
        // Fire `Started` exactly once, from whichever record first revealed the
        // id, and put it ahead of that record's own events.
        if !self.started {
            if let Some(session) = self.term.session.clone() {
                self.started = true;
                out.insert(
                    0,
                    Event::Started {
                        session,
                        model: model_of(&value),
                    },
                );
            }
        }
        out
    }

    /// Consume the parser for everything only knowable at the end.
    #[must_use]
    pub fn finish(mut self) -> Terminal {
        if self.format == Format::Text {
            self.term.text = self.term.text.trim_end().to_string();
        }
        self.term
    }

    /// Claude Code `--output-format json` / `stream-json`.
    ///
    /// Verified against claude 2.1.205: `system/init` opens with the id,
    /// `assistant` records carry Anthropic content blocks, `rate_limit_event`
    /// reports quota, and `result` closes with the answer and usage.
    fn claude(&mut self, v: &Value) -> Vec<Event> {
        let ty = v.get("type").and_then(Value::as_str).unwrap_or_default();
        if let Some(id) = v.get("session_id").and_then(Value::as_str) {
            self.term.session.get_or_insert_with(|| id.to_string());
        }
        match ty {
            "rate_limit_event" => {
                let limit = claude_rate_limit(v.get("rate_limit_info"));
                self.term.rate_limit.clone_from(&limit);
                limit.into_iter().map(Event::RateLimit).collect()
            }
            // Both roles carry content blocks: `assistant` holds text/thinking/
            // tool_use, `user` carries the tool_result observations back.
            "assistant" | "user" => self.content_blocks(v),
            "result" => {
                if let Some(text) = v.get("result").and_then(Value::as_str) {
                    self.term.text = text.to_string();
                }
                self.term.usage = claude_usage(v);
                self.term.stop = if v.get("is_error").and_then(Value::as_bool) == Some(true) {
                    Stop::Error
                } else {
                    stop_from(v.get("stop_reason"))
                };
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    /// Anthropic content blocks, shared by Claude's `assistant` and `user`
    /// records.
    fn content_blocks(&mut self, v: &Value) -> Vec<Event> {
        let blocks = v
            .get("message")
            .and_then(|m| m.get("content"))
            .and_then(Value::as_array);
        let Some(blocks) = blocks else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for block in blocks {
            let ty = block
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or_default();
            match ty {
                "text" => {
                    if let Some(t) = block.get("text").and_then(Value::as_str) {
                        out.push(Event::Text(t.to_string()));
                    }
                }
                "thinking" => {
                    if let Some(t) = block.get("thinking").and_then(Value::as_str) {
                        out.push(Event::Thinking(t.to_string()));
                    }
                }
                "tool_use" => {
                    let name = block
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("tool")
                        .to_string();
                    let id = block.get("id").and_then(Value::as_str).map(str::to_string);
                    if let Some(id) = &id {
                        self.tools.insert(id.clone(), name.clone());
                    }
                    out.push(Event::ToolCall {
                        id,
                        name,
                        input: block.get("input").cloned().unwrap_or(Value::Null),
                    });
                }
                "tool_result" => out.push(Event::ToolResult {
                    id: block
                        .get("tool_use_id")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    ok: block
                        .get("is_error")
                        .and_then(Value::as_bool)
                        .map(|is_error| !is_error),
                    output: flatten_text(block.get("content")),
                }),
                _ => {}
            }
        }
        out
    }

    /// Codex `exec --json`.
    ///
    /// Verified against codex-cli 0.145.0: `thread.started` opens with
    /// `thread_id`, items arrive as `item.started` → `item.completed` pairs, and
    /// `turn.completed` carries usage. A tool item appears twice: once
    /// in-progress with an empty `aggregated_output`, once finished, so the
    /// call is emitted on first sighting and the result only once it completes.
    fn codex(&mut self, v: &Value) -> Vec<Event> {
        let ty = v.get("type").and_then(Value::as_str).unwrap_or_default();
        if let Some(id) = v.get("thread_id").and_then(Value::as_str) {
            self.term.session.get_or_insert_with(|| id.to_string());
        }
        match ty {
            "turn.completed" => {
                self.term.usage = codex_usage(v.get("usage"));
                Vec::new()
            }
            "turn.failed" => {
                self.term.stop = Stop::Error;
                Vec::new()
            }
            "item.started" | "item.updated" | "item.completed" => {
                let Some(item) = v.get("item") else {
                    return Vec::new();
                };
                let item_ty = item.get("type").and_then(Value::as_str).unwrap_or_default();
                let id = item.get("id").and_then(Value::as_str).map(str::to_string);
                let done = ty == "item.completed";

                // Every item is reported at least twice: in progress, then
                // finished. Announce each one exactly once, on first sighting,
                // and keep the id → name binding for the result.
                let name = tool_name(item, item_ty);
                let first = id
                    .as_ref()
                    .is_none_or(|id| self.tools.insert(id.clone(), name.clone()).is_none());

                match item_ty {
                    // The settled text is authoritative; a turn may contain
                    // several messages, so the last one to complete wins.
                    "agent_message" => {
                        if !done {
                            return Vec::new();
                        }
                        let text = item.get("text").and_then(Value::as_str).unwrap_or_default();
                        self.term.text = text.to_string();
                        vec![Event::Text(text.to_string())]
                    }
                    "reasoning" if done => item
                        .get("text")
                        .and_then(Value::as_str)
                        .map(|t| Event::Thinking(t.to_string()))
                        .into_iter()
                        .collect(),
                    "command_execution" | "mcp_tool_call" | "file_change" | "web_search" => {
                        let mut out = Vec::new();
                        if first {
                            out.push(Event::ToolCall {
                                id: id.clone(),
                                name,
                                input: codex_tool_input(item, item_ty),
                            });
                        }
                        // Only the finished record carries real output: the
                        // in-progress one has an empty string and a null code.
                        if done {
                            out.push(Event::ToolResult {
                                id,
                                ok: item
                                    .get("exit_code")
                                    .and_then(Value::as_i64)
                                    .map(|code| code == 0),
                                output: item
                                    .get("aggregated_output")
                                    .and_then(Value::as_str)
                                    .unwrap_or_default()
                                    .to_string(),
                            });
                        }
                        out
                    }
                    _ => Vec::new(),
                }
            }
            _ => Vec::new(),
        }
    }

    /// Copilot `--output-format json` (JSONL).
    ///
    /// Verified against GitHub Copilot CLI 1.0.75: `assistant.message_delta`
    /// streams text, `assistant.message` carries the settled answer,
    /// `tool.execution_start` / `_complete` bracket a tool, and the final
    /// `result` carries `sessionId` and usage.
    fn copilot(&mut self, v: &Value) -> Vec<Event> {
        let ty = v.get("type").and_then(Value::as_str).unwrap_or_default();
        let data = v.get("data");
        let field = |key: &str| -> Option<String> {
            data.and_then(|d| d.get(key))
                .and_then(Value::as_str)
                .map(str::to_string)
        };
        match ty {
            // The delta stream is what a live transcript renders.
            "assistant.message_delta" => field("deltaContent")
                .filter(|t| !t.is_empty())
                .map(Event::Text)
                .into_iter()
                .collect(),
            // The settled message is authoritative but already shown as deltas,
            // so it updates the terminal text without re-emitting it.
            "assistant.message" => {
                if let Some(content) = field("content") {
                    self.term.text = content;
                }
                Vec::new()
            }
            "assistant.reasoning" => field("content")
                .filter(|t| !t.is_empty())
                .map(Event::Thinking)
                .into_iter()
                .collect(),
            "tool.execution_start" => {
                let id = field("toolCallId");
                let name = field("toolName").unwrap_or_else(|| "tool".into());
                if let Some(id) = &id {
                    self.tools.insert(id.clone(), name.clone());
                }
                vec![Event::ToolCall {
                    id,
                    name,
                    input: data
                        .and_then(|d| d.get("arguments"))
                        .cloned()
                        .unwrap_or(Value::Null),
                }]
            }
            "tool.execution_complete" => vec![Event::ToolResult {
                id: field("toolCallId"),
                ok: data.and_then(|d| d.get("success")).and_then(Value::as_bool),
                output: data
                    .and_then(|d| d.get("result"))
                    .and_then(|r| r.get("content"))
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            }],
            // Copilot's terminal record is flat, not nested under `data`.
            "result" => {
                if let Some(id) = v.get("sessionId").and_then(Value::as_str) {
                    self.term.session = Some(id.to_string());
                }
                if let Some(usage) = v.get("usage") {
                    self.term.usage.premium_requests =
                        usage.get("premiumRequests").and_then(Value::as_u64);
                }
                if v.get("exitCode").and_then(Value::as_i64).unwrap_or(0) != 0 {
                    self.term.stop = Stop::Error;
                }
                Vec::new()
            }
            _ => Vec::new(),
        }
    }
}

/// The model named by a record, if it names one. Claude puts it at the top
/// level; Copilot nests it under `data`.
fn model_of(v: &Value) -> Option<String> {
    v.get("model")
        .or_else(|| v.get("data").and_then(|d| d.get("model")))
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// A `stop_reason` string that is neither absent nor the normal end.
fn stop_from(v: Option<&Value>) -> Stop {
    match v.and_then(Value::as_str) {
        None | Some("end_turn" | "stop" | "completed") => Stop::Completed,
        Some(other) => Stop::Other(other.to_string()),
    }
}

/// Claude's `rate_limit_info` object.
fn claude_rate_limit(v: Option<&Value>) -> Option<RateLimit> {
    let v = v?;
    Some(RateLimit {
        status: v.get("status").and_then(Value::as_str)?.to_string(),
        window: v
            .get("rateLimitType")
            .and_then(Value::as_str)
            .map(str::to_string),
        resets_at: v.get("resetsAt").and_then(Value::as_i64),
    })
}

/// Claude's terminal `usage` block plus its top-level `total_cost_usd`.
fn claude_usage(v: &Value) -> Usage {
    let u = v.get("usage");
    let get = |key: &str| u.and_then(|u| u.get(key)).and_then(Value::as_u64);
    Usage {
        input_tokens: get("input_tokens"),
        output_tokens: get("output_tokens"),
        cache_read_tokens: get("cache_read_input_tokens"),
        cache_write_tokens: get("cache_creation_input_tokens"),
        cost_usd: v.get("total_cost_usd").and_then(Value::as_f64),
        premium_requests: None,
    }
}

/// Codex's `turn.completed` usage block. Codex prices nothing itself, so
/// `cost_usd` stays absent rather than being derived from a local table.
fn codex_usage(v: Option<&Value>) -> Usage {
    let get = |key: &str| v.and_then(|u| u.get(key)).and_then(Value::as_u64);
    Usage {
        input_tokens: get("input_tokens"),
        output_tokens: get("output_tokens"),
        cache_read_tokens: get("cached_input_tokens"),
        cache_write_tokens: get("cache_write_input_tokens"),
        cost_usd: None,
        premium_requests: None,
    }
}

/// The display name of a Codex item: MCP and collaboration items name the tool
/// they invoked, everything else is identified by its item type.
fn tool_name(item: &Value, item_ty: &str) -> String {
    item.get("tool")
        .and_then(Value::as_str)
        .unwrap_or(item_ty)
        .to_string()
}

/// The arguments of a Codex tool item, in whatever shape that item uses.
fn codex_tool_input(item: &Value, item_ty: &str) -> Value {
    match item_ty {
        "command_execution" => serde_json::json!({ "command": item.get("command") }),
        "mcp_tool_call" => item.get("arguments").cloned().unwrap_or(Value::Null),
        // `file_change` carries `changes`, `web_search` a `query`; neither has a
        // single canonical argument field, so the item stands in for itself.
        _ => item.clone(),
    }
}

/// Flatten a tool result's `content` into the observation the model saw.
///
/// Anthropic tool results are either a bare string or an array of content
/// blocks. Text blocks flatten to their text; any other block kind (an image,
/// or a shape added in a future API version) is kept as its raw JSON rather
/// than dropped, so a caller inspecting a tool result never silently loses part
/// of it. This is lossy in presentation, never in content.
fn flatten_text(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .map(|b| match b.get("text").and_then(Value::as_str) {
                Some(text) => text.to_string(),
                None => b.to_string(),
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Some(other) => other.to_string(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drive a parser over `lines`, returning every event and the terminal.
    fn run(agent: Agent, lines: &[&str]) -> (Vec<Event>, Terminal) {
        let mut p = Parser::new(agent, Format::Stream);
        let events = lines.iter().flat_map(|l| p.push(l)).collect();
        (events, p.finish())
    }

    // Lines below are trimmed copies of transcripts captured from the live CLIs.

    #[test]
    fn claude_stream_yields_start_thinking_text_and_terminal_facts() {
        let (events, term) = run(
            Agent::Claude,
            &[
                r#"{"type":"system","subtype":"init","session_id":"sess-a","model":"claude-haiku-4-5"}"#,
                r#"{"type":"assistant","session_id":"sess-a","message":{"content":[{"type":"thinking","thinking":"brief"}]}}"#,
                r#"{"type":"assistant","session_id":"sess-a","message":{"content":[{"type":"text","text":"pong"}]}}"#,
                r#"{"type":"result","subtype":"success","is_error":false,"result":"pong","session_id":"sess-a","total_cost_usd":0.017,"usage":{"input_tokens":10,"output_tokens":45,"cache_read_input_tokens":18764,"cache_creation_input_tokens":7322}}"#,
            ],
        );
        assert_eq!(
            events[0],
            Event::Started {
                session: "sess-a".into(),
                model: Some("claude-haiku-4-5".into())
            }
        );
        assert_eq!(events[1], Event::Thinking("brief".into()));
        assert_eq!(events[2], Event::Text("pong".into()));
        assert_eq!(term.session.as_deref(), Some("sess-a"));
        assert_eq!(term.text, "pong");
        assert_eq!(term.stop, Stop::Completed);
        assert_eq!(term.usage.input_tokens, Some(10));
        assert_eq!(term.usage.cache_read_tokens, Some(18764));
        assert_eq!(term.usage.cache_write_tokens, Some(7322));
        assert_eq!(term.usage.cost_usd, Some(0.017));
    }

    #[test]
    fn claude_started_fires_only_once() {
        let (events, _) = run(
            Agent::Claude,
            &[
                r#"{"type":"system","subtype":"init","session_id":"s"}"#,
                r#"{"type":"assistant","session_id":"s","message":{"content":[{"type":"text","text":"a"}]}}"#,
                r#"{"type":"assistant","session_id":"s","message":{"content":[{"type":"text","text":"b"}]}}"#,
            ],
        );
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, Event::Started { .. }))
                .count(),
            1
        );
    }

    #[test]
    fn claude_pairs_tool_use_with_its_result() {
        let (events, _) = run(
            Agent::Claude,
            &[
                r#"{"type":"assistant","session_id":"s","message":{"content":[{"type":"tool_use","id":"toolu_1","name":"Bash","input":{"command":"ls"}}]}}"#,
                r#"{"type":"user","session_id":"s","message":{"content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"a.txt"}]}}"#,
            ],
        );
        let call = events
            .iter()
            .find(|e| matches!(e, Event::ToolCall { .. }))
            .unwrap();
        let Event::ToolCall { id, name, input } = call else {
            unreachable!()
        };
        assert_eq!(id.as_deref(), Some("toolu_1"));
        assert_eq!(name, "Bash");
        assert_eq!(input["command"], "ls");
        assert!(events.contains(&Event::ToolResult {
            id: Some("toolu_1".into()),
            ok: None,
            output: "a.txt".into(),
        }));
    }

    #[test]
    fn claude_reports_a_rate_limit_without_failing() {
        let (events, term) = run(
            Agent::Claude,
            &[
                r#"{"type":"rate_limit_event","session_id":"s","rate_limit_info":{"status":"allowed","resetsAt":1785260400,"rateLimitType":"five_hour"}}"#,
            ],
        );
        let limit = RateLimit {
            status: "allowed".into(),
            window: Some("five_hour".into()),
            resets_at: Some(1_785_260_400),
        };
        assert!(events.contains(&Event::RateLimit(limit.clone())));
        assert_eq!(term.rate_limit, Some(limit.clone()));
        assert!(
            !limit.is_blocking(),
            "an `allowed` heartbeat is not a block"
        );
    }

    #[test]
    fn claude_error_result_sets_the_stop_reason() {
        let (_, term) = run(
            Agent::Claude,
            &[r#"{"type":"result","is_error":true,"result":"boom","session_id":"s"}"#],
        );
        assert_eq!(term.stop, Stop::Error);
    }

    #[test]
    fn copilot_streams_deltas_and_takes_its_answer_from_the_settled_message() {
        let (events, term) = run(
            Agent::Copilot,
            &[
                r#"{"type":"assistant.message_delta","data":{"messageId":"m","deltaContent":"po"}}"#,
                r#"{"type":"assistant.message_delta","data":{"messageId":"m","deltaContent":"ng"}}"#,
                r#"{"type":"assistant.message","data":{"messageId":"m","model":"gpt-5-mini","content":"pong"}}"#,
                r#"{"type":"result","sessionId":"768c8e7d","exitCode":0,"usage":{"premiumRequests":0}}"#,
            ],
        );
        // The deltas stream; the settled message must not double them.
        let texts: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                Event::Text(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(texts, ["po", "ng"]);
        assert_eq!(term.text, "pong", "the answer is the settled message");
        assert_eq!(term.session.as_deref(), Some("768c8e7d"));
        assert_eq!(term.usage.premium_requests, Some(0));
    }

    #[test]
    fn copilot_brackets_a_tool_call_with_its_completion() {
        let (events, _) = run(
            Agent::Copilot,
            &[
                r#"{"type":"tool.execution_start","data":{"toolCallId":"call_1","toolName":"bash","arguments":{"command":"ls"}}}"#,
                r#"{"type":"tool.execution_complete","data":{"toolCallId":"call_1","success":true,"result":{"content":"a.txt"}}}"#,
            ],
        );
        assert!(matches!(
            &events[0],
            Event::ToolCall { id, name, .. }
                if id.as_deref() == Some("call_1") && name == "bash"
        ));
        assert_eq!(
            events[1],
            Event::ToolResult {
                id: Some("call_1".into()),
                ok: Some(true),
                output: "a.txt".into()
            }
        );
    }

    #[test]
    fn codex_reads_the_thread_id_and_the_completed_message() {
        let (events, term) = run(
            Agent::Codex,
            &[
                r#"{"type":"thread.started","thread_id":"0199-xyz"}"#,
                r#"{"type":"item.completed","item":{"id":"i1","type":"agent_message","text":"pong"}}"#,
                r#"{"type":"turn.completed","usage":{"input_tokens":12,"output_tokens":3,"cached_input_tokens":9}}"#,
            ],
        );
        assert_eq!(
            events[0],
            Event::Started {
                session: "0199-xyz".into(),
                model: None
            }
        );
        assert_eq!(term.session.as_deref(), Some("0199-xyz"));
        assert_eq!(term.text, "pong");
        assert_eq!(term.usage.input_tokens, Some(12));
        assert_eq!(term.usage.cache_read_tokens, Some(9));
    }

    #[test]
    fn codex_command_execution_becomes_a_call_and_a_result() {
        let (events, _) = run(
            Agent::Codex,
            &[
                r#"{"type":"item.completed","item":{"id":"c1","type":"command_execution","command":"ls","exit_code":0,"aggregated_output":"a.txt"}}"#,
            ],
        );
        assert!(matches!(&events[0], Event::ToolCall { name, .. } if name == "command_execution"));
        assert_eq!(
            events[1],
            Event::ToolResult {
                id: Some("c1".into()),
                ok: Some(true),
                output: "a.txt".into()
            }
        );
    }

    /// Codex reports one tool twice: in progress, then finished. The call must
    /// be announced once and the empty in-progress output must never surface as
    /// a result. Both lines are verbatim from a codex 0.145.0 transcript.
    #[test]
    fn codex_started_then_completed_yields_one_call_and_one_result() {
        let (events, _) = run(
            Agent::Codex,
            &[
                r#"{"type":"item.started","item":{"id":"item_1","type":"command_execution","command":"/bin/zsh -lc ls","aggregated_output":"","exit_code":null,"status":"in_progress"}}"#,
                r#"{"type":"item.completed","item":{"id":"item_1","type":"command_execution","command":"/bin/zsh -lc ls","aggregated_output":"a.txt\n","exit_code":0,"status":"completed"}}"#,
            ],
        );
        let calls = events
            .iter()
            .filter(|e| matches!(e, Event::ToolCall { .. }))
            .count();
        assert_eq!(calls, 1, "the same item must not be announced twice");
        let results: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                Event::ToolResult { output, .. } => Some(output.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            results,
            ["a.txt\n"],
            "the in-progress blank must not appear"
        );
    }

    /// A turn can hold several messages; the answer is the last to settle.
    #[test]
    fn codex_last_completed_message_is_the_answer() {
        let (_, term) = run(
            Agent::Codex,
            &[
                r#"{"type":"item.completed","item":{"id":"i0","type":"agent_message","text":"I'll list the directory."}}"#,
                r#"{"type":"item.completed","item":{"id":"i2","type":"agent_message","text":"DONE"}}"#,
            ],
        );
        assert_eq!(term.text, "DONE");
    }

    #[test]
    fn capture_is_bounded_and_keeps_the_earliest_output() {
        let mut buf = String::new();
        // Far more than the cap, in chunks, as a streaming agent would.
        for i in 0..50_000 {
            append_capped(&mut buf, &format!("line {i} aaaaaaaaaaaaaaaaaaaaaaaaaaaa"));
        }
        assert!(buf.len() <= MAX_CAPTURE, "grew to {}", buf.len());
        assert!(buf.starts_with("line 0 "), "the earliest output is kept");
    }

    #[test]
    fn capping_never_splits_a_multibyte_character() {
        let mut buf = "x".repeat(MAX_CAPTURE - 3);
        // A 4-byte character that cannot fit in the 3 bytes remaining.
        assert!(append_capped(&mut buf, "🙂🙂"));
        assert!(buf.len() <= MAX_CAPTURE);
        // The invariant is simply that this is still a valid Rust string, which
        // would have panicked on a mid-character slice above.
        assert!(buf.is_char_boundary(buf.len()));
    }

    #[test]
    fn a_full_buffer_reports_that_it_took_nothing() {
        let mut buf = "x".repeat(MAX_CAPTURE);
        assert!(!append_capped(&mut buf, "more"));
        assert_eq!(buf.len(), MAX_CAPTURE);
    }

    /// A vendor changing its output shape looks like a clean exit with nothing
    /// parsed. Counting the misses turns that from a mystery into a diagnosis.
    #[test]
    fn unparseable_lines_are_counted_and_sampled() {
        let (_, term) = run(
            Agent::Claude,
            &[
                "<html>an error page, not JSON</html>",
                "another bad line",
                r#"{"type":"result","result":"ok","session_id":"s"}"#,
            ],
        );
        assert_eq!(term.unparsed, 2);
        assert_eq!(
            term.first_unparsed.as_deref(),
            Some("<html>an error page, not JSON</html>")
        );
    }

    #[test]
    fn a_clean_stream_reports_no_parse_failures() {
        let (_, term) = run(
            Agent::Claude,
            &[r#"{"type":"result","result":"ok","session_id":"s"}"#],
        );
        assert_eq!(term.unparsed, 0);
        assert!(term.first_unparsed.is_none());
    }

    /// Non-text content blocks are preserved as raw JSON rather than dropped, so
    /// a caller inspecting a tool result never silently loses part of it.
    #[test]
    fn tool_result_blocks_that_are_not_text_are_kept_not_dropped() {
        let (events, _) = run(
            Agent::Claude,
            &[
                r#"{"type":"user","session_id":"s","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":[{"type":"text","text":"seen"},{"type":"image","source":{"data":"abc"}}]}]}}"#,
            ],
        );
        let output = events
            .iter()
            .find_map(|e| match e {
                Event::ToolResult { output, .. } => Some(output),
                _ => None,
            })
            .unwrap_or_else(|| panic!("expected a tool result, got {events:?}"));
        assert!(output.contains("seen"));
        assert!(output.contains("image"), "the image block was dropped");
    }

    #[test]
    fn garbage_lines_are_skipped_not_fatal() {
        let (events, term) = run(
            Agent::Claude,
            &[
                "Warning: something on stdout",
                "",
                r#"{"type":"result","result":"ok","session_id":"s"}"#,
            ],
        );
        assert!(events.iter().all(|e| !matches!(e, Event::Text(_))));
        assert_eq!(term.text, "ok");
    }

    #[test]
    fn text_format_passes_lines_through_verbatim() {
        let mut p = Parser::new(Agent::Copilot, Format::Text);
        let events: Vec<_> = ["hello", "world"].iter().flat_map(|l| p.push(l)).collect();
        assert_eq!(
            events,
            [Event::Text("hello".into()), Event::Text("world".into())]
        );
        assert_eq!(p.finish().text, "hello\nworld");
    }
}
