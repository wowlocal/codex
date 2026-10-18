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
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::PoisonError;
use std::sync::RwLock;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;

use direct::DirectEntry;

pub use direct::IMPLICIT_DIRECT;
pub use direct::PINNED_DOMAINS;
pub use direct::is_pinned_host;

static STATE: OnceLock<State> = OnceLock::new();

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
}

/// A validated upstream proxy and the destinations that bypass it.
///
/// `Debug` and [`Self::redacted`] never expose credentials. [`Self::url`] does, and must only be
/// handed to a transport's proxy configuration.
#[derive(Clone, PartialEq, Eq)]
pub struct RequiredProxy {
    url: String,
    no_proxy: String,
    direct: Vec<DirectEntry>,
}

impl RequiredProxy {
    /// Validates the proxy URL and the configured direct destinations.
    pub fn new(url: &str, direct: &[String]) -> Result<Self, RequiredProxyError> {
        let url = normalize_proxy_url(url)?;
        let mut entries = DirectEntry::loopback();
        let mut no_proxy: Vec<String> = entries.iter().map(DirectEntry::to_string).collect();
        for entry in direct {
            let (parsed, normalized) = DirectEntry::parse(entry)?;
            if !no_proxy.contains(&normalized) {
                entries.push(parsed);
                no_proxy.push(normalized);
            }
        }
        Ok(Self {
            url,
            no_proxy: no_proxy.join(","),
            direct: entries,
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
        let host = host.trim_start_matches('[').trim_end_matches(']');
        !is_pinned_host(host) && self.direct.iter().any(|entry| entry.matches(host))
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

/// Whether a value is a URL that carries a username or password.
pub fn carries_credentials(value: &str) -> bool {
    matches!(redact_url_credentials(value), Cow::Owned(_))
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
