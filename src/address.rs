//! Validated routing data. A claimed address is never proof of identity.
use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use std::{fmt, str::FromStr};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Address(String);
impl Address {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl FromStr for Address {
    type Err = anyhow::Error;
    fn from_str(value: &str) -> Result<Self> {
        let Some((runtime, id)) = value.split_once(':') else {
            bail!("address must have the form runtime:local-id");
        };
        if runtime.is_empty()
            || runtime.len() > 16
            || !runtime
                .bytes()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
            || id.is_empty()
            || id.len() > 128
            || !id
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'.'))
            || id == "."
            || id == ".."
        {
            bail!("invalid address: use an ASCII runtime and a local id containing only letters, digits, '.', '_' or '-'");
        }
        Ok(Self(value.to_owned()))
    }
}
impl TryFrom<String> for Address {
    type Error = anyhow::Error;
    fn try_from(value: String) -> Result<Self> {
        value.parse()
    }
}
impl From<Address> for String {
    fn from(value: Address) -> String {
        value.0
    }
}
impl fmt::Display for Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}
pub fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_shell_and_path_syntax_including_deserialization() {
        for input in [
            "",
            "a:",
            "a:..",
            "a:../../x",
            "a:x;printf bad",
            "a:x\n",
            "a:x`id`",
            "a:x$(id)",
            "a:x:y",
            "a:☃",
        ] {
            assert!(input.parse::<Address>().is_err(), "{input:?}");
            assert!(serde_json::from_value::<Address>(serde_json::json!(input)).is_err());
        }
        assert!("codex:abc-123".parse::<Address>().is_ok());
    }
    #[test]
    fn quoted_arguments_round_trip_through_a_real_shell() {
        let input = "a'; printf injected; # $(id)\n";
        let output = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("printf %s {}", shell_quote(input)))
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, input.as_bytes());
    }
}
