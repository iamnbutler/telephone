//! Liveness checks.
//!
//! A session registry entry outlives the process it describes, so discovery
//! has to ask the OS what's actually still running -- and ask carefully. A pid
//! on its own is not an identity: pids get reused, and a recycled one will
//! happily accept a message meant for a session that exited hours ago.

use std::collections::HashMap;
use std::process::Command;

/// True if `pid` is a live process we're allowed to see.
///
/// `kill(pid, 0)` performs permission and existence checks without sending a
/// signal. This does not guard against pid reuse; pair it with [`start_times`]
/// and [`start_time_matches`] when the caller has a recorded start time.
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

/// How far a process's start may sit from its session's recorded start before
/// we treat the pid as belonging to somebody else.
///
/// A session records its start time a moment after the process begins, so some
/// slack is required. Five minutes is generous enough never to hide a real
/// session, while still catching a pid recycled hours or days later.
const START_TOLERANCE_MILLIS: u64 = 5 * 60 * 1000;

/// Process start times for `pids`, as epoch milliseconds.
///
/// Derived from elapsed time rather than `ps -o lstart=`, whose output is
/// locale-formatted (`Mon 14 Sep` here, `Mon Sep 14` elsewhere) and so can't
/// be compared against a recorded timestamp as a string.
///
/// One `ps` call for the whole set rather than one per pid: discovery runs on
/// every command, and a process spawn per session adds up.
pub fn start_times(pids: &[u32]) -> HashMap<u32, u64> {
    let mut out = HashMap::new();
    if pids.is_empty() {
        return out;
    }
    let list = pids
        .iter()
        .map(|p| p.to_string())
        .collect::<Vec<_>>()
        .join(",");

    let Ok(result) = Command::new("ps").args(["-o", "pid=,etime=", "-p", &list]).output() else {
        return out;
    };
    let now = crate::envelope::now_millis();
    for line in String::from_utf8_lossy(&result.stdout).lines() {
        let line = line.trim();
        let Some((pid, elapsed)) = line.split_once(char::is_whitespace) else { continue };
        let Ok(pid) = pid.trim().parse::<u32>() else { continue };
        let Some(seconds) = parse_etime(elapsed.trim()) else { continue };
        out.insert(pid, now.saturating_sub(seconds * 1000));
    }
    out
}

/// Parses `ps` elapsed time, which is `[[DD-]HH:]MM:SS`.
fn parse_etime(s: &str) -> Option<u64> {
    let (days, rest) = match s.split_once('-') {
        Some((d, rest)) => (d.parse::<u64>().ok()?, rest),
        None => (0, s),
    };
    let mut parts = rest.split(':').rev();
    let seconds = parts.next()?.parse::<u64>().ok()?;
    let minutes = parts.next().unwrap_or("0").parse::<u64>().ok()?;
    let hours = parts.next().unwrap_or("0").parse::<u64>().ok()?;
    Some(days * 86_400 + hours * 3_600 + minutes * 60 + seconds)
}

/// Whether the process holding a pid started close enough to the session that
/// claimed it.
///
/// Returns `true` when there's nothing to compare: absence of evidence
/// shouldn't hide a session that is probably fine. This mirrors [`is_alive`],
/// which also errs toward keeping a session discoverable.
pub fn start_time_matches(session_started_at: Option<u64>, process_started_at: Option<&u64>) -> bool {
    match (session_started_at, process_started_at) {
        (Some(session), Some(&process)) => session.abs_diff(process) <= START_TOLERANCE_MILLIS,
        _ => true,
    }
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

    #[test]
    fn our_own_start_time_is_recent_and_plausible() {
        let me = std::process::id();
        let times = start_times(&[me]);
        let started = times.get(&me).expect("ps should report our own pid");
        let now = crate::envelope::now_millis();
        assert!(*started <= now, "we cannot have started in the future");
        assert!(now - started < 60 * 60 * 1000, "a test process is not an hour old");
    }

    #[test]
    fn etime_covers_every_shape_ps_emits() {
        assert_eq!(parse_etime("05"), Some(5));
        assert_eq!(parse_etime("01:30"), Some(90));
        assert_eq!(parse_etime("02:00:00"), Some(7_200));
        assert_eq!(parse_etime("3-04:05:06"), Some(3 * 86_400 + 4 * 3_600 + 5 * 60 + 6));
        assert_eq!(parse_etime("nonsense"), None);
    }

    #[test]
    fn a_recycled_pid_is_rejected_but_a_missing_record_is_not() {
        let session = 1_789_395_543_000u64;
        // The process starts a moment before the session records itself.
        assert!(start_time_matches(Some(session), Some(&(session - 800))));
        // Same pid, process started days later: the case that matters.
        assert!(!start_time_matches(Some(session), Some(&(session + 3 * 86_400_000))));
        // Nothing recorded, or nothing observed: don't hide the session.
        assert!(start_time_matches(None, Some(&session)));
        assert!(start_time_matches(Some(session), None));
    }
}
