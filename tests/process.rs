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

use agent_abstraction::{Agent, Request, stream};

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

    running.cancel().await;
    settle().await;

    assert!(!alive(grandchild), "cancel left a grandchild alive");
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
