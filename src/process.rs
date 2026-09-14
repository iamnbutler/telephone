//! Deadline- and output-bounded subprocesses. A guard owns cleanup after spawn.
use anyhow::{anyhow, Context};
use std::{
    fmt,
    io::{self, Read},
    os::{
        fd::OwnedFd,
        unix::{net::UnixStream, process::CommandExt},
    },
    process::{Child, Command, Output, Stdio},
    time::{Duration, Instant},
};

const MAX_OUTPUT: usize = 64 * 1024;
#[derive(Debug)]
pub enum Failure {
    BeforeStart(anyhow::Error),
    AfterStart(anyhow::Error),
}
impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BeforeStart(e) => write!(f, "command was not started: {e:#}"),
            Self::AfterStart(e) => write!(f, "command started; outcome may be uncertain: {e:#}"),
        }
    }
}
impl std::error::Error for Failure {}

struct ChildGuard {
    child: Child,
    reaped: bool,
}
impl ChildGuard {
    fn exited(&self) -> io::Result<bool> {
        // Observe without reaping: retaining the zombie reserves its PID until
        // group cleanup, avoiding a signal to a recycled process-group ID.
        // SAFETY: zeroed siginfo_t is valid output storage; waitid writes it.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        // SAFETY: the PID is our unreaped child and info is valid output storage.
        let result = unsafe {
            libc::waitid(
                libc::P_PID,
                self.child.id() as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if result != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: waitid initialized siginfo for a child status notification.
        Ok(unsafe { info.si_pid() } != 0)
    }
    fn finish(&mut self) -> io::Result<std::process::ExitStatus> {
        let exited = self.exited()?;
        let mut group_error = kill_group(self.child.id()).err();
        if let Some(error) = &group_error {
            // XNU killpg1 filters zombies, then returns EPERM when the group has
            // no signalable members. Only accept that case for an observed exit.
            // apple-oss-distributions/xnu/bsd/kern/kern_sig.c: killpg1
            if cfg!(target_os = "macos") && exited && error.raw_os_error() == Some(libc::EPERM) {
                group_error = None;
            }
        }
        if !exited {
            // Signal the owned child as well: it may have changed groups, in
            // which case killpg can return ESRCH while the child is still alive.
            // A successful group signal alone is not a reason to wait forever.
            self.child.kill()?;
        }
        let status = self.child.wait()?;
        self.reaped = true;
        match group_error {
            Some(error) => Err(error),
            None => Ok(status),
        }
    }
}
fn kill_group(pid: u32) -> io::Result<()> {
    // SAFETY: this is an owned, unreaped child started in its own process group.
    let result = unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
    if result != 0 {
        let e = io::Error::last_os_error();
        if e.raw_os_error() != Some(libc::ESRCH) {
            return Err(e);
        }
    }
    Ok(())
}
impl Drop for ChildGuard {
    fn drop(&mut self) {
        if self.reaped {
            return;
        }
        if let Err(e) = self.finish() {
            crate::warn(format!("child cleanup failed: {e}"));
        }
    }
}

pub fn run(mut command: Command, timeout: Duration) -> Result<Output, Failure> {
    let setup = (|| -> anyhow::Result<_> {
        let (stdout, stdout_child) = UnixStream::pair().context("creating stdout channel")?;
        let (stderr, stderr_child) = UnixStream::pair().context("creating stderr channel")?;
        stdout.set_nonblocking(true)?;
        stderr.set_nonblocking(true)?;
        command
            .stdin(Stdio::null())
            .stdout(Stdio::from(OwnedFd::from(stdout_child)))
            .stderr(Stdio::from(OwnedFd::from(stderr_child)))
            .process_group(0);
        Ok((stdout, stderr))
    })()
    .map_err(Failure::BeforeStart)?;
    let (mut stdout, mut stderr) = setup;
    let child = command
        .spawn()
        .context("spawning command")
        .map_err(Failure::BeforeStart)?;
    let mut guard = ChildGuard {
        child,
        reaped: false,
    };
    // Close the parent's copies of the child's output descriptors.
    drop(command);
    // All propagation below is guarded: cleanup runs on every early return.
    let result = (|| -> anyhow::Result<Output> {
        let start = Instant::now();
        let mut out = Vec::new();
        let mut err = Vec::new();
        let mut out_eof = false;
        let mut err_eof = false;
        loop {
            if start.elapsed() >= timeout {
                return Err(anyhow!("command exceeded its {:?} deadline", timeout));
            }
            if !out_eof {
                out_eof = drain(&mut stdout, &mut out)?;
            }
            if !err_eof {
                err_eof = drain(&mut stderr, &mut err)?;
            }
            if guard.exited().context("checking child status")? && out_eof && err_eof {
                let status = guard.finish().context("reaping completed command")?;
                return Ok(Output {
                    status,
                    stdout: out,
                    stderr: err,
                });
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    })();
    match result {
        Ok(output) => Ok(output),
        Err(error) => {
            // Report cleanup failures alongside the original error. Drop is a
            // final safety net, not the normal (unreportable) error path.
            if !guard.reaped {
                if let Err(cleanup) = guard.finish() {
                    return Err(Failure::AfterStart(
                        error.context(format!("child cleanup also failed: {cleanup}")),
                    ));
                }
            }
            Err(Failure::AfterStart(error))
        }
    }
}

fn drain(stream: &mut UnixStream, output: &mut Vec<u8>) -> anyhow::Result<bool> {
    // Bound each poll so a constantly writing child cannot starve the deadline.
    let mut buffer = [0; 8192];
    for _ in 0..8 {
        match stream.read(&mut buffer) {
            Ok(0) => return Ok(true),
            Ok(n) => {
                if output.len() + n > MAX_OUTPUT {
                    return Err(anyhow!("command output exceeded {MAX_OUTPUT} bytes"));
                }
                output.extend_from_slice(&buffer[..n]);
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(false),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e).context("reading command output"),
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn captures_real_process_output_and_exit_status() {
        let mut command = Command::new("sh");
        command.args(["-c", "printf hello; printf problem >&2; exit 7"]);
        let out = run(command, Duration::from_secs(2)).unwrap();
        assert_eq!(out.stdout, b"hello");
        assert_eq!(out.stderr, b"problem");
        assert_eq!(out.status.code(), Some(7));
    }
    #[test]
    fn bounds_hung_and_noisy_processes() {
        let mut hung = Command::new("sh");
        hung.args(["-c", "sleep 30 & wait"]);
        let start = Instant::now();
        assert!(matches!(
            run(hung, Duration::from_millis(150)),
            Err(Failure::AfterStart(_))
        ));
        assert!(start.elapsed() < Duration::from_secs(3));
        assert!(matches!(
            run(Command::new("yes"), Duration::from_secs(2)),
            Err(Failure::AfterStart(_))
        ));
        assert!(matches!(
            run(
                Command::new("/no/such/telephone-command"),
                Duration::from_secs(1)
            ),
            Err(Failure::BeforeStart(_))
        ));
    }

    #[test]
    fn deadline_reaps_child_and_stops_its_process_group() {
        let temp = tempfile::tempdir().unwrap();
        let pids = temp.path().join("pids");
        let mut command = Command::new("sh");
        command
            .args([
                "-c",
                "sleep 30 & printf '%s %s' \"$$\" \"$!\" > \"$1\"; wait",
                "telephone-deadline-test",
            ])
            .arg(&pids);
        assert!(matches!(
            run(command, Duration::from_millis(250)),
            Err(Failure::AfterStart(_))
        ));
        let text = std::fs::read_to_string(pids).unwrap();
        let ids: Vec<u32> = text
            .split_whitespace()
            .map(|p| p.parse().unwrap())
            .collect();
        assert_eq!(ids.len(), 2);
        assert!(
            !crate::proc::is_alive(ids[0]),
            "direct child was not reaped"
        );
        // A descendant may briefly be a zombie awaiting init. It must not be running.
        let output = Command::new("ps")
            .args(["-o", "stat=", "-p", &ids[1].to_string()])
            .output()
            .unwrap();
        assert!(String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .all(|line| line.trim().starts_with('Z')));
    }
}
