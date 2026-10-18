//! Fork kill switch: install the upstream proxy from the OS credential store before any network
//! client starts, then follow `codex proxy` changes without restarting long-lived processes.

use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use codex_keyring_store::CredentialStoreError;
use codex_keyring_store::DefaultKeyringStore;
use codex_keyring_store::KeyringStore;
use codex_required_proxy::Reloader;
use codex_required_proxy::RequiredProxy;

use crate::startup_env;
use crate::startup_env::StartupProxyConfig;

const KEYCHAIN_SERVICE: &str = "Codex Fork Proxy";
const ALLOW_DIRECT_ENV: &str = "CODEX_FORK_DANGEROUSLY_ALLOW_DIRECT";
const REQUIRE_PROXY_ENV: &str = "CODEX_FORK_REQUIRE_PROXY";
/// Touched by `codex proxy` so running processes reload the credential store.
const REVISION_FILE: &str = ".codex-fork-proxy-revision";
const RELOAD_INTERVAL: Duration = Duration::from_secs(2);

/// Whether a process may start without an upstream proxy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Enforcement {
    Required,
    AllowDirect,
}

impl Enforcement {
    /// Release builds always require the proxy unless explicitly overridden. Debug builds,
    /// including tests with temporary Codex homes, require it only when one is stored.
    pub(crate) fn from_env() -> Self {
        let enabled = |key| std::env::var_os(key).is_some_and(|value| value == "1");
        if enabled(ALLOW_DIRECT_ENV) || (cfg!(debug_assertions) && !enabled(REQUIRE_PROXY_ENV)) {
            Self::AllowDirect
        } else {
            Self::Required
        }
    }
}

/// Where one Codex home keeps its upstream proxy: each home has its own credential.
pub(crate) struct ProxyStore {
    codex_home: PathBuf,
    account: String,
}

impl ProxyStore {
    pub(crate) fn for_codex_home() -> anyhow::Result<Self> {
        let codex_home = codex_core::config::find_codex_home()?.to_path_buf();
        let account = std::fs::canonicalize(&codex_home)
            .unwrap_or_else(|_| codex_home.clone())
            .display()
            .to_string();
        Ok(Self {
            codex_home,
            account,
        })
    }

    pub(crate) fn codex_home(&self) -> &Path {
        &self.codex_home
    }

    pub(crate) fn load(&self) -> Result<Option<String>, CredentialStoreError> {
        DefaultKeyringStore.load(KEYCHAIN_SERVICE, &self.account)
    }

    /// Stores the URL, confirms it reads back, and tells running processes to reload.
    pub(crate) fn save(&self, url: &str) -> anyhow::Result<()> {
        DefaultKeyringStore.save(KEYCHAIN_SERVICE, &self.account, url)?;
        anyhow::ensure!(
            self.load()?.as_deref() == Some(url),
            "the system credential store did not return the saved upstream proxy"
        );
        self.notify_running_processes()
    }

    pub(crate) fn delete(&self) -> anyhow::Result<bool> {
        let deleted = DefaultKeyringStore.delete(KEYCHAIN_SERVICE, &self.account)?;
        self.notify_running_processes()?;
        Ok(deleted)
    }

    fn revision_path(&self) -> PathBuf {
        self.codex_home.join(REVISION_FILE)
    }

    fn notify_running_processes(&self) -> anyhow::Result<()> {
        let revision = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        std::fs::write(self.revision_path(), revision.to_string())?;
        Ok(())
    }
}

/// Installs the required proxy, or refuses to start without one (kill switch).
pub(crate) fn install(startup: &StartupProxyConfig) -> anyhow::Result<()> {
    anyhow::ensure!(
        startup.env_proxy_keys.is_empty(),
        "{} sets {} in [env], which no longer configures the proxy. Run `codex proxy import` \
         to move the proxy into the system credential store.",
        startup.path.display(),
        startup.env_proxy_keys.join(", ")
    );
    let store = ProxyStore::for_codex_home()?;
    let url = match (store.load(), Enforcement::from_env()) {
        (Ok(Some(url)), _) => url,
        (Ok(None), Enforcement::AllowDirect) => return Ok(()),
        (Ok(None), Enforcement::Required) => anyhow::bail!(
            "No upstream proxy is stored for {}. Run `codex proxy import` to move it from [env], \
             or `codex proxy set`. Refusing to start without the proxy (kill switch).",
            store.codex_home().display()
        ),
        (Err(error), Enforcement::AllowDirect) => {
            eprintln!(
                "warning: cannot read the upstream proxy from the system credential store \
                 ({error}); continuing without it"
            );
            return Ok(());
        }
        (Err(error), Enforcement::Required) => anyhow::bail!(
            "Cannot read the upstream proxy from the system credential store ({error}). \
             Refusing to start without the proxy (kill switch)."
        ),
    };
    let proxy = RequiredProxy::new(&url, &startup.direct)?;
    let watched = vec![store.revision_path(), startup.path.clone()];
    let config_path = startup.path.clone();
    let reloader = Reloader::new(watched, RELOAD_INTERVAL, move || {
        reload(&store, &config_path)
    });
    codex_required_proxy::install(proxy, Some(reloader))?;

    // Programs started by Codex reach the network through a loopback proxy without credentials.
    let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))?;
    listener.set_nonblocking(true)?;
    let address = codex_command_proxy::spawn(
        tokio::net::TcpListener::from_std(listener)?,
        codex_required_proxy::current,
    )?;
    codex_required_proxy::set_command_proxy(address)?;
    Ok(())
}

fn reload(store: &ProxyStore, config_path: &Path) -> Result<RequiredProxy, String> {
    let direct = startup_env::read_direct(config_path).map_err(|error| error.to_string())?;
    let url = store
        .load()
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "the upstream proxy was removed from the credential store".to_string())?;
    RequiredProxy::new(&url, &direct).map_err(|error| error.to_string())
}
