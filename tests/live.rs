//! End-to-end tests against the real agent CLIs.
//!
//! Ignored by default: they spawn actual agents, consume real quota, and need
//! each CLI installed and authenticated. Run them deliberately:
//!
//! ```console
//! cargo test --test live -- --ignored --test-threads 1
//! ```
//!
//! Each test skips itself when its binary is absent, so running the set on a
//! machine with only one agent installed reports on that one rather than
//! failing on the others.

use std::time::Duration;

use agent_abstraction::{
    Agent, AuthState, AuthStatus, EnvPolicy, Event, Format, Permission, Probe, Request,
    SessionStore, VersionStatus, run, stream,
};

/// A prompt with exactly one correct answer, so the assertion is about the
/// plumbing rather than the model's judgement.
const PING: &str = "Reply with the single word: pong. No punctuation, no explanation.";

/// Whether the agent's binary is on PATH; tests no-op without it.
fn available(agent: Agent) -> bool {
    let found = std::env::var_os("PATH").is_some_and(|paths| {
        std::env::split_paths(&paths).any(|dir| dir.join(agent.bin()).is_file())
    });
    if !found {
        eprintln!("skipping: `{}` is not installed", agent.bin());
    }
    found
}

/// A request that keeps the run cheap, sandboxed and bounded.
fn ping(agent: Agent) -> Request {
    let request = Request::new(agent, PING)
        .permission(Permission::ReadOnly)
        .timeout(Duration::from_secs(180));
    match agent {
        // The cheapest model on each side; Codex and Copilot pick their own.
        Agent::Claude => request.model("haiku"),
        Agent::Codex | Agent::Copilot => request,
    }
}

#[tokio::test]
#[ignore = "spawns a real agent and consumes quota"]
async fn claude_answers_and_reports_usage() {
    if !available(Agent::Claude) {
        return;
    }
    let outcome = run(&ping(Agent::Claude)).await.expect("claude run failed");

    assert!(outcome.is_ok(), "unexpected stop: {outcome:?}");
    assert_eq!(outcome.text.trim().to_lowercase(), "pong");
    assert!(outcome.session.is_some(), "claude must report a session id");
    // Claude prices its own runs, so both tokens and cost should be present.
    assert!(outcome.usage.output_tokens.is_some(), "{:?}", outcome.usage);
    assert!(outcome.usage.cost_usd.is_some(), "{:?}", outcome.usage);
}

/// The failure this crate most has to get right: claude exits **0** for an
/// unknown model and puts the explanation where the answer goes. Before this
/// was classified, a caller checking `Result::is_ok` rendered "There's an issue
/// with the selected model" as the model's reply.
///
/// Cheap: the turn is refused before any tokens are spent.
#[tokio::test]
#[ignore = "spawns a real agent"]
async fn an_unknown_model_is_an_error_not_an_answer() {
    if !available(Agent::Claude) {
        return;
    }
    let request = ping(Agent::Claude).model("bogus-model-xyz");
    let err = run(&request)
        .await
        .expect_err("an unknown model must not come back as an answer");

    let agent_abstraction::Error::AgentError {
        status, message, ..
    } = &err
    else {
        panic!("expected AgentError, got {err:?}")
    };
    // Not asserted as exactly 404: the status is passed through from the
    // provider, and the point is that whatever it sends survives.
    assert!(
        status.is_some(),
        "the provider status should survive: {err:?}"
    );
    assert!(
        message.to_lowercase().contains("model"),
        "the agent's own wording should survive: {message}"
    );
}

/// Codex fails the same way and adds a wrinkle: it forwards the upstream error
/// body as a JSON string, so without unwrapping, the caller is handed JSON
/// instead of a sentence.
#[tokio::test]
#[ignore = "spawns a real agent"]
async fn codex_reports_an_unknown_model_as_a_sentence_not_json() {
    if !available(Agent::Codex) {
        return;
    }
    let request = ping(Agent::Codex).model("bogus-model-xyz");
    let err = run(&request)
        .await
        .expect_err("an unknown model must not come back as an answer");

    let agent_abstraction::Error::AgentError { message, .. } = &err else {
        panic!("expected AgentError, got {err:?}")
    };
    assert!(
        !message.trim_start().starts_with('{'),
        "the upstream envelope should have been unwrapped: {message}"
    );
    assert!(
        message.to_lowercase().contains("model"),
        "the agent's own wording should survive: {message}"
    );
}

/// The compiled-in catalogue is a snapshot and the CLI is the authority. Where
/// the CLI can be asked, drift should be visible rather than discovered by a
/// user picking a model that no longer exists.
///
/// Cheap: `codex debug models` spends no tokens.
#[tokio::test]
async fn the_codex_catalogue_still_matches_what_codex_reports() {
    if !available(Agent::Codex) {
        return;
    }
    let discovered = Agent::Codex
        .discover_models()
        .await
        .expect("codex should list its own models");
    let compiled = Agent::Codex.models();

    let ids = |models: &[agent_abstraction::Model]| -> Vec<String> {
        models.iter().map(|m| m.id.to_string()).collect()
    };
    assert_eq!(
        ids(&discovered),
        ids(&compiled),
        "codex's model list has moved; update `codex_models` in src/model.rs"
    );
    assert!(
        !discovered.iter().any(|m| m.id == "codex-auto-review"),
        "a model codex marks hidden must not reach a picker"
    );
    assert!(
        !discovered.iter().any(|m| m.id == "gpt-5.3-codex-spark"),
        "a visible model codex marks unsupported in the API must not reach a picker"
    );
}

/// Both of these have an interactive picker and no headless listing, so asking
/// must fail rather than quietly hand back the compiled list.
#[tokio::test]
async fn agents_without_a_headless_listing_refuse_to_guess() {
    for agent in [Agent::Claude, Agent::Copilot] {
        if !available(agent) {
            continue;
        }
        let err = agent
            .discover_models()
            .await
            .expect_err("should not invent a model list");
        assert!(
            matches!(err, agent_abstraction::Error::Unsupported { .. }),
            "{agent} should report it cannot enumerate models, got {err:?}"
        );
    }
}

/// The catalogue advertises effort levels; this proves each CLI actually takes
/// the one this crate sends, and takes it the way this crate sends it. Codex is
/// the interesting case: no flag, a config override, so a wrong key would be
/// silently ignored rather than refused.
///
/// Copilot is absent on purpose. Its only plan-permitted model is `auto`, which
/// rejects the flag outright; that is asserted separately below.
///
/// Each agent runs at the lowest level it documents, which is also the cheapest.
#[tokio::test]
#[ignore = "spawns a real agent and consumes quota"]
async fn effort_reaches_the_agents_that_take_it() {
    for agent in [Agent::Claude, Agent::Codex] {
        if !available(agent) {
            continue;
        }
        let outcome = run(&ping(agent).effort("low"))
            .await
            .unwrap_or_else(|e| panic!("{agent} rejected effort low: {e}"));
        assert!(
            outcome.is_ok(),
            "{agent} did not finish cleanly at effort low: {outcome:?}"
        );
    }
}

/// Effort support is not uniform across an agent even where `--help` documents
/// one set of levels. Copilot's `auto` exits 1 rather than ignoring the flag:
///
/// ```text
/// Error: Model "auto" does not support reasoning effort configuration
/// ```
///
/// Which is why the catalogue leaves `auto` with no levels. This asserts the
/// behaviour that decision rests on, so a future Copilot release that starts
/// accepting it shows up here.
#[tokio::test]
#[ignore = "spawns a real agent"]
async fn copilot_auto_still_refuses_an_effort() {
    if !available(Agent::Copilot) {
        return;
    }
    let err = run(&ping(Agent::Copilot).model("auto").effort("low"))
        .await
        .expect_err("auto should still refuse an effort");
    assert!(
        err.to_string().to_lowercase().contains("reasoning effort"),
        "the CLI's own complaint should survive: {err}"
    );
}

/// The levels are the provider's to define, so a bad one must surface rather
/// than be swallowed. Cheap: refused before any tokens are spent.
#[tokio::test]
#[ignore = "spawns a real agent"]
async fn a_bogus_effort_is_reported_not_ignored() {
    if !available(Agent::Codex) {
        return;
    }
    let err = run(&ping(Agent::Codex).effort("bogus-level"))
        .await
        .expect_err("an invalid effort must not pass silently");
    let message = err.to_string().to_lowercase();
    assert!(
        message.contains("effort") || message.contains("invalid"),
        "the provider's complaint should survive: {err}"
    );
}

/// The false-termination regression, which only the real CLI can prove.
///
/// An agent asked about rate limits and logging in answers in exactly the
/// vocabulary that describes being rate limited and logged out, and the run
/// used to classify itself from its own answer: the turn finished, the answer
/// was discarded, and the caller was handed a banner quoting one of the
/// answer's own sentences back as though a provider had written it.
///
/// The prompt asks for the trigger phrases deliberately. A pass is an ordinary
/// successful outcome whose text contains them.
#[tokio::test]
#[ignore = "spawns a real agent"]
async fn an_answer_about_limits_and_login_is_not_a_failed_run() {
    if !available(Agent::Claude) {
        return;
    }
    let prompt = "In three sentences of plain prose, explain the difference between \
                  a provider rate limit and an authentication failure. Use the exact \
                  phrases \"rate limit\", \"usage limit\", \"not authenticated\" and \
                  \"please run /login\" somewhere in your answer. Do not use a list.";
    let request = Request::new(Agent::Claude, prompt)
        .permission(Permission::ReadOnly)
        .timeout(Duration::from_secs(180));
    let outcome = run(&request)
        .await
        .expect("an answer about limits and login must not fail the run");
    let text = outcome.text.to_lowercase();
    assert!(
        text.contains("rate limit") || text.contains("usage limit"),
        "the model did not use the trigger wording, so this proved nothing: {}",
        outcome.text
    );
    assert!(
        !outcome.text.trim().is_empty(),
        "the answer survived classification but arrived empty"
    );
}

/// Cheap and free of tokens: `codex app-server` answers from the account, not
/// the model. Pins the shape a quota panel depends on.
#[tokio::test]
async fn codex_reports_account_usage_without_a_terminal() {
    if !available(Agent::Codex) {
        return;
    }
    assert!(Agent::Codex.reports_account_usage());
    let usage = Agent::Codex
        .account_usage()
        .await
        .expect("codex should report account usage");

    assert!(
        usage.plan.is_some(),
        "a plan name should survive: {usage:?}"
    );
    let window = usage
        .windows
        .first()
        .unwrap_or_else(|| panic!("at least one quota window: {usage:?}"));
    assert!(
        window.used_percent.is_some(),
        "a percentage is the point of asking: {window:?}"
    );
    assert!(window.window_minutes.is_some(), "{window:?}");
}

/// Claude's context tracker, end to end: it is the one agent that reports the
/// window alongside the tokens, so a share of it is knowable.
#[tokio::test]
#[ignore = "spawns a real agent and consumes quota"]
async fn claude_reports_enough_to_track_context() {
    if !available(Agent::Claude) {
        return;
    }
    let outcome = run(&ping(Agent::Claude)).await.expect("claude run failed");
    let usage = &outcome.usage;

    let context = usage.context_tokens.expect("context tokens");
    let window = usage.context_window.expect("context window");
    assert!(context > 0 && window > 0, "{usage:?}");
    assert!(
        context <= window,
        "context cannot exceed the window: {usage:?}"
    );
    let share = usage.context_used().expect("both halves present");
    assert!((0.0..=1.0).contains(&share), "share out of range: {share}");
    assert!(usage.max_output_tokens.is_some(), "{usage:?}");
}

/// The accounting difference this release exists to fix. Codex sends the whole
/// prompt as `input_tokens`; after normalizing, the parts must add back up to
/// the total it reported.
/// The `[1m]` aliases end to end: the run must report the widened window, not
/// the 200k of the Haiku helper that claude lists first in its per-model
/// usage. This is the regression test for sessions presenting as capped at
/// 200k.
#[tokio::test]
#[ignore = "spawns a real agent and consumes quota"]
async fn a_1m_alias_reports_the_widened_window() {
    if !available(Agent::Claude) {
        return;
    }
    let request = Request::new(Agent::Claude, PING)
        .model("sonnet[1m]")
        .permission(Permission::ReadOnly)
        .timeout(Duration::from_secs(180));
    let outcome = run(&request).await.expect("sonnet[1m] run failed");

    assert_eq!(
        outcome.usage.context_window,
        Some(1_000_000),
        "the widened window should be reported: {:?}",
        outcome.usage
    );
    let share = outcome.usage.context_used().expect("both halves present");
    assert!(
        share < 0.1,
        "a fresh session should be far from full: {share}"
    );
}

#[tokio::test]
#[ignore = "spawns a real agent and consumes quota"]
async fn codex_token_parts_reconcile_with_the_total_it_reported() {
    if !available(Agent::Codex) {
        return;
    }
    let outcome = run(&ping(Agent::Codex)).await.expect("codex run failed");
    let usage = &outcome.usage;

    let context = usage.context_tokens.expect("context tokens");
    let uncached = usage.input_tokens.expect("input tokens");
    let cached = usage.cache_read_tokens.unwrap_or(0);
    assert_eq!(
        uncached + cached,
        context,
        "normalized input plus cache should equal the prompt codex reported: {usage:?}"
    );
    assert!(
        usage.reasoning_tokens.is_some(),
        "codex separates reasoning tokens: {usage:?}"
    );
}

/// Copilot reports spend in AI credits, the unit that replaced premium
/// requests, on its own event rather than only at the end.
#[tokio::test]
#[ignore = "spawns a real agent and consumes quota"]
async fn copilot_reports_its_credit_spend() {
    if !available(Agent::Copilot) {
        return;
    }
    let outcome = run(&ping(Agent::Copilot))
        .await
        .expect("copilot run failed");
    assert!(
        outcome.usage.ai_credits_nano.is_some(),
        "the checkpoint event should reach the outcome: {:?}",
        outcome.usage
    );
    assert!(outcome.usage.duration_ms.is_some(), "{:?}", outcome.usage);
}

/// The whole human-in-the-loop round trip against the real CLI: a gated tool
/// call arrives as an event, the decision goes back mid-turn, and the denial is
/// honoured. The file is the proof: if the answer had not reached claude, it
/// would exist.
#[tokio::test]
#[ignore = "spawns a real agent and consumes quota"]
async fn a_denied_approval_stops_the_tool_from_running() {
    if !available(Agent::Claude) {
        return;
    }
    let dir =
        std::env::temp_dir().join(format!("agent-abstraction-approval-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let target = dir.join("must-not-exist.txt");
    let _ = std::fs::remove_file(&target);

    let request = Request::new(
        Agent::Claude,
        "Use the Bash tool to run exactly: touch must-not-exist.txt",
    )
    .model("haiku")
    .cwd(&dir)
    // Not `ReadOnly`: that removes Bash outright, so there would be nothing
    // to be asked about. Here the human is the gate instead.
    .permission(Permission::Edit)
    .approvals()
    .timeout(Duration::from_secs(180));

    let mut run = stream(&request).expect("stream should start");
    let mut asked = Vec::new();
    while let Some(event) = run.recv().await {
        if let Event::ApprovalRequest(approval) = event {
            asked.push(approval.tool.clone());
            run.respond(&approval.id, &agent_abstraction::Decision::deny())
                .await
                .expect("the decision should reach claude");
        }
    }
    let outcome = run.finish().await.expect("a denial is not a failed run");

    assert!(
        asked.iter().any(|tool| tool == "Bash"),
        "claude should have asked before running a mutating command, asked: {asked:?}"
    );
    assert!(
        !target.exists(),
        "the denial was not honoured: the file was created anyway"
    );
    assert!(
        outcome.is_ok(),
        "the turn should still complete: {outcome:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The point of `interactive`: a correction typed mid-turn actually redirects
/// the agent. Three sleeps are requested and the run is told to stop after the
/// first, so the assertion is that the later two never ran.
///
/// The timing is generous: the message only has to arrive before the third
/// command, and each is an eight second sleep.
#[tokio::test]
#[ignore = "spawns a real agent and consumes quota"]
async fn a_message_sent_mid_turn_redirects_the_agent() {
    if !available(Agent::Claude) {
        return;
    }
    let dir = std::env::temp_dir().join(format!("agent-abstraction-duplex-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    for name in ["step-b.txt", "step-c.txt"] {
        let _ = std::fs::remove_file(dir.join(name));
    }

    let request = Request::new(
        Agent::Claude,
        "Using the Bash tool, run these three commands ONE AT A TIME, in order: \
         `sleep 8`, then `touch step-b.txt`, then `touch step-c.txt`.",
    )
    .model("haiku")
    .cwd(&dir)
    .permission(Permission::Bypass)
    .interactive()
    .timeout(Duration::from_secs(180));

    let mut run = stream(&request).expect("stream should start");
    let mut sent = false;
    while let Some(event) = run.recv().await {
        // Send once the agent is demonstrably working, so the message lands
        // mid-turn rather than before the turn begins.
        if !sent && matches!(event, Event::ToolCall { .. }) {
            sent = true;
            run.send("STOP. Do not run any more commands. Reply with only: ABORTED")
                .await
                .expect("the message should reach claude");
        }
    }
    let outcome = run
        .finish()
        .await
        .expect("an interrupted turn still completes");

    assert!(
        sent,
        "the agent never called a tool, so nothing was interrupted"
    );
    assert!(
        !dir.join("step-c.txt").exists(),
        "the third command ran, so the message did not redirect the agent: {outcome:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Codex uses app-server for an interactive turn. This proves the full live
/// path rather than only its JSON shapes: a running tool call is steered, text
/// arrives as deltas, and the app-server usage record carries the context
/// window `AgencyZero` needs for its meter.
#[tokio::test]
#[ignore = "spawns a real agent and consumes quota"]
async fn codex_app_server_accepts_a_live_steer() {
    if !available(Agent::Codex) {
        return;
    }
    let dir = std::env::temp_dir().join(format!(
        "agent-abstraction-codex-duplex-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let target = dir.join("must-not-exist.txt");
    let _ = std::fs::remove_file(&target);

    let request = Request::new(
        Agent::Codex,
        "Run `sleep 5` as one shell command. Wait for it to finish. Then, as a separate \
         command, run `touch must-not-exist.txt`. Do not combine the commands.",
    )
    .cwd(&dir)
    .permission(Permission::Bypass)
    .interactive()
    .timeout(Duration::from_secs(180));

    let mut run = stream(&request).expect("app-server should start");
    assert_eq!(run.argv(), ["codex", "app-server", "--stdio"]);
    let mut sent = false;
    let mut text_events = 0;
    while let Some(event) = run.recv().await {
        if matches!(event, Event::Text(_)) {
            text_events += 1;
        }
        if !sent && matches!(event, Event::ToolCall { .. }) {
            sent = true;
            run.send("STOP. Do not run another command. Reply with only: ABORTED")
                .await
                .expect("turn/steer should accept the message");
        }
    }
    let outcome = run.finish().await.expect("steered turn should complete");
    assert!(sent, "Codex never called the first tool");
    assert!(text_events > 0, "assistant deltas were not streamed");
    assert!(
        outcome.usage.context_window.is_some(),
        "app-server did not report its context window: {:?}",
        outcome.usage
    );
    assert!(
        !target.exists(),
        "the second command ran, so the steer did not redirect Codex"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A single declared cwd is the common project shape in `AgencyZero`. It must be
/// writable without adding the same directory a second time as an extra root.
#[tokio::test]
#[ignore = "spawns a real agent and consumes quota"]
async fn codex_app_server_can_write_inside_its_cwd() {
    if !available(Agent::Codex) {
        return;
    }
    let dir = std::env::temp_dir().join(format!(
        "agent-abstraction-codex-writable-cwd-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let target = dir.join("written-inside-cwd.txt");
    let _ = std::fs::remove_file(&target);

    let request = Request::new(
        Agent::Codex,
        "Create the file written-inside-cwd.txt in the current working directory. \
         Its exact contents must be: writable",
    )
    .cwd(&dir)
    .permission(Permission::Auto)
    .interactive()
    .timeout(Duration::from_secs(180));

    let outcome = run(&request)
        .await
        .expect("Codex should write inside its declared cwd");
    assert!(outcome.is_ok(), "the turn did not complete: {outcome:?}");
    assert_eq!(
        std::fs::read_to_string(&target).expect("Codex did not create the file"),
        "writable"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Auto is the workspace-rooted posture that may also use the network without
/// asking the host. `AgencyZero` relies on this for ordinary GitHub reads and
/// pushes; an approval request here can strand the whole turn before its card
/// reaches the frontend.
#[tokio::test]
#[ignore = "spawns a real agent, uses the network, and consumes quota"]
async fn codex_auto_uses_github_without_an_approval_request() {
    if !available(Agent::Codex) {
        return;
    }
    let dir = std::env::temp_dir().join(format!(
        "agent-abstraction-codex-auto-network-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");

    let request = Request::new(
        Agent::Codex,
        "Run exactly this read-only command: git ls-remote https://github.com/pathscale/agencyzero.git HEAD. Then reply done.",
    )
    .cwd(&dir)
    .permission(Permission::Auto)
    .interactive()
    .approvals()
    .timeout(Duration::from_secs(180));

    let mut run = stream(&request).expect("app-server should start");
    let mut asked = false;
    while let Some(event) = run.recv().await {
        if matches!(event, Event::ApprovalRequest(_)) {
            asked = true;
        }
    }
    let outcome = run.finish().await.expect("the GitHub read should complete");

    assert!(
        !asked,
        "Codex asked the host to approve an Auto network read"
    );
    assert!(outcome.is_ok(), "the turn did not complete: {outcome:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A Codex sandbox escape is a server request the host can deny mid-turn.
#[tokio::test]
#[ignore = "spawns a real agent and consumes quota"]
async fn codex_app_server_routes_approvals() {
    if !available(Agent::Codex) {
        return;
    }
    let dir = std::env::temp_dir().join(format!(
        "agent-abstraction-codex-approval-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let outside = std::env::temp_dir().join(format!(
        "agent-abstraction-codex-outside-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&outside);

    let request = Request::new(
        Agent::Codex,
        format!(
            "Use a shell command to create exactly this file: {}",
            outside.display()
        ),
    )
    .cwd(&dir)
    .permission(Permission::Auto)
    .approvals()
    .timeout(Duration::from_secs(180));

    let mut run = stream(&request).expect("app-server should start");
    let mut asked = false;
    while let Some(event) = run.recv().await {
        if let Event::ApprovalRequest(approval) = event {
            asked = true;
            run.respond(&approval.id, &agent_abstraction::Decision::deny())
                .await
                .expect("the denial should reach app-server");
        }
    }
    let outcome = run
        .finish()
        .await
        .expect("a denial should not fail the turn");
    assert!(asked, "Codex did not ask for the out-of-root write");
    assert!(!outside.exists(), "the denied write still created the file");
    assert!(outcome.is_ok(), "the turn did not recover: {outcome:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The allow half of the app-server approval round trip. `AgencyZero` can
/// remember a scoped decision and answer immediately, so an accepted request
/// must resume the same turn rather than leaving it waiting forever.
#[tokio::test]
#[ignore = "spawns a real agent and consumes quota"]
async fn codex_app_server_resumes_after_an_allowed_approval() {
    if !available(Agent::Codex) {
        return;
    }
    let dir = std::env::temp_dir().join(format!(
        "agent-abstraction-codex-allow-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let outside = std::path::PathBuf::from(std::env::var_os("HOME").expect("home dir"))
        .join(format!(".codex-allowed-outside-{}", std::process::id()));
    let _ = std::fs::remove_file(&outside);

    let request = Request::new(
        Agent::Codex,
        format!(
            "Use a shell command to create exactly this file, then reply done: {}",
            outside.display()
        ),
    )
    .cwd(&dir)
    .permission(Permission::Auto)
    .approvals()
    .timeout(Duration::from_secs(180));

    let mut run = stream(&request).expect("app-server should start");
    let mut asked = false;
    while let Some(event) = run.recv().await {
        if let Event::ApprovalRequest(approval) = event {
            asked = true;
            run.respond(&approval.id, &agent_abstraction::Decision::Allow)
                .await
                .expect("the approval should reach app-server");
        }
    }
    let outcome = run
        .finish()
        .await
        .expect("an approval should resume the turn");
    assert!(asked, "Codex did not ask for the out-of-root write");
    assert!(outside.exists(), "the approved write did not run");
    assert!(outcome.is_ok(), "the turn did not complete: {outcome:?}");
    let _ = std::fs::remove_file(&outside);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A live counter has to agree with the number that replaces it when the run
/// ends, or a UI would show a total that jumps at the last moment. Drives a
/// multi-step task so several model calls report, then checks the accumulated
/// figures against the terminal record.
#[tokio::test]
#[ignore = "spawns a real agent and consumes quota"]
async fn live_usage_events_agree_with_the_final_outcome() {
    if !available(Agent::Claude) {
        return;
    }
    let request = Request::new(
        Agent::Claude,
        "Using the Bash tool, run these one at a time: `echo one`, `echo two`, \
         `echo three`. Then say done.",
    )
    .model("haiku")
    .permission(Permission::Bypass)
    .timeout(Duration::from_secs(180));

    let mut run = stream(&request).expect("stream should start");
    let mut live = agent_abstraction::Usage::default();
    let mut snapshots = 0;
    while let Some(event) = run.recv().await {
        if let Event::Usage(usage) = event {
            snapshots += 1;
            assert_eq!(
                usage.output_tokens, None,
                "a mid-turn output count is understated and must be withheld"
            );
            live.accumulate(&usage);
        }
    }
    let outcome = run.finish().await.expect("run failed");

    assert!(
        snapshots > 1,
        "a multi-step turn should report more than one model call, got {snapshots}"
    );
    assert_eq!(
        live.input_tokens, outcome.usage.input_tokens,
        "accumulated input should match the terminal record"
    );
    assert_eq!(
        live.context_tokens, outcome.usage.context_tokens,
        "the last snapshot's context should be the final context"
    );
}

/// Sending is refused where it cannot work, rather than silently doing nothing.
#[tokio::test]
async fn a_follow_up_is_refused_where_it_cannot_be_delivered() {
    assert!(matches!(
        Request::new(Agent::Copilot, PING).interactive().argv(),
        Err(agent_abstraction::Error::Unsupported { .. })
    ));
    assert!(
        Request::new(Agent::Codex, PING)
            .interactive()
            .argv()
            .is_ok()
    );
    // A run that never opened the channel has nowhere to put a message. This
    // half needs the binary, since it has to actually spawn; the argv checks
    // above do not.
    if !available(Agent::Claude) {
        return;
    }
    let plain = stream(&Request::new(Agent::Claude, PING)).expect("stream");
    assert!(
        matches!(
            plain.send("late").await,
            Err(agent_abstraction::Error::Unsupported { .. })
        ),
        "a non-interactive run should refuse a follow-up"
    );
    let _ = plain.cancel().await;
}

/// Both refusals, checked without spawning: Copilot cannot ask, and `run`
/// cannot carry the question to anyone.
#[tokio::test]
async fn approvals_are_refused_where_they_cannot_work() {
    let unsupported = Request::new(Agent::Copilot, PING)
        .permission(Permission::Edit)
        .approvals();
    assert!(matches!(
        unsupported.argv(),
        Err(agent_abstraction::Error::Unsupported { .. })
    ));
    assert!(
        Request::new(Agent::Codex, PING)
            .permission(Permission::Edit)
            .approvals()
            .argv()
            .is_ok()
    );
    let discarded = Request::new(Agent::Codex, PING)
        .permission(Permission::Edit)
        .approvals();

    // Read-only removes the tools that would be asked about, so it is refused
    // rather than silently never asking.
    assert!(
        matches!(
            Request::new(Agent::Claude, PING)
                .permission(Permission::ReadOnly)
                .approvals()
                .argv(),
            Err(agent_abstraction::Error::Unsupported { .. })
        ),
        "approvals under read-only should be refused"
    );
    assert!(
        matches!(
            run(&discarded).await,
            Err(agent_abstraction::Error::Unsupported { .. })
        ),
        "`run` discards events, so nobody could answer"
    );
}

#[tokio::test]
#[ignore = "spawns a real agent and consumes quota"]
async fn codex_answers_and_reports_usage() {
    if !available(Agent::Codex) {
        return;
    }
    let outcome = run(&ping(Agent::Codex)).await.expect("codex run failed");

    assert!(outcome.is_ok(), "unexpected stop: {outcome:?}");
    assert!(
        outcome.text.trim().to_lowercase().contains("pong"),
        "got {:?}",
        outcome.text
    );
    assert!(outcome.session.is_some(), "codex must report a thread id");
    assert!(outcome.usage.input_tokens.is_some(), "{:?}", outcome.usage);
}

#[tokio::test]
#[ignore = "spawns a real agent and consumes quota"]
async fn copilot_answers_and_reports_a_session() {
    if !available(Agent::Copilot) {
        return;
    }
    let outcome = run(&ping(Agent::Copilot))
        .await
        .expect("copilot run failed");

    assert!(outcome.is_ok(), "unexpected stop: {outcome:?}");
    assert!(
        outcome.text.trim().to_lowercase().contains("pong"),
        "got {:?}",
        outcome.text
    );
    assert!(
        outcome.session.is_some(),
        "copilot must report a session id"
    );
}

/// A read-only `codex exec` can safely inspect a non-repository scratch
/// directory. Running the rest of the suite from the repo would never catch a
/// regression here, because the repo already satisfies Codex's guard.
#[tokio::test]
#[ignore = "spawns a real agent and consumes quota"]
async fn codex_runs_outside_a_git_repository() {
    if !available(Agent::Codex) {
        return;
    }
    let scratch = std::env::temp_dir().join(format!("aa-nogit-{}", std::process::id()));
    std::fs::create_dir_all(&scratch).unwrap();
    assert!(
        !scratch.join(".git").exists(),
        "the point of this test is that it is not a repo"
    );

    let outcome = run(&ping(Agent::Codex)
        .cwd(&scratch)
        .permission(Permission::ReadOnly))
    .await
    .expect("codex refused to run outside a git repo");

    assert!(outcome.is_ok(), "unexpected stop: {outcome:?}");
    assert!(
        outcome.text.trim().to_lowercase().contains("pong"),
        "got {:?}",
        outcome.text
    );

    std::fs::remove_dir_all(&scratch).ok();
}

/// Claude and Copilot let the caller *assign* the session id up front. That is
/// only worth relying on if the agent actually honours the id we hand it, so
/// this asserts the round trip rather than trusting `--help`: the id we chose
/// must come back unchanged, and must then be resumable.
#[tokio::test]
#[ignore = "spawns a real agent and consumes quota"]
async fn a_caller_assigned_session_id_is_honoured_and_resumable() {
    for agent in [Agent::Claude, Agent::Copilot] {
        if !available(agent) {
            continue;
        }
        // Both CLIs require a valid UUID.
        let chosen = uuid::Uuid::new_v4().to_string();

        let first = run(&ping(agent).session_id(&chosen))
            .await
            .unwrap_or_else(|e| panic!("{agent} rejected an assigned id: {e}"));
        assert_eq!(
            first.session.as_deref(),
            Some(chosen.as_str()),
            "{agent} did not honour the id it was given"
        );

        // The id is only useful if it also resumes the same conversation.
        let second = run(&ping(agent).resume(&chosen))
            .await
            .unwrap_or_else(|e| panic!("{agent} could not resume the assigned id: {e}"));
        assert_eq!(
            second.session.as_deref(),
            Some(chosen.as_str()),
            "{agent} moved to a different session on resume"
        );
    }
}

/// Codex cannot be told an id: `codex exec` has no `--session-id`, so the only
/// way to learn its `thread_id` is to read it back. Asking for an assigned one
/// must fail loudly rather than silently starting an unrelated conversation.
#[tokio::test]
async fn codex_refuses_an_assigned_session_id() {
    let err = Request::new(Agent::Codex, "hi")
        .session_id("11111111-2222-3333-4444-555555555555")
        .argv()
        .unwrap_err();
    assert!(
        matches!(err, agent_abstraction::Error::Unsupported { .. }),
        "got {err:?}"
    );
}

/// Codex reports its `thread_id` in `thread.started`, which is the very first
/// record of the stream and arrives *before* the model replies. A host can
/// therefore persist the binding as soon as the stream opens rather than
/// waiting for the turn to finish.
#[tokio::test]
#[ignore = "spawns a real agent and consumes quota"]
async fn codex_reveals_its_thread_id_before_it_answers() {
    if !available(Agent::Codex) {
        return;
    }
    let mut running = stream(&ping(Agent::Codex).format(Format::Stream)).expect("spawn failed");

    let mut first_event = None;
    let mut text_seen_before_start = false;
    while let Some(event) = running.recv().await {
        match (&first_event, &event) {
            (None, Event::Started { session, .. }) => {
                assert!(!session.is_empty());
                first_event = Some(session.clone());
            }
            (None, Event::Text(_)) => text_seen_before_start = true,
            _ => {}
        }
    }
    let outcome = running.finish().await.expect("run failed");

    assert!(
        first_event.is_some(),
        "codex never announced a thread id on the stream"
    );
    assert!(
        !text_seen_before_start,
        "the id must arrive before any answer text, so a binding can be stored early"
    );
    assert_eq!(outcome.session, first_event);
}

/// `EnvPolicy::Minimal` only earns its place if a run under it still works.
/// An isolation setting that silently breaks authentication is worse than none,
/// because the failure surfaces as "not logged in" rather than as a config
/// mistake. This is the test that keeps the per-agent list honest.
#[tokio::test]
#[ignore = "spawns a real agent and consumes quota"]
async fn every_agent_still_works_under_a_minimal_environment() {
    for agent in Agent::ALL {
        if !available(agent) {
            continue;
        }
        let outcome = run(&ping(agent).env_policy(EnvPolicy::Minimal))
            .await
            .unwrap_or_else(|e| {
                panic!(
                    "{agent} could not authenticate under EnvPolicy::Minimal, \
                                        so its essential_env list is incomplete: {e}"
                )
            });
        assert!(
            outcome.text.trim().to_lowercase().contains("pong"),
            "{agent} answered {:?}",
            outcome.text
        );
    }
}

/// Codex cannot be *told* a session id, so continuity depends entirely on
/// reading its `thread_id` back off the stream and storing it ourselves.
///
/// This is the test that proves that path works end to end, with no reading of
/// `$CODEX_HOME/sessions/`: turn one states a fact, the id is captured and
/// persisted, and a second run in a separate process resumes from the store and
/// recalls it. If Codex ever stopped emitting `thread.started`, this fails.
#[tokio::test]
#[ignore = "spawns a real agent and consumes quota"]
async fn codex_resumes_from_a_captured_thread_id_without_scraping() {
    if !available(Agent::Codex) {
        return;
    }
    let dir = std::env::temp_dir().join(format!("aa-codex-resume-{}", std::process::id()));
    let store = SessionStore::open(&dir);
    let project = std::env::current_dir().unwrap();
    let name = "codex-memory";

    let first = Request::new(Agent::Codex, "Remember the number 5619. Reply OK.")
        .permission(Permission::ReadOnly)
        .timeout(Duration::from_secs(180))
        .session(&store, &project, name, false)
        .expect("planning the first turn failed");
    let first = run(&first).await.expect("first turn failed");
    let thread = first
        .session
        .clone()
        .expect("codex must report a thread id on the stream");

    // The binding must be on disk, since that is the only place the id exists
    // for us: nothing reads Codex's own session directory.
    let stored = store
        .get(&project, name)
        .expect("store read failed")
        .expect("no binding was persisted");
    assert_eq!(stored.token, thread);
    assert_eq!(stored.agent, Agent::Codex);

    let second = Request::new(Agent::Codex, "What number did I ask you to remember?")
        .permission(Permission::ReadOnly)
        .timeout(Duration::from_secs(180))
        .session(&store, &project, name, false)
        .expect("planning the second turn failed");
    assert_eq!(
        second.session_phase(),
        Some(agent_abstraction::Phase::Continue),
        "the second turn must continue the stored thread"
    );
    let second = run(&second).await.expect("second turn failed");

    assert!(
        second.text.contains("5619"),
        "codex lost its context on resume: {:?}",
        second.text
    );

    std::fs::remove_dir_all(&dir).ok();
}

/// The streaming path must deliver events *before* the run settles, and the
/// terminal answer must still be authoritative afterwards.
#[tokio::test]
#[ignore = "spawns a real agent and consumes quota"]
async fn streaming_delivers_events_then_an_outcome() {
    if !available(Agent::Claude) {
        return;
    }
    let request = ping(Agent::Claude).format(Format::Stream);
    let mut running = stream(&request).expect("spawn failed");

    let mut started = false;
    let mut text = String::new();
    while let Some(event) = running.recv().await {
        match event {
            Event::Started { session, .. } => {
                assert!(!session.is_empty());
                started = true;
            }
            Event::Text(chunk) => text.push_str(&chunk),
            _ => {}
        }
    }
    let outcome = running.finish().await.expect("run failed");

    assert!(started, "the stream must announce the session");
    assert!(text.to_lowercase().contains("pong"), "streamed {text:?}");
    assert_eq!(outcome.text.trim().to_lowercase(), "pong");
}

/// The point of the whole session layer: a second turn on the same name must
/// see what the first turn was told, without the caller handling any id.
#[tokio::test]
#[ignore = "spawns a real agent and consumes quota"]
async fn a_named_session_carries_context_across_turns() {
    if !available(Agent::Claude) {
        return;
    }
    let dir = std::env::temp_dir().join(format!("aa-live-{}", std::process::id()));
    let store = SessionStore::open(&dir);
    let project = std::env::current_dir().unwrap();
    let name = "live-memory";

    let first = Request::new(Agent::Claude, "Remember the number 4271. Reply OK.")
        .model("haiku")
        .permission(Permission::ReadOnly)
        .timeout(Duration::from_secs(180))
        .session(&store, &project, name, false)
        .expect("planning the first turn failed");
    assert_eq!(
        first.session_phase(),
        Some(agent_abstraction::Phase::Create)
    );
    let first = run(&first).await.expect("first turn failed");
    let session = first.session.clone().expect("no session id captured");

    let second = Request::new(Agent::Claude, "What number did I ask you to remember?")
        .model("haiku")
        .permission(Permission::ReadOnly)
        .timeout(Duration::from_secs(180))
        .session(&store, &project, name, false)
        .expect("planning the second turn failed");
    assert_eq!(
        second.session_phase(),
        Some(agent_abstraction::Phase::Continue),
        "the second turn must continue, not create"
    );
    let second = run(&second).await.expect("second turn failed");

    assert!(
        second.text.contains("4271"),
        "the resumed turn lost its context: {:?}",
        second.text
    );
    assert_eq!(
        second.session.as_deref(),
        Some(session.as_str()),
        "a linear resume must stay on the same session"
    );

    std::fs::remove_dir_all(&dir).ok();
}

/// Forking must branch: the new turn sees the parent's context but lands on a
/// different session id, leaving the original resumable.
#[tokio::test]
#[ignore = "spawns a real agent and consumes quota"]
async fn forking_branches_to_a_new_session() {
    if !available(Agent::Claude) {
        return;
    }
    let dir = std::env::temp_dir().join(format!("aa-fork-{}", std::process::id()));
    let store = SessionStore::open(&dir);
    let project = std::env::current_dir().unwrap();
    let name = "live-fork";

    let first = Request::new(Agent::Claude, "Remember the number 8813. Reply OK.")
        .model("haiku")
        .timeout(Duration::from_secs(180))
        .session(&store, &project, name, false)
        .unwrap();
    let parent = run(&first).await.expect("first turn failed");
    let parent_id = parent.session.expect("no session id");

    let forked = Request::new(Agent::Claude, "What number did I ask you to remember?")
        .model("haiku")
        .timeout(Duration::from_secs(180))
        .session(&store, &project, name, true)
        .unwrap();
    assert_eq!(forked.session_phase(), Some(agent_abstraction::Phase::Fork));
    let forked = run(&forked).await.expect("forked turn failed");

    assert!(
        forked.text.contains("8813"),
        "the fork lost the parent's context: {:?}",
        forked.text
    );
    assert_ne!(
        forked.session.as_deref(),
        Some(parent_id.as_str()),
        "a fork must land on a new session id, not append to the parent"
    );

    std::fs::remove_dir_all(&dir).ok();
}

/// A missing binary must be an actionable error, not a spawn failure.
#[tokio::test]
async fn a_missing_agent_reports_how_to_install_it() {
    let request = Request::new(Agent::Codex, "hi").bin("agent-abstraction-no-such-binary");
    let err = run(&request).await.unwrap_err();
    // Assert on the structured field rather than the rendered message: the
    // wording is free to change, the contract that an install hint is carried
    // at all is not.
    let agent_abstraction::Error::NotInstalled { agent, hint, .. } = &err else {
        panic!("expected NotInstalled, got {err:?}")
    };
    assert_eq!(*agent, Agent::Codex);
    assert!(
        !hint.is_empty(),
        "a missing agent must say how to install it"
    );
}

/// Probing costs no quota, just `--version`, so unlike the rest of this file it
/// runs by default. It is also the test that catches flag drift *before* a run
/// fails on it: if an agent updates underneath us, this goes red and names the
/// version rather than leaving someone to decode an unexpected-argument error.
#[tokio::test]
async fn installed_agents_match_the_versions_the_flags_were_verified_against() {
    let mut checked = 0;
    for agent in Agent::ALL {
        if !available(agent) {
            continue;
        }
        let probe = Probe::run(agent).await.expect("probe failed");
        checked += 1;

        assert!(
            probe.version.is_some(),
            "{agent} reported {:?}, which carries no readable version",
            probe.reported
        );
        assert_eq!(
            probe.status,
            VersionStatus::Verified,
            "{}",
            probe.advisory().unwrap_or_default()
        );
    }
    eprintln!("probed {checked} installed agents");
}

/// Checking login costs no quota, so like the version probe this runs by
/// default. It is the test that keeps the status parsing honest: both CLIs
/// answer in their own shape, and a parser that silently stopped recognizing
/// `Logged in using ChatGPT` would report a working setup as unknown.
#[tokio::test]
async fn installed_agents_report_their_login_state() {
    for agent in Agent::ALL {
        if !available(agent) {
            continue;
        }
        let status = AuthStatus::check(agent).await.expect("check failed");

        if agent.auth_status_argv().is_some() {
            // Claude and Codex answer, so the result must be a real yes or no.
            // Unknown here means the parsing no longer matches the CLI.
            assert_ne!(
                status.state,
                AuthState::Unknown,
                "{agent} answered {:?}, which this crate no longer recognizes",
                status.detail
            );
            assert!(
                status.is_logged_in(),
                "{agent} is not logged in: {}",
                status.summary()
            );
        } else {
            // Copilot cannot be asked, and must say so rather than claiming a
            // logout that would send someone to fix a working setup.
            assert_eq!(status.state, AuthState::Unknown);
            assert!(!status.needs_login());
        }
        eprintln!("{agent}: {}", status.summary());
    }
}

/// Structured output is the capability a consumer needs to get findings back as
/// data rather than prose to re-parse. The two CLIs deliver it differently,
/// Claude inline and Codex through a file this crate writes, so this proves the
/// unified interface against both rather than against the flag mapping alone.
#[tokio::test]
#[ignore = "spawns a real agent and consumes quota"]
async fn a_schema_constrains_the_answer_to_data() {
    const SCHEMA: &str = r#"{
        "type": "object",
        "properties": {"name": {"type": "string"}, "age": {"type": "integer"}},
        "required": ["name", "age"],
        "additionalProperties": false
    }"#;

    for agent in [Agent::Claude, Agent::Codex] {
        if !available(agent) {
            continue;
        }
        let request = Request::new(agent, "Alice is 30 years old.")
            .permission(Permission::ReadOnly)
            .timeout(Duration::from_secs(180))
            .schema(SCHEMA);
        let request = match agent {
            Agent::Claude => request.model("haiku"),
            _ => request,
        };

        let outcome = run(&request)
            .await
            .unwrap_or_else(|e| panic!("{agent} schema run failed: {e}"));

        let value = outcome
            .structured
            .unwrap_or_else(|| panic!("{agent} returned no structured answer: {:?}", outcome.text));
        assert_eq!(value["name"], "Alice", "{agent}: {value}");
        assert_eq!(value["age"], 30, "{agent}: {value}");
    }
}

/// Copilot has no schema support, so asking must fail before spawning rather
/// than returning prose that a caller would try to parse as data.
#[tokio::test]
async fn copilot_refuses_a_schema_before_spawning() {
    let err = Request::new(Agent::Copilot, "hi")
        .schema(r#"{"type":"object"}"#)
        .argv()
        .unwrap_err();
    assert!(
        matches!(err, agent_abstraction::Error::Unsupported { .. }),
        "got {err:?}"
    );
}

/// The schema file Codex reads must not outlive the run that needed it.
#[tokio::test]
#[ignore = "spawns a real agent and consumes quota"]
async fn a_codex_schema_file_is_cleaned_up() {
    if !available(Agent::Codex) {
        return;
    }
    let before = schema_files_in_temp();
    let outcome = run(&Request::new(Agent::Codex, "Alice is 30 years old.")
        .permission(Permission::ReadOnly)
        .timeout(Duration::from_secs(180))
        // `additionalProperties: false` is mandatory for Codex: without it the
        // provider rejects the schema with a 400 before the model runs.
        .schema(
            r#"{"type":"object","properties":{"name":{"type":"string"}},
                "required":["name"],"additionalProperties":false}"#,
        ))
    .await
    .expect("run failed");

    assert!(outcome.structured.is_some());
    assert_eq!(
        schema_files_in_temp(),
        before,
        "a schema file was left behind in the temp directory"
    );
}

/// Count this crate's schema files currently in the temp directory.
fn schema_files_in_temp() -> usize {
    std::fs::read_dir(std::env::temp_dir()).map_or(0, |entries| {
        entries
            .flatten()
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with("agent-abstraction-schema-")
            })
            .count()
    })
}

/// The point of the whole streaming path: text has to arrive in pieces *while*
/// the agent is still working, not in one lump at the end. A run under the old
/// default reported nothing until it finished, which for a long turn is
/// indistinguishable from a hang.
#[tokio::test]
#[ignore = "spawns a real agent and consumes quota"]
async fn text_arrives_in_pieces_before_the_run_finishes() {
    if !available(Agent::Claude) {
        return;
    }
    // Long enough to produce several chunks rather than one short answer.
    let request = Request::new(
        Agent::Claude,
        "Count from 1 to 10, one number per line, with a short comment on each.",
    )
    .model("haiku")
    .permission(Permission::ReadOnly)
    .timeout(Duration::from_secs(180));

    // No .format(): this asserts the *default* streams, which is the fix.
    let mut running = stream(&request).expect("spawn failed");

    let mut chunks: Vec<String> = Vec::new();
    while let Some(event) = running.recv().await {
        if let Event::Text(text) = event {
            chunks.push(text);
        }
    }
    let outcome = running.finish().await.expect("run failed");

    assert!(
        chunks.len() > 1,
        "text arrived as {} chunk(s), so nothing was actually streamed: {chunks:?}",
        chunks.len()
    );
    // Every chunk must be a real piece, not the whole answer repeated.
    let joined: String = chunks.concat();
    assert!(
        joined.len() <= outcome.text.len() + 64,
        "chunks total {} bytes against a {} byte answer, which means the \
         finished message was emitted on top of the deltas",
        joined.len(),
        outcome.text.len()
    );
    assert!(outcome.text.contains('1'), "answer: {:?}", outcome.text);
    eprintln!(
        "streamed {} chunks for a {} byte answer",
        chunks.len(),
        outcome.text.len()
    );
}

/// `/compact` is a command the CLI runs, not text the model reads.
///
/// The distinction is the whole feature and it is invisible from the outside:
/// an agent that treated the slash as prose would answer a question *about*
/// compaction and look, to a caller checking only `is_ok`, exactly like a
/// compaction that worked. So the assertion is on the lifecycle records, which
/// only a real compaction produces.
///
/// Two turns first, because a conversation shorter than that is refused. That
/// refusal is itself the other half of the contract: it arrives as a completed
/// run carrying the reason, never as an error.
#[tokio::test]
#[ignore = "spawns a real agent and consumes quota"]
async fn claude_runs_compact_as_a_command_and_reports_its_phases() {
    if !available(Agent::Claude) {
        return;
    }
    let dir = std::env::temp_dir().join("agent-abstraction-compact-live");
    std::fs::create_dir_all(&dir).expect("scratch dir");

    // A session with enough behind it to be worth summarising.
    let mut session = None;
    for prompt in [
        "Remember the number 41. Reply with just: ok",
        "Remember the colour teal. Reply with just: ok",
    ] {
        let mut request = Request::new(Agent::Claude, prompt)
            .model("haiku")
            .cwd(&dir)
            .timeout(Duration::from_secs(180));
        if let Some(id) = &session {
            request = request.resume(id);
        }
        let outcome = run(&request).await.expect("seeding turn failed");
        session = outcome.session.clone();
    }
    let session = session.expect("claude must report a session id");

    let request = Request::command(
        Agent::Claude,
        &agent_abstraction::Command::Compact { instructions: None },
    )
    .model("haiku")
    .cwd(&dir)
    .resume(&session)
    .timeout(Duration::from_secs(300));

    let mut stream = stream(&request).expect("compact run failed to start");
    let mut phases = Vec::new();
    let mut catalogue = None;
    while let Some(event) = stream.recv().await {
        match event {
            Event::Compaction(phase) => phases.push(phase),
            Event::Commands(commands) => catalogue = Some(commands),
            _ => {}
        }
    }
    let outcome = stream.finish().await.expect("compact run failed");

    assert!(outcome.is_ok(), "unexpected stop: {outcome:?}");
    assert!(
        phases
            .iter()
            .any(|phase| matches!(phase, agent_abstraction::Compaction::Started)),
        "no compaction began, so the slash was read as prose: {phases:?}"
    );
    let finished = phases
        .iter()
        .find_map(|phase| match phase {
            agent_abstraction::Compaction::Finished { ok, error } => Some((*ok, error.clone())),
            _ => None,
        })
        .expect("a compaction that starts must settle");
    assert!(finished.0, "compaction refused: {:?}", finished.1);

    // The catalogue rides the same run, and is the agent's own rather than ours.
    let catalogue = catalogue.expect("claude publishes its commands at init");
    assert!(
        catalogue.has("compact"),
        "an agent that just compacted must list the command: {catalogue:?}"
    );
    assert!(
        !catalogue.utilities().is_empty(),
        "utilities are the commands that are not skills: {catalogue:?}"
    );
    eprintln!(
        "compacted; {} commands, {} of them skills",
        catalogue.all.len(),
        catalogue.skills.len()
    );
}

/// The other agents have no command vocabulary, and the refusal is raised
/// before anything spawns, so this costs nothing.
#[tokio::test]
#[ignore = "spawns a real agent and consumes quota"]
async fn only_claude_takes_a_slash_command() {
    for agent in [Agent::Codex, Agent::Copilot] {
        let request = Request::command(
            agent,
            &agent_abstraction::Command::Compact { instructions: None },
        );
        let error = run(&request).await.expect_err("must refuse");
        assert!(
            matches!(error, agent_abstraction::Error::Unsupported { .. }),
            "{agent} refused with the wrong error: {error:?}"
        );
    }
}
