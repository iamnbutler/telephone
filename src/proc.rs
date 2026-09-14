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
    if pid == 0 || pid > i32::MAX as u32 {
        return false;
    }
    // SAFETY: signal 0 sends nothing; it only probes existence/permission.
    let result = unsafe { libc_kill(pid as i32, 0) };
    // Sandboxes can deny the probe even when the process exists. Only a
    // missing process means dead; permission denied must remain discoverable.
    result == 0
        || std::io::Error::last_os_error().kind() == std::io::ErrorKind::PermissionDenied
}

/// The direct parent is useful when a runtime starts an MCP server without
/// exporting its session identity.
pub fn parent_pid() -> u32 {
    // SAFETY: getppid takes no arguments and has no memory safety requirements.
    unsafe { libc_getppid() as u32 }
}

extern "C" {
    #[link_name = "kill"]
    fn libc_kill(pid: i32, sig: i32) -> i32;
    #[link_name = "getppid"]
    fn libc_getppid() -> i32;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_processes_are_discoverable() {
        assert!(is_alive(std::process::id()));
        assert!(is_alive(parent_pid()));
    }

    #[test]
    fn process_group_ids_are_not_session_pids() {
        assert!(!is_alive(0));
        assert!(!is_alive(u32::MAX));
    }
}
