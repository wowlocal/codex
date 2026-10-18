//! `codex proxy`: manage the required upstream proxy in the OS credential store.

use std::io::IsTerminal;
use std::io::Read;
use std::io::Write;

use anyhow::Context;
use codex_core::config::edit::ConfigEdit;
use codex_core::config::edit::ConfigEditsBuilder;
use codex_protocol::shell_environment::CODEX_SESSION_ID_ENV_VAR;
use codex_protocol::shell_environment::CODEX_THREAD_ID_ENV_VAR;
use codex_required_proxy::RequiredProxy;

use crate::proxy_route_cmd;
use crate::proxy_route_cmd::RouteCommand;
use crate::proxy_skill;
use crate::startup_env;
use crate::startup_env::StartupProxyConfig;
use crate::startup_proxy::Enforcement;
use crate::startup_proxy::ProxyStore;

#[derive(Debug, clap::Parser)]
pub(crate) struct ProxyCommand {
    #[command(subcommand)]
    action: ProxyAction,
}

#[derive(Debug, clap::Subcommand)]
enum ProxyAction {
    /// Store the upstream proxy URL, read from stdin (hidden on a terminal).
    Set,
    /// Show the upstream proxy without credentials and the destinations that bypass it.
    Status,
    /// Remove the upstream proxy. Codex refuses to start until another one is stored.
    Clear,
    /// Move proxy variables from `[env]` in config.toml into the system credential store.
    Import,
    /// For agents: show or change how this thread's commands reach the network.
    Route(RouteCommand),
}

pub(crate) fn run(command: ProxyCommand, startup: &StartupProxyConfig) -> anyhow::Result<()> {
    let store = ProxyStore::for_codex_home()?;
    match command.action {
        ProxyAction::Route(route) => return proxy_route_cmd::run(route),
        ProxyAction::Set => {
            refuse_in_agent_session("set")?;
            let url = read_proxy_url()?;
            let proxy = RequiredProxy::new(&url, &startup.direct)?;
            store.save(proxy.url())?;
            println!(
                "Stored the upstream proxy {} for {}.",
                proxy.redacted(),
                store.codex_home().display()
            );
            install_skill(&store)?;
        }
        ProxyAction::Status => print_status(&store, startup),
        ProxyAction::Clear => {
            refuse_in_agent_session("clear")?;
            if store.delete()? {
                println!(
                    "Removed the upstream proxy for {}. Codex refuses to start until another one \
                     is stored (kill switch).",
                    store.codex_home().display()
                );
            } else {
                println!(
                    "No upstream proxy was stored for {}.",
                    store.codex_home().display()
                );
            }
        }
        ProxyAction::Import => {
            refuse_in_agent_session("import")?;
            import(&store, startup)?;
            install_skill(&store)?;
        }
    }
    Ok(())
}

fn install_skill(store: &ProxyStore) -> anyhow::Result<()> {
    let path = proxy_skill::install(store.codex_home())?;
    println!(
        "Installed the proxy routing skill for agents: {}",
        path.display()
    );
    Ok(())
}

/// Commands started by an agent must not redirect or remove Codex's proxy.
fn refuse_in_agent_session(action: &str) -> anyhow::Result<()> {
    let in_agent_session = [CODEX_THREAD_ID_ENV_VAR, CODEX_SESSION_ID_ENV_VAR]
        .iter()
        .any(|key| std::env::var_os(key).is_some());
    anyhow::ensure!(
        !in_agent_session,
        "`codex proxy {action}` cannot run inside a Codex agent session; run it in your own terminal"
    );
    Ok(())
}

fn read_proxy_url() -> anyhow::Result<String> {
    let url = if std::io::stdin().is_terminal() {
        read_hidden_line("Upstream proxy URL (input hidden): ")?
    } else {
        let mut url = String::new();
        std::io::stdin().read_to_string(&mut url)?;
        url
    };
    let url = url.trim().to_string();
    anyhow::ensure!(!url.is_empty(), "no upstream proxy URL was provided");
    Ok(url)
}

fn read_hidden_line(prompt: &str) -> anyhow::Result<String> {
    use crossterm::event::Event;
    use crossterm::event::KeyCode;
    use crossterm::event::KeyEventKind;
    use crossterm::event::KeyModifiers;

    eprint!("{prompt}");
    std::io::stderr().flush()?;
    crossterm::terminal::enable_raw_mode()?;
    let read_line = || -> anyhow::Result<String> {
        let mut line = String::new();
        loop {
            match crossterm::event::read()? {
                Event::Key(key) if key.kind != KeyEventKind::Release => match key.code {
                    KeyCode::Enter => return Ok(line),
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        anyhow::bail!("cancelled")
                    }
                    KeyCode::Char(character) => line.push(character),
                    KeyCode::Backspace => {
                        line.pop();
                    }
                    _ => {}
                },
                Event::Paste(text) => line.push_str(&text),
                _ => {}
            }
        }
    };
    let line = read_line();
    crossterm::terminal::disable_raw_mode()?;
    eprintln!();
    line
}

fn print_status(store: &ProxyStore, startup: &StartupProxyConfig) {
    let home = store.codex_home().display();
    match store.load() {
        Ok(Some(url)) => match RequiredProxy::new(&url, &startup.direct) {
            Ok(proxy) => {
                println!(
                    "Upstream proxy: {} (system credential store)",
                    proxy.redacted()
                );
                println!("Codex home: {home}");
                println!("Direct: {}", proxy.no_proxy().replace(',', ", "));
            }
            Err(error) => println!("The stored upstream proxy for {home} is unusable: {error}"),
        },
        Ok(None) => println!("No upstream proxy is stored for {home}."),
        Err(error) => println!("Cannot read the system credential store: {error}"),
    }
    let kill_switch = match Enforcement::from_env() {
        Enforcement::Required => "on",
        Enforcement::AllowDirect => "off without a stored proxy (debug build or override)",
    };
    println!("Kill switch: {kill_switch}");
    if !startup.env_proxy_keys.is_empty() {
        println!(
            "[env] in {} still sets {}; run `codex proxy import`.",
            startup.path.display(),
            startup.env_proxy_keys.join(", ")
        );
    }
}

fn import(store: &ProxyStore, startup: &StartupProxyConfig) -> anyhow::Result<()> {
    let contents = std::fs::read_to_string(&startup.path)
        .with_context(|| format!("cannot read {}", startup.path.display()))?;
    let plan = import_plan(&contents, &startup.direct)?;
    store.save(plan.proxy.url())?;
    let mut edits: Vec<ConfigEdit> = plan
        .env_keys
        .iter()
        .map(|key| ConfigEdit::ClearPath {
            segments: vec!["env".to_string(), key.clone()],
        })
        .collect();
    if plan.empties_env {
        edits.push(ConfigEdit::ClearPath {
            segments: vec!["env".to_string()],
        });
    }
    if !plan.direct.is_empty() {
        edits.push(ConfigEdit::SetPath {
            segments: vec!["proxy".to_string(), "direct".to_string()],
            value: toml_edit::value(
                plan.direct
                    .iter()
                    .map(String::as_str)
                    .collect::<toml_edit::Array>(),
            ),
        });
    }
    ConfigEditsBuilder::for_config_path(&startup.path)
        .with_edits(edits)
        .apply_blocking()
        .context("the proxy is stored, but config.toml could not be updated")?;
    println!(
        "Moved the upstream proxy {} into the system credential store and removed {} from [env] \
         in {}.",
        plan.proxy.redacted(),
        plan.env_keys.join(", "),
        startup.path.display()
    );
    println!("Direct: {}", plan.proxy.no_proxy().replace(',', ", "));
    Ok(())
}

/// What `codex proxy import` moves out of `[env]`.
#[derive(Debug, PartialEq, Eq)]
struct ImportPlan {
    proxy: RequiredProxy,
    /// The complete `[proxy] direct` list: existing entries followed by imported `NO_PROXY` ones.
    direct: Vec<String>,
    env_keys: Vec<String>,
    empties_env: bool,
}

fn import_plan(contents: &str, existing_direct: &[String]) -> anyhow::Result<ImportPlan> {
    // Parser diagnostics would quote the offending line, which can hold credentials.
    let config: toml::Table =
        toml::from_str(contents).map_err(|_| anyhow::anyhow!("config.toml is not valid TOML"))?;
    let env = config
        .get("env")
        .and_then(toml::Value::as_table)
        .cloned()
        .unwrap_or_default();
    let env_keys: Vec<String> = env
        .keys()
        .filter(|key| startup_env::is_proxy_key(key))
        .cloned()
        .collect();
    anyhow::ensure!(
        !env_keys.is_empty(),
        "[env] in config.toml does not set a proxy"
    );

    let mut urls = std::collections::BTreeSet::new();
    let mut direct = existing_direct.to_vec();
    for key in &env_keys {
        let value = env[key]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("[env] {key} is not a string"))?;
        match key.to_ascii_uppercase().as_str() {
            "HTTP_PROXY" | "HTTPS_PROXY" | "ALL_PROXY" => {
                urls.insert(codex_required_proxy::normalize_proxy_url(value)?);
            }
            "NO_PROXY" => {
                for entry in value.split(',').map(str::trim) {
                    let implicit = codex_required_proxy::IMPLICIT_DIRECT.contains(&entry);
                    if !entry.is_empty() && !implicit && !direct.iter().any(|known| known == entry)
                    {
                        direct.push(entry.to_string());
                    }
                }
            }
            _ => {}
        }
    }
    let mut urls = urls.into_iter();
    let (Some(url), None) = (urls.next(), urls.next()) else {
        anyhow::bail!(
            "[env] must set exactly one proxy URL in HTTP_PROXY, HTTPS_PROXY or ALL_PROXY; \
             store the proxy with `codex proxy set` instead"
        );
    };
    Ok(ImportPlan {
        proxy: RequiredProxy::new(&url, &direct)?,
        direct,
        empties_env: env.len() == env_keys.len(),
        env_keys,
    })
}

#[cfg(test)]
#[path = "proxy_cmd_tests.rs"]
mod tests;
