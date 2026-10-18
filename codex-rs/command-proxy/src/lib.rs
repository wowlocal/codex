//! Loopback proxy for programs started by Codex.
//!
//! Agent commands and MCP servers receive `HTTP(S)_PROXY` pointing here instead of the upstream
//! proxy, so they never see its credentials. Each connection is routed per destination:
//! OpenAI destinations always go through the upstream proxy, loopback and the user's
//! `[proxy] direct` entries connect directly, and agent threads may send other destinations
//! direct through the control endpoint at `http://codex-proxy.invalid/routing`. Programs
//! without a thread identity, such as MCP servers, always use the upstream proxy otherwise.
//!
//! This is cooperative routing for programs that honor proxy variables, not a firewall.

mod head;
mod route;
mod upstream;

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;

use codex_required_proxy::RequiredProxy;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio::net::TcpStream;

use head::RequestHead;
use route::Identity;
use route::Policies;
use route::Route;
use upstream::Failure;

pub use route::DefaultRoute;
pub use route::PACKAGE_REGISTRIES;
pub use route::RoutingUpdate;
pub use route::ThreadRouting;

/// Host of the control endpoint, reachable only through this proxy.
pub const CONTROL_HOST: &str = "codex-proxy.invalid";
/// Path of a thread's routing on the control endpoint.
pub const ROUTING_PATH: &str = "/routing";
const MAX_CONTROL_BODY_BYTES: usize = 16 * 1024;

type RequiredProvider = dyn Fn() -> Option<Arc<RequiredProxy>> + Send + Sync;

struct State {
    required: Box<RequiredProvider>,
    policies: Policies,
}

/// Serves the command proxy on `listener` and returns its address.
///
/// `required` returns the current upstream proxy; without one, every request fails.
pub fn spawn(
    listener: TcpListener,
    required: impl Fn() -> Option<Arc<RequiredProxy>> + Send + Sync + 'static,
) -> io::Result<SocketAddr> {
    let address = listener.local_addr()?;
    let state = Arc::new(State {
        required: Box::new(required),
        policies: Policies::default(),
    });
    tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((client, _)) => {
                    let state = Arc::clone(&state);
                    tokio::spawn(async move {
                        if let Err(error) = handle(client, &state).await {
                            tracing::debug!(%error, "command proxy connection ended");
                        }
                    });
                }
                Err(error) => tracing::warn!(%error, "command proxy accept failed"),
            }
        }
    });
    Ok(address)
}

async fn handle(mut client: TcpStream, state: &State) -> io::Result<()> {
    let (head_bytes, rest) = head::read_head(&mut client).await?;
    let Ok(request) = RequestHead::parse(&head_bytes) else {
        return respond(&mut client, 400, "bad-request", "malformed request").await;
    };
    let identity = Identity::from_proxy_authorization(request.header("proxy-authorization"));
    let Some(required) = (state.required)() else {
        return respond(
            &mut client,
            502,
            "no-upstream",
            "Codex has no upstream proxy",
        )
        .await;
    };

    if request.method.eq_ignore_ascii_case("CONNECT") {
        let Some((host, port)) = split_authority(&request.target) else {
            return respond(&mut client, 400, "bad-request", "malformed CONNECT target").await;
        };
        let route = state.policies.route(&required, &identity, &host);
        tracing::debug!(%host, ?route, "command proxy tunnel");
        let server = match route {
            Route::Upstream => upstream::tunnel(&request.target, required.url()).await,
            Route::Direct => upstream::connect_direct(&host, port)
                .await
                .map(|stream| Box::new(stream) as Box<dyn upstream::Stream>),
        };
        let mut server = match server {
            Ok(server) => server,
            Err(failure) => return fail(&mut client, failure).await,
        };
        client
            .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            .await?;
        server.write_all(&rest).await?;
        tokio::io::copy_bidirectional(&mut client, &mut server).await?;
        return Ok(());
    }

    let Some(url) = url::Url::parse(&request.target)
        .ok()
        .filter(|url| url.scheme() == "http" && url.host_str().is_some())
    else {
        return respond(
            &mut client,
            400,
            "bad-request",
            "expected an absolute http:// URL",
        )
        .await;
    };
    let host = url.host_str().unwrap_or_default().to_string();
    if host.eq_ignore_ascii_case(CONTROL_HOST) {
        return control(&mut client, &request, rest, &identity, state, &required).await;
    }
    let route = state.policies.route(&required, &identity, &host);
    tracing::debug!(%host, ?route, "command proxy request");
    let close = [("Connection", "close")];
    let (mut server, head): (Box<dyn upstream::Stream>, Vec<u8>) = match route {
        Route::Upstream => match upstream::connect(required.url()).await {
            Ok(upstream) => {
                let mut extra = close.to_vec();
                if let Some(authorization) = upstream.authorization.as_deref() {
                    extra.push(("Proxy-Authorization", authorization));
                }
                let head = request.to_bytes(&request.target, &extra);
                (upstream.stream, head)
            }
            Err(failure) => return fail(&mut client, failure).await,
        },
        Route::Direct => {
            let port = url.port_or_known_default().unwrap_or(80);
            match upstream::connect_direct(host.trim_matches(['[', ']']), port).await {
                Ok(stream) => {
                    let mut target = url.path().to_string();
                    if let Some(query) = url.query() {
                        target.push('?');
                        target.push_str(query);
                    }
                    (Box::new(stream), request.to_bytes(&target, &close))
                }
                Err(failure) => return fail(&mut client, failure).await,
            }
        }
    };
    server.write_all(&head).await?;
    server.write_all(&rest).await?;
    tokio::io::copy_bidirectional(&mut client, &mut server).await?;
    Ok(())
}

/// Reads or changes the calling thread's routing.
async fn control(
    client: &mut TcpStream,
    request: &RequestHead,
    mut body: Vec<u8>,
    identity: &Identity,
    state: &State,
    required: &RequiredProxy,
) -> io::Result<()> {
    let Identity::Thread(thread) = identity else {
        return respond(client, 403, "forbidden", "only agent threads have routing").await;
    };
    if url::Url::parse(&request.target)
        .ok()
        .is_none_or(|url| url.path() != ROUTING_PATH)
    {
        return respond(client, 404, "not-found", "unknown control path").await;
    }
    if request.method.eq_ignore_ascii_case("POST") {
        let length: usize = request
            .header("content-length")
            .and_then(|value| value.parse().ok())
            .unwrap_or_default();
        if length > MAX_CONTROL_BODY_BYTES {
            return respond(client, 413, "bad-request", "routing update too large").await;
        }
        while body.len() < length {
            let mut chunk = vec![0u8; length - body.len()];
            let read = client.read(&mut chunk).await?;
            if read == 0 {
                break;
            }
            body.extend_from_slice(&chunk[..read]);
        }
        let update = match serde_json::from_slice::<RoutingUpdate>(&body) {
            Ok(update) => update,
            Err(error) => {
                let message = format!("invalid routing update: {error}");
                return respond(client, 400, "bad-request", &message).await;
            }
        };
        if let Err(error) = state.policies.update(thread, update) {
            return respond(client, 400, "bad-request", &error.to_string()).await;
        }
        tracing::info!(%thread, "agent changed command routing");
    }
    let routing = state.policies.routing(required, thread);
    let body = serde_json::to_string(&routing).map_err(io::Error::other)?;
    write_response(client, 200, "ok", "application/json", &body).await
}

fn split_authority(authority: &str) -> Option<(String, u16)> {
    let (host, port) = authority.rsplit_once(':')?;
    let host = host.trim_start_matches('[').trim_end_matches(']');
    let port = port.parse().ok()?;
    (!host.is_empty()).then(|| (host.to_string(), port))
}

async fn fail(client: &mut TcpStream, failure: Failure) -> io::Result<()> {
    respond(client, 502, failure.tag, &failure.message).await
}

async fn respond(client: &mut TcpStream, status: u16, tag: &str, message: &str) -> io::Result<()> {
    write_response(client, status, tag, "text/plain; charset=utf-8", message).await
}

async fn write_response(
    client: &mut TcpStream,
    status: u16,
    tag: &str,
    content_type: &str,
    body: &str,
) -> io::Result<()> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        413 => "Payload Too Large",
        _ => "Bad Gateway",
    };
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nX-Codex-Proxy: {tag}\r\nContent-Type: {content_type}\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    client.write_all(response.as_bytes()).await?;
    client.shutdown().await
}

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;
