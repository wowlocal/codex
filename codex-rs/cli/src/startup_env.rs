//! Load the fork's process environment before starting any runtime or network client.

use std::collections::BTreeMap;
use std::ffi::OsString;

use anyhow::Context;
use serde::Deserialize;

#[derive(Default, Deserialize)]
struct StartupConfig {
    #[serde(default)]
    env: BTreeMap<String, String>,
}

/// Apply user configuration while the CLI is still single threaded.
///
/// # Safety
/// Must run before threads or the async runtime are started.
pub(super) unsafe fn apply() -> anyhow::Result<()> {
    let explicit_path = std::env::var_os("CODEX_FORK_CONFIG");
    let path = match explicit_path.as_ref() {
        Some(path) => std::path::PathBuf::from(path),
        None => codex_core::config::find_codex_home()?
            .join("config.toml")
            .to_path_buf(),
    };
    let contents = match std::fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && explicit_path.is_none() => {
            String::new()
        }
        Err(error) => return Err(error).context("cannot read startup environment configuration"),
    };
    let values = resolve(&contents, std::env::vars_os().collect())?;
    for (key, value) in values {
        if std::env::var_os(&key).as_ref() == Some(&value) {
            continue;
        }
        // SAFETY: upheld by the single-threaded caller, before arg0 dispatch.
        unsafe { std::env::set_var(key, value) };
    }
    Ok(())
}

fn resolve(
    contents: &str,
    mut inherited: BTreeMap<OsString, OsString>,
) -> anyhow::Result<BTreeMap<OsString, OsString>> {
    // Do not include parser diagnostics: the source line can contain proxy credentials.
    // Unparseable TOML cannot safely be assumed to contain no configured proxy.
    let config: StartupConfig = toml::from_str(contents).map_err(|_| {
        anyhow::anyhow!("Error loading config.toml: invalid startup [env] configuration")
    })?;
    for (key, value) in config.env {
        anyhow::ensure!(
            !key.is_empty() && !key.contains(['=', '\0']) && !value.contains('\0'),
            "invalid startup environment entry"
        );
        // Either spelling in the inherited environment takes precedence over config.
        let proxy_key = matches!(
            key.to_ascii_uppercase().as_str(),
            "HTTP_PROXY" | "HTTPS_PROXY" | "ALL_PROXY" | "NO_PROXY"
        );
        if proxy_key
            && (inherited.contains_key(&OsString::from(key.to_ascii_lowercase()))
                || inherited.contains_key(&OsString::from(key.to_ascii_uppercase())))
        {
            continue;
        }
        inherited.entry(key.into()).or_insert_with(|| value.into());
    }
    let mut proxies = BTreeMap::new();
    for key in ["HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "NO_PROXY"] {
        let lower = key.to_ascii_lowercase();
        let value = inherited
            .get(&OsString::from(key))
            .or_else(|| inherited.get(&OsString::from(&lower)));
        let Some(value) = value else { continue };
        let value = value
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("invalid {key} encoding"))?;
        if value.trim().is_empty() {
            // Empty uppercase values still mask lowercase ones, especially NO_PROXY="*".
            inherited.insert(key.into(), OsString::new());
            inherited.insert(lower.into(), OsString::new());
            continue;
        }
        let value = if key == "NO_PROXY" {
            value.to_string()
        } else {
            let value = if value.contains("://") {
                value.trim().to_string()
            } else {
                format!("http://{}", value.trim())
            };
            let url = url::Url::parse(&value).map_err(|_| anyhow::anyhow!("invalid {key} URL"))?;
            anyhow::ensure!(
                matches!(url.scheme(), "http" | "https" | "socks5" | "socks5h")
                    && url.host_str().is_some()
                    && matches!(url.path(), "" | "/")
                    && url.query().is_none()
                    && url.fragment().is_none(),
                "unsupported {key} URL"
            );
            // Canonical ASCII URLs also keep legacy URI-based clients from silently ignoring
            // otherwise valid proxy credentials containing non-ASCII characters.
            url.as_str().trim_end_matches('/').to_string()
        };
        proxies.insert(key, value);
    }
    // A single configured proxy covers both HTTP and HTTPS, including legacy clients.
    for (key, alternate) in [("HTTP_PROXY", "HTTPS_PROXY"), ("HTTPS_PROXY", "HTTP_PROXY")] {
        if !proxies.contains_key(key)
            && let Some(value) = proxies
                .get("ALL_PROXY")
                .or_else(|| proxies.get(alternate))
                .cloned()
        {
            proxies.insert(key, value);
        }
    }
    for (key, value) in proxies {
        inherited.insert(key.into(), value.clone().into());
        inherited.insert(key.to_ascii_lowercase().into(), value.into());
    }
    Ok(inherited)
}

#[cfg(test)]
#[path = "startup_env_tests.rs"]
mod tests;
