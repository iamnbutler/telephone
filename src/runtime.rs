//! Native runtimes supported by Telephone.
use serde::{Deserialize, Serialize};
use std::{fmt, str::FromStr};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Runtime {
    Claude,
    Codex,
}
impl Runtime {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }
}
impl fmt::Display for Runtime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}
#[derive(Debug, Clone, Copy)]
pub struct UnsupportedRuntime;
impl fmt::Display for UnsupportedRuntime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "unsupported address runtime. Telephone supports only Codex and Claude Code."
        )
    }
}
impl std::error::Error for UnsupportedRuntime {}
impl FromStr for Runtime {
    type Err = UnsupportedRuntime;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "claude" => Ok(Self::Claude),
            "codex" => Ok(Self::Codex),
            _ => Err(UnsupportedRuntime),
        }
    }
}
