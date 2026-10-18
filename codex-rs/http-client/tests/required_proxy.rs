#![allow(clippy::expect_used)]
//! The fork's required proxy replaces environment discovery for every route, including legacy
//! transport-default clients. This binary holds a single test because the proxy is process-wide.

use codex_http_client::HttpClientBuilder;
use codex_http_client::HttpClientFactory;
use codex_http_client::OutboundProxyPolicy;
use codex_http_client::OutboundProxyRoute;
use codex_required_proxy::RequiredProxy;
use pretty_assertions::assert_eq;
use std::io::Read;
use std::io::Write;
use std::net::TcpListener;

#[tokio::test]
async fn required_proxy_routes_resolved_and_legacy_clients() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind proxy");
    let proxy_url = format!(
        "http://user:secret@{}",
        listener.local_addr().expect("addr")
    );
    let required = RequiredProxy::new(&proxy_url, &["192.168.0.0/16".to_string()])
        .expect("valid required proxy");
    codex_required_proxy::install(required.clone(), /*reloader*/ None).expect("install");

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
        let (mut stream, _) = listener.accept().expect("accept");
        let mut request = [0u8; 1024];
        let read = stream.read(&mut request).expect("read request");
        stream
            .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok")
            .expect("write response");
        String::from_utf8_lossy(&request[..read]).to_ascii_lowercase()
    });
    // `.invalid` cannot resolve, so only the proxy can answer this request.
    #[allow(deprecated)]
    let client = HttpClientBuilder::new()
        .build_with_transport_default_proxy()
        .expect("legacy client");
    let body = client
        .get("http://codex-required-proxy.invalid/probe")
        .send()
        .await
        .expect("request through the required proxy")
        .text()
        .await
        .expect("body");
    assert_eq!(body, "ok");
    let request = proxy.join().expect("proxy thread");
    assert!(
        request.starts_with("get http://codex-required-proxy.invalid/probe http/1.1\r\n"),
        "{request}"
    );
    // Basic base64("user:secret"), lowercased with the rest of the request.
    assert!(request.contains("proxy-authorization: basic dxnlcjpzzwnyzxq=\r\n"));
}
