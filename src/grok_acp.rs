//! Grok ACP stdio transport.
//!
//! Spawn is `grok agent stdio`. One child per [`crate::stream`] so an IDE can
//! keep many sessions and many backends live in parallel: this is not a
//! process-wide singleton, and Claude/Codex keep their own processes. A host
//! that wants many Grok sessions on one child holds the process; this crate
//! does not serialize the IDE onto one pipe, and it does not flatten Grok
//! into Codex's spawn-per-turn model.
//!
//! Mid-turn input is `_x.ai/interject` with `{sessionId, text}` (confirmed
//! live on grok 1.0.30: bare `x.ai/interject` is -32601). That injects into
//! the current turn. It does not queue, and it does not cancel.
//!
//! Interrupt is `session/cancel`. That kicks an in-flight turn so a resume
//! can reattach. The session id stays.
//!
//! `/compact` is `_x.ai/compact_conversation`. `/clear` is not a Grok
//! command (Grok uses `/new`).

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use serde_json::{Value, json};

use crate::agent::{Continue, Permission};
use crate::approval::{Approval, Decision};
use crate::command::Compaction;
use crate::event::{Event, Terminal, append_capped};
use crate::outcome::{RateLimit, Stop, Usage};
use crate::request::Request;

/// Grok 4.6 / 4.5 context window when the payload omits `size`.
const GROK_CONTEXT_WINDOW: u64 = 500_000;

const INIT_ID: u64 = 1;
const SESSION_ID: u64 = 2;
const PROMPT_ID: u64 = 3;
const COMPACT_ID: u64 = 4;
const USAGE_ID: u64 = 5;
const INFO_ID: u64 = 6;
const NEXT_ID: u64 = 10;

/// Piggyback `x.ai/session/usage` at most once a minute across turns.
static LAST_USAGE_SECS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
const USAGE_INTERVAL_SECS: u64 = 60;
/// Mid-turn occupancy polls. Grok has no `usage_update`; this is how the
/// chip grows while tools run. Once per 15s, and never overlapping, so a
/// tool-heavy turn does not queue occupancy RPCs ahead of tools.
const INFO_POLL_SECS: u64 = 15;

/// One decoded ACP record.
#[derive(Debug, Default)]
pub(crate) struct Step {
    pub events: Vec<Event>,
    pub writes: Vec<String>,
    pub steer_responses: Vec<SteerResponse>,
}

/// One `_x.ai/interject` request.
#[derive(Debug)]
pub(crate) struct SteerRequest {
    pub id: u64,
    pub wire: String,
}

/// Agent acceptance or rejection of one interject.
#[derive(Debug)]
pub(crate) struct SteerResponse {
    pub id: u64,
    pub result: std::result::Result<String, String>,
}

#[derive(Debug)]
struct PendingApproval {
    rpc_id: Value,
    options: Value,
}

/// State that spans the JSON-RPC records of one turn.
#[derive(Debug)]
pub(crate) struct Protocol {
    request: Request,
    pub terminal: Terminal,
    pub session_id: Option<String>,
    pub finished: bool,
    pub failure: Option<String>,
    pending: HashMap<String, PendingApproval>,
    pending_steers: HashSet<u64>,
    interrupt_requested: bool,
    next_id: u64,
    compacting: bool,
    last_steer: Option<String>,
    /// `session/load` (and `session/new`) replay history as `session/update`
    /// before the RPC returns. AZ already has that transcript. Emitting it
    /// again duplicates the top of the thread on reconnect.
    replaying: bool,
    usage_fallback: bool,
    info_fallback: bool,
    /// In-flight `_x.ai/session/info` polls (not the end-of-turn `INFO_ID`).
    pending_info: HashSet<u64>,
    last_info_secs: u64,
}

impl Protocol {
    pub fn new(request: Request) -> Self {
        Self {
            request,
            terminal: Terminal::default(),
            session_id: None,
            finished: false,
            failure: None,
            pending: HashMap::new(),
            pending_steers: HashSet::new(),
            interrupt_requested: false,
            next_id: NEXT_ID,
            compacting: false,
            last_steer: None,
            replaying: true,
            usage_fallback: false,
            info_fallback: false,
            pending_info: HashSet::new(),
            last_info_secs: 0,
        }
    }

    /// First write: ACP `initialize`. Session open waits on the reply.
    pub fn opening() -> Vec<String> {
        vec![wire(&json!({
            "jsonrpc": "2.0",
            "id": INIT_ID,
            "method": "initialize",
            "params": {
                "protocolVersion": 1,
                "clientInfo": {
                    "name": "agent-abstraction",
                    "version": env!("CARGO_PKG_VERSION"),
                },
                "clientCapabilities": {
                    "fs": { "readTextFile": false, "writeTextFile": false },
                    "terminal": false
                },
            },
        }))]
    }

    fn cwd(&self) -> PathBuf {
        self.request
            .cwd
            .clone()
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."))
    }

    fn session_params(&self) -> Value {
        let cwd = self.cwd();
        let extra: Vec<String> = self
            .request
            .extra_dirs
            .iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect();
        let mut meta = json!({});
        if let Some(system) = &self.request.system {
            meta["rules"] = json!(system);
        }
        match self.request.permission {
            Permission::Bypass => meta["yoloMode"] = json!(true),
            Permission::Auto => meta["autoMode"] = json!(true),
            _ => {}
        }
        let mut params = json!({
            "cwd": cwd,
            "mcpServers": [],
        });
        if !extra.is_empty() {
            params["additionalDirectories"] = json!(extra);
        }
        if meta.as_object().is_some_and(|map| !map.is_empty()) {
            params["_meta"] = meta;
        }
        params
    }

    fn open_session(&self) -> Value {
        let mut params = self.session_params();
        match &self.request.cont {
            Continue::New | Continue::NewWith(_) => json!({
                "jsonrpc": "2.0",
                "id": SESSION_ID,
                "method": "session/new",
                "params": params,
            }),
            Continue::Resume(session_id) => {
                params["sessionId"] = json!(session_id);
                json!({
                    "jsonrpc": "2.0",
                    "id": SESSION_ID,
                    "method": "session/load",
                    "params": params,
                })
            }
            Continue::Fork(session_id) => {
                params["sessionId"] = json!(session_id);
                json!({
                    "jsonrpc": "2.0",
                    "id": SESSION_ID,
                    "method": "session/fork",
                    "params": params,
                })
            }
        }
    }

    fn start_prompt(&self, session_id: &str) -> String {
        if self.request.operation == crate::request::Operation::Interrupt {
            return Self::cancel_wire(session_id);
        }
        if self.request.is_command {
            return self.command_wire(session_id);
        }
        wire(&json!({
            "jsonrpc": "2.0",
            "id": PROMPT_ID,
            "method": "session/prompt",
            "params": {
                "sessionId": session_id,
                "prompt": [{ "type": "text", "text": self.request.prompt }],
            },
        }))
    }

    fn command_wire(&self, session_id: &str) -> String {
        // AgencyZero's mapped command is `/compact`. Grok's ACP name is
        // `compact_conversation`. `/clear` is not a Grok command (it has
        // `/new`). Anything else is refused rather than sent as prose.
        let prompt = self.request.prompt.trim();
        let Some(rest) = prompt.strip_prefix("/compact") else {
            return String::new();
        };
        if !rest.is_empty() && !rest.starts_with(' ') {
            return String::new();
        }
        let mut params = json!({ "sessionId": session_id });
        let how = rest.trim();
        if !how.is_empty() {
            params["instructions"] = json!(how);
        }
        wire(&json!({
            "jsonrpc": "2.0",
            "id": COMPACT_ID,
            "method": "_x.ai/compact_conversation",
            "params": params,
        }))
    }

    fn cancel_wire(session_id: &str) -> String {
        wire(&json!({
            "jsonrpc": "2.0",
            "method": "session/cancel",
            "params": { "sessionId": session_id },
        }))
    }

    /// Turn one JSON-RPC line into host events and follow-up writes.
    pub fn push(&mut self, value: &Value) -> Step {
        let mut step = Step::default();

        if let Some(id) = json_id(value) {
            if let Some(error) = rpc_error(value) {
                return self.push_error(id, error, &mut step);
            }
            match id {
                INIT_ID => {
                    step.writes.push(wire(&self.open_session()));
                    return step;
                }
                SESSION_ID => return self.push_session(value, &mut step),
                PROMPT_ID => {
                    self.finish_prompt(value);
                    self.request_occupancy_then_billing(&mut step);
                    return step;
                }
                INFO_ID => {
                    if let Some(usage) = grok_session_info(value) {
                        self.terminal.usage.accumulate(&usage);
                        step.events.push(Event::Usage(usage));
                    }
                    self.request_billing_or_finish(&mut step);
                    return step;
                }
                USAGE_ID => {
                    if let Some(limit) = grok_session_usage(value) {
                        self.terminal.rate_limit = Some(limit.clone());
                        step.events.push(Event::RateLimit(limit));
                    }
                    self.finished = true;
                    return step;
                }
                COMPACT_ID => {
                    self.compacting = false;
                    self.terminal.stop = Stop::Completed;
                    step.events
                        .push(Event::Compaction(crate::command::Compaction::Finished {
                            ok: true,
                            error: None,
                        }));
                    // Post-compact occupancy is `session/info.context.used`
                    // (tokens_after), not the compact turn's billed input and
                    // not AZ's 8k estimate.
                    self.request_occupancy_then_billing(&mut step);
                    return step;
                }
                other if self.pending_info.remove(&other) => {
                    if let Some(usage) = grok_session_info(value) {
                        self.terminal.usage.accumulate(&usage);
                        step.events.push(Event::Usage(usage));
                    }
                    return step;
                }
                other if self.pending_steers.contains(&other) => {
                    self.pending_steers.remove(&other);
                    step.steer_responses.push(SteerResponse {
                        id: other,
                        result: Ok(String::new()),
                    });
                    return step;
                }
                _ => {}
            }
        }

        let method = value.get("method").and_then(Value::as_str).unwrap_or("");
        match method {
            "session/update" | "x.ai/session/update" | "_x.ai/session/update" => {
                if !self.replaying {
                    step.events.extend(self.session_update(value));
                    if occupancy_tick(value) {
                        if let Some(wire) = self.poll_occupancy() {
                            step.writes.push(wire);
                        }
                    }
                }
            }
            "session/request_permission" | "x.ai/session/request_permission" => {
                let asked = self.permission_request(value);
                if let Some(event) = asked.event {
                    step.events.push(event);
                }
                if let Some(write) = asked.write {
                    step.writes.push(write);
                }
            }
            _ => {}
        }
        step
    }

    fn push_error(&mut self, id: u64, error: String, step: &mut Step) -> Step {
        if id == SESSION_ID
            && matches!(self.request.cont, Continue::Fork(_))
            && error.to_ascii_lowercase().contains("method")
        {
            // `session/fork` missing: Grok extension name.
            let mut params = self.session_params();
            if let Continue::Fork(session_id) = &self.request.cont {
                params["sessionId"] = json!(session_id);
            }
            step.writes.push(wire(&json!({
                "jsonrpc": "2.0",
                "id": SESSION_ID,
                "method": "_x.ai/session/fork",
                "params": params,
            })));
            return std::mem::take(step);
        }
        if id == COMPACT_ID && error.to_ascii_lowercase().contains("method") {
            let Some(session_id) = self.session_id.clone() else {
                self.failure = Some(error);
                self.finished = true;
                return std::mem::take(step);
            };
            let mut params = json!({ "sessionId": session_id });
            if let Some(rest) = self.request.prompt.strip_prefix("/compact") {
                let how = rest.trim();
                if !how.is_empty() {
                    params["instructions"] = json!(how);
                }
            }
            step.writes.push(wire(&json!({
                "jsonrpc": "2.0",
                "id": COMPACT_ID,
                "method": "x.ai/compact_conversation",
                "params": params,
            })));
            return std::mem::take(step);
        }
        if id == INFO_ID {
            if looks_like_method_not_found(&error) && !self.info_fallback {
                if let Some(session_id) = self.session_id.clone() {
                    self.info_fallback = true;
                    step.writes.push(info_wire(&session_id, INFO_ID, true));
                    return std::mem::take(step);
                }
            }
            // Occupancy is optional. Fall through to weekly % / finish.
            self.request_billing_or_finish(step);
            return std::mem::take(step);
        }
        if id == USAGE_ID {
            if looks_like_method_not_found(&error) && !self.usage_fallback {
                if let Some(session_id) = self.session_id.clone() {
                    self.usage_fallback = true;
                    step.writes.push(usage_wire(&session_id, true));
                    return std::mem::take(step);
                }
            }
            // Weekly % is optional. A miss must not fail the turn.
            self.finished = true;
            return std::mem::take(step);
        }
        if self.pending_info.remove(&id) {
            // A mid-turn occupancy poll failed. The turn is still live.
            return std::mem::take(step);
        }
        if self.pending_steers.remove(&id) {
            if looks_like_method_not_found(&error) {
                if let Some(retry) = self.interject_fallback() {
                    step.writes.push(retry.wire);
                    self.pending_steers.insert(retry.id);
                    step.steer_responses.push(SteerResponse {
                        id,
                        result: Ok(String::new()),
                    });
                    return std::mem::take(step);
                }
            }
            step.steer_responses.push(SteerResponse {
                id,
                result: Err(error),
            });
            return std::mem::take(step);
        }
        self.failure = Some(error);
        self.finished = true;
        std::mem::take(step)
    }

    fn push_session(&mut self, value: &Value, step: &mut Step) -> Step {
        let session_id = value
            .pointer("/result/sessionId")
            .or_else(|| value.pointer("/result/session_id"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| match &self.request.cont {
                Continue::Resume(id) => Some(id.clone()),
                _ => None,
            });
        let Some(session_id) = session_id else {
            self.failure = Some("session/new returned no sessionId".into());
            self.finished = true;
            return std::mem::take(step);
        };
        self.session_id = Some(session_id.clone());
        self.terminal.session = Some(session_id.clone());
        self.replaying = false;
        let model = value
            .pointer("/result/models/current/id")
            .or_else(|| value.pointer("/result/model"))
            .and_then(Value::as_str)
            .map(str::to_string);
        self.terminal.model.clone_from(&model);
        step.events.push(Event::Started {
            session: session_id.clone(),
            model,
        });
        if self.request.is_command {
            let prompt = self.request.prompt.trim();
            if prompt == "/clear" || prompt.starts_with("/clear ") {
                self.failure =
                    Some("Grok has /new, not /clear; refusing rather than sending prose".into());
                self.finished = true;
                return std::mem::take(step);
            }
            self.compacting = true;
            step.events
                .push(Event::Compaction(crate::command::Compaction::Started));
        }
        let next = self.start_prompt(&session_id);
        if next.is_empty() {
            self.finished = true;
        } else {
            step.writes.push(next);
            if let Some(wire) = self.poll_occupancy() {
                step.writes.push(wire);
            }
            if self.request.operation == crate::request::Operation::Interrupt {
                self.interrupt_requested = true;
                self.terminal.stop = Stop::Other("interrupted".into());
                self.finished = true;
            }
        }
        std::mem::take(step)
    }

    fn finish_prompt(&mut self, value: &Value) {
        let reason = value
            .pointer("/result/stopReason")
            .or_else(|| value.pointer("/result/stop_reason"))
            .and_then(Value::as_str)
            .unwrap_or("end_turn");
        self.terminal.stop = match reason {
            "cancelled" | "canceled" => Stop::Other("interrupted".into()),
            "max_tokens" | "max_turns" => Stop::Other(reason.into()),
            "refusal" | "error" => Stop::Error,
            _ => Stop::Completed,
        };
        if let Some(usage) = value
            .pointer("/result/usage")
            .or_else(|| value.pointer("/result/_meta/usage"))
        {
            // Accumulate: a replace would wipe occupancy learned from
            // `auto_compact_completed` or a 1-call `turn_completed` when the
            // prompt result only carries the turn's billed aggregate.
            self.terminal.usage.accumulate(&grok_usage(usage));
        }
    }

    fn request_occupancy_then_billing(&mut self, step: &mut Step) {
        if let Some(wire) = self.maybe_info_request() {
            step.writes.push(wire);
            return;
        }
        self.request_billing_or_finish(step);
    }

    fn request_billing_or_finish(&mut self, step: &mut Step) {
        if let Some(wire) = self.maybe_usage_request() {
            step.writes.push(wire);
        } else {
            self.finished = true;
        }
    }

    fn maybe_info_request(&mut self) -> Option<String> {
        let session_id = self.session_id.as_ref()?;
        self.last_info_secs = unix_secs();
        Some(info_wire(session_id, INFO_ID, self.info_fallback))
    }

    /// Live occupancy during a turn. Grok never emits `usage_update`; the
    /// only occupancy RPC is `_x.ai/session/info`. Polled after tool results
    /// so the header can tick up instead of jumping at `turn_completed`.
    fn poll_occupancy(&mut self) -> Option<String> {
        let session_id = self.session_id.as_ref()?;
        if !self.pending_info.is_empty() {
            return None;
        }
        let now = unix_secs();
        if self.last_info_secs != 0 && now.saturating_sub(self.last_info_secs) < INFO_POLL_SECS {
            return None;
        }
        let id = self.next_id;
        self.next_id += 1;
        self.pending_info.insert(id);
        self.last_info_secs = now.max(1);
        Some(info_wire(session_id, id, self.info_fallback))
    }

    fn should_fetch_usage(&self) -> bool {
        self.session_id.is_some() && {
            let now = unix_secs();
            let last = LAST_USAGE_SECS.load(std::sync::atomic::Ordering::Relaxed);
            now.saturating_sub(last) >= USAGE_INTERVAL_SECS
        }
    }

    fn maybe_usage_request(&mut self) -> Option<String> {
        let session_id = self.session_id.as_ref()?;
        if !self.should_fetch_usage() {
            self.finished = true;
            return None;
        }
        LAST_USAGE_SECS.store(unix_secs(), std::sync::atomic::Ordering::Relaxed);
        Some(usage_wire(session_id, false))
    }

    fn session_update(&mut self, value: &Value) -> Vec<Event> {
        let update = value
            .pointer("/params/update")
            .or_else(|| value.get("params"))
            .cloned()
            .unwrap_or(Value::Null);
        let kind = update
            .get("sessionUpdate")
            .or_else(|| update.get("session_update"))
            .and_then(Value::as_str)
            .unwrap_or("");
        match kind {
            "agent_message_chunk" | "agent_message" => text_content(&update)
                .into_iter()
                .map(|text| {
                    append_capped(&mut self.terminal.text, &text);
                    Event::Text(text)
                })
                .collect(),
            "agent_thought_chunk" | "agent_thought" => text_content(&update)
                .into_iter()
                .map(Event::Thinking)
                .collect(),
            "tool_call" => vec![Event::ToolCall {
                id: update
                    .get("toolCallId")
                    .or_else(|| update.get("tool_call_id"))
                    .and_then(Value::as_str)
                    .map(str::to_string),
                name: tool_name(&update),
                input: update
                    .get("rawInput")
                    .or_else(|| update.get("raw_input"))
                    .cloned()
                    .unwrap_or(Value::Null),
            }],
            "tool_call_update" => {
                let status = update.get("status").and_then(Value::as_str).unwrap_or("");
                if status != "completed" && status != "failed" {
                    return Vec::new();
                }
                vec![Event::ToolResult {
                    id: update
                        .get("toolCallId")
                        .or_else(|| update.get("tool_call_id"))
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    ok: Some(status == "completed"),
                    output: update
                        .get("rawOutput")
                        .or_else(|| update.get("raw_output"))
                        .map(value_as_text)
                        .unwrap_or_default(),
                }]
            }
            "available_commands_update" => {
                let names = command_names(&update);
                if names.is_empty() {
                    return Vec::new();
                }
                vec![Event::Commands(crate::command::Commands {
                    all: names,
                    skills: Vec::new(),
                })]
            }
            "usage_update" | "turn_completed" => {
                // Grok puts the turn's usage on `_x.ai/session/update`
                // `turn_completed`, not on ACP `usage_update` and not as
                // `costUsd`. Ticks are 1e-9 USD (session 1.626e9 ticks ≈ $1.63).
                let payload = update.get("usage").unwrap_or(&update);
                let usage = grok_usage(payload);
                self.terminal.usage.accumulate(&usage);
                vec![Event::Usage(usage)]
            }
            "auto_compact_completed" => {
                // Live occupancy after Grok's own compact. Session 01a09ca7:
                // `tokens_before` 179_555 / `tokens_after` 10_201 — not the
                // turn's billed `totalTokens`.
                let after = update
                    .get("tokens_after")
                    .or_else(|| update.get("tokensAfter"))
                    .and_then(Value::as_u64);
                let mut events = vec![Event::Compaction(Compaction::Finished {
                    ok: true,
                    error: None,
                })];
                if let Some(after) = after {
                    let usage = Usage {
                        context_tokens: Some(after),
                        context_window: Some(GROK_CONTEXT_WINDOW),
                        ..Usage::default()
                    };
                    self.terminal.usage.accumulate(&usage);
                    events.push(Event::Usage(usage));
                }
                events
            }
            _ => Vec::new(),
        }
    }

    fn permission_request(&mut self, value: &Value) -> PermissionStep {
        let Some(id) = value.get("id").cloned() else {
            return PermissionStep::default();
        };
        let params = value.get("params").unwrap_or(&Value::Null);
        let tool_call = params.get("toolCall").unwrap_or(params);
        let tool = tool_name(tool_call);
        let input = tool_call
            .get("rawInput")
            .or_else(|| tool_call.get("raw_input"))
            .cloned()
            .unwrap_or(Value::Null);
        let options = params.get("options").cloned().unwrap_or_else(|| json!([]));
        let key = match &id {
            Value::Number(n) => n.to_string(),
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        self.pending.insert(
            key.clone(),
            PendingApproval {
                rpc_id: id,
                options,
            },
        );
        // Auto/Bypass must answer on the ACP pipe. Grok still emits
        // `session/request_permission` for writes outside the workspace
        // (session 01a09ca7: `~/.grok/config.toml` sat 30 minutes). Leaving
        // the question for the host — and then not auto-answering it —
        // stalls the turn until APPROVAL_TIMEOUT.
        if matches!(
            self.request.permission,
            Permission::Auto | Permission::Bypass
        ) {
            return PermissionStep {
                event: None,
                write: self.respond(&key, &Decision::Allow),
            };
        }
        PermissionStep {
            event: Some(Event::ApprovalRequest(Approval {
                id: key,
                tool,
                input,
            })),
            write: None,
        }
    }

    /// Encode mid-turn input. Does not cancel the turn.
    pub fn steer(&mut self, message: &str) -> Option<SteerRequest> {
        let session_id = self.session_id.as_ref()?;
        self.last_steer = Some(message.to_string());
        let id = self.next_id;
        self.next_id += 1;
        self.pending_steers.insert(id);
        Some(SteerRequest {
            id,
            wire: wire(&json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": "_x.ai/interject",
                "params": { "sessionId": session_id, "text": message },
            })),
        })
    }

    fn interject_fallback(&mut self) -> Option<SteerRequest> {
        let session_id = self.session_id.as_ref()?;
        let text = self.last_steer.clone().unwrap_or_default();
        let id = self.next_id;
        self.next_id += 1;
        self.pending_steers.insert(id);
        Some(SteerRequest {
            id,
            wire: wire(&json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": "x.ai/interject",
                "params": { "sessionId": session_id, "text": text },
            })),
        })
    }

    /// Kick the in-flight turn. Session remains and can be resumed.
    pub fn interrupt(&mut self) -> Option<String> {
        let session_id = self.session_id.as_ref()?;
        self.interrupt_requested = true;
        Some(Self::cancel_wire(session_id))
    }

    pub fn respond(&mut self, id: &str, decision: &Decision) -> Option<String> {
        let pending = self.pending.remove(id)?;
        let option_id = pick_option(&pending.options, decision)
            .or_else(|| matches!(decision, Decision::Allow).then(|| "allow-once".to_string()));
        let result = match option_id {
            Some(option_id) => json!({
                "outcome": { "outcome": "selected", "optionId": option_id }
            }),
            None => json!({ "outcome": { "outcome": "cancelled" } }),
        };
        Some(wire(&json!({
            "jsonrpc": "2.0",
            "id": pending.rpc_id,
            "result": result,
        })))
    }
}

fn json_id(value: &Value) -> Option<u64> {
    value.get("id").and_then(Value::as_u64)
}

fn rpc_error(value: &Value) -> Option<String> {
    let error = value.get("error")?;
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("rpc error");
    let code = error.get("code").and_then(Value::as_i64);
    Some(match code {
        Some(code) => format!("{code} {message}"),
        None => message.to_string(),
    })
}

fn looks_like_method_not_found(error: &str) -> bool {
    let lower = error.to_ascii_lowercase();
    lower.contains("method not found")
        || lower.contains("-32601")
        || lower.contains("unknown method")
}

fn text_content(update: &Value) -> Option<String> {
    let content = update.get("content")?;
    if let Some(text) = content.get("text").and_then(Value::as_str) {
        return Some(text.to_string()).filter(|s| !s.is_empty());
    }
    if let Some(items) = content.as_array() {
        let text: String = items
            .iter()
            .filter_map(|item| item.get("text").and_then(Value::as_str))
            .collect();
        return Some(text).filter(|s| !s.is_empty());
    }
    None
}

fn tool_name(call: &Value) -> String {
    call.pointer("/_meta/x.ai~1tool/name")
        .or_else(|| call.get("title"))
        .or_else(|| call.get("kind"))
        .and_then(Value::as_str)
        .unwrap_or("tool")
        .to_string()
}

fn command_names(update: &Value) -> Vec<String> {
    update
        .get("availableCommands")
        .or_else(|| update.get("available_commands"))
        .and_then(Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .filter_map(|entry| {
                    entry
                        .get("name")
                        .or_else(|| entry.get("command"))
                        .and_then(Value::as_str)
                        .map(|name| name.trim_start_matches('/').to_string())
                })
                .collect()
        })
        .unwrap_or_default()
}

fn unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn info_wire(session_id: &str, id: u64, fallback: bool) -> String {
    // Live grok 1.0.30: `_x.ai/session/info` returns
    // `result.context.{used,total}` — occupancy vs the window. Bare
    // `x.ai/session/info` is -32601. `_x.ai/session/usage` is billed totals.
    let method = if fallback {
        "x.ai/session/info"
    } else {
        "_x.ai/session/info"
    };
    wire(&json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": { "sessionId": session_id },
    }))
}

fn occupancy_tick(value: &Value) -> bool {
    let kind = value
        .pointer("/params/update/sessionUpdate")
        .or_else(|| value.pointer("/params/update/session_update"))
        .or_else(|| value.pointer("/params/sessionUpdate"))
        .and_then(Value::as_str)
        .unwrap_or("");
    match kind {
        "tool_call_update" => {
            let status = value
                .pointer("/params/update/status")
                .or_else(|| value.pointer("/params/status"))
                .and_then(Value::as_str)
                .unwrap_or("");
            status == "completed" || status == "failed"
        }
        "auto_compact_completed" => true,
        _ => false,
    }
}

fn usage_wire(session_id: &str, fallback: bool) -> String {
    // Live grok 1.0.30: `_x.ai/billing` returns weekly allowance
    // (`creditUsagePercent`). `_x.ai/session/usage` is session token totals
    // only — not the header chip.
    let method = if fallback {
        "x.ai/billing"
    } else {
        "_x.ai/billing"
    };
    wire(&json!({
        "jsonrpc": "2.0",
        "id": USAGE_ID,
        "method": method,
        "params": { "sessionId": session_id },
    }))
}

fn json_f64(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_u64().map(|n| n as f64))
        .or_else(|| value.as_i64().map(|n| n as f64))
}

fn first_f64(root: &Value, keys: &[&str]) -> Option<f64> {
    for key in keys {
        if let Some(n) = root.get(*key).and_then(json_f64) {
            return Some(n);
        }
    }
    None
}

fn first_reset(root: &Value) -> Option<i64> {
    for key in [
        "resetsAt",
        "resets_at",
        "resetAt",
        "reset_at",
        "resetAtUnix",
        "billingPeriodEnd",
        "end",
    ] {
        if let Some(n) = root.get(key).and_then(Value::as_i64) {
            return Some(n);
        }
        if let Some(s) = root.get(key).and_then(Value::as_str) {
            if let Ok(n) = s.parse::<i64>() {
                return Some(n);
            }
            if let Some(n) = parse_iso_utc(s) {
                return Some(n);
            }
        }
    }
    None
}

fn parse_iso_utc(s: &str) -> Option<i64> {
    if s.len() < 19 {
        return None;
    }
    let y: i64 = s[0..4].parse().ok()?;
    let m: i64 = s[5..7].parse().ok()?;
    let d: i64 = s[8..10].parse().ok()?;
    let hh: i64 = s[11..13].parse().ok()?;
    let mm: i64 = s[14..16].parse().ok()?;
    let ss: i64 = s[17..19].parse().ok()?;
    if !(1..=12).contains(&m) || d == 0 || d > 31 {
        return None;
    }
    let mut y = y;
    let mut m = m;
    if m <= 2 {
        y -= 1;
        m += 9;
    } else {
        m -= 3;
    }
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * m + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    Some(days * 86400 + hh * 3600 + mm * 60 + ss)
}

/// Live occupancy from `_x.ai/session/info`.
///
/// Probe 2026-09-14 grok 1.0.30:
/// `{result:{result:{context:{used:1475,total:500000,...}}}}`.
/// This is the status-line figure (`signals.json` `contextTokensUsed`), not
/// the turn's billed `totalTokens`.
fn grok_session_info(value: &Value) -> Option<Usage> {
    let result = value.get("result").unwrap_or(value);
    let inner = result.get("result").unwrap_or(result);
    let ctx = inner.get("context")?;
    let used = first_u64(ctx, &["used", "contextTokensUsed", "context_tokens"])?;
    let total = first_u64(
        ctx,
        &["total", "contextWindowTokens", "size", "context_window"],
    )
    .unwrap_or(GROK_CONTEXT_WINDOW);
    if total == 0 {
        return None;
    }
    Some(Usage {
        context_tokens: Some(used),
        context_window: Some(total),
        ..Usage::default()
    })
}

fn first_u64(root: &Value, keys: &[&str]) -> Option<u64> {
    for key in keys {
        if let Some(n) = root.get(*key).and_then(Value::as_u64) {
            return Some(n);
        }
        if let Some(n) = root.get(*key).and_then(Value::as_f64) {
            if n >= 0.0 {
                return Some(n as u64);
            }
        }
    }
    None
}

/// Weekly allowance from `_x.ai/billing`. Live grok 1.0.30:
/// `{config:{creditUsagePercent, billingPeriodEnd}, subscription_tier}`.
fn grok_session_usage(value: &Value) -> Option<RateLimit> {
    let result = value.get("result").unwrap_or(value);
    let config = result.get("config").unwrap_or(result);
    let candidates = [
        config,
        result,
        result.get("usage").unwrap_or(result),
        result.get("limit").unwrap_or(result),
        result.get("weekly").unwrap_or(result),
        result.get("billing").unwrap_or(result),
        result.get("billingCycle").unwrap_or(result),
    ];
    for obj in candidates {
        let mut percent = first_f64(
            obj,
            &[
                "creditUsagePercent",
                "usedPercent",
                "used_percent",
                "percentUsed",
                "percent_used",
                "utilization",
            ],
        );
        if let (None, Some(used), Some(limit)) = (
            percent,
            first_f64(obj, &["used", "usedCredits", "spent"]),
            first_f64(obj, &["limit", "allowance", "cap", "max"]),
        ) {
            if limit > 0.0 {
                percent = Some(used / limit * 100.0);
            }
        }
        let Some(mut percent) = percent else {
            continue;
        };
        if percent <= 1.0 {
            percent *= 100.0;
        }
        return Some(RateLimit {
            status: "allowed".into(),
            window: Some("weekly".into()),
            resets_at: first_reset(obj).or_else(|| first_reset(result)),
            overage_status: None,
            is_using_overage: None,
            used_percent: Some(percent),
        });
    }
    None
}

fn grok_usage(usage: &Value) -> Usage {
    let num = |camel: &str, snake: &str| {
        usage
            .get(camel)
            .or_else(|| usage.get(snake))
            .and_then(Value::as_u64)
    };
    let input = num("inputTokens", "input_tokens");
    let cache_read = num("cachedReadTokens", "cache_read_input_tokens");
    // Grok's `inputTokens` includes the cached prefix (Codex shape). Fresh
    // input is the remainder; stuffing the raw sum here double-counts cache
    // in every host that also reads `cache_read_tokens`.
    let fresh = match (input, cache_read) {
        (Some(input), Some(cached)) if input >= cached => Some(input - cached),
        (Some(input), _) => Some(input),
        _ => None,
    };
    let window = num("size", "context_window")
        .or_else(|| num("contextWindowTokens", "context_window_tokens"))
        .unwrap_or(GROK_CONTEXT_WINDOW);
    Usage {
        input_tokens: fresh,
        output_tokens: num("outputTokens", "output_tokens"),
        cache_read_tokens: cache_read,
        cache_write_tokens: num("cacheCreationTokens", "cache_creation_input_tokens"),
        context_tokens: grok_occupancy(usage, window),
        context_window: Some(window),
        reasoning_tokens: num("reasoningTokens", "reasoning_tokens"),
        cost_usd: usage
            .get("costUsdTicks")
            .or_else(|| usage.get("cost_usd_ticks"))
            .and_then(|value| value.as_f64().or_else(|| value.as_u64().map(|n| n as f64)))
            // CLI `costUsdTicks` is 1e-9 USD (session 01a09ca7: 1.644e9 ticks).
            // xAI API `cost_in_usd_ticks` is 1e-10; do not mix the two.
            .map(|ticks| ticks / 1_000_000_000.0)
            .or_else(|| usage.get("cost").and_then(Value::as_f64))
            .or_else(|| usage.get("costUsd").and_then(Value::as_f64)),
        ..Usage::default()
    }
}

/// Live window fill, never the turn's billed sum.
///
/// `turn_completed.usage.totalTokens` / `inputTokens` are billed sums.
/// A 1-call compact or notes pass reports the *old* window as `inputTokens`
/// (session 01a09ca7: 264k after compact, live fill 29k). Treating that as
/// occupancy made the next prompt look like 260k and re-trigger cliff
/// steers. Occupancy is only `used` / `contextTokensUsed` on the payload,
/// `_x.ai/session/info`, or `auto_compact_completed.tokens_after`.
fn grok_occupancy(usage: &Value, window: u64) -> Option<u64> {
    let num = |camel: &str, snake: &str| {
        usage
            .get(camel)
            .or_else(|| usage.get(snake))
            .and_then(Value::as_u64)
    };
    let plausible = |used: u64| used > 0 && used <= window.saturating_mul(2);
    num("used", "context_tokens")
        .or_else(|| num("contextTokensUsed", "context_tokens_used"))
        .filter(|&used| plausible(used))
}

#[derive(Default)]
struct PermissionStep {
    event: Option<Event>,
    write: Option<String>,
}

fn value_as_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

fn pick_option(options: &Value, decision: &Decision) -> Option<String> {
    let entries = options.as_array()?;
    let want_allow = matches!(decision, Decision::Allow);
    let id_of = |entry: &Value| {
        entry
            .get("optionId")
            .or_else(|| entry.get("option_id"))
            .or_else(|| entry.get("id"))
            .and_then(Value::as_str)
            .map(str::to_string)
    };
    for entry in entries {
        let kind = entry
            .get("kind")
            .or_else(|| entry.get("optionId"))
            .or_else(|| entry.get("option_id"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_ascii_lowercase();
        let allow = kind.contains("allow") || kind.contains("approve");
        let deny = kind.contains("reject") || kind.contains("deny");
        if want_allow && allow || !want_allow && deny {
            return id_of(entry);
        }
    }
    None
}

fn wire(value: &Value) -> String {
    format!("{value}\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::Agent;
    use crate::request::Request;

    fn protocol() -> Protocol {
        Protocol::new(Request::new(Agent::Grok, "hi").permission(Permission::Auto))
    }

    #[test]
    fn opening_is_initialize_only() {
        let opening = Protocol::opening();
        assert_eq!(opening.len(), 1);
        assert!(opening[0].contains("\"method\":\"initialize\""));
        assert!(opening[0].contains("\"protocolVersion\":1"));
    }

    #[test]
    fn auto_is_native_automode_not_yolo() {
        let mut p = protocol();
        let _ = p.push(&json!({"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1}}));
        let open = p.open_session();
        let meta = &open["params"]["_meta"];
        assert_eq!(meta["autoMode"], json!(true));
        assert!(meta.get("yoloMode").is_none());
    }

    #[test]
    fn steer_is_interject_not_a_new_prompt() {
        let mut p = protocol();
        p.session_id = Some("sess-1".into());
        let steer = p.steer("stop the tests").expect("session is known");
        assert!(steer.wire.contains("_x.ai/interject"));
        assert!(steer.wire.contains("\"sessionId\":\"sess-1\""));
        assert!(steer.wire.contains("\"text\":\"stop the tests\""));
        assert!(!steer.wire.contains("session/prompt"));
        assert!(!steer.wire.contains("session/cancel"));
    }

    #[test]
    fn interrupt_is_cancel_and_keeps_the_session() {
        let mut p = protocol();
        p.session_id = Some("sess-1".into());
        let wire = p.interrupt().expect("session is known");
        assert!(wire.contains("session/cancel"));
        assert!(wire.contains("\"sessionId\":\"sess-1\""));
        assert_eq!(p.session_id.as_deref(), Some("sess-1"));
    }

    #[test]
    fn compact_uses_the_acp_method() {
        let mut request = Request::command(
            Agent::Grok,
            &crate::Command::Compact {
                instructions: Some("keep the auth tests".into()),
            },
        );
        request.cont = Continue::Resume("sess-9".into());
        let p = Protocol::new(request);
        let wire = p.command_wire("sess-9");
        assert!(wire.contains("compact_conversation"));
        assert!(wire.contains("keep the auth tests"));
        assert!(!wire.contains("session/prompt"));
    }

    #[test]
    fn load_replay_does_not_emit_history_as_a_new_turn() {
        let mut p = protocol();
        let replay = json!({
            "method": "session/update",
            "params": {
                "update": {
                    "sessionUpdate": "agent_message_chunk",
                    "content": { "type": "text", "text": "old reply" }
                }
            }
        });
        assert!(p.push(&replay).events.is_empty());
        let _ = p.push_session(
            &json!({"jsonrpc":"2.0","id":2,"result":{"sessionId":"sess-1"}}),
            &mut Step::default(),
        );
        let live = p.push(&replay);
        assert_eq!(live.events, vec![Event::Text("old reply".into())]);
    }

    #[test]
    fn agent_text_chunks_become_events() {
        let mut p = protocol();
        p.session_id = Some("sess-1".into());
        p.replaying = false;
        let events = p.session_update(&json!({
            "method": "session/update",
            "params": {
                "sessionId": "sess-1",
                "update": {
                    "sessionUpdate": "agent_message_chunk",
                    "content": { "type": "text", "text": "pong" }
                }
            }
        }));
        assert_eq!(events, vec![Event::Text("pong".into())]);
        assert_eq!(p.terminal.text.trim_end(), "pong");
    }

    #[test]
    fn turn_completed_usage_converts_cost_ticks() {
        let mut p = protocol();
        p.replaying = false;
        let events = p.session_update(&json!({
            "method": "session/update",
            "params": {
                "update": {
                    "sessionUpdate": "turn_completed",
                    "usage": {
                        "inputTokens": 18033,
                        "outputTokens": 189,
                        "cachedReadTokens": 0,
                        "costUsdTicks": 126480000
                    }
                }
            }
        }));
        match events.as_slice() {
            [Event::Usage(usage)] => {
                assert_eq!(usage.input_tokens, Some(18033));
                assert_eq!(usage.output_tokens, Some(189));
                // Billed input is not occupancy; session/info supplies that.
                assert_eq!(usage.context_tokens, None);
                assert_eq!(usage.context_window, Some(500_000));
                let cost = usage.cost_usd.expect("ticks convert");
                assert!((cost - 0.12648).abs() < 1e-9);
            }
            other => panic!("expected Usage, got {other:?}"),
        }
    }

    #[test]
    fn grok_usage_uses_occupancy_not_turn_aggregate() {
        // Session 01a09ca7 turn 1: billed input includes cache; live context
        // was 71,933. totalTokens 593,447 is the in-turn sum and must not
        // become context_tokens.
        let usage = grok_usage(&json!({
            "inputTokens": 579_290,
            "outputTokens": 1_644,
            "cachedReadTokens": 506_624,
            "totalTokens": 593_447,
            "used": 71_933,
            "costUsdTicks": 1_644_192_400u64
        }));
        assert_eq!(usage.input_tokens, Some(72_666));
        assert_eq!(usage.cache_read_tokens, Some(506_624));
        assert_eq!(usage.context_tokens, Some(71_933));
        assert_eq!(usage.context_window, Some(500_000));
        let cost = usage.cost_usd.expect("ticks convert");
        assert!((cost - 1.644_192_4).abs() < 1e-9);
    }

    #[test]
    fn grok_usage_leaves_context_unset_without_occupancy() {
        let usage = grok_usage(&json!({
            "inputTokens": 579_290,
            "cachedReadTokens": 506_624,
            "totalTokens": 593_447,
            "modelCalls": 12
        }));
        assert_eq!(usage.input_tokens, Some(72_666));
        assert_eq!(usage.context_tokens, None);
        assert_eq!(usage.context_window, Some(500_000));
    }

    #[test]
    fn grok_usage_one_call_input_is_not_occupancy() {
        // Compact/learn are 1-call turns whose input is the old window.
        let usage = grok_usage(&json!({
            "inputTokens": 264_675,
            "outputTokens": 2_045,
            "cachedReadTokens": 0,
            "totalTokens": 266_720,
            "modelCalls": 1
        }));
        assert_eq!(usage.context_tokens, None);
        assert_eq!(usage.context_window, Some(500_000));
    }

    #[test]
    fn grok_usage_rejects_billed_sum_as_occupancy() {
        // Session 01a09ca7 turn 4: 65 calls, 7.6M billed, live context ~180k.
        let usage = grok_usage(&json!({
            "inputTokens": 7_588_418,
            "cachedReadTokens": 7_292_032,
            "totalTokens": 7_641_353,
            "modelCalls": 65
        }));
        assert_eq!(usage.context_tokens, None);
        assert_eq!(usage.context_window, Some(500_000));
    }

    #[test]
    fn auto_compact_completed_sets_live_occupancy() {
        let mut p = protocol();
        p.replaying = false;
        let events = p.session_update(&json!({
            "method": "session/update",
            "params": {
                "update": {
                    "sessionUpdate": "auto_compact_completed",
                    "tokens_before": 179_555,
                    "tokens_after": 10_201
                }
            }
        }));
        assert!(events.iter().any(|event| matches!(
            event,
            Event::Compaction(Compaction::Finished {
                ok: true,
                error: None
            })
        )));
        match events.iter().find(|event| matches!(event, Event::Usage(_))) {
            Some(Event::Usage(usage)) => {
                assert_eq!(usage.context_tokens, Some(10_201));
                assert_eq!(usage.context_window, Some(500_000));
            }
            other => panic!("expected Usage after compact, got {other:?}"),
        }
    }

    #[test]
    fn auto_answers_permission_without_asking_the_host() {
        let mut p = protocol();
        p.session_id = Some("sess-1".into());
        let step = p.push(&json!({
            "jsonrpc": "2.0",
            "id": 99,
            "method": "session/request_permission",
            "params": {
                "toolCall": { "title": "search_replace", "rawInput": { "path": "/Users/revenge/.grok/config.toml" } },
                "options": [
                    { "optionId": "allow-once", "kind": "allow_once" },
                    { "optionId": "reject-once", "kind": "reject_once" }
                ]
            }
        }));
        assert!(
            step.events.is_empty(),
            "Auto must not surface an approval card"
        );
        assert_eq!(step.writes.len(), 1);
        assert!(step.writes[0].contains("\"id\":99"));
        assert!(step.writes[0].contains("allow-once"));
        assert!(!step.writes[0].contains("cancelled"));
    }

    #[test]
    fn session_usage_percent_becomes_weekly_rate_limit() {
        let limit = grok_session_usage(&json!({
            "jsonrpc": "2.0",
            "id": 5,
            "result": {
                "config": {
                    "creditUsagePercent": 41.0,
                    "billingPeriodEnd": "2026-09-20T11:48:54Z"
                },
                "subscription_tier": "SuperGrok Plus"
            }
        }))
        .expect("percent present");
        assert_eq!(limit.used_percent, Some(41.0));
        assert_eq!(limit.window.as_deref(), Some("weekly"));
        assert!(!limit.is_blocking());
    }

    #[test]
    fn session_info_occupancy_is_used_vs_total() {
        let usage = grok_session_info(&json!({
            "jsonrpc": "2.0",
            "id": 6,
            "result": {
                "result": {
                    "sessionId": "sess-1",
                    "context": {
                        "used": 325_827,
                        "total": 500_000,
                        "usagePct": 65
                    }
                }
            }
        }))
        .expect("occupancy present");
        assert_eq!(usage.context_tokens, Some(325_827));
        assert_eq!(usage.context_window, Some(500_000));
        assert!(usage.input_tokens.is_none());
    }

    #[test]
    fn completed_tool_polls_session_info_for_live_occupancy() {
        let mut p = protocol();
        p.session_id = Some("sess-1".into());
        p.replaying = false;
        let step = p.push(&json!({
            "method": "session/update",
            "params": {
                "update": {
                    "sessionUpdate": "tool_call_update",
                    "status": "completed",
                    "toolCallId": "call-1"
                }
            }
        }));
        assert_eq!(step.writes.len(), 1);
        assert!(step.writes[0].contains("_x.ai/session/info"));
        assert!(step.writes[0].contains("\"sessionId\":\"sess-1\""));
        assert!(
            !step.writes[0].contains("\"id\":6"),
            "mid-turn poll is not the end-of-turn INFO_ID"
        );
        let again = p.push(&json!({
            "method": "session/update",
            "params": {
                "update": {
                    "sessionUpdate": "tool_call_update",
                    "status": "completed",
                    "toolCallId": "call-2"
                }
            }
        }));
        assert!(
            again.writes.is_empty(),
            "occupancy polls are 15s apart and one in flight"
        );
    }

    #[test]
    fn end_of_turn_requests_session_info_before_billing() {
        let mut p = protocol();
        p.session_id = Some("sess-1".into());
        p.replaying = false;
        let step = p.push(&json!({
            "jsonrpc": "2.0",
            "id": 3,
            "result": { "stopReason": "end_turn" }
        }));
        assert_eq!(step.writes.len(), 1);
        assert!(step.writes[0].contains("_x.ai/session/info"));
        assert!(step.writes[0].contains("\"id\":6"));
        assert!(!p.finished);
    }
}
