//! `codex proxy route`: let an agent thread choose how its own commands reach the network.
//!
//! Requests go to the command proxy named by the calling command's `HTTP_PROXY`, whose
//! username identifies the thread. Codex's own traffic and MCP servers are unaffected.

use std::io::Read;
use std::io::Write;
use std::net::TcpStream;

use base64::Engine as _;
use codex_command_proxy::CONTROL_HOST;
use codex_command_proxy::DefaultRoute;
use codex_command_proxy::PACKAGE_REGISTRIES;
use codex_command_proxy::ROUTING_PATH;
use codex_command_proxy::RoutingUpdate;
use codex_command_proxy::ThreadRouting;
use codex_required_proxy::THREAD_IDENTITY_PREFIX;

#[derive(Debug, clap::Parser)]
pub(crate) struct RouteCommand {
    #[command(subcommand)]
    action: RouteAction,
}

#[derive(Debug, clap::Subcommand)]
enum RouteAction {
    /// Show how this thread's commands reach the network.
    Status,
    /// Route this thread's commands through the proxy or directly unless another rule applies.
    Default { route: RouteChoice },
    /// Connect these domains, private IP addresses or private CIDR ranges directly.
    Add {
        #[arg(required = true)]
        entries: Vec<String>,
    },
    /// Stop connecting these destinations directly.
    Remove {
        #[arg(required = true)]
        entries: Vec<String>,
    },
    /// Connect package registries (npm, PyPI, crates.io, Go, Homebrew and others) directly again.
    Packages,
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
enum RouteChoice {
    Proxy,
    Direct,
}

pub(crate) fn run(command: RouteCommand) -> anyhow::Result<()> {
    let update = match command.action {
        RouteAction::Status => None,
        RouteAction::Default { route } => Some(RoutingUpdate {
            default: Some(match route {
                RouteChoice::Proxy => DefaultRoute::Proxy,
                RouteChoice::Direct => DefaultRoute::Direct,
            }),
            ..RoutingUpdate::default()
        }),
        RouteAction::Add { entries } => Some(RoutingUpdate {
            add: entries,
            ..RoutingUpdate::default()
        }),
        RouteAction::Remove { entries } => Some(RoutingUpdate {
            remove: entries,
            ..RoutingUpdate::default()
        }),
        RouteAction::Packages => Some(RoutingUpdate {
            add: PACKAGE_REGISTRIES.iter().map(ToString::to_string).collect(),
            ..RoutingUpdate::default()
        }),
    };
    let routing = exchange(update.as_ref())?;
    let default = match routing.default {
        DefaultRoute::Proxy => "through the upstream proxy",
        DefaultRoute::Direct => "directly",
    };
    let direct = if routing.direct.is_empty() {
        "none".to_string()
    } else {
        routing.direct.join(", ")
    };
    println!("Commands in this thread connect {default} unless a rule below applies.");
    println!("Direct for this thread: {direct}");
    println!("Always direct: {}", routing.always_direct.join(", "));
    println!(
        "Always through the proxy: {}",
        routing.always_proxied.join(", ")
    );
    Ok(())
}

fn exchange(update: Option<&RoutingUpdate>) -> anyhow::Result<ThreadRouting> {
    let not_in_thread = || {
        anyhow::anyhow!(
            "`codex proxy route` only works in commands started by a Codex agent thread with \
             the command proxy"
        )
    };
    let proxy = ["HTTP_PROXY", "http_proxy"]
        .iter()
        .find_map(|key| std::env::var(key).ok())
        .ok_or_else(not_in_thread)?;
    let url = url::Url::parse(&proxy).map_err(|_| not_in_thread())?;
    let (Some("127.0.0.1"), Some(port)) = (url.host_str(), url.port()) else {
        return Err(not_in_thread());
    };
    if !url.username().starts_with(THREAD_IDENTITY_PREFIX) {
        return Err(not_in_thread());
    }

    let body = update
        .map(serde_json::to_string)
        .transpose()?
        .unwrap_or_default();
    let method = if update.is_some() { "POST" } else { "GET" };
    let identity = base64::engine::general_purpose::STANDARD.encode(format!("{}:", url.username()));
    let request = format!(
        "{method} http://{CONTROL_HOST}{ROUTING_PATH} HTTP/1.1\r\nHost: {CONTROL_HOST}\r\n\
         Proxy-Authorization: Basic {identity}\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let mut stream = TcpStream::connect(("127.0.0.1", port))?;
    stream.write_all(request.as_bytes())?;
    let mut response = String::new();
    stream.read_to_string(&mut response)?;
    let (head, body) = response
        .split_once("\r\n\r\n")
        .ok_or_else(|| anyhow::anyhow!("the command proxy sent an invalid response"))?;
    let status = head.split(' ').nth(1).unwrap_or_default();
    anyhow::ensure!(status == "200", "{}", body.trim());
    Ok(serde_json::from_str(body)?)
}
