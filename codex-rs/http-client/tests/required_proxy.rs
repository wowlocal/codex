#![allow(clippy::expect_used)]
//! The fork's required proxy replaces environment discovery for every route, including legacy
//! transport-default clients. This binary holds a single test because the proxy is process-wide.

use codex_http_client::ClientRouteClass;
use codex_http_client::HttpClientBuilder;
use codex_http_client::HttpClientFactory;
use codex_http_client::OutboundProxyPolicy;
use codex_http_client::OutboundProxyRoute;
use codex_http_client::RouteAwareClientPool;
use codex_required_proxy::RequiredProxy;
use pretty_assertions::assert_eq;
use std::io::Read;
use std::io::Write;
use std::net::TcpListener;
use std::time::Duration;

#[tokio::test]
async fn required_proxy_routes_resolved_and_legacy_clients() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind proxy");
    let proxy_url = format!(
        "http://user:secret@{}",
        listener.local_addr().expect("addr")
    );
    let home = tempfile::tempdir().expect("temp home");
    let revision = home.path().join("proxy-revision");
    let direct_host = "direct-probe.invalid";
    let required = RequiredProxy::new(&proxy_url, &["192.168.0.0/16".into(), direct_host.into()])
        .expect("valid required proxy");
    let reloaded =
        RequiredProxy::new(&proxy_url, &["192.168.0.0/16".into()]).expect("valid updated proxy");
    let reloader =
        codex_required_proxy::Reloader::new(vec![revision.clone()], Duration::ZERO, move || {
            Ok(reloaded.clone())
        });
    codex_required_proxy::install(required.clone(), Some(reloader)).expect("install");
    let gateway = codex_command_proxy::spawn(
        tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind gateway"),
        codex_required_proxy::current,
    )
    .expect("spawn gateway");
    codex_required_proxy::set_command_proxy(gateway).expect("install gateway");

    for policy in [
        OutboundProxyPolicy::ReqwestDefault,
        OutboundProxyPolicy::RespectSystemProxy,
    ] {
        let route = HttpClientFactory::new(policy).resolve_proxy_route("https://api.openai.com/v1");
        assert_eq!(
            route,
            OutboundProxyRoute::Proxy {
                url: required.url().to_string(),
                no_proxy: Some(required.no_proxy().to_string()),
            }
        );
        assert!(!route.bypasses("https://api.openai.com/v1"));
        assert!(route.bypasses("http://192.168.1.10/mcp"));
        assert!(route.bypasses("http://localhost:8080/"));
    }

    let proxy = std::thread::spawn(move || {
        let mut requests = Vec::new();
        for _ in 0..6 {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut request = [0u8; 1024];
            let read = stream.read(&mut request).expect("read request");
            stream
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok")
                .expect("write response");
            requests.push(String::from_utf8_lossy(&request[..read]).to_ascii_lowercase());
        }
        requests
    });
    // `.invalid` cannot resolve, so every constructor must use the required proxy.
    #[allow(deprecated)]
    let clients = [
        HttpClientBuilder::new()
            .build_with_transport_default_proxy()
            .expect("legacy client"),
        HttpClientBuilder::new()
            .build_direct()
            .expect("exceptional direct client"),
        HttpClientBuilder::new().build_direct_with_custom_ca_fallback(),
        RouteAwareClientPool::new(
            HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault),
            ClientRouteClass::Auth,
        )
        .with_direct_proxy()
        .into_client(),
        RouteAwareClientPool::new(
            HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault),
            ClientRouteClass::Auth,
        )
        .with_legacy_direct_proxy_and_custom_ca_fallback()
        .into_client(),
    ];
    for client in clients {
        let body = client
            .get("http://codex-required-proxy.invalid/probe")
            .send()
            .await
            .expect("request through the required proxy")
            .text()
            .await
            .expect("body");
        assert_eq!(body, "ok");
    }
    let origin = TcpListener::bind("127.0.0.1:0").expect("bind direct fixture");
    let origin_address = origin.local_addr().expect("direct fixture address");
    let direct = std::thread::spawn(move || {
        let (mut stream, _) = origin.accept().expect("accept direct request");
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .expect("read deadline");
        let mut head = [0; 1024];
        let size = stream.read(&mut head).expect("read direct request");
        stream
            .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 6\r\n\r\ndirect")
            .expect("write direct response");
        assert_eq!(stream.read(&mut [0]).expect("connection closes"), 0);
        String::from_utf8_lossy(&head[..size]).to_ascii_lowercase()
    });
    let direct_url = format!("http://{direct_host}:{}/probe", origin_address.port());
    let destination = direct_url.clone();
    tokio::task::spawn_blocking(move || {
        let client = codex_http_client::configure_required_blocking_reqwest_proxy(
            reqwest::blocking::Client::builder(),
        )
        .resolve(direct_host, origin_address)
        .timeout(Duration::from_secs(3))
        .build()
        .expect("blocking client");
        let request = || {
            client
                .get(&destination)
                .send()
                .expect("request")
                .text()
                .expect("body")
        };
        assert_eq!(request(), "direct");
        std::fs::write(revision, "updated").expect("change proxy configuration");
        assert_eq!(request(), "ok");
    })
    .await
    .expect("blocking requests");
    let direct_request = direct.join().expect("direct fixture thread");
    assert!(direct_request.starts_with("get /probe http/1.1\r\n"));
    assert!(!direct_request.contains("proxy-authorization:"));

    let requests = proxy.join().expect("proxy thread");
    for (index, request) in requests.iter().enumerate() {
        let url = if index == 5 {
            &direct_url
        } else {
            "http://codex-required-proxy.invalid/probe"
        };
        assert!(
            request.starts_with(&format!("get {url} http/1.1\r\n")),
            "{request}"
        );
        // Basic base64("user:secret"), lowercased with the rest of the request.
        assert!(request.contains("proxy-authorization: basic dxnlcjpzzwnyzxq=\r\n"));
    }
}
