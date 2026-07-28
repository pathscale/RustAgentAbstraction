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

use agent_abstraction::{Agent, Event, Format, Permission, Request, SessionStore, run, stream};

/// A prompt with exactly one correct answer, so the assertion is about the
/// plumbing rather than the model's judgement.
const PING: &str = "Reply with the single word: pong. No punctuation, no explanation.";

/// Whether the agent's binary is on PATH; tests no-op without it.
fn available(agent: Agent) -> bool {
    let found = which::which(agent.bin()).is_ok();
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

/// `codex exec` aborts outside a git repository unless the check is waived.
/// This crate waives it on every invocation, so a run from a scratch directory
/// must still work. Running the rest of the suite from the repo would never
/// catch a regression here, because the repo *is* a git checkout.
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

    let outcome = run(&ping(Agent::Codex).cwd(&scratch))
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
    assert!(
        matches!(err, agent_abstraction::Error::NotInstalled { .. }),
        "got {err:?}"
    );
    assert!(err.to_string().contains("npm install"), "{err}");
}
