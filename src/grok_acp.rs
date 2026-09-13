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
use crate::event::{Event, Terminal, append_capped};
use crate::outcome::{Stop, Usage};
use crate::request::Request;

const INIT_ID: u64 = 1;
const SESSION_ID: u64 = 2;
const PROMPT_ID: u64 = 3;
const COMPACT_ID: u64 = 4;
const NEXT_ID: u64 = 10;

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
                    return step;
                }
                COMPACT_ID => {
                    self.compacting = false;
                    self.terminal.stop = Stop::Completed;
                    self.finished = true;
                    step.events
                        .push(Event::Compaction(crate::command::Compaction::Finished {
                            ok: true,
                            error: None,
                        }));
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
                step.events.extend(self.session_update(value));
            }
            "session/request_permission" => {
                if let Some(event) = self.permission_request(value) {
                    step.events.push(event);
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
            self.terminal.usage = grok_usage(usage);
        }
        self.finished = true;
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
            "usage_update" => {
                let usage = grok_usage(&update);
                self.terminal.usage.accumulate(&usage);
                vec![Event::Usage(usage)]
            }
            _ => Vec::new(),
        }
    }

    fn permission_request(&mut self, value: &Value) -> Option<Event> {
        let id = value.get("id")?.clone();
        let params = value.get("params")?;
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
        Some(Event::ApprovalRequest(Approval {
            id: key,
            tool,
            input,
        }))
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
        let option_id = pick_option(&pending.options, decision);
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

fn grok_usage(usage: &Value) -> Usage {
    let num = |camel: &str, snake: &str| {
        usage
            .get(camel)
            .or_else(|| usage.get(snake))
            .and_then(Value::as_u64)
    };
    Usage {
        input_tokens: num("inputTokens", "input_tokens"),
        output_tokens: num("outputTokens", "output_tokens"),
        cache_read_tokens: num("cachedReadTokens", "cache_read_input_tokens"),
        cache_write_tokens: num("cacheCreationTokens", "cache_creation_input_tokens"),
        context_tokens: num("totalTokens", "total_tokens").or_else(|| num("used", "used")),
        context_window: num("size", "context_window"),
        cost_usd: usage
            .get("cost")
            .and_then(Value::as_f64)
            .or_else(|| usage.get("costUsd").and_then(Value::as_f64)),
        ..Usage::default()
    }
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
            return entry
                .get("optionId")
                .or_else(|| entry.get("option_id"))
                .or_else(|| entry.get("id"))
                .and_then(Value::as_str)
                .map(str::to_string);
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
    fn agent_text_chunks_become_events() {
        let mut p = protocol();
        p.session_id = Some("sess-1".into());
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
}
