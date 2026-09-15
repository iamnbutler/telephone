//! Opt-in HTTP delivery to a bound, existing OpenCode session. No discovery probes.
//! The local operator explicitly trusts the loopback server; HTTP Basic authenticates
//! the caller, not the server. This is not a transport for untrusted/remote endpoints.
use crate::{address::Address, envelope::Envelope, private_fs, registry::Delivered};
use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use serde::{de::DeserializeOwned, Deserialize};
use serde_json::json;
use std::{path::PathBuf, time::Duration};

mod protocol;
mod route;
use protocol::{HttpFailure, Message, MessageInfo, PreflightError, Session, UncertainDelivery};
pub use route::{Route, RouteConfig};

const RESPONSE_LIMIT: u64 = 256 * 1024;

/// Native OpenCode delivery composes the shared registered inbox.
pub struct OpenCode {
    inbox: super::inbox_only::InboxOnly,
}

impl OpenCode {
    pub fn new(root: PathBuf) -> Self {
        Self {
            inbox: super::inbox_only::InboxOnly {
                runtime: crate::runtime::InboxRuntime::OpenCode,
                root,
            },
        }
    }

    fn enrich(
        &self,
        mut report: crate::discovery::Discovery,
    ) -> Result<crate::discovery::Discovery> {
        if report.agents.is_empty() {
            return Ok(report);
        }
        let store = crate::store::Store::open(&self.inbox.root)?;
        let mut warnings = Vec::new();
        report.agents.retain_mut(|agent| {
            match store.opencode_route(agent.address()) {
                Ok(Some(route)) => {
                    agent
                        .transports
                        .insert(0, crate::registry::Transport::OpenCodeHttp);
                    agent.cwd = Some(route.directory().into());
                }
                Ok(None) => {}
                Err(error) => {
                    warnings.push(format!("{}: {error:#}", agent.addr()));
                    return false;
                }
            }
            true
        });
        for warning in warnings {
            report.warn(
                crate::runtime::Runtime::OpenCode,
                crate::discovery::Code::SourceUnavailable,
                None,
                warning,
            );
        }
        Ok(report)
    }
}

impl crate::registry::Adapter for OpenCode {
    fn runtime(&self) -> crate::runtime::Runtime {
        crate::runtime::Runtime::OpenCode
    }
    fn discover(&self) -> Result<crate::discovery::Discovery> {
        self.enrich(self.inbox.discover()?)
    }
    fn find_exact(&self, address: &Address) -> Result<crate::discovery::Discovery> {
        self.enrich(self.inbox.find_exact(address)?)
    }
    fn deliver(&self, agent: &crate::registry::Agent, env: &Envelope) -> Result<Delivered> {
        self.inbox.check_target(agent, env)?;
        let store = crate::store::Store::open(&self.inbox.root)?;
        let unavailable = match store.opencode_route(&env.to)? {
            Some(route) => match route.deliver(env)? {
                Attempt::Accepted => {
                    return Ok(Delivered::Accepted {
                        via: "OpenCode HTTP (processing unconfirmed)",
                    })
                }
                Attempt::Unavailable(reason) => Some(reason),
                Attempt::Uncertain(error) => return Err(error.into()),
            },
            None => None,
        };
        let mut result = self.inbox.deliver(agent, env)?;
        if let (Some(reason), Delivered::Queued { note, .. }) = (unavailable, &mut result) {
            *note = format!("OpenCode preflight failed before sending: {reason}. {note}");
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests;

impl Route {
    fn client(&self) -> Result<Client<'_>> {
        let raw = private_fs::read_secret(self.credentials(), 8192)
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
        self.client()?.verify().map_err(Into::into)
    }

    fn deliver(&self, env: &Envelope) -> Result<Attempt> {
        // Credential/configuration errors fail closed, not silently into another transport.
        let client = self.client()?;
        let context = match client.verify().and_then(|()| client.recorded_context()) {
            Ok(context) => context,
            Err(error) => return Ok(Attempt::Unavailable(error)),
        };
        // Never propagate a POST error with ? into a preflight/fallback branch.
        Ok(match client.prompt(env, &context, false) {
            Ok(()) => Attempt::Accepted,
            Err(error) => Attempt::Uncertain(error),
        })
    }
}

/// Only Unavailable permits inbox fallback; POST outcomes never do.
enum Attempt {
    Unavailable(PreflightError),
    Accepted,
    Uncertain(UncertainDelivery),
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

fn context_string(value: String) -> Result<String, PreflightError> {
    if value.is_empty() || value.len() > 256 || value.chars().any(char::is_control) {
        return Err(PreflightError::InvalidContext);
    }
    Ok(value)
}

impl Client<'_> {
    fn url(&self, path: &str) -> String {
        self.route.url(path)
    }

    fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T, PreflightError> {
        let mut response = self
            .agent
            .get(self.url(path))
            .query("directory", self.route.directory())
            .header("Authorization", &self.auth)
            .call()
            .map_err(HttpFailure::from)?;
        if response.status().as_u16() != 200 {
            return Err(HttpFailure::Status(response.status().as_u16()).into());
        }
        let text = response
            .body_mut()
            .with_config()
            .limit(RESPONSE_LIMIT)
            .read_to_string()
            .map_err(HttpFailure::from)?;
        serde_json::from_str(&text).map_err(|_| PreflightError::InvalidResponse)
    }

    fn verify(&self) -> Result<(), PreflightError> {
        let response = self
            .agent
            .get(self.url("/global/health"))
            .call()
            .map_err(HttpFailure::from)?;
        if response.status().as_u16() != 401 {
            return Err(PreflightError::Unprotected);
        }
        let session: Session = self.get(&format!("/session/{}", self.route.session_id()))?;
        if session.id != self.route.session_id() || session.directory != self.route.directory() {
            return Err(PreflightError::SessionMismatch);
        }
        if session.time.archived.is_some() {
            return Err(PreflightError::Archived);
        }
        Ok(())
    }

    fn recorded_context(&self) -> Result<PromptContext, PreflightError> {
        let messages: Vec<Message> = self.get(&format!(
            "/session/{}/message?limit=1",
            self.route.session_id()
        ))?;
        let [message]: [Message; 1] = messages
            .try_into()
            .map_err(|_| PreflightError::NoRecordedTurn)?;
        let (common, model) = match message.info {
            MessageInfo::User { common, model } | MessageInfo::Assistant { common, model } => {
                (common, model)
            }
        };
        if common.session_id != self.route.session_id() {
            return Err(PreflightError::MessageSessionMismatch);
        }
        Ok(PromptContext {
            agent: context_string(common.agent)?,
            provider: context_string(model.provider)?,
            model: context_string(model.model)?,
            variant: common.variant.map(context_string).transpose()?,
        })
    }

    fn prompt(
        &self,
        env: &Envelope,
        context: &PromptContext,
        no_reply: bool,
    ) -> Result<(), UncertainDelivery> {
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
        // Value is already JSON; encoding it has no application-level failure path.
        let encoded = body.to_string();
        let response = self
            .agent
            .post(self.url(&format!(
                "/session/{}/prompt_async",
                self.route.session_id()
            )))
            .query("directory", self.route.directory())
            .header("Authorization", &self.auth)
            .header("Content-Type", "application/json")
            .send(encoded)
            .map_err(|error| UncertainDelivery(HttpFailure::from(error)))?;
        if response.status().as_u16() != 204 {
            return Err(UncertainDelivery(HttpFailure::Status(
                response.status().as_u16(),
            )));
        }
        // 204 only accepts asynchronous work. It can fail later; never claim model receipt.
        Ok(())
    }
}

pub fn address(address: &Address) -> Result<()> {
    if address.runtime()? != crate::runtime::Runtime::OpenCode {
        bail!("native OpenCode bindings require an opencode: address");
    }
    Ok(())
}
