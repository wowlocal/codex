//! The upstream proxy this fork requires for Codex's own network traffic.
//!
//! The CLI loads the proxy URL from the OS credential store and installs it here before any
//! network client starts. The URL never enters the process environment, so child processes,
//! shell snapshots, feedback reports and logs cannot observe its credentials. Network clients
//! consult [`current`] instead of `HTTP(S)_PROXY`; transports that cannot honor the proxy must
//! refuse to connect instead of falling back to a direct route.
//!
//! Long-lived processes such as the app-server daemon pick up proxy changes through an optional
//! [`Reloader`] without restarting. A failed reload keeps the previous proxy: once installed, the
//! process never loses its required proxy.

mod direct;

use std::borrow::Cow;
use std::fmt;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::PoisonError;
use std::sync::RwLock;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;

pub use direct::DirectList;
pub use direct::IMPLICIT_DIRECT;
pub use direct::PINNED_DOMAINS;
pub use direct::is_pinned_host;

static STATE: OnceLock<State> = OnceLock::new();
static COMMAND_PROXY: OnceLock<SocketAddr> = OnceLock::new();

/// Proxy username for programs started by Codex itself, such as MCP servers.
pub const CODEX_IDENTITY: &str = "codex";
/// Proxy username prefix for agent commands, followed by their thread ID.
pub const THREAD_IDENTITY_PREFIX: &str = "thread-";
/// Local helper handoff. This contains only a numeric loopback address, never upstream credentials.
pub const COMMAND_PROXY_ENV: &str = "CODEX_FORK_COMMAND_PROXY_V1";
// Some clients only send Basic authentication when both URL fields are nonempty.
// The loopback gateway uses only the username, so this fixed value is public.
const COMMAND_PROXY_IDENTITY_PASSWORD: &str = "codex-local";

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RequiredProxyError {
    /// The URL is never included: it can contain credentials.
    #[error("the upstream proxy URL is invalid")]
    InvalidUrl,
    #[error("the upstream proxy must be an http:// or https:// URL without a path or query")]
    UnsupportedUrl,
    #[error("invalid [proxy] direct entry `{entry}`: {reason}")]
    InvalidDirectEntry { entry: String, reason: &'static str },
    #[error("the upstream proxy was already installed")]
    AlreadyInstalled,
    #[error("the command proxy was already published")]
    CommandProxyAlreadySet,
    #[error("the required command proxy is unavailable; refusing an unproxied subprocess")]
    CommandProxyUnavailable,
    #[error("the parent command proxy must be a numeric loopback address with a nonzero port")]
    InvalidCommandProxy,
    #[error(
        "this standalone helper has no required proxy boundary; launch it through the Codex CLI"
    )]
    MissingHelperBoundary,
}

/// A validated upstream proxy and the destinations that bypass it.
///
/// `Debug` and [`Self::redacted`] never expose credentials. [`Self::url`] does, and must only be
/// handed to a transport's proxy configuration.
#[derive(Clone, PartialEq, Eq)]
pub struct RequiredProxy {
    url: String,
    no_proxy: String,
    direct: DirectList,
}

impl RequiredProxy {
    /// Validates the proxy URL and the configured direct destinations.
    pub fn new(url: &str, direct: &[String]) -> Result<Self, RequiredProxyError> {
        let url = normalize_proxy_url(url)?;
        let entries: Vec<String> = IMPLICIT_DIRECT
            .iter()
            .map(ToString::to_string)
            .chain(direct.iter().cloned())
            .collect();
        let direct = DirectList::parse(&entries)?;
        Ok(Self {
            url,
            no_proxy: direct.entries().join(","),
            direct,
        })
    }

    /// The proxy URL, including credentials.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// A `NO_PROXY`-style list of loopback and configured direct destinations.
    pub fn no_proxy(&self) -> &str {
        &self.no_proxy
    }

    /// Whether `host` (a domain or IP literal, brackets optional) connects directly.
    /// Pinned OpenAI destinations never do.
    pub fn bypasses_host(&self, host: &str) -> bool {
        self.direct.matches(host)
    }

    /// The proxy URL for a destination, or `None` when it connects directly.
    pub fn proxy_for(&self, destination: &url::Url) -> Option<&str> {
        match destination.host_str() {
            Some(host) if self.bypasses_host(host) => None,
            _ => Some(&self.url),
        }
    }

    /// The proxy origin without credentials, suitable for diagnostics.
    pub fn redacted(&self) -> String {
        redact_url_credentials(&self.url).into_owned()
    }
}

impl fmt::Debug for RequiredProxy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RequiredProxy")
            .field("url", &self.redacted())
            .field("no_proxy", &self.no_proxy)
            .finish()
    }
}

/// Reloads the required proxy when watched files change.
///
/// Checks run at most once per interval, on the threads that ask for [`current`]; concurrent
/// callers skip the check instead of waiting for it.
pub struct Reloader {
    watched: Vec<PathBuf>,
    interval: Duration,
    load: Box<dyn Fn() -> Result<RequiredProxy, String> + Send + Sync>,
    check: Mutex<ReloadCheck>,
}

struct ReloadCheck {
    next: Instant,
    stamps: Vec<Option<SystemTime>>,
}

impl Reloader {
    /// `load` must return the proxy that the watched files currently describe.
    pub fn new(
        watched: Vec<PathBuf>,
        interval: Duration,
        load: impl Fn() -> Result<RequiredProxy, String> + Send + Sync + 'static,
    ) -> Self {
        let stamps = modification_times(&watched);
        Self {
            watched,
            interval,
            load: Box::new(load),
            check: Mutex::new(ReloadCheck {
                next: Instant::now() + interval,
                stamps,
            }),
        }
    }

    fn refresh(&self, current: &RwLock<Arc<RequiredProxy>>) {
        let Ok(mut check) = self.check.try_lock() else {
            return;
        };
        let now = Instant::now();
        if now < check.next {
            return;
        }
        check.next = now + self.interval;
        let stamps = modification_times(&self.watched);
        if stamps == check.stamps {
            return;
        }
        check.stamps = stamps;
        match (self.load)() {
            Ok(proxy) => {
                *current.write().unwrap_or_else(PoisonError::into_inner) = Arc::new(proxy);
                tracing::info!("reloaded the required upstream proxy");
            }
            Err(reason) => {
                tracing::warn!(%reason, "required proxy reload failed; keeping the previous proxy");
            }
        }
    }
}

struct State {
    current: RwLock<Arc<RequiredProxy>>,
    reloader: Option<Reloader>,
}

/// Installs the process-wide upstream proxy. Call once, before any network client starts.
pub fn install(proxy: RequiredProxy, reloader: Option<Reloader>) -> Result<(), RequiredProxyError> {
    STATE
        .set(State {
            current: RwLock::new(Arc::new(proxy)),
            reloader,
        })
        .map_err(|_| RequiredProxyError::AlreadyInstalled)
}

/// Whether this process requires the upstream proxy for all of its own traffic.
pub fn is_active() -> bool {
    STATE.get().is_some()
}

/// The current upstream proxy, if this process requires one.
pub fn current() -> Option<Arc<RequiredProxy>> {
    let state = STATE.get()?;
    if let Some(reloader) = &state.reloader {
        reloader.refresh(&state.current);
    }
    Some(Arc::clone(
        &state.current.read().unwrap_or_else(PoisonError::into_inner),
    ))
}

/// Publishes the loopback proxy that programs started by Codex use. Call once.
pub fn set_command_proxy(address: SocketAddr) -> Result<(), RequiredProxyError> {
    if !address.ip().is_loopback() || address.port() == 0 {
        return Err(RequiredProxyError::InvalidCommandProxy);
    }
    COMMAND_PROXY
        .set(address)
        .map_err(|_| RequiredProxyError::CommandProxyAlreadySet)
}

/// The loopback gateway that handles upstream authentication for Codex's HTTP clients.
pub fn command_proxy_address() -> Option<SocketAddr> {
    COMMAND_PROXY.get().copied()
}

const COMMAND_PROXY_URL_KEYS: &[&str] = &[
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
    "FTP_PROXY",
    "WS_PROXY",
    "WSS_PROXY",
    "GRPC_PROXY",
    "PIP_PROXY",
    "NPM_CONFIG_PROXY",
    "NPM_CONFIG_HTTPS_PROXY",
    "YARN_PROXY",
    "YARN_HTTP_PROXY",
    "YARN_HTTPS_PROXY",
    "GLOBAL_AGENT_HTTP_PROXY",
    "GLOBAL_AGENT_HTTPS_PROXY",
];
const COMMAND_NO_PROXY_KEYS: &[&str] = &["NO_PROXY", "GLOBAL_AGENT_NO_PROXY", "NPM_CONFIG_NOPROXY"];

/// Proxy environment for a program started by Codex, or nothing without a command proxy.
///
/// Agent commands pass their thread ID so the agent can adjust that thread's routing; other
/// programs, such as MCP servers, get Codex's fixed routing. The URLs carry no credentials.
pub fn command_proxy_env(thread_id: Option<&str>) -> Vec<(String, String)> {
    let address = COMMAND_PROXY.get().copied().or_else(|| {
        // Legacy callers cannot return an error. An unavailable gateway must still fail closed.
        is_active().then_some(SocketAddr::from(([127, 0, 0, 1], 0)))
    });
    let Some(address) = address else {
        return Vec::new();
    };
    let identity = match thread_id {
        Some(thread_id) => format!("{THREAD_IDENTITY_PREFIX}{thread_id}"),
        None => CODEX_IDENTITY.to_string(),
    };
    let url = command_proxy_url(&identity, address);
    let no_proxy = IMPLICIT_DIRECT.join(",");
    let mut env: Vec<(String, String)> = Vec::new();
    for key in COMMAND_PROXY_URL_KEYS {
        env.push((key.to_string(), url.clone()));
        env.push((key.to_ascii_lowercase(), url.clone()));
    }
    for key in COMMAND_NO_PROXY_KEYS {
        env.push((key.to_string(), no_proxy.clone()));
        env.push((key.to_ascii_lowercase(), no_proxy.clone()));
    }
    // Node.js clients honor proxy variables only when this is set.
    env.push(("NODE_USE_ENV_PROXY".to_string(), "1".to_string()));
    if is_active() {
        env.push((COMMAND_PROXY_ENV.to_string(), address.to_string()));
    }
    env
}

/// Proxy boundary for Codex-owned subprocesses that can report startup errors.
pub fn command_proxy_env_checked(
    thread_id: Option<&str>,
) -> Result<Vec<(String, String)>, RequiredProxyError> {
    if is_active() && COMMAND_PROXY.get().is_none() {
        return Err(RequiredProxyError::CommandProxyUnavailable);
    }
    Ok(command_proxy_env(thread_id))
}

/// Environment keys owned by the command-proxy boundary, case-insensitively on every platform.
fn command_proxy_variable_names() -> impl Iterator<Item = &'static str> {
    COMMAND_PROXY_URL_KEYS
        .iter()
        .chain(COMMAND_NO_PROXY_KEYS)
        .copied()
}

/// The exact environment variable spellings that can be exported by the command proxy.
pub fn command_proxy_env_keys() -> impl Iterator<Item = String> {
    command_proxy_variable_names()
        .flat_map(|key| [key.to_string(), key.to_ascii_lowercase()])
        .chain(["NODE_USE_ENV_PROXY", COMMAND_PROXY_ENV].map(ToString::to_string))
}

pub fn is_command_proxy_env_key(name: &str) -> bool {
    command_proxy_variable_names()
        .chain(["NODE_USE_ENV_PROXY", COMMAND_PROXY_ENV])
        .any(|key| name.eq_ignore_ascii_case(key))
}

fn command_proxy_url(identity: &str, address: SocketAddr) -> String {
    format!("http://{identity}:{COMMAND_PROXY_IDENTITY_PASSWORD}@{address}")
}

/// Adopt a local parent's credential-free gateway without reading its Keychain item.
pub fn bootstrap_local_helper_from_env() -> Result<bool, RequiredProxyError> {
    if is_active() {
        return Ok(true);
    }
    let Some(value) = std::env::var_os(COMMAND_PROXY_ENV) else {
        return Ok(false);
    };
    let address: SocketAddr = value
        .to_str()
        .and_then(|value| value.parse().ok())
        .filter(|address: &SocketAddr| address.ip().is_loopback() && address.port() != 0)
        .ok_or(RequiredProxyError::InvalidCommandProxy)?;
    if COMMAND_PROXY
        .get()
        .is_some_and(|current| *current != address)
    {
        return Err(RequiredProxyError::CommandProxyAlreadySet);
    }
    install(
        RequiredProxy::new(&command_proxy_url(CODEX_IDENTITY, address), &[])?,
        None,
    )?;
    if COMMAND_PROXY.get().is_none() {
        set_command_proxy(address)?;
    }
    Ok(true)
}

/// Release helper entrypoints may delegate to a local parent, never silently select direct.
pub fn require_local_helper_proxy() -> Result<(), RequiredProxyError> {
    if bootstrap_local_helper_from_env()? {
        return Ok(());
    }
    let enabled = |key| std::env::var_os(key).is_some_and(|value| value == "1");
    if enabled("CODEX_FORK_DANGEROUSLY_ALLOW_DIRECT")
        || (cfg!(debug_assertions) && !enabled("CODEX_FORK_REQUIRE_PROXY"))
    {
        Ok(())
    } else {
        Err(RequiredProxyError::MissingHelperBoundary)
    }
}

/// Validates a proxy URL and returns its canonical form.
pub fn normalize_proxy_url(value: &str) -> Result<String, RequiredProxyError> {
    let value = with_default_scheme(value.trim());
    let url = url::Url::parse(&value).map_err(|_| RequiredProxyError::InvalidUrl)?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !matches!(url.path(), "" | "/")
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(RequiredProxyError::UnsupportedUrl);
    }
    // Canonical ASCII URLs keep URI-based clients from rejecting non-ASCII credentials.
    Ok(url.as_str().trim_end_matches('/').to_string())
}

/// Whether a value is a proxy URL whose userinfo may be a secret.
///
/// Legacy passwordless loopback identities and Codex's fixed loopback marker are public.
pub fn carries_credentials(value: &str) -> bool {
    let Ok(url) = url::Url::parse(&with_default_scheme(value.trim())) else {
        return false;
    };
    let loopback = matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
    let identity = url.username();
    let command_proxy_identity = identity == CODEX_IDENTITY
        || identity
            .strip_prefix(THREAD_IDENTITY_PREFIX)
            .is_some_and(|thread| !thread.is_empty());
    let gateway_address = value
        .trim()
        .rsplit_once('@')
        .and_then(|(_, authority)| authority.parse::<SocketAddr>().ok());
    if command_proxy_identity
        && let Some(address) = gateway_address
        && address.ip().is_loopback()
        && value.trim() == command_proxy_url(identity, address)
    {
        return false;
    }
    url.password().is_some() || (!url.username().is_empty() && !loopback)
}

/// Replaces URL credentials with `[REDACTED]`; other values are returned unchanged.
pub fn redact_url_credentials(value: &str) -> Cow<'_, str> {
    let Ok(url) = url::Url::parse(&with_default_scheme(value.trim())) else {
        return Cow::Borrowed(value);
    };
    if url.username().is_empty() && url.password().is_none() {
        return Cow::Borrowed(value);
    }
    let host = url.host_str().unwrap_or_default();
    let port = url
        .port_or_known_default()
        .map(|port| format!(":{port}"))
        .unwrap_or_default();
    Cow::Owned(format!("{}://[REDACTED]@{host}{port}", url.scheme()))
}

fn with_default_scheme(value: &str) -> Cow<'_, str> {
    if value.contains("://") {
        Cow::Borrowed(value)
    } else {
        Cow::Owned(format!("http://{value}"))
    }
}

fn modification_times(paths: &[PathBuf]) -> Vec<Option<SystemTime>> {
    paths
        .iter()
        .map(|path| {
            std::fs::metadata(path)
                .and_then(|meta| meta.modified())
                .ok()
        })
        .collect()
}

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;
