use super::*;
use base64::Engine as _;
use pretty_assertions::assert_eq;
use std::sync::Mutex;

/// A fake upstream proxy that records request heads and accepts every tunnel.
async fn fake_upstream() -> (SocketAddr, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("address");
    let seen = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&seen);
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            let recorded = Arc::clone(&recorded);
            tokio::spawn(async move {
                let (request, _) = head::read_head(&mut stream).await.expect("head");
                let request = String::from_utf8(request).expect("utf-8");
                let response: &[u8] = if request.starts_with("CONNECT ") {
                    b"HTTP/1.1 200 Connection Established\r\n\r\n"
                } else {
                    b"HTTP/1.1 200 OK\r\nContent-Length: 8\r\nConnection: close\r\n\r\nupstream"
                };
                recorded.lock().expect("seen").push(request);
                stream.write_all(response).await.expect("respond");
                stream.shutdown().await.ok();
            });
        }
    });
    (address, seen)
}

async fn command_proxy(upstream: SocketAddr) -> SocketAddr {
    let required = Arc::new(
        RequiredProxy::new(&format!("http://user:secret@{upstream}"), &[]).expect("valid proxy"),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    spawn(listener, move || Some(Arc::clone(&required))).expect("spawn")
}

fn identity_header(identity: &str) -> String {
    let encoded = base64::engine::general_purpose::STANDARD.encode(format!("{identity}:"));
    format!("Proxy-Authorization: Basic {encoded}\r\n")
}

/// Sends `request` and returns the response head, or the whole response when it closes.
async fn exchange(proxy: SocketAddr, request: String) -> String {
    let mut stream = TcpStream::connect(proxy).await.expect("connect");
    stream.write_all(request.as_bytes()).await.expect("write");
    if request.starts_with("CONNECT ") {
        let (response, _) = head::read_head(&mut stream).await.expect("response");
        return String::from_utf8(response).expect("utf-8");
    }
    let mut response = String::new();
    stream.read_to_string(&mut response).await.expect("read");
    response
}

#[tokio::test]
async fn codex_traffic_uses_the_upstream_proxy_with_its_credentials() {
    let (upstream, seen) = fake_upstream().await;
    let proxy = command_proxy(upstream).await;

    let tunnel = exchange(
        proxy,
        "CONNECT example.invalid:443 HTTP/1.1\r\nHost: example.invalid:443\r\n\r\n".to_string(),
    )
    .await;
    assert!(tunnel.starts_with("HTTP/1.1 200 "), "{tunnel}");

    let plain = exchange(
        proxy,
        "GET http://example.invalid/a?b=c HTTP/1.1\r\nHost: example.invalid\r\n\
         Proxy-Authorization: Basic Y29kZXg6\r\n\r\n"
            .to_string(),
    )
    .await;
    assert!(plain.ends_with("\r\n\r\nupstream"), "{plain}");

    let seen = seen.lock().expect("seen").clone();
    assert_eq!(
        seen,
        vec![
            "CONNECT example.invalid:443 HTTP/1.1\r\nHost: example.invalid:443\r\n\
             Proxy-Authorization: Basic dXNlcjpzZWNyZXQ=\r\n\r\n"
                .to_string(),
            "GET http://example.invalid/a?b=c HTTP/1.1\r\nHost: example.invalid\r\n\
             Connection: close\r\nProxy-Authorization: Basic dXNlcjpzZWNyZXQ=\r\n\r\n"
                .to_string(),
        ]
    );
}

#[tokio::test]
async fn agent_threads_choose_their_own_routing_but_not_for_openai() {
    let (upstream, seen) = fake_upstream().await;
    let proxy = command_proxy(upstream).await;
    let thread = identity_header("thread-t1");
    let update = r#"{"default":"direct","add":["registry.npmjs.org"]}"#;

    let changed = exchange(
        proxy,
        format!(
            "POST http://{CONTROL_HOST}{ROUTING_PATH} HTTP/1.1\r\nHost: {CONTROL_HOST}\r\n\
             {thread}Content-Length: {}\r\n\r\n{update}",
            update.len()
        ),
    )
    .await;
    let body = changed.split("\r\n\r\n").nth(1).expect("body");
    let routing: ThreadRouting = serde_json::from_str(body).expect("routing");
    assert_eq!(
        (routing.default, routing.direct.len()),
        (DefaultRoute::Direct, PACKAGE_REGISTRIES.len())
    );

    // The thread's direct route never reaches the upstream proxy; whether the destination
    // itself answers depends on local DNS.
    exchange(
        proxy,
        format!("CONNECT example.invalid:443 HTTP/1.1\r\n{thread}\r\n"),
    )
    .await;
    let pinned = exchange(
        proxy,
        format!("CONNECT api.openai.com:443 HTTP/1.1\r\n{thread}\r\n"),
    )
    .await;
    assert!(pinned.starts_with("HTTP/1.1 200 "), "{pinned}");
    let mcp = exchange(
        proxy,
        "CONNECT example.invalid:443 HTTP/1.1\r\n\r\n".to_string(),
    )
    .await;
    assert!(mcp.starts_with("HTTP/1.1 200 "), "{mcp}");
    let tunnels: Vec<String> = seen
        .lock()
        .expect("seen")
        .iter()
        .map(|request| request.lines().next().unwrap_or_default().to_string())
        .collect();
    assert_eq!(
        tunnels,
        vec![
            "CONNECT api.openai.com:443 HTTP/1.1".to_string(),
            "CONNECT example.invalid:443 HTTP/1.1".to_string(),
        ]
    );

    let forbidden = exchange(
        proxy,
        format!(
            "POST http://{CONTROL_HOST}{ROUTING_PATH} HTTP/1.1\r\nContent-Length: 2\r\n\r\n{{}}"
        ),
    )
    .await;
    assert!(forbidden.starts_with("HTTP/1.1 403 "), "{forbidden}");
    let pinned_update = r#"{"add":["api.openai.com"]}"#;
    let rejected = exchange(
        proxy,
        format!(
            "POST http://{CONTROL_HOST}{ROUTING_PATH} HTTP/1.1\r\n{thread}Content-Length: {}\r\n\r\n\
             {pinned_update}",
            pinned_update.len()
        ),
    )
    .await;
    assert!(
        rejected.starts_with("HTTP/1.1 400 ")
            && rejected.ends_with("OpenAI destinations always use the proxy"),
        "{rejected}"
    );
}
