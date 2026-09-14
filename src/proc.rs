//! Liveness checks.
//!
//! A session registry entry outlives the process it describes, so discovery
//! has to ask the OS whether the pid is still there.

/// True if `pid` is a live process we're allowed to see.
///
/// `kill(pid, 0)` performs permission and existence checks without sending a
/// signal. This does not guard against pid reuse; callers that care should
/// also compare the recorded process start time.
pub fn is_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    // SAFETY: signal 0 sends nothing; it only probes existence/permission.
    unsafe { libc_kill(pid as i32, 0) == 0 }
}

extern "C" {
    #[link_name = "kill"]
    fn libc_kill(pid: i32, sig: i32) -> i32;
}
