//! Process lifecycle: cancelling a run must actually stop the work.
//!
//! These use a shell script as a stand-in agent rather than a real CLI, so they
//! are deterministic, spend no quota, and run in CI. The script ignores the argv
//! entirely, which is fine because what is under test is the process handling,
//! not the flag mapping.
//!
//! Unix only: containing a process tree on Windows needs a Job Object, which
//! this crate does not set up yet.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::time::Duration;

use agent_abstraction::{Agent, EnvPolicy, Request, stream};

/// A scratch directory unique to one test.
fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("aa-proc-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Write an executable script that spawns a **grandchild** and then waits.
///
/// The grandchild is the point: killing only the direct child would leave it
/// running, which is precisely the leak being tested for.
fn fake_agent(dir: &Path) -> PathBuf {
    let script = dir.join("agent.sh");
    let pidfile = dir.join("grandchild.pid");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\n\
             # A long-running helper, as an agent's shell tool would spawn.\n\
             sh -c 'echo $$ > {pid}; sleep 120' &\n\
             # Emit something so the reader has work to do, then outlive it.\n\
             echo '{{\"type\":\"system\",\"session_id\":\"s\"}}'\n\
             sleep 120\n",
            pid = pidfile.display()
        ),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    script
}

/// Whether a pid is still alive, via a null signal.
fn alive(pid: i32) -> bool {
    // `kill -0` reports liveness without actually signalling.
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Wait for the grandchild to record its pid, then return it.
async fn grandchild_pid(dir: &Path) -> i32 {
    let pidfile = dir.join("grandchild.pid");
    for _ in 0..100 {
        if let Ok(text) = std::fs::read_to_string(&pidfile) {
            if let Ok(pid) = text.trim().parse::<i32>() {
                return pid;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("the fake agent never spawned its grandchild");
}

/// Give the OS a moment to reap after a kill.
async fn settle() {
    tokio::time::sleep(Duration::from_millis(300)).await;
}

/// The default that matters for a GUI: closing a window must stop the agent,
/// not leave it running invisibly and spending quota.
#[tokio::test]
async fn dropping_a_run_kills_the_agent_and_its_children() {
    let dir = scratch("drop");
    let script = fake_agent(&dir);

    let running = stream(&Request::new(Agent::Claude, "hi").bin(script.to_str().unwrap()))
        .expect("spawn failed");
    let grandchild = grandchild_pid(&dir).await;
    assert!(alive(grandchild), "the grandchild should be running");

    drop(running);
    settle().await;

    assert!(
        !alive(grandchild),
        "dropping the run left a grandchild ({grandchild}) alive; \
         killing only the CLI orphans whatever it spawned"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// `cancel` is the deterministic form: when it returns, the tree is gone.
#[tokio::test]
async fn cancel_stops_the_whole_tree_before_returning() {
    let dir = scratch("cancel");
    let script = fake_agent(&dir);

    let running = stream(&Request::new(Agent::Claude, "hi").bin(script.to_str().unwrap()))
        .expect("spawn failed");
    let grandchild = grandchild_pid(&dir).await;

    let err = running.cancel().await.unwrap_err();
    assert!(err.is_cancelled(), "cancel should report itself: {err:?}");

    // No settle(): cooperative cancellation must have reaped the tree before
    // returning, so the check is immediate rather than after a grace period.
    assert!(
        !alive(grandchild),
        "cancel returned while a grandchild was still alive, so it is not \
         awaiting its own cleanup"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// A timeout must contain the tree too, not just the process it timed out.
#[tokio::test]
async fn a_timed_out_run_kills_its_children() {
    let dir = scratch("timeout");
    let script = fake_agent(&dir);

    let request = Request::new(Agent::Claude, "hi")
        .bin(script.to_str().unwrap())
        .timeout(Duration::from_secs(3));
    let running = stream(&request).expect("spawn failed");
    let grandchild = grandchild_pid(&dir).await;

    let err = running.finish().await.unwrap_err();
    assert!(
        matches!(err, agent_abstraction::Error::Timeout { .. }),
        "got {err:?}"
    );
    settle().await;

    assert!(!alive(grandchild), "the timeout left a grandchild alive");
    std::fs::remove_dir_all(&dir).ok();
}

/// The opt-out still works: an explicitly detached run survives its handle.
#[tokio::test]
async fn detach_lets_a_run_outlive_its_handle() {
    let dir = scratch("detach");
    let script = fake_agent(&dir);

    let running = stream(&Request::new(Agent::Claude, "hi").bin(script.to_str().unwrap()))
        .expect("spawn failed");
    let grandchild = grandchild_pid(&dir).await;

    running.detach();
    settle().await;

    assert!(
        alive(grandchild),
        "detach must not kill the run; that is the whole point of it"
    );
    // Do not leave it behind for the rest of the suite.
    let _ = std::process::Command::new("kill")
        .args(["-9", &grandchild.to_string()])
        .status();
    std::fs::remove_dir_all(&dir).ok();
}

/// Write a script that dumps its own environment, as a stand-in for an agent
/// (or any command an agent runs) observing what it inherited.
fn env_dumping_agent(dir: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt as _;

    let script = dir.join("dump-env.sh");
    std::fs::write(&script, "#!/bin/sh\nenv\n").unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
    script
}

/// Collect everything the fake agent printed.
async fn captured_env(request: &Request) -> String {
    let mut running = stream(request).expect("spawn failed");
    let mut seen = String::new();
    while let Some(event) = running.recv().await {
        if let agent_abstraction::Event::Text(line) = event {
            seen.push_str(&line);
            seen.push('\n');
        }
    }
    let _ = running.finish().await;
    seen
}

/// `EnvPolicy::Minimal` has to actually withhold the host's environment.
///
/// Cargo injects a pile of `CARGO_*` variables into this test process, which
/// stand in for the unrelated secrets a Tauri or server host would be holding.
/// Under `Inherit` they reach the agent; under `Minimal` they must not.
#[tokio::test]
async fn a_minimal_environment_withholds_the_hosts_variables() {
    let dir = scratch("env");
    let script = env_dumping_agent(&dir);
    let base = || {
        Request::new(Agent::Claude, "hi")
            .bin(script.to_str().unwrap())
            .format(agent_abstraction::Format::Text)
    };

    let inherited = captured_env(&base()).await;
    assert!(
        inherited.contains("CARGO"),
        "the control case is broken: Inherit should pass the host environment"
    );

    let minimal = captured_env(&base().env_policy(EnvPolicy::Minimal)).await;
    assert!(
        !minimal.contains("CARGO"),
        "host variables leaked under EnvPolicy::Minimal:\n{minimal}"
    );
    // ...while still passing what the agent needs to work at all.
    assert!(minimal.contains("PATH="), "PATH must survive:\n{minimal}");
    assert!(minimal.contains("HOME="), "HOME must survive:\n{minimal}");

    // An explicit variable always wins over the policy.
    let explicit = captured_env(
        &base()
            .env_policy(EnvPolicy::Minimal)
            .env("AA_EXPLICIT", "kept"),
    )
    .await;
    assert!(explicit.contains("AA_EXPLICIT=kept"), "{explicit}");

    std::fs::remove_dir_all(&dir).ok();
}

/// A line with no newline must not be buffered without limit. `lines()` would
/// accumulate the whole thing, so a stream that never emits `\n` could exhaust
/// memory long before any total cap applied.
#[tokio::test]
async fn an_endless_line_does_not_exhaust_memory() {
    use std::os::unix::fs::PermissionsExt as _;

    let dir = scratch("longline");
    let script = dir.join("flood.sh");
    // 64 MiB on a single line, no trailing newline until the very end.
    std::fs::write(
        &script,
        "#!/bin/sh\nawk 'BEGIN{for(i=0;i<1000000;i++)printf \"%s\", \"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\"; print \"\"}'\n",
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();

    let request = Request::new(Agent::Claude, "hi")
        .bin(script.to_str().unwrap())
        .format(agent_abstraction::Format::Text)
        .timeout(Duration::from_secs(60));
    let outcome = stream(&request)
        .expect("spawn failed")
        .finish()
        .await
        .expect("run failed");

    // Whatever is kept must respect the cap rather than the 64 MiB produced.
    assert!(
        outcome.text.len() <= agent_abstraction::MAX_CAPTURE,
        "kept {} bytes, over the cap",
        outcome.text.len()
    );
    std::fs::remove_dir_all(&dir).ok();
}
