//! Known harnesses. Strings belong at CLI, JSON and database boundaries.
use serde::{Deserialize, Serialize};
use std::{fmt, str::FromStr};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Runtime {
    Claude,
    Codex,
    OpenCode,
    Zed,
    Delta,
    Generic,
}

impl Runtime {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::OpenCode => "opencode",
            Self::Zed => "zed",
            Self::Delta => "delta",
            Self::Generic => "generic",
        }
    }
    pub const fn registered(self) -> Option<InboxRuntime> {
        match self {
            Self::Claude | Self::Codex => None,
            Self::OpenCode => Some(InboxRuntime::OpenCode),
            Self::Zed => Some(InboxRuntime::Zed),
            Self::Delta => Some(InboxRuntime::Delta),
            Self::Generic => Some(InboxRuntime::Generic),
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
        f.write_str("unsupported address runtime")
    }
}
impl std::error::Error for UnsupportedRuntime {}
impl FromStr for Runtime {
    type Err = UnsupportedRuntime;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "claude" => Ok(Self::Claude),
            "codex" => Ok(Self::Codex),
            "opencode" => Ok(Self::OpenCode),
            "zed" => Ok(Self::Zed),
            "delta" => Ok(Self::Delta),
            "generic" => Ok(Self::Generic),
            _ => Err(UnsupportedRuntime),
        }
    }
}

/// Only these runtimes can own explicitly registered inboxes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum InboxRuntime {
    #[value(name = "opencode")]
    OpenCode,
    Zed,
    Delta,
    Generic,
}
impl InboxRuntime {
    pub const ALL: [Self; 4] = [Self::OpenCode, Self::Zed, Self::Delta, Self::Generic];
    pub const fn runtime(self) -> Runtime {
        match self {
            Self::OpenCode => Runtime::OpenCode,
            Self::Zed => Runtime::Zed,
            Self::Delta => Runtime::Delta,
            Self::Generic => Runtime::Generic,
        }
    }
    pub const fn as_str(self) -> &'static str {
        self.runtime().as_str()
    }
}
impl fmt::Display for InboxRuntime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn wire_names_and_registration_subset_are_stable() {
        for runtime in [
            Runtime::Claude,
            Runtime::Codex,
            Runtime::OpenCode,
            Runtime::Zed,
            Runtime::Delta,
            Runtime::Generic,
        ] {
            assert_eq!(runtime.as_str().parse::<Runtime>().unwrap(), runtime);
            assert_eq!(serde_json::to_value(runtime).unwrap(), runtime.as_str());
        }
        for runtime in InboxRuntime::ALL {
            assert_eq!(runtime.runtime().registered(), Some(runtime));
            assert_eq!(
                serde_json::from_value::<InboxRuntime>(serde_json::json!(runtime.as_str()))
                    .unwrap(),
                runtime
            );
        }
        assert!(serde_json::from_str::<InboxRuntime>("\"claude\"").is_err());
        assert!("unknown".parse::<Runtime>().is_err());
    }
}
