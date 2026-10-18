//! Routing for programs started by Codex, including per-thread choices made by the agent.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::PoisonError;

use base64::Engine as _;
use codex_required_proxy::DirectList;
use codex_required_proxy::RequiredProxy;
use codex_required_proxy::RequiredProxyError;
use codex_required_proxy::THREAD_IDENTITY_PREFIX;
use serde::Deserialize;
use serde::Serialize;

/// Where an agent thread's commands go when no other rule applies.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DefaultRoute {
    #[default]
    Proxy,
    Direct,
}

/// A thread's routing, as reported by the control endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadRouting {
    pub thread: String,
    pub default: DefaultRoute,
    /// Destinations the agent sent direct for this thread.
    pub direct: Vec<String>,
    /// Destinations that are always direct: loopback and the user's `[proxy] direct`.
    pub always_direct: Vec<String>,
    /// Registrable domains that always use the upstream proxy.
    pub always_proxied: Vec<String>,
}

/// A change to a thread's routing.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoutingUpdate {
    pub default: Option<DefaultRoute>,
    #[serde(default)]
    pub add: Vec<String>,
    #[serde(default)]
    pub remove: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Identity {
    /// Codex itself and the programs it starts outside agent threads, such as MCP servers.
    Codex,
    Thread(String),
}

impl Identity {
    /// Clients without a recognizable identity get Codex's fixed routing.
    pub(crate) fn from_proxy_authorization(value: Option<&str>) -> Self {
        let username = value
            .and_then(|value| value.strip_prefix("Basic "))
            .and_then(|encoded| {
                base64::engine::general_purpose::STANDARD
                    .decode(encoded.trim())
                    .ok()
            })
            .and_then(|decoded| String::from_utf8(decoded).ok())
            .map(|credentials| {
                credentials
                    .split(':')
                    .next()
                    .unwrap_or_default()
                    .to_string()
            });
        match username
            .as_deref()
            .and_then(|user| user.strip_prefix(THREAD_IDENTITY_PREFIX))
        {
            Some(thread) if !thread.is_empty() => Self::Thread(thread.to_string()),
            _ => Self::Codex,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Route {
    Upstream,
    Direct,
}

/// Registries that package managers download from; direct for every new agent thread.
pub const PACKAGE_REGISTRIES: &[&str] = &[
    "registry.npmjs.org",
    "registry.yarnpkg.com",
    "pypi.org",
    "files.pythonhosted.org",
    "crates.io",
    "index.crates.io",
    "static.crates.io",
    "proxy.golang.org",
    "sum.golang.org",
    "formulae.brew.sh",
    "ghcr.io",
    "pkg-containers.githubusercontent.com",
    "rubygems.org",
    "repo.maven.apache.org",
];

#[derive(Debug, Clone)]
struct ThreadPolicy {
    default: DefaultRoute,
    entries: Vec<String>,
    direct: DirectList,
}

/// The routing of a thread that has not changed it.
fn initial_policy() -> &'static ThreadPolicy {
    static INITIAL: OnceLock<ThreadPolicy> = OnceLock::new();
    INITIAL.get_or_init(|| {
        let registries: Vec<String> = PACKAGE_REGISTRIES.iter().map(ToString::to_string).collect();
        let direct = DirectList::parse(&registries).unwrap_or_default();
        ThreadPolicy {
            default: DefaultRoute::Proxy,
            entries: direct.entries(),
            direct,
        }
    })
}

/// Per-thread routing chosen by the agent, kept for the lifetime of this process.
#[derive(Debug, Default)]
pub(crate) struct Policies {
    threads: Mutex<HashMap<String, ThreadPolicy>>,
}

impl Policies {
    pub(crate) fn route(&self, required: &RequiredProxy, identity: &Identity, host: &str) -> Route {
        if codex_required_proxy::is_pinned_host(host) {
            return Route::Upstream;
        }
        if required.bypasses_host(host) {
            return Route::Direct;
        }
        let Identity::Thread(thread) = identity else {
            return Route::Upstream;
        };
        let threads = self.threads.lock().unwrap_or_else(PoisonError::into_inner);
        let policy = threads.get(thread).unwrap_or_else(|| initial_policy());
        if policy.default == DefaultRoute::Direct || policy.direct.matches(host) {
            Route::Direct
        } else {
            Route::Upstream
        }
    }

    pub(crate) fn update(
        &self,
        thread: &str,
        update: RoutingUpdate,
    ) -> Result<(), RequiredProxyError> {
        let mut threads = self.threads.lock().unwrap_or_else(PoisonError::into_inner);
        let policy = threads
            .entry(thread.to_string())
            .or_insert_with(|| initial_policy().clone());
        let mut entries: Vec<String> = policy
            .entries
            .iter()
            .filter(|entry| !update.remove.contains(entry))
            .cloned()
            .collect();
        for entry in update.add {
            if !entries.contains(&entry) {
                entries.push(entry);
            }
        }
        let direct = DirectList::parse(&entries)?;
        policy.direct = direct;
        policy.entries = policy.direct.entries();
        if let Some(default) = update.default {
            policy.default = default;
        }
        Ok(())
    }

    pub(crate) fn routing(&self, required: &RequiredProxy, thread: &str) -> ThreadRouting {
        let threads = self.threads.lock().unwrap_or_else(PoisonError::into_inner);
        let policy = threads.get(thread).unwrap_or_else(|| initial_policy());
        ThreadRouting {
            thread: thread.to_string(),
            default: policy.default,
            direct: policy.entries.clone(),
            always_direct: required
                .no_proxy()
                .split(',')
                .map(ToString::to_string)
                .collect(),
            always_proxied: codex_required_proxy::PINNED_DOMAINS
                .iter()
                .map(ToString::to_string)
                .collect(),
        }
    }
}

#[cfg(test)]
#[path = "route_tests.rs"]
mod tests;
