//! Process-group teardown: the crate's only `unsafe`.
//!
//! It lives in its own module so the answer to "where does this crate use
//! `unsafe`, and why" is one small file rather than a block buried in the
//! runner.
//!
//! # Why any unsafe at all
//!
//! Killing an agent means killing what the agent *started*. A code review runs
//! `git`, a test runner, a language server; those are children of the CLI, not
//! of us, and `Child::kill` reaches only the CLI itself. The portable answer is
//! to put each run in its own process group ([`std::process::Command::process_group`],
//! which is safe) and signal the group.
//!
//! Signalling a group is where safety runs out: `std` has no API for it, so the
//! options are `libc::kill`, which is `unsafe` because it is a raw FFI call, or
//! a dependency such as `nix` for a safe wrapper. A whole crate for one call is
//! the larger cost, so the crate takes the `unsafe` and confines it here.
//!
//! `Cargo.toml` sets `unsafe_code = "deny"` rather than `forbid` precisely so
//! this one audited use can be excepted; nothing else in the crate may add one
//! without also changing that lint.
//!
//! # Windows
//!
//! Not implemented. Containing a process tree on Windows needs a Job Object
//! (`CreateJobObject` + `AssignProcessToJobObject` with
//! `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`), which this crate does not set up, so
//! only the direct child is killed and grandchildren survive cancellation. This
//! is a real gap for a Windows host, tracked in the README's cancellation
//! section, and the tests that prove the Unix behaviour are `#![cfg(unix)]`.

/// Signal an entire process group so commands the agent spawned die with it.
///
/// Best effort by nature: the group may already have exited, which is not a
/// failure. Call this **before** reaping the child, because reaping clears the
/// pid this needs to address the group.
#[cfg(unix)]
pub(crate) fn kill_process_group(child: &tokio::process::Child) {
    let Some(pid) = child.id() else {
        // Already reaped, so there is no pid left to address. Signalling now
        // would risk hitting a pid the OS has since recycled.
        return;
    };
    kill_group_by_pid(pid);
}

/// Signal a group by its leader pid, for a caller holding the pid rather than
/// the [`tokio::process::Child`].
///
/// [`crate::Run`]'s `Drop` needs this. `Drop` cannot await, so its only other
/// option is to abort the driver task and rely on the runtime polling that task
/// so its guard runs. That makes teardown depend on scheduling, and it does not
/// reliably happen: a dropped `Run` left grandchildren alive and *sleeping* on
/// Linux, while the identical teardown worked from `cancel` and from a timeout,
/// both of which call this directly. Killing here is synchronous and depends on
/// nothing being polled.
#[cfg(unix)]
pub(crate) fn kill_group_by_pid(pid: u32) {
    let Ok(pid) = i32::try_from(pid) else {
        // Unreachable in practice: a pid always fits in an i32.
        return;
    };

    // SAFETY: `libc::kill` is an FFI call with no safe wrapper in `std`. The
    // negated pid addresses the process group this child leads, established by
    // `process_group(0)` at spawn. Signalling a group that has already exited
    // returns `ESRCH`, which is ignored here; it is not undefined behaviour.
    // No pointers are passed and no memory is shared, so the call cannot
    // violate any invariant this crate relies on.
    #[expect(
        unsafe_code,
        reason = "process-group signalling has no safe equivalent in std; \
                  taking a dependency for one call costs more than it saves"
    )]
    unsafe {
        libc::kill(-pid, libc::SIGKILL);
    }
}

/// No-op: see the module docs. Only the direct child is killed on Windows.
#[cfg(not(unix))]
pub(crate) fn kill_process_group(_child: &tokio::process::Child) {}

/// No-op counterpart for non-unix.
#[cfg(not(unix))]
pub(crate) fn kill_group_by_pid(_pid: u32) {}
