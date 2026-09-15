//! Bounded discovery with explicit partial results. Diagnostics are data, not logs.
use crate::{registry::Agent, runtime::Runtime};
use serde::Serialize;
use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

pub const MAX_AGENTS: usize = 256;
pub const MAX_ENTRIES: usize = 4096;
pub const MAX_BYTES: usize = 8 * 1024 * 1024;
const MAX_WARNINGS: usize = 64;
const SCAN_TIME: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Code {
    Unreadable,
    InvalidRecord,
    LimitReached,
    SourceUnavailable,
    NativeUnavailable,
}

#[derive(Debug, Clone, Serialize)]
pub struct Warning {
    pub runtime: Runtime,
    pub code: Code,
    pub path: Option<String>,
    pub message: String,
}
impl std::fmt::Display for Warning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.runtime, self.message)?;
        if let Some(path) = &self.path {
            write!(f, " ({path})")?;
        }
        Ok(())
    }
}

#[derive(Debug)]
pub struct Discovery {
    pub agents: Vec<Agent>,
    pub warnings: Vec<Warning>,
    pub warnings_omitted: usize,
    pub complete: bool,
}
impl Default for Discovery {
    fn default() -> Self {
        Self {
            agents: Vec::new(),
            warnings: Vec::new(),
            warnings_omitted: 0,
            complete: true,
        }
    }
}
impl Discovery {
    pub fn warn(
        &mut self,
        runtime: Runtime,
        code: Code,
        path: Option<&Path>,
        message: impl std::fmt::Display,
    ) {
        self.complete = false;
        self.note(runtime, code, path, message);
    }
    pub fn note(
        &mut self,
        runtime: Runtime,
        code: Code,
        path: Option<&Path>,
        message: impl std::fmt::Display,
    ) {
        self.add_warning(Warning {
            runtime,
            code,
            path: path.map(|p| p.to_string_lossy().chars().take(4096).collect()),
            message: message.to_string().chars().take(512).collect(),
        });
    }
    fn add_warning(&mut self, warning: Warning) {
        if self.warnings.len() < MAX_WARNINGS {
            self.warnings.push(warning);
        } else {
            self.warnings_omitted = self.warnings_omitted.saturating_add(1);
        }
    }
    pub fn merge(&mut self, other: Self) {
        self.complete &= other.complete;
        self.agents.extend(other.agents);
        self.warnings_omitted = self.warnings_omitted.saturating_add(other.warnings_omitted);
        for warning in other.warnings {
            self.add_warning(warning);
        }
    }
    pub fn push(&mut self, agent: Agent) -> bool {
        if let Some(old) = self
            .agents
            .iter_mut()
            .find(|old| old.addr() == agent.addr())
        {
            if agent.last_seen > old.last_seen {
                *old = agent;
            }
            return true;
        }
        if self.agents.len() == MAX_AGENTS {
            self.warn(
                agent.runtime(),
                Code::LimitReached,
                None,
                "agent limit reached (256); use an exact address",
            );
            return false;
        }
        self.agents.push(agent);
        true
    }
}

pub struct Budget {
    pub deadline: Instant,
    entries: usize,
    pub bytes: usize,
    exhausted: bool,
}
impl Budget {
    pub fn new() -> Self {
        Self {
            deadline: Instant::now() + SCAN_TIME,
            entries: 0,
            bytes: MAX_BYTES,
            exhausted: false,
        }
    }
    pub fn check(&mut self, runtime: Runtime, report: &mut Discovery) -> bool {
        if self.exhausted {
            return false;
        }
        if Instant::now() >= self.deadline || self.bytes == 0 {
            self.exhausted = true;
            report.warn(
                runtime,
                Code::LimitReached,
                None,
                "discovery time or metadata-byte limit reached; results are partial",
            );
            return false;
        }
        true
    }
    fn entry(&mut self, runtime: Runtime, report: &mut Discovery) -> bool {
        if !self.check(runtime, report) {
            return false;
        }
        if self.entries >= MAX_ENTRIES {
            if self.entries == MAX_ENTRIES {
                report.warn(
                    runtime,
                    Code::LimitReached,
                    None,
                    "directory entry limit reached (4096); results are partial",
                );
                self.entries += 1;
            }
            return false;
        }
        self.entries += 1;
        true
    }
}

/// Traversal is bounded independently of how many entries match. Nested symlink
/// directories are not followed. Errors preserve other readable entries.
pub fn files(
    root: &Path,
    recursive: bool,
    matches: fn(&Path) -> bool,
    runtime: Runtime,
    budget: &mut Budget,
    report: &mut Discovery,
) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut pending = vec![root.to_owned()];
    while let Some(dir) = pending.pop() {
        if !budget.check(runtime, report) {
            break;
        }
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && dir == root => continue,
            Err(e) => {
                report.warn(runtime, Code::Unreadable, Some(&dir), e);
                continue;
            }
        };
        for entry in entries {
            if !budget.entry(runtime, report) {
                return found;
            }
            let entry = match entry {
                Ok(entry) => entry,
                Err(e) => {
                    report.warn(runtime, Code::Unreadable, Some(&dir), e);
                    continue;
                }
            };
            let path = entry.path();
            // Inspect only directories and candidate records; unrelated config
            // files are not discovery inputs.
            let kind = match entry.file_type() {
                Ok(kind) => kind,
                Err(e) => {
                    report.warn(runtime, Code::Unreadable, Some(&path), e);
                    continue;
                }
            };
            if recursive && kind.is_dir() {
                pending.push(path);
            } else if matches(&path) {
                found.push(path);
            }
        }
    }
    found
}
