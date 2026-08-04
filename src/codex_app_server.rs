//! Codex app-server transport protocol.
//!
//! `codex exec --json` is a one-way event stream. It cannot accept another
//! message or route a permission question back to a host. App-server exposes
//! both over JSON-RPC on stdio, while also producing token-level assistant
//! deltas and an explicit set of writable roots.
//!
//! The shapes here were generated from codex-cli 0.145.0 and exercised against
//! codex-cli 0.146.0 on 2026-08-04. They are kept as small `serde_json::Value`
//! projections rather
//! than vendoring the multi-megabyte generated schema into this crate.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::agent::{Agent, Continue, Permission};
use crate::approval::{Approval, Decision};
use crate::event::{Event, Terminal};
use crate::outcome::{Stop, Usage};
use crate::request::Request;

const OPEN_ID: u64 = 2;
const TURN_ID: u64 = 3;

#[derive(Debug, Clone)]
enum PendingApproval {
    Command { rpc_id: Value },
    File { rpc_id: Value },
    Permissions { rpc_id: Value, requested: Value },
}

/// One decoded app-server record.
#[derive(Debug, Default)]
pub(crate) struct Step {
    pub events: Vec<Event>,
    pub writes: Vec<String>,
    pub steer_responses: Vec<SteerResponse>,
}

/// One `turn/steer` request written to app-server.
#[derive(Debug)]
pub(crate) struct SteerRequest {
    pub id: u64,
    pub wire: String,
}

/// App-server's acceptance or rejection of one `turn/steer` request.
#[derive(Debug)]
pub(crate) struct SteerResponse {
    pub id: u64,
    pub result: std::result::Result<String, String>,
}

/// State that spans the JSON-RPC records of one turn.
#[derive(Debug)]
pub(crate) struct Protocol {
    request: Request,
    pub terminal: Terminal,
    pub thread_id: Option<String>,
    pub turn_id: Option<String>,
    pub finished: bool,
    pub failure: Option<String>,
    pending: HashMap<String, PendingApproval>,
    pending_steers: HashSet<u64>,
    next_id: u64,
}

impl Protocol {
    pub fn new(request: Request) -> Self {
        Self {
            request,
            terminal: Terminal::default(),
            thread_id: None,
            turn_id: None,
            finished: false,
            failure: None,
            pending: HashMap::new(),
            pending_steers: HashSet::new(),
            next_id: 10,
        }
    }

    /// Initialize the connection and open or resume its thread.
    pub fn opening(&self) -> Vec<String> {
        vec![
            wire(&json!({
                "id": 1,
                "method": "initialize",
                "params": {
                    "clientInfo": {
                        "name": "agent-abstraction",
                        "title": "agent-abstraction",
                        "version": env!("CARGO_PKG_VERSION"),
                    },
                    "capabilities": {"experimentalApi": true},
                },
            })),
            wire(&json!({"method": "initialized"})),
            wire(&self.open_thread()),
        ]
    }

    fn open_thread(&self) -> Value {
        let plan = self.request.plan();
        let roots = roots(&self.request);
        let cwd = cwd(&self.request);
        let mut params = json!({
            "cwd": cwd,
            "model": plan.model,
            "approvalPolicy": approval_policy(&plan),
            "sandbox": sandbox_mode(plan.permission),
            "runtimeWorkspaceRoots": roots,
        });
        match &plan.cont {
            Continue::New => json!({
                "id": OPEN_ID,
                "method": "thread/start",
                "params": params,
            }),
            Continue::Resume(thread_id) => {
                params["threadId"] = json!(thread_id);
                json!({
                    "id": OPEN_ID,
                    "method": "thread/resume",
                    "params": params,
                })
            }
            // Codex rejects these before the transport is selected.
            Continue::NewWith(_) | Continue::Fork(_) => unreachable!("unsupported Codex session"),
        }
    }

    fn start_turn(&self, thread_id: &str) -> String {
        let plan = self.request.plan();
        let roots = roots(&self.request);
        // app-server does not implicitly add `cwd` to a turn's writable roots.
        // Verified against codex-cli 0.146.0 on 2026-08-04: omitting the first
        // root leaves a one-directory request able to read its cwd but unable
        // to write there. Every declared root therefore belongs in both the
        // runtime scope and the workspace-write sandbox policy.
        let writable = roots.clone();
        let sandbox = match plan.permission {
            Permission::ReadOnly | Permission::Plan => {
                json!({"type": "readOnly", "networkAccess": false})
            }
            Permission::Edit | Permission::Auto => json!({
                "type": "workspaceWrite",
                "writableRoots": writable,
                "networkAccess": false,
            }),
            Permission::Bypass => json!({"type": "dangerFullAccess"}),
        };
        wire(&json!({
            "id": TURN_ID,
            "method": "turn/start",
            "params": {
                "threadId": thread_id,
                "input": [{
                    "type": "text",
                    "text": Agent::Codex.effective_prompt(&plan),
                }],
                "cwd": cwd(&self.request),
                "model": plan.model,
                "effort": plan.effort,
                "approvalPolicy": approval_policy(&plan),
                "runtimeWorkspaceRoots": roots,
                "sandboxPolicy": sandbox,
                "outputSchema": plan.schema,
            },
        }))
    }

    /// Turn one app-server response, notification, or request into host events.
    #[allow(
        clippy::too_many_lines,
        reason = "one protocol dispatcher keeps each app-server method visible in one match"
    )]
    pub fn push(&mut self, value: &Value) -> Step {
        let mut step = Step::default();

        if value.get("id").and_then(Value::as_u64) == Some(OPEN_ID) {
            if let Some(error) = rpc_error(value) {
                self.failure = Some(error);
                self.finished = true;
                return step;
            }
            if let Some(thread_id) = value
                .pointer("/result/thread/id")
                .and_then(Value::as_str)
                .map(str::to_string)
            {
                self.thread_id = Some(thread_id.clone());
                self.terminal.session = Some(thread_id.clone());
                let model = value
                    .pointer("/result/model")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                self.terminal.model.clone_from(&model);
                step.events.push(Event::Started {
                    session: thread_id.clone(),
                    model,
                });
                step.writes.push(self.start_turn(&thread_id));
            }
            return step;
        }

        if value.get("id").and_then(Value::as_u64) == Some(TURN_ID) {
            if let Some(error) = rpc_error(value) {
                self.failure = Some(error);
                self.finished = true;
            } else if let Some(turn_id) = value.pointer("/result/turn/id").and_then(Value::as_str) {
                self.turn_id = Some(turn_id.to_string());
            }
            return step;
        }

        // Unlike streamed notifications, `turn/steer` has a decisive JSON-RPC
        // response: `{ result: { turnId } }` means Codex accepted the input,
        // while an error means it did not enter the turn. Keep that receipt
        // separate from transcript rendering so a host can still render on
        // send without mistaking a local pipe write for provider acceptance.
        if let Some(id) = value.get("id").and_then(Value::as_u64)
            && self.pending_steers.remove(&id)
        {
            let result = if let Some(error) = rpc_error(value) {
                Err(error)
            } else {
                value
                    .pointer("/result/turnId")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .ok_or_else(|| "turn/steer returned no accepted turn id".to_string())
            };
            step.steer_responses.push(SteerResponse { id, result });
            return step;
        }

        let method = value
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let params = value.get("params").cloned().unwrap_or(Value::Null);

        match method {
            "turn/started" => {
                if let Some(turn_id) = params.pointer("/turn/id").and_then(Value::as_str) {
                    self.turn_id = Some(turn_id.to_string());
                }
            }
            "item/agentMessage/delta" => {
                if let Some(delta) = params.get("delta").and_then(Value::as_str) {
                    append_fragment(&mut self.terminal.text, delta);
                    step.events.push(Event::Text(delta.to_string()));
                }
            }
            "item/reasoning/summaryTextDelta" | "item/reasoning/textDelta" => {
                if let Some(delta) = params.get("delta").and_then(Value::as_str) {
                    step.events.push(Event::Thinking(delta.to_string()));
                }
            }
            "item/started" => {
                if let Some(event) = tool_call(params.get("item")) {
                    step.events.push(event);
                }
            }
            "item/completed" => {
                if let Some(item) = params.get("item") {
                    if item.get("type").and_then(Value::as_str) == Some("agentMessage") {
                        if item.get("phase").and_then(Value::as_str) == Some("final_answer")
                            && let Some(text) = item.get("text").and_then(Value::as_str)
                        {
                            self.terminal.text = text.to_string();
                        }
                        step.events.push(Event::MessageBoundary);
                    } else if let Some(event) = tool_result(item) {
                        step.events.push(event);
                    }
                }
            }
            "thread/tokenUsage/updated" => {
                if let Some(usage) = usage(&params) {
                    self.terminal.usage = usage;
                    step.events.push(Event::Usage(usage));
                }
            }
            "turn/completed" => {
                let status = params.pointer("/turn/status").and_then(Value::as_str);
                self.terminal.stop = match status {
                    Some("completed") => Stop::Completed,
                    Some("failed") | None => Stop::Error,
                    Some(other) => Stop::Other(other.to_string()),
                };
                self.terminal.error_message = params
                    .pointer("/turn/error/message")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                self.finished = true;
            }
            "error" => {
                self.terminal.stop = Stop::Error;
                self.terminal.error_message = params
                    .get("message")
                    .and_then(Value::as_str)
                    .map(str::to_string);
            }
            "item/commandExecution/requestApproval"
            | "item/fileChange/requestApproval"
            | "item/permissions/requestApproval" => {
                if let Some(id) = value.get("id").cloned() {
                    let key = approval_key(&id);
                    let (tool, pending) = match method {
                        "item/commandExecution/requestApproval" => {
                            ("Bash", PendingApproval::Command { rpc_id: id })
                        }
                        "item/fileChange/requestApproval" => {
                            ("Write", PendingApproval::File { rpc_id: id })
                        }
                        _ => (
                            "Permissions",
                            PendingApproval::Permissions {
                                rpc_id: id,
                                requested: params
                                    .get("permissions")
                                    .cloned()
                                    .unwrap_or_else(|| json!({})),
                            },
                        ),
                    };
                    self.pending.insert(key.clone(), pending);
                    step.events.push(Event::ApprovalRequest(Approval {
                        id: key,
                        tool: tool.into(),
                        input: params,
                    }));
                }
            }
            _ => {}
        }

        step
    }

    /// Encode a same-turn user message once thread and turn ids are known.
    pub fn steer(&mut self, message: &str) -> Option<SteerRequest> {
        let thread_id = self.thread_id.as_ref()?;
        let turn_id = self.turn_id.as_ref()?;
        let id = self.next_id;
        self.next_id += 1;
        self.pending_steers.insert(id);
        Some(SteerRequest {
            id,
            wire: wire(&json!({
                "id": id,
                "method": "turn/steer",
                "params": {
                    "threadId": thread_id,
                    "expectedTurnId": turn_id,
                    "input": [{"type": "text", "text": message}],
                },
            })),
        })
    }

    /// Encode the response to a server-side permission request.
    pub fn respond(&mut self, id: &str, decision: &Decision) -> Option<String> {
        let pending = self.pending.remove(id)?;
        let allow = matches!(decision, Decision::Allow);
        let (rpc_id, result) = match pending {
            PendingApproval::Command { rpc_id } | PendingApproval::File { rpc_id } => (
                rpc_id,
                json!({"decision": if allow { "accept" } else { "decline" }}),
            ),
            PendingApproval::Permissions { rpc_id, requested } => (
                rpc_id,
                if allow {
                    json!({"permissions": requested, "scope": "session"})
                } else {
                    json!({"permissions": {}, "scope": "turn"})
                },
            ),
        };
        Some(wire(&json!({"id": rpc_id, "result": result})))
    }
}

fn wire(value: &Value) -> String {
    format!("{value}\n")
}

/// Append a streamed text fragment without inventing line breaks between
/// deltas, while retaining the crate-wide capture bound.
fn append_fragment(buf: &mut String, fragment: &str) {
    let remaining = crate::MAX_CAPTURE.saturating_sub(buf.len());
    if remaining == 0 {
        return;
    }
    let mut cut = fragment.len().min(remaining);
    while cut > 0 && !fragment.is_char_boundary(cut) {
        cut -= 1;
    }
    buf.push_str(&fragment[..cut]);
}

fn approval_key(id: &Value) -> String {
    id.as_str().map_or_else(|| id.to_string(), str::to_string)
}

fn rpc_error(value: &Value) -> Option<String> {
    value
        .pointer("/error/message")
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn cwd(request: &Request) -> Option<String> {
    request
        .cwd
        .clone()
        .or_else(|| std::env::current_dir().ok())
        .as_deref()
        .map(absolute)
        .map(|path| path.to_string_lossy().into_owned())
}

fn roots(request: &Request) -> Vec<String> {
    cwd(request)
        .into_iter()
        .chain(
            request
                .extra_dirs
                .iter()
                .map(|path| absolute(path).to_string_lossy().into_owned()),
        )
        .collect()
}

fn absolute(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(path)
    }
}

fn approval_policy(plan: &crate::agent::Plan) -> &'static str {
    if plan.approvals {
        "on-request"
    } else {
        "never"
    }
}

fn sandbox_mode(permission: Permission) -> &'static str {
    match permission {
        Permission::ReadOnly | Permission::Plan => "read-only",
        Permission::Edit | Permission::Auto => "workspace-write",
        Permission::Bypass => "danger-full-access",
    }
}

fn usage(params: &Value) -> Option<Usage> {
    let last = params.pointer("/tokenUsage/last")?;
    let input = last.get("inputTokens").and_then(Value::as_u64);
    let cached = last.get("cachedInputTokens").and_then(Value::as_u64);
    Some(Usage {
        input_tokens: input.map(|total| total.saturating_sub(cached.unwrap_or(0))),
        output_tokens: last.get("outputTokens").and_then(Value::as_u64),
        cache_read_tokens: cached,
        cache_write_tokens: last.get("cacheWriteInputTokens").and_then(Value::as_u64),
        context_tokens: input,
        context_window: params
            .pointer("/tokenUsage/modelContextWindow")
            .and_then(Value::as_u64),
        reasoning_tokens: last.get("reasoningOutputTokens").and_then(Value::as_u64),
        ..Usage::default()
    })
}

fn tool_call(item: Option<&Value>) -> Option<Event> {
    let item = item?;
    let kind = item.get("type")?.as_str()?;
    let id = item.get("id").and_then(Value::as_str).map(str::to_string);
    let (name, input) = match kind {
        "commandExecution" => (
            "command_execution".to_string(),
            json!({"command": item.get("command"), "cwd": item.get("cwd")}),
        ),
        "fileChange" => ("file_change".to_string(), item.get("changes").cloned()?),
        "mcpToolCall" => (
            format!(
                "mcp__{}__{}",
                item.get("server")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown"),
                item.get("tool")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
            ),
            item.get("arguments").cloned().unwrap_or(Value::Null),
        ),
        "dynamicToolCall" => (
            item.get("tool")
                .and_then(Value::as_str)
                .unwrap_or("dynamic_tool")
                .to_string(),
            item.get("arguments").cloned().unwrap_or(Value::Null),
        ),
        "webSearch" => (
            "web_search".to_string(),
            json!({"query": item.get("query"), "action": item.get("action")}),
        ),
        _ => return None,
    };
    Some(Event::ToolCall { id, name, input })
}

fn tool_result(item: &Value) -> Option<Event> {
    let kind = item.get("type")?.as_str()?;
    let id = item.get("id").and_then(Value::as_str).map(str::to_string);
    let status = item.get("status").and_then(Value::as_str);
    let ok = status.map(|value| matches!(value, "completed" | "success"));
    let output = match kind {
        "commandExecution" => item
            .get("aggregatedOutput")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        "fileChange" => serde_json::to_string(item.get("changes").unwrap_or(&Value::Null)).ok()?,
        "mcpToolCall" => serde_json::to_string(
            item.get("result")
                .or_else(|| item.get("error"))
                .unwrap_or(&Value::Null),
        )
        .ok()?,
        "dynamicToolCall" => {
            serde_json::to_string(item.get("contentItems").unwrap_or(&Value::Null)).ok()?
        }
        "webSearch" => serde_json::to_string(item.get("results").unwrap_or(&Value::Null)).ok()?,
        _ => return None,
    };
    Some(Event::ToolResult { id, ok, output })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> Request {
        Request::new(Agent::Codex, "hello")
            .cwd("/workspace")
            .add_dir("/repo")
            .permission(Permission::Auto)
            .interactive()
            .approvals()
    }

    #[test]
    fn opens_codex_with_the_requested_posture_and_roots() {
        let opening = Protocol::new(request()).opening();
        let open: Value = serde_json::from_str(opening.last().expect("open request")).unwrap();
        assert_eq!(open["method"], "thread/start");
        assert_eq!(open["params"]["approvalPolicy"], "on-request");
        assert_eq!(open["params"]["sandbox"], "workspace-write");
        assert_eq!(
            open["params"]["runtimeWorkspaceRoots"],
            json!(["/workspace", "/repo"])
        );
    }

    #[test]
    fn a_thread_response_starts_the_turn_and_exposes_the_session() {
        let mut protocol = Protocol::new(request());
        let step = protocol.push(&json!({
            "id": OPEN_ID,
            "result": {"thread": {"id": "thread-7"}, "model": "gpt-5.6-sol"},
        }));
        assert!(matches!(
            step.events.as_slice(),
            [Event::Started { session, .. }] if session == "thread-7"
        ));
        let turn: Value = serde_json::from_str(&step.writes[0]).unwrap();
        assert_eq!(turn["params"]["sandboxPolicy"]["type"], "workspaceWrite");
        assert_eq!(
            turn["params"]["sandboxPolicy"]["writableRoots"],
            json!(["/workspace", "/repo"])
        );
    }

    #[test]
    fn deltas_and_usage_are_normalized() {
        let mut protocol = Protocol::new(request());
        let first = protocol.push(&json!({
            "method": "item/agentMessage/delta",
            "params": {"delta": "po"},
        }));
        let second = protocol.push(&json!({
            "method": "item/agentMessage/delta",
            "params": {"delta": "ng"},
        }));
        assert_eq!(first.events, vec![Event::Text("po".into())]);
        assert_eq!(second.events, vec![Event::Text("ng".into())]);
        assert_eq!(protocol.terminal.text, "pong");

        let step = protocol.push(&json!({
            "method": "thread/tokenUsage/updated",
            "params": {"tokenUsage": {"last": {
                "inputTokens": 100,
                "cachedInputTokens": 60,
                "outputTokens": 5,
                "reasoningOutputTokens": 2
            }, "modelContextWindow": 258_400}},
        }));
        let Event::Usage(usage) = &step.events[0] else {
            panic!("usage event")
        };
        assert_eq!(usage.input_tokens, Some(40));
        assert_eq!(usage.context_tokens, Some(100));
        assert_eq!(usage.context_window, Some(258_400));
    }

    #[test]
    fn completed_agent_messages_preserve_their_boundary() {
        let mut protocol = Protocol::new(request());
        let step = protocol.push(&json!({
            "method": "item/completed",
            "params": {"item": {
                "type": "agentMessage",
                "phase": "commentary",
                "text": "I checked it."
            }}
        }));

        assert_eq!(step.events, vec![Event::MessageBoundary]);
    }

    fn running_protocol() -> Protocol {
        let mut protocol = Protocol::new(request());
        protocol.push(&json!({
            "id": OPEN_ID,
            "result": {"thread": {"id": "thread-7"}, "model": "gpt-5.6-sol"},
        }));
        protocol.push(&json!({
            "id": TURN_ID,
            "result": {"turn": {"id": "turn-9"}},
        }));
        protocol
    }

    #[test]
    fn a_steer_resolves_only_from_its_acceptance_response() {
        let mut protocol = running_protocol();
        let request = protocol.steer("change course").expect("steer request");
        let wire: Value = serde_json::from_str(&request.wire).unwrap();
        assert_eq!(wire["method"], "turn/steer");
        assert_eq!(wire["params"]["expectedTurnId"], "turn-9");

        let unrelated = protocol.push(&json!({
            "id": request.id + 1,
            "result": {"turnId": "turn-9"},
        }));
        assert!(unrelated.steer_responses.is_empty());

        let accepted = protocol.push(&json!({
            "id": request.id,
            "result": {"turnId": "turn-9"},
        }));
        assert!(matches!(
            accepted.steer_responses.as_slice(),
            [SteerResponse { id, result: Ok(turn_id) }]
                if *id == request.id && turn_id == "turn-9"
        ));
    }

    #[test]
    fn a_rejected_steer_preserves_the_app_server_error() {
        let mut protocol = running_protocol();
        let request = protocol.steer("too late").expect("steer request");
        let rejected = protocol.push(&json!({
            "id": request.id,
            "error": {"code": -32602, "message": "no active turn"},
        }));
        assert!(matches!(
            rejected.steer_responses.as_slice(),
            [SteerResponse { result: Err(message), .. }] if message == "no active turn"
        ));
    }

    #[test]
    fn permissions_can_be_granted_for_the_session() {
        let mut protocol = Protocol::new(request());
        let step = protocol.push(&json!({
            "id": 91,
            "method": "item/permissions/requestApproval",
            "params": {
                "itemId": "item-1",
                "cwd": "/workspace",
                "permissions": {"fileSystem": {"write": ["/repo"]}}
            }
        }));
        let Event::ApprovalRequest(approval) = &step.events[0] else {
            panic!("approval")
        };
        let response = protocol
            .respond(&approval.id, &Decision::Allow)
            .expect("response");
        let response: Value = serde_json::from_str(&response).unwrap();
        assert_eq!(response["id"], 91);
        assert_eq!(response["result"]["scope"], "session");
        assert_eq!(
            response["result"]["permissions"]["fileSystem"]["write"],
            json!(["/repo"])
        );
    }
}
