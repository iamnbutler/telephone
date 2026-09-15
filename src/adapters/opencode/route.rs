//! Persistent input is decoded once into a route that cannot be mutated unchecked.
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
};

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteConfig {
    pub endpoint: String,
    pub session_id: String,
    pub directory: String,
    pub credentials: PathBuf,
}

#[derive(Clone)]
struct LoopbackEndpoint(SocketAddr);

impl TryFrom<&str> for LoopbackEndpoint {
    type Error = anyhow::Error;
    fn try_from(value: &str) -> Result<Self> {
        let socket: SocketAddr = value
            .strip_prefix("http://")
            .context("OpenCode endpoint must be http://127.0.0.1:PORT or http://[::1]:PORT")?
            .parse()
            .context("OpenCode endpoint must contain only a literal loopback address and port")?;
        if !socket.ip().is_loopback() || socket.port() == 0 || value != format!("http://{socket}") {
            bail!("OpenCode endpoint must be a canonical literal loopback address with a nonzero port");
        }
        Ok(Self(socket))
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(try_from = "RouteConfig", into = "RouteConfig")]
pub struct Route {
    endpoint: LoopbackEndpoint,
    session_id: String,
    directory: String,
    credentials: PathBuf,
}

impl TryFrom<RouteConfig> for Route {
    type Error = anyhow::Error;
    fn try_from(config: RouteConfig) -> Result<Self> {
        let endpoint = LoopbackEndpoint::try_from(config.endpoint.as_str())?;
        if !config.session_id.starts_with("ses_")
            || config.session_id.len() <= 4
            || config.session_id.len() > 128
            || !config
                .session_id
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'_')
        {
            bail!("invalid OpenCode session ID; use the server's ses_ ID, not a Telephone UUID");
        }
        if config.directory.len() > 4096
            || !Path::new(&config.directory).is_absolute()
            || config.directory.chars().any(char::is_control)
            || !config.credentials.is_absolute()
        {
            bail!("OpenCode directory and credential file must be absolute paths; directory must be bounded and contain no controls");
        }
        Ok(Self {
            endpoint,
            session_id: config.session_id,
            directory: config.directory,
            credentials: config.credentials,
        })
    }
}

impl From<Route> for RouteConfig {
    fn from(route: Route) -> Self {
        Self {
            endpoint: format!("http://{}", route.endpoint.0),
            session_id: route.session_id,
            directory: route.directory,
            credentials: route.credentials,
        }
    }
}

impl Route {
    pub(super) fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.endpoint.0)
    }
    pub(super) fn session_id(&self) -> &str {
        &self.session_id
    }
    pub(super) fn directory(&self) -> &str {
        &self.directory
    }
    pub(super) fn credentials(&self) -> &Path {
        &self.credentials
    }
}
