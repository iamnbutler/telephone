//! Wire types and sanitized errors. Never retain response bodies or auth headers in errors.
use serde::Deserialize;

#[derive(Debug, thiserror::Error)]
pub(super) enum HttpFailure {
    #[error("I/O {0:?}")]
    Io(std::io::ErrorKind),
    #[error("timeout during {0:?}")]
    Timeout(ureq::Timeout),
    #[error("HTTP {0}")]
    Status(u16),
    #[error("redirect refused")]
    Redirect,
    #[error("body exceeds {0}-byte limit")]
    BodyLimit(u64),
    #[error("response headers exceed {limit}-byte limit ({observed} bytes)")]
    HeaderLimit { observed: usize, limit: usize },
    #[error("invalid HTTP protocol")]
    Protocol,
    #[error("invalid HTTP request")]
    InvalidRequest,
    #[error("connection failed")]
    Connection,
    #[error("unclassified HTTP failure")]
    Other,
}

impl From<ureq::Error> for HttpFailure {
    fn from(error: ureq::Error) -> Self {
        use ureq::Error;
        match error {
            Error::Io(error) => Self::Io(error.kind()),
            Error::Timeout(stage) => Self::Timeout(stage),
            Error::StatusCode(code) => Self::Status(code),
            Error::TooManyRedirects | Error::RedirectFailed => Self::Redirect,
            Error::BodyExceedsLimit(limit) => Self::BodyLimit(limit),
            Error::LargeResponseHeader(observed, limit) => Self::HeaderLimit { observed, limit },
            Error::Protocol(_) | Error::BodyStalled => Self::Protocol,
            Error::BadUri(_) | Error::Http(_) => Self::InvalidRequest,
            Error::HostNotFound | Error::ConnectionFailed => Self::Connection,
            // ureq is non-exhaustive. Discard unknown diagnostics rather than leak secrets.
            _ => Self::Other,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub(super) enum PreflightError {
    #[error("OpenCode read-only request failed: {0}")]
    Http(#[from] HttpFailure),
    #[error("OpenCode returned an invalid response")]
    InvalidResponse,
    #[error("OpenCode server must require HTTP authentication; refusing an unprotected or unexpected endpoint")]
    Unprotected,
    #[error("OpenCode session ID or directory does not match its binding")]
    SessionMismatch,
    #[error("OpenCode session is archived")]
    Archived,
    #[error("OpenCode session needs a recorded turn before native delivery")]
    NoRecordedTurn,
    #[error("OpenCode message belongs to another session")]
    MessageSessionMismatch,
    #[error("invalid recorded OpenCode agent/model field")]
    InvalidContext,
}

#[derive(Debug, thiserror::Error)]
#[error("OpenCode delivery is uncertain after POST began: {0}; no fallback or retry was attempted")]
pub(super) struct UncertainDelivery(pub HttpFailure);

#[derive(Deserialize)]
pub(super) struct Session {
    pub id: String,
    pub directory: String,
    pub time: SessionTime,
}

#[derive(Deserialize)]
pub(super) struct SessionTime {
    pub archived: Option<u64>,
}

#[derive(Deserialize)]
pub(super) struct Message {
    pub info: MessageInfo,
}

#[derive(Deserialize)]
#[serde(tag = "role", rename_all = "lowercase")]
pub(super) enum MessageInfo {
    User {
        #[serde(flatten)]
        common: MessageContext,
        model: Model,
    },
    Assistant {
        #[serde(flatten)]
        common: MessageContext,
        #[serde(flatten)]
        model: Model,
    },
}

#[derive(Deserialize)]
pub(super) struct MessageContext {
    #[serde(rename = "sessionID")]
    pub session_id: String,
    pub agent: String,
    pub variant: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct Model {
    #[serde(rename = "providerID")]
    pub provider: String,
    #[serde(rename = "modelID")]
    pub model: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn recorded_turns_decode_by_role_without_defaulting_missing_selections() {
        for role in ["user", "assistant"] {
            let mut info = json!({"role":role, "sessionID":"ses_test", "agent":"plan",
                "variant":"high", "futureField":"ignored"});
            if role == "user" {
                info["model"] = json!({"providerID":"provider", "modelID":"model"});
            } else {
                info["providerID"] = json!("provider");
                info["modelID"] = json!("model");
            }
            let parsed: MessageInfo = serde_json::from_value(info.clone()).unwrap();
            let (common, model) = match parsed {
                MessageInfo::User { common, model } | MessageInfo::Assistant { common, model } => {
                    (common, model)
                }
            };
            assert_eq!(common.session_id, "ses_test");
            assert_eq!(common.agent, "plan");
            assert_eq!(common.variant.as_deref(), Some("high"));
            assert_eq!(model.provider, "provider");
            assert_eq!(model.model, "model");
            info.as_object_mut().unwrap().remove("agent");
            assert!(serde_json::from_value::<MessageInfo>(info).is_err());
        }
        assert!(serde_json::from_value::<MessageInfo>(json!({"role":"system"})).is_err());
        assert!(
            serde_json::from_value::<Session>(json!({"id":"ses_test","directory":"/tmp"})).is_err()
        );
    }

    #[test]
    fn transport_errors_preserve_class_without_retaining_sensitive_diagnostics() {
        let error = HttpFailure::from(ureq::Error::Io(std::io::Error::new(
            std::io::ErrorKind::ConnectionReset,
            "credential-must-not-appear",
        )));
        assert!(matches!(
            error,
            HttpFailure::Io(std::io::ErrorKind::ConnectionReset)
        ));
        let error = UncertainDelivery(error);
        assert!(!format!("{error:#?} {error}").contains("credential-must-not-appear"));
    }
}
