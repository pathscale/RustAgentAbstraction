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
    Agent, EnvPolicy, Event, Format, Permission, Probe, Request, SessionStore, VersionStatus, run,
    stream,
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
