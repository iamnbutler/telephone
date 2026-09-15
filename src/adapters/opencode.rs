//! Opt-in HTTP delivery to a bound, existing OpenCode session. No discovery probes.
//! The local operator explicitly trusts the loopback server; HTTP Basic authenticates
//! the caller, not the server. This is not a transport for untrusted/remote endpoints.
use crate::{address::Address, envelope::Envelope, private_fs, registry::Delivered};
use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{net::SocketAddr, path::PathBuf, time::Duration};

const RESPONSE_LIMIT: u64 = 256 * 1024;

/// Keep actionable failure classes, never response bodies, URLs or auth headers.
fn http_error(error: ureq::Error) -> anyhow::Error {
    use ureq::Error;
    let detail = match error {
        Error::Io(error) => format!("I/O {:?}", error.kind()),
        Error::Timeout(stage) => format!("timeout during {stage:?}"),
        Error::StatusCode(code) => format!("HTTP {code}"),
        Error::TooManyRedirects | Error::RedirectFailed => "redirect refused".into(),
        Error::BodyExceedsLimit(limit) => format!("body exceeds {limit}-byte limit"),
        Error::Protocol(_) => "invalid HTTP protocol".into(),
        Error::BadUri(_) | Error::Http(_) => "invalid HTTP request".into(),
        Error::HostNotFound | Error::ConnectionFailed => "connection failed".into(),
        other => format!(
            "unexpected HTTP failure ({:?})",
            std::mem::discriminant(&other)
        ),
    };
    anyhow::anyhow!("{detail}")
}

#[cfg(test)]
mod tests;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Route {
    pub endpoint: String,
    pub session_id: String,
    pub directory: String,
    pub credentials: PathBuf,
}

impl Route {
    pub fn validate(&self) -> Result<()> {
        let socket: SocketAddr = self
            .endpoint
            .strip_prefix("http://")
            .context("OpenCode endpoint must be http://127.0.0.1:PORT or http://[::1]:PORT")?
            .parse()
            .context("OpenCode endpoint must contain only a literal loopback address and port")?;
        if !socket.ip().is_loopback()
            || socket.port() == 0
            || self.endpoint != format!("http://{socket}")
        {
            bail!("OpenCode endpoint must be a canonical literal loopback address with a nonzero port");
        }
        if !self.session_id.starts_with("ses_")
            || self.session_id.len() <= 4
            || self.session_id.len() > 128
            || !self
                .session_id
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'_')
        {
            bail!("invalid OpenCode session ID; use the server's ses_ ID, not a Telephone UUID");
        }
        if self.directory.len() > 4096
            || !std::path::Path::new(&self.directory).is_absolute()
            || self.directory.chars().any(char::is_control)
            || !self.credentials.is_absolute()
        {
            bail!("OpenCode directory and credential file must be absolute paths; directory must be bounded and contain no controls");
        }
        Ok(())
    }

    fn client(&self) -> Result<Client<'_>> {
        self.validate()?;
        let raw = private_fs::read_secret(&self.credentials, 8192)
            .context("reading OpenCode credentials (owner-only JSON file required)")?;
        // Do not include serde diagnostics: unknown fields may contain secrets.
        let credentials: Credentials = serde_json::from_str(&raw).map_err(|_| {
            anyhow::anyhow!("invalid OpenCode credential JSON; expected username and password")
        })?;
        if credentials.username.is_empty()
            || credentials.username.contains(':')
            || credentials.password.is_empty()
            || credentials.username.len() > 256
            || credentials.password.len() > 4096
            || credentials.username.chars().any(char::is_control)
            || credentials.password.chars().any(char::is_control)
        {
            bail!("invalid OpenCode credentials");
        }
        let auth = format!(
            "Basic {}",
            STANDARD.encode(format!("{}:{}", credentials.username, credentials.password))
        );
        let agent = ureq::Agent::config_builder()
            .proxy(None)
            .max_redirects(0)
            .http_status_as_error(false)
            .max_idle_connections(0)
            .max_response_header_size(16 * 1024)
            .timeout_global(Some(Duration::from_secs(3)))
            .timeout_connect(Some(Duration::from_secs(1)))
            .build()
            .into();
        Ok(Client {
            route: self,
            agent,
            auth,
        })
    }

    /// Read-only validation, including proving that the server requires credentials.
    pub fn verify(&self) -> Result<()> {
        self.client()?.verify()
    }

    pub fn deliver(&self, env: &Envelope) -> Result<Attempt> {
        let client = self.client()?;
        // No message bytes have been submitted: a failed preflight is safe to queue.
        let context = match client.verify().and_then(|()| client.recorded_context()) {
            Ok(context) => context,
            Err(error) => {
                return Ok(Attempt::Unavailable(format!(
                    "OpenCode preflight failed before sending: {error:#}"
                )))
            }
        };
        client.prompt(env, &context, false)?;
        Ok(Attempt::Delivered(Delivered::Accepted {
            via: "OpenCode HTTP (processing unconfirmed)",
        }))
    }
}

pub enum Attempt {
    Unavailable(String),
    Delivered(Delivered),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Credentials {
    username: String,
    password: String,
}

struct Client<'a> {
    route: &'a Route,
    agent: ureq::Agent,
    auth: String,
}

struct PromptContext {
    agent: String,
    provider: String,
    model: String,
    variant: Option<String>,
}

fn context_string(value: &Value) -> Result<String> {
    let value = value
        .as_str()
        .context("missing recorded OpenCode agent/model field")?;
    if value.is_empty() || value.len() > 256 || value.chars().any(char::is_control) {
        bail!("invalid recorded OpenCode agent/model field");
    }
    Ok(value.to_owned())
}

impl Client<'_> {
    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.route.endpoint)
    }

    fn get(&self, path: &str) -> Result<Value> {
        let mut response = self
            .agent
            .get(self.url(path))
            .query("directory", &self.route.directory)
            .header("Authorization", &self.auth)
            .call()
            .map_err(http_error)
            .context("OpenCode read-only request failed")?;
        if response.status().as_u16() != 200 {
            bail!(
                "OpenCode read-only request returned HTTP {}",
                response.status().as_u16()
            );
        }
        let text = response
            .body_mut()
            .with_config()
            .limit(RESPONSE_LIMIT)
            .read_to_string()
            .map_err(http_error)
            .context("reading bounded OpenCode response")?;
        serde_json::from_str(&text).map_err(|_| anyhow::anyhow!("OpenCode returned invalid JSON"))
    }

    fn verify(&self) -> Result<()> {
        let response = self
            .agent
            .get(self.url("/global/health"))
            .call()
            .map_err(http_error)
            .context("OpenCode authentication probe failed")?;
        if response.status().as_u16() != 401 {
            bail!("OpenCode server must require HTTP authentication; refusing an unprotected or unexpected endpoint");
        }
        let session = self.get(&format!("/session/{}", self.route.session_id))?;
        if session["id"] != self.route.session_id || session["directory"] != self.route.directory {
            bail!("OpenCode session ID or directory does not match its binding");
        }
        if !session["time"]["archived"].is_null() {
            bail!("OpenCode session is archived");
        }
        Ok(())
    }

    fn recorded_context(&self) -> Result<PromptContext> {
        let messages = self.get(&format!(
            "/session/{}/message?limit=1",
            self.route.session_id
        ))?;
        let messages = messages
            .as_array()
            .context("invalid OpenCode message listing")?;
        if messages.len() != 1 {
            bail!("OpenCode session needs a recorded turn before native delivery");
        }
        let info = &messages[0]["info"];
        if info["sessionID"] != self.route.session_id {
            bail!("OpenCode message belongs to another session");
        }
        let (provider, model) = match info["role"].as_str() {
            Some("user") => (&info["model"]["providerID"], &info["model"]["modelID"]),
            Some("assistant") => (&info["providerID"], &info["modelID"]),
            _ => bail!("unexpected OpenCode message role"),
        };
        Ok(PromptContext {
            agent: context_string(&info["agent"])?,
            provider: context_string(provider)?,
            model: context_string(model)?,
            variant: if info["variant"].is_null() {
                None
            } else {
                Some(context_string(&info["variant"])?)
            },
        })
    }

    fn prompt(&self, env: &Envelope, context: &PromptContext, no_reply: bool) -> Result<()> {
        // Let OpenCode assign its ordered native IDs. Telephone's correlation ID
        // remains in the peer envelope; it is not an OpenCode idempotency token.
        let mut body = json!({"agent":context.agent,"model":{"providerID":context.provider,"modelID":context.model},
            "parts":[{"type":"text","text":super::format_for_delivery(env)}]});
        if let Some(variant) = &context.variant {
            body["variant"] = json!(variant);
        }
        // Test-only call sites suppress model work against a real isolated server.
        // Preserve recorded agent/model selections, not defaults or sender choices.
        // Never set system prompts or tool-permission overrides.
        if no_reply {
            body["noReply"] = json!(true);
        }
        let encoded =
            serde_json::to_string(&body).context("encoding OpenCode message before sending")?;
        let response = self.agent.post(self.url(&format!("/session/{}/prompt_async", self.route.session_id)))
            .query("directory", &self.route.directory)
            .header("Authorization", &self.auth)
            .header("Content-Type", "application/json")
            .send(encoded)
            .map_err(http_error).context("OpenCode delivery is uncertain after POST began; no fallback or retry was attempted")?;
        if response.status().as_u16() != 204 {
            bail!("OpenCode delivery is uncertain after POST returned HTTP {}; no fallback or retry was attempted", response.status().as_u16());
        }
        // 204 only accepts asynchronous work. It can fail later; never claim model receipt.
        Ok(())
    }
}

pub fn address(address: &Address) -> Result<()> {
    if !address.as_str().starts_with("opencode:") {
        bail!("native OpenCode bindings require an opencode: address");
    }
    Ok(())
}
