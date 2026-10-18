//! Load the fork's startup configuration before starting any runtime or network client.
//!
//! `[env]` entries become process environment variables. Proxy settings never do: the upstream
//! proxy lives in the OS credential store (see `startup_proxy`), and inherited proxy variables
//! that carry credentials are removed so child processes cannot observe them.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::Path;
use std::path::PathBuf;

use anyhow::Context;
use serde::Deserialize;

#[derive(Default, Deserialize)]
struct StartupConfig {
    #[serde(default)]
    env: BTreeMap<String, String>,
    #[serde(default)]
    proxy: ProxyTable,
}

#[derive(Default, Deserialize)]
struct ProxyTable {
    #[serde(default)]
    direct: Vec<String>,
}

/// Proxy routing read from the startup configuration.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct StartupProxyConfig {
    /// The configuration file that was read, which `codex proxy import` edits.
    pub(super) path: PathBuf,
    /// `[proxy] direct` entries.
    pub(super) direct: Vec<String>,
    /// `[env]` entries that configure a proxy and are therefore not exported.
    pub(super) env_proxy_keys: Vec<String>,
}

#[derive(Debug, Default, PartialEq, Eq)]
struct Resolved {
    set: Vec<(OsString, OsString)>,
    remove: Vec<OsString>,
    direct: Vec<String>,
    env_proxy_keys: Vec<String>,
}

/// Apply user configuration while the CLI is still single threaded.
///
/// # Safety
/// Must run before threads or the async runtime are started.
pub(super) unsafe fn apply() -> anyhow::Result<StartupProxyConfig> {
    let explicit_path = std::env::var_os("CODEX_FORK_CONFIG");
    let path = match explicit_path.as_ref() {
        Some(path) => PathBuf::from(path),
        None => codex_core::config::find_codex_home()?
            .join("config.toml")
            .to_path_buf(),
    };
    let contents = read(&path, /*missing_is_empty*/ explicit_path.is_none())?;
    let resolved = resolve(&contents, std::env::vars_os())?;
    for key in resolved.remove {
        // SAFETY: upheld by the single-threaded caller, before arg0 dispatch.
        unsafe { std::env::remove_var(key) };
    }
    for (key, value) in resolved.set {
        // SAFETY: upheld by the single-threaded caller, before arg0 dispatch.
        unsafe { std::env::set_var(key, value) };
    }
    Ok(StartupProxyConfig {
        path,
        direct: resolved.direct,
        env_proxy_keys: resolved.env_proxy_keys,
    })
}

/// Reads `[proxy] direct` again, for processes that follow proxy changes while running.
pub(super) fn read_direct(path: &Path) -> anyhow::Result<Vec<String>> {
    let contents = read(path, /*missing_is_empty*/ true)?;
    Ok(resolve(&contents, std::iter::empty())?.direct)
}

/// Whether an environment variable configures a proxy.
pub(super) fn is_proxy_key(key: &str) -> bool {
    key.to_ascii_uppercase().contains("PROXY")
}

fn read(path: &Path, missing_is_empty: bool) -> anyhow::Result<String> {
    match std::fs::read_to_string(path) {
        Ok(contents) => Ok(contents),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && missing_is_empty => {
            Ok(String::new())
        }
        Err(error) => Err(error).context("cannot read startup environment configuration"),
    }
}

fn resolve(
    contents: &str,
    inherited: impl IntoIterator<Item = (OsString, OsString)>,
) -> anyhow::Result<Resolved> {
    // Do not include parser diagnostics: the source line can contain proxy credentials.
    // Unparseable TOML cannot safely be assumed to contain no proxy configuration.
    let config: StartupConfig = toml::from_str(contents).map_err(|_| {
        anyhow::anyhow!("Error loading config.toml: invalid startup [env] configuration")
    })?;
    let inherited: BTreeMap<OsString, OsString> = inherited.into_iter().collect();
    let mut resolved = Resolved {
        direct: config.proxy.direct,
        ..Resolved::default()
    };
    for (key, value) in config.env {
        anyhow::ensure!(
            !key.is_empty() && !key.contains(['=', '\0']) && !value.contains('\0'),
            "invalid startup environment entry"
        );
        if is_proxy_key(&key) {
            resolved.env_proxy_keys.push(key);
        } else if !inherited.contains_key(&OsString::from(&key)) {
            // Inherited variables take precedence over config.
            resolved.set.push((key.into(), value.into()));
        }
    }
    resolved.remove = inherited
        .into_iter()
        .filter(|(key, value)| {
            key.to_str().is_some_and(is_proxy_key)
                && value
                    .to_str()
                    .is_some_and(codex_required_proxy::carries_credentials)
        })
        .map(|(key, _)| key)
        .collect();
    Ok(resolved)
}

#[cfg(test)]
#[path = "startup_env_tests.rs"]
mod tests;
