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
#[non_exhaustive]
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

/// The ceiling on a single output line before it is truncated.
///
/// [`MAX_CAPTURE`] bounds the *total* kept, but a reader that accumulates until
/// a newline can exhaust memory on one line that never ends. An agent emitting a
/// huge tool result as one JSON object is the ordinary case; a broken or hostile
/// one emitting an endless line is the case this exists for.
pub const MAX_LINE: usize = 512 * 1024;

/// The ceiling on any single event's payload.
///
/// [`MAX_CAPTURE`] bounds what is *kept*, and the channel bounds how many events
/// are queued, but neither bounds how large one event is. With a 512 KiB line
/// limit and a 256-deep channel, a stalled consumer could hold roughly 130 MiB
/// of events. Bounding the payload brings that to about 16 MiB, which is a
/// number worth being able to state.
///
/// 64 KiB is far more than a UI renders of a single tool result and is generous
/// for a model turn.
pub const MAX_EVENT_BYTES: usize = 64 * 1024;

/// Marks a payload this crate shortened, so a truncated value is never mistaken
/// for what the agent actually produced.
pub const TRUNCATION_MARK: &str = "…(truncated)";

/// The ceiling on an identifier: a session id, a tool-call id, a tool name.
///
/// Identifiers are **rejected** past this, never truncated. A shortened session
/// id resumes nothing and a shortened tool id matches no call, so a truncated
/// one is not a smaller version of the value, it is a wrong one. Dropping it
/// loses correlation for that event; keeping a corrupted one loses correlation
/// *and* lies about it.
///
/// Generous by three orders of magnitude: real ids are UUIDs of about 36 bytes
/// and tool names are a dozen. Anything near this is malformed rather than
/// merely long.
pub const MAX_IDENTIFIER_BYTES: usize = 4 * 1024;

/// The ceiling on the total bytes held in the pending-tool map.
///
/// Counting entries alone bounded nothing: 1024 entries of unbounded id and
/// name could retain hundreds of megabytes. This bounds the bytes, which is
/// what actually needed bounding.
pub(crate) const MAX_PENDING_TOOL_BYTES: usize = 256 * 1024;

/// The ceiling on how many tool calls may be tracked at once.
///
/// Entries are removed as results arrive, so this only bites when an agent
/// announces calls it never completes.
pub(crate) const MAX_PENDING_TOOLS: usize = 1024;

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

/// Whether an identifier is small enough to be usable.
///
/// The predicate deliberately returns a yes/no rather than a shortened value:
/// see [`MAX_IDENTIFIER_BYTES`] for why truncating one is worse than losing it.
fn usable_identifier(value: &str) -> bool {
    value.len() <= MAX_IDENTIFIER_BYTES
}

/// Keep an identifier only if it is usable.
fn accept_identifier(value: Option<String>) -> Option<String> {
    value.filter(|v| usable_identifier(v))
}

/// Shorten `text` to [`MAX_EVENT_BYTES`], marking it if anything was dropped.
fn bound_text(text: String) -> String {
    if text.len() <= MAX_EVENT_BYTES {
        return text;
    }
    let mut cut = MAX_EVENT_BYTES - TRUNCATION_MARK.len();
    while cut > 0 && !text.is_char_boundary(cut) {
        cut -= 1;
    }
    let mut out = text[..cut].to_string();
    out.push_str(TRUNCATION_MARK);
    out
}

/// Shorten a tool call's arguments, which are structured rather than text.
///
/// A truncated JSON value would no longer parse, so an oversized one is
/// replaced wholesale by an object recording what was dropped. That keeps the
/// value valid JSON, which is what a consumer expects of this field.
fn bound_value(value: Value) -> Value {
    let size = value.to_string().len();
    if size <= MAX_EVENT_BYTES {
        return value;
    }
    serde_json::json!({
        "truncated": true,
        "original_bytes": size,
        "note": "arguments exceeded MAX_EVENT_BYTES and were dropped rather than \
                 truncated, which would have produced invalid JSON",
    })
}

/// Apply [`MAX_EVENT_BYTES`] to an event's payload.
///
/// Payloads only. Identifiers, the session id and tool-call ids, are left
/// whole however long they are: they are short in practice, and shortening one
/// would break the thing it exists for, resuming a conversation or matching a
/// result to its call. A truncated identifier is worse than a large one.
fn enforce_bounds(event: Event) -> Event {
    match event {
        Event::Text(text) => Event::Text(bound_text(text)),
        Event::Thinking(text) => Event::Thinking(bound_text(text)),
        // An unusable id is dropped rather than shortened, so the event still
        // reports what the agent did while making the loss of correlation
        // explicit instead of silently wrong.
        Event::ToolCall { id, name, input } => Event::ToolCall {
            id: accept_identifier(id),
            name: bound_identifier(name),
            input: bound_value(input),
        },
        Event::ToolResult { id, ok, output } => Event::ToolResult {
            id: accept_identifier(id),
            ok,
            output: bound_text(output),
        },
        // `model` is for display, so shortening it costs nothing. The session
        // id is not: `Started` is only emitted once a usable one exists, so it
        // needs no filtering here.
        Event::Started { session, model } => Event::Started {
            session,
            model: model.map(bound_identifier),
        },
        Event::RateLimit(limit) => Event::RateLimit(RateLimit {
            status: bound_identifier(limit.status),
            window: limit.window.map(bound_identifier),
            resets_at: limit.resets_at,
        }),
    }
}

/// Shorten a short-by-nature field to [`MAX_IDENTIFIER_BYTES`].
///
/// For values that are descriptive rather than correlating, a tool name or a
/// model or a quota status word, where a shortened value is still meaningful.
fn bound_identifier(text: String) -> String {
    if text.len() <= MAX_IDENTIFIER_BYTES {
        return text;
    }
    let mut cut = MAX_IDENTIFIER_BYTES - TRUNCATION_MARK.len();
    while cut > 0 && !text.is_char_boundary(cut) {
        cut -= 1;
    }
    let mut out = text[..cut].to_string();
    out.push_str(TRUNCATION_MARK);
    out
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
    /// The schema-conforming answer, where the agent reports one separately.
    pub structured: Option<Value>,
    /// The provider status code when the agent reported a failed turn, such as
    /// a 404 for an unknown model.
    pub error_status: Option<u16>,
    /// The agent's own description of a failed turn, where it gives one apart
    /// from the answer text. Claude puts its explanation in `result`, so this
    /// stays `None` there; Codex reports it under `turn.failed`.
    pub error_message: Option<String>,
}

/// Unwrap an error body an agent passed through as a JSON string.
///
/// Codex 0.145.0 forwards the upstream response verbatim, so `turn.failed`
/// carries `{"type":"error","status":400,"error":{"message":"..."}}` encoded as
/// a *string*. Showing that to a user means showing them JSON, and the status
/// worth branching on is buried inside it. Anything that is not that shape is
/// returned as-is.
fn unwrap_error_body(message: &str) -> (Option<u16>, String) {
    let Ok(body) = serde_json::from_str::<Value>(message) else {
        return (None, message.to_string());
    };
    let status = body
        .get("status")
        .and_then(Value::as_u64)
        .and_then(|s| u16::try_from(s).ok());
    let inner = body
        .get("error")
        .and_then(|e| e.get("message"))
        .and_then(Value::as_str)
        .map(str::to_string);
    (status, inner.unwrap_or_else(|| message.to_string()))
}

/// Incrementally turns one agent's output into [`Event`]s and a [`Terminal`].
#[derive(Debug)]
pub(crate) struct Parser {
    agent: Agent,
    format: Format,
    term: Terminal,
    /// Tool names by call id, so a result can be attributed to its call.
    tools: HashMap<String, String>,
    /// Bytes currently held in `tools`, kept alongside it because a map has no
    /// cheap way to answer that.
    tool_bytes: usize,
    /// What the stream has shown so far.
    seen: Seen,
}

/// Milestones a stream passes, tracked because later handling depends on them.
///
/// Four independent yes/no facts about position in the stream. Clippy flags the
/// count, but packing them into bitflags would trade four self-describing names
/// for one opaque integer, and nothing here is hot enough to want that.
#[derive(Debug, Default)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "four independent stream milestones; naming each beats packing them"
)]
struct Seen {
    /// A [`Event::Started`] has been emitted, so it fires only once.
    started: bool,
    /// A record was recognized as this agent's own shape, so the output really
    /// is what was asked for.
    structured: bool,
    /// The agent's terminal record arrived, so the turn completed.
    terminal: bool,
    /// Token-level deltas arrived.
    ///
    /// Claude sends deltas *and* the completed message they build up to, so
    /// emitting both would show every answer twice. Detected rather than
    /// configured: the deltas always precede the completed message, so seeing
    /// one is proof the finished copy is a duplicate.
    deltas: bool,
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
            tool_bytes: 0,
            seen: Seen::default(),
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
            return vec![enforce_bounds(Event::Text(line.to_string()))];
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
        // A record that names a type this parser knows is evidence the stream
        // really is the shape we asked for.
        if let Some(ty) = value.get("type").and_then(Value::as_str)
            && self.recognizes(ty)
        {
            self.seen.structured = true;
        }
        let mut out = match self.agent {
            Agent::Claude => self.claude(&value),
            Agent::Codex => self.codex(&value),
            Agent::Copilot => self.copilot(&value),
        };
        // Every event leaves through here, so bounding once at the exit covers
        // all three agents rather than each parser remembering.
        out = out.into_iter().map(enforce_bounds).collect();

        // Fire `Started` exactly once, from whichever record first revealed the
        // id, and put it ahead of that record's own events.
        if !self.seen.started {
            if let Some(session) = self.term.session.clone() {
                self.seen.started = true;
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

    /// Whether `ty` is a record type this agent's parser understands.
    fn recognizes(&self, ty: &str) -> bool {
        match self.agent {
            Agent::Claude => matches!(
                ty,
                "system" | "assistant" | "user" | "result" | "rate_limit_event"
            ),
            Agent::Codex => {
                ty.starts_with("thread.") || ty.starts_with("turn.") || ty.starts_with("item.")
            }
            Agent::Copilot => {
                ty == "result"
                    || ty.starts_with("assistant.")
                    || ty.starts_with("tool.")
                    || ty.starts_with("session.")
            }
        }
    }

    /// Track a tool call so its result can be attributed, bounded so an agent
    /// that announces calls it never finishes cannot grow this without limit.
    fn remember_tool(&mut self, id: &str, name: &str) {
        // An unusable id cannot correlate anything, so tracking it only costs
        // memory.
        if !usable_identifier(id) {
            return;
        }
        let name = bound_identifier(name.to_string());
        let cost = id.len() + name.len();
        // Both budgets matter: the count bounds a flood of tiny entries, the
        // bytes bound a few enormous ones. Counting entries alone was no bound
        // at all while the entries themselves were unbounded.
        if self.tools.len() >= MAX_PENDING_TOOLS
            || self.tool_bytes.saturating_add(cost) > MAX_PENDING_TOOL_BYTES
        {
            return;
        }
        self.tool_bytes += cost;
        if let Some(previous) = self.tools.insert(id.to_string(), name) {
            // Replacing an entry must not double-count its predecessor.
            self.tool_bytes = self.tool_bytes.saturating_sub(id.len() + previous.len());
        }
    }

    /// Stop tracking a call once its result has arrived, releasing its budget.
    fn forget_tool(&mut self, id: &str) {
        if let Some(name) = self.tools.remove(id) {
            self.tool_bytes = self.tool_bytes.saturating_sub(id.len() + name.len());
        }
    }

    /// Whether any structured record has been recognized on this stream.
    ///
    /// A structured run that recognized nothing did not merely fail to answer;
    /// it means the output was not the shape this parser understands.
    pub(crate) fn saw_structured_record(&self) -> bool {
        self.seen.structured
    }

    /// Whether the stream carried its terminal record, the one that closes a
    /// turn and carries the answer and usage.
    pub(crate) fn saw_terminal_record(&self) -> bool {
        self.seen.terminal
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
    /// Verified against claude 2.1.212: `system/init` opens with the id,
    /// `assistant` records carry Anthropic content blocks, `rate_limit_event`
    /// reports quota, and `result` closes with the answer and usage.
    fn claude(&mut self, v: &Value) -> Vec<Event> {
        let ty = v.get("type").and_then(Value::as_str).unwrap_or_default();
        // An unusable session id must not be captured: it would be persisted as
        // a binding that can never resume anything.
        if let Some(id) = v.get("session_id").and_then(Value::as_str)
            && usable_identifier(id)
        {
            self.term.session.get_or_insert_with(|| id.to_string());
        }
        match ty {
            "rate_limit_event" => {
                let limit = claude_rate_limit(v.get("rate_limit_info"));
                self.term.rate_limit.clone_from(&limit);
                limit.into_iter().map(Event::RateLimit).collect()
            }
            // Token-level deltas, present only with `--include-partial-messages`.
            "stream_event" => self.claude_delta(v),
            // Both roles carry content blocks: `assistant` holds text/thinking/
            // tool_use, `user` carries the tool_result observations back.
            "assistant" | "user" => self.content_blocks(v),
            "result" => {
                self.seen.terminal = true;
                if let Some(text) = v.get("result").and_then(Value::as_str) {
                    self.term.text = text.to_string();
                }
                // Claude returns the conforming value as its own field, so it
                // needs no re-parsing out of the answer text.
                if let Some(value) = v.get("structured_output") {
                    self.term.structured = Some(value.clone());
                }
                self.term.usage = claude_usage(v);
                // `subtype` says "success" even for a failed turn, so
                // `is_error` is the field that actually decides.
                self.term.stop = if v.get("is_error").and_then(Value::as_bool) == Some(true) {
                    self.term.error_status = v
                        .get("api_error_status")
                        .and_then(Value::as_u64)
                        .and_then(|s| u16::try_from(s).ok());
                    Stop::Error
                } else {
                    stop_from(v.get("stop_reason"))
                };
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    /// One token-level delta from Claude's `stream_event` records.
    ///
    /// These wrap the provider's own streaming events. Only the deltas that
    /// carry visible text are surfaced; the block start/stop and message
    /// envelopes describe structure this crate already expresses through the
    /// event vocabulary.
    fn claude_delta(&mut self, v: &Value) -> Vec<Event> {
        let Some(event) = v.get("event") else {
            return Vec::new();
        };
        if event.get("type").and_then(Value::as_str) != Some("content_block_delta") {
            return Vec::new();
        }
        let Some(delta) = event.get("delta") else {
            return Vec::new();
        };
        // Seeing any delta means the completed message that follows is a
        // duplicate of what has already been streamed.
        self.seen.deltas = true;

        match delta.get("type").and_then(Value::as_str) {
            Some("text_delta") => delta
                .get("text")
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
                .map(|text| Event::Text(text.to_string()))
                .into_iter()
                .collect(),
            Some("thinking_delta") => delta
                .get("thinking")
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
                .map(|text| Event::Thinking(text.to_string()))
                .into_iter()
                .collect(),
            // `input_json_delta` streams a tool call's arguments a fragment at a
            // time. The completed `tool_use` block carries them whole, which is
            // what a consumer can actually act on, so the fragments are skipped.
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
                // Skipped once deltas have streamed the same text, or the
                // transcript would show every answer twice.
                // Guarded on `deltas`: once tokens have streamed, the finished
                // copy falls through to the catch-all and is dropped.
                "text" if !self.seen.deltas => {
                    if let Some(t) = block.get("text").and_then(Value::as_str) {
                        out.push(Event::Text(t.to_string()));
                    }
                }
                "thinking" if !self.seen.deltas => {
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
                        self.remember_tool(id, &name);
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
                        .inspect(|id| {
                            // The call has been answered, so stop tracking it.
                            self.forget_tool(id);
                        })
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
        if let Some(id) = v.get("thread_id").and_then(Value::as_str)
            && usable_identifier(id)
        {
            self.term.session.get_or_insert_with(|| id.to_string());
        }
        match ty {
            "turn.completed" => {
                self.seen.terminal = true;
                self.term.usage = codex_usage(v.get("usage"));
                Vec::new()
            }
            "turn.failed" => {
                self.seen.terminal = true;
                self.term.stop = Stop::Error;
                if let Some(message) = v
                    .get("error")
                    .and_then(|e| e.get("message"))
                    .and_then(Value::as_str)
                {
                    let (status, message) = unwrap_error_body(message);
                    self.term.error_status = status;
                    self.term.error_message = Some(bound_text(message));
                }
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
                            if let Some(id) = &id {
                                self.forget_tool(id);
                            }
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
                    self.remember_tool(id, &name);
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
                id: field("toolCallId").inspect(|id| {
                    self.forget_tool(id);
                }),
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
                self.seen.terminal = true;
                if let Some(id) = v.get("sessionId").and_then(Value::as_str)
                    && usable_identifier(id)
                {
                    self.term.session = Some(id.to_string());
                }
                if let Some(usage) = v.get("usage") {
                    self.term.usage.premium_requests =
                        usage.get("premiumRequests").and_then(Value::as_u64);
                }
                if let Some(code) = v.get("exitCode").and_then(Value::as_i64)
                    && code != 0
                {
                    self.term.stop = Stop::Error;
                    // Copilot reports no explanation with the code, so name the
                    // code rather than leave the failure blank.
                    self.term.error_message = Some(format!("copilot exited with code {code}"));
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

    /// Verbatim from a `--include-partial-messages` run. Claude sends both the
    /// deltas and the completed message they build up to, so emitting both
    /// would show every answer twice in a transcript.
    #[test]
    fn claude_token_deltas_stream_without_duplicating_the_finished_message() {
        let (events, _) = run(
            Agent::Claude,
            &[
                r#"{"type":"system","subtype":"init","session_id":"s"}"#,
                r#"{"type":"stream_event","session_id":"s","event":{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}}"#,
                r#"{"type":"stream_event","session_id":"s","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"po"}}}"#,
                r#"{"type":"stream_event","session_id":"s","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"ng"}}}"#,
                r#"{"type":"stream_event","session_id":"s","event":{"type":"content_block_stop","index":0}}"#,
                // The completed copy of the same text.
                r#"{"type":"assistant","session_id":"s","message":{"content":[{"type":"text","text":"pong"}]}}"#,
                r#"{"type":"result","subtype":"success","is_error":false,"result":"pong","session_id":"s"}"#,
            ],
        );
        let texts: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                Event::Text(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(texts, ["po", "ng"], "the finished message must not repeat");
    }

    /// Thinking streams the same way, and must not double either.
    #[test]
    fn claude_thinking_deltas_stream_without_duplication() {
        let (events, _) = run(
            Agent::Claude,
            &[
                r#"{"type":"stream_event","session_id":"s","event":{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"weighing"}}}"#,
                r#"{"type":"assistant","session_id":"s","message":{"content":[{"type":"thinking","thinking":"weighing"}]}}"#,
            ],
        );
        let thoughts: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                Event::Thinking(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(thoughts, ["weighing"]);
    }

    /// Without partial messages there are no deltas, so the completed message
    /// is the only source and must still be emitted.
    #[test]
    fn a_completed_message_still_streams_when_no_deltas_arrived() {
        let (events, _) = run(
            Agent::Claude,
            &[
                r#"{"type":"assistant","session_id":"s","message":{"content":[{"type":"text","text":"pong"}]}}"#,
            ],
        );
        assert!(events.contains(&Event::Text("pong".into())), "{events:?}");
    }

    /// Tool calls are not duplicated by deltas, so they keep coming from the
    /// completed block even once deltas have been seen.
    #[test]
    fn tool_calls_survive_delta_suppression() {
        let (events, _) = run(
            Agent::Claude,
            &[
                r#"{"type":"stream_event","session_id":"s","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi"}}}"#,
                r#"{"type":"assistant","session_id":"s","message":{"content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"ls"}}]}}"#,
            ],
        );
        assert!(
            events.iter().any(|e| matches!(e, Event::ToolCall { .. })),
            "suppression must apply to text only: {events:?}"
        );
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

    /// Verbatim from codex-cli 0.145.0 with an unknown model. It exits **0**
    /// and forwards the upstream body as a JSON *string*, so the status worth
    /// branching on is nested one level inside a field that is itself text.
    #[test]
    fn a_codex_failed_turn_yields_the_reason_and_the_status() {
        let (_, term) = run(
            Agent::Codex,
            &[
                r#"{"type":"thread.started","thread_id":"019fad62"}"#,
                r#"{"type":"turn.failed","error":{"message":"{\"type\":\"error\",\"status\":400,\"error\":{\"type\":\"invalid_request_error\",\"message\":\"The 'bogus-model-xyz' model is not supported when using Codex with a ChatGPT account.\"}}"}}"#,
            ],
        );
        assert_eq!(term.stop, Stop::Error);
        assert_eq!(term.error_status, Some(400));
        assert_eq!(
            term.error_message.as_deref(),
            Some(
                "The 'bogus-model-xyz' model is not supported when using Codex with a ChatGPT account."
            ),
            "the caller should get the sentence, not the envelope"
        );
    }

    /// An error that is not the double-encoded shape must survive untouched
    /// rather than be dropped for failing to match it.
    #[test]
    fn a_plain_codex_failure_message_passes_through() {
        let (_, term) = run(
            Agent::Codex,
            &[
                r#"{"type":"turn.failed","error":{"message":"stream disconnected before completion"}}"#,
            ],
        );
        assert_eq!(term.error_status, None);
        assert_eq!(
            term.error_message.as_deref(),
            Some("stream disconnected before completion")
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

    /// The channel bounds how many events queue, not how large they are. With
    /// a 512 KiB line limit that left ~130 MiB reachable in flight.
    #[test]
    fn an_enormous_tool_result_is_bounded_and_marked() {
        let huge = "x".repeat(MAX_EVENT_BYTES * 4);
        let line = serde_json::json!({
            "type": "user",
            "session_id": "s",
            "message": {"content": [{
                "type": "tool_result", "tool_use_id": "t1", "content": huge
            }]}
        })
        .to_string();

        let (events, _) = run(Agent::Claude, &[&line]);
        let Some(Event::ToolResult { output, id, .. }) = events
            .iter()
            .find(|e| matches!(e, Event::ToolResult { .. }))
            .cloned()
        else {
            panic!("expected a tool result, got {events:?}")
        };
        assert!(
            output.len() <= MAX_EVENT_BYTES,
            "kept {} bytes",
            output.len()
        );
        assert!(
            output.ends_with(TRUNCATION_MARK),
            "truncation must be visible"
        );
        assert_eq!(id.as_deref(), Some("t1"), "the id must survive whole");
    }

    /// A usable identifier passes through whole, however awkward its length.
    /// Truncating one is worse than losing it: a shortened session id resumes
    /// nothing and a shortened tool id matches no call.
    #[test]
    fn usable_identifiers_are_never_shortened() {
        // Long enough to be unusual, small enough to still be an identifier.
        let id = "s".repeat(MAX_IDENTIFIER_BYTES);
        let line =
            serde_json::json!({"type": "system", "subtype": "init", "session_id": id}).to_string();
        let (events, term) = run(Agent::Claude, &[&line]);

        let Some(Event::Started { session, .. }) = events.first().cloned() else {
            panic!("expected Started, got {events:?}")
        };
        assert_eq!(session.len(), id.len(), "the session id was shortened");
        assert_eq!(term.session.as_deref(), Some(id.as_str()));
    }

    /// The hole this closes: identifiers were exempt from every bound, so a
    /// 512 KiB id rode through and the "16 MiB queued" figure was wrong.
    /// Rejecting is right where truncating is not, because a binding that
    /// cannot resume is worse than no binding.
    #[test]
    fn an_oversized_session_id_is_rejected_rather_than_stored() {
        let id = "s".repeat(MAX_IDENTIFIER_BYTES + 1);
        for (agent, line) in [
            (
                Agent::Claude,
                serde_json::json!({"type": "system", "subtype": "init", "session_id": id})
                    .to_string(),
            ),
            (
                Agent::Codex,
                serde_json::json!({"type": "thread.started", "thread_id": id}).to_string(),
            ),
            (
                Agent::Copilot,
                serde_json::json!({"type": "result", "sessionId": id, "exitCode": 0}).to_string(),
            ),
        ] {
            let (events, term) = run(agent, &[&line]);
            assert!(term.session.is_none(), "{agent} stored an unusable id");
            assert!(
                !events.iter().any(|e| matches!(e, Event::Started { .. })),
                "{agent} announced a session it cannot resume"
            );
        }
    }

    /// A tool event with an unusable id is still reported: what the agent did
    /// is worth knowing even when it cannot be correlated. The id is dropped,
    /// never shortened into something that would match the wrong call.
    #[test]
    fn an_oversized_tool_id_drops_the_id_but_keeps_the_event() {
        let id = "t".repeat(MAX_IDENTIFIER_BYTES + 1);
        let line = serde_json::json!({
            "type": "assistant", "session_id": "s",
            "message": {"content": [{
                "type": "tool_use", "id": id, "name": "Bash", "input": {"command": "ls"}
            }]}
        })
        .to_string();

        let (events, _) = run(Agent::Claude, &[&line]);
        let Some(Event::ToolCall { id: seen, name, .. }) = events
            .iter()
            .find(|e| matches!(e, Event::ToolCall { .. }))
            .cloned()
        else {
            panic!("the call itself must still be reported, got {events:?}")
        };
        assert_eq!(seen, None, "an unusable id must be dropped, not shortened");
        assert_eq!(name, "Bash");
    }

    /// Bounding the entry count bounded nothing while the entries themselves
    /// were unbounded: 1024 pending calls could retain hundreds of megabytes.
    #[test]
    fn the_pending_tool_map_is_bounded_by_bytes_not_only_entries() {
        let mut parser = Parser::new(Agent::Claude, Format::Stream);
        // Ids and names just under the identifier ceiling, so the entry count
        // is nowhere near its limit while the bytes are.
        for i in 0..MAX_PENDING_TOOLS {
            let line = serde_json::json!({
                "type": "assistant", "session_id": "s",
                "message": {"content": [{
                    "type": "tool_use",
                    "id": format!("{i:0>width$}", width = MAX_IDENTIFIER_BYTES),
                    "name": "x".repeat(MAX_IDENTIFIER_BYTES),
                    "input": {}
                }]}
            })
            .to_string();
            parser.push(&line);
        }
        assert!(
            parser.tool_bytes <= MAX_PENDING_TOOL_BYTES,
            "pending tools grew to {} bytes",
            parser.tool_bytes
        );
    }

    /// Answering a call must release its budget, or a long run of ordinary
    /// paired calls would exhaust it and stop correlating.
    #[test]
    fn a_completed_tool_call_releases_its_budget() {
        let mut parser = Parser::new(Agent::Claude, Format::Stream);
        let call = |id: &str| {
            serde_json::json!({
                "type": "assistant", "session_id": "s",
                "message": {"content": [{
                    "type": "tool_use", "id": id, "name": "Bash", "input": {}
                }]}
            })
            .to_string()
        };
        let result = |id: &str| {
            serde_json::json!({
                "type": "user", "session_id": "s",
                "message": {"content": [{
                    "type": "tool_result", "tool_use_id": id, "content": "done"
                }]}
            })
            .to_string()
        };

        for i in 0..(MAX_PENDING_TOOLS * 4) {
            let id = format!("toolu_{i}");
            parser.push(&call(&id));
            parser.push(&result(&id));
        }
        assert_eq!(parser.tool_bytes, 0, "budget leaked across paired calls");
        assert!(parser.tools.is_empty());
    }

    /// The claim the changelog makes has to survive an adversarial line: every
    /// field at its worst, times the channel depth, still under 20 MiB.
    #[test]
    fn a_worst_case_event_stays_within_the_stated_ceiling() {
        let huge = "x".repeat(MAX_LINE);
        let line = serde_json::json!({
            "type": "assistant", "session_id": huge,
            "message": {"content": [{
                "type": "tool_use", "id": huge, "name": huge, "input": {"command": huge}
            }]}
        })
        .to_string();

        let (events, _) = run(Agent::Claude, &[&line]);
        for event in &events {
            let size = serde_json::to_string(event).unwrap().len();
            // Payload plus identifiers, with room for JSON framing.
            let ceiling = MAX_EVENT_BYTES + 4 * MAX_IDENTIFIER_BYTES;
            assert!(size <= ceiling, "an event reached {size} bytes: {event:?}");
        }
    }

    /// Truncating JSON would produce something that no longer parses, so an
    /// oversized argument object is replaced rather than cut.
    #[test]
    fn oversized_tool_arguments_stay_valid_json() {
        let line = serde_json::json!({
            "type": "assistant",
            "session_id": "s",
            "message": {"content": [{
                "type": "tool_use", "id": "t1", "name": "Bash",
                "input": {"command": "y".repeat(MAX_EVENT_BYTES * 3)}
            }]}
        })
        .to_string();

        let (events, _) = run(Agent::Claude, &[&line]);
        let Some(Event::ToolCall { input, .. }) = events
            .iter()
            .find(|e| matches!(e, Event::ToolCall { .. }))
            .cloned()
        else {
            panic!("expected a tool call, got {events:?}")
        };
        assert_eq!(input["truncated"], true, "got {input}");
        assert!(
            input.is_object(),
            "the replacement must still be valid JSON"
        );
        assert!(input.to_string().len() <= MAX_EVENT_BYTES);
    }

    #[test]
    fn ordinary_payloads_pass_through_untouched() {
        let (events, _) = run(
            Agent::Claude,
            &[
                r#"{"type":"assistant","session_id":"s","message":{"content":[{"type":"text","text":"pong"}]}}"#,
            ],
        );
        assert!(events.contains(&Event::Text("pong".into())), "{events:?}");
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
