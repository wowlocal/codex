//! Realtime must share the required environment proxy route, including for ws URLs.
use std::collections::HashMap;
use std::time::Duration;

use codex_api::Provider;
use codex_api::RealtimeEventParser;
use codex_api::RealtimeOutputModality;
use codex_api::RealtimeSessionConfig;
use codex_api::RealtimeSessionMode;
use codex_api::RealtimeWebsocketClient;
use codex_api::RetryConfig;
use codex_protocol::protocol::RealtimeVoice;
use http::HeaderMap;
use pretty_assertions::assert_eq;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;

#[tokio::test]
async fn realtime_proxy_rejection_never_reaches_origin() -> anyhow::Result<()> {
    const ORIGIN: &str = "CODEX_TEST_REALTIME_PROXY_ORIGIN";
    if let Ok(base_url) = std::env::var(ORIGIN) {
        let client = RealtimeWebsocketClient::new(Provider {
            name: "proxy-test".into(),
            base_url,
            query_params: Some(HashMap::new()),
            headers: HeaderMap::new(),
            retry: RetryConfig {
                max_attempts: 1,
                base_delay: Duration::from_millis(/*millis*/ 1),
                retry_429: false,
                retry_5xx: false,
                retry_transport: false,
            },
            stream_idle_timeout: Duration::from_secs(/*secs*/ 2),
        });
        let result = tokio::time::timeout(
            Duration::from_secs(/*secs*/ 3),
            client.connect(
                RealtimeSessionConfig {
                    instructions: String::new(),
                    initial_items: Vec::new(),
                    delegation_ack_filler: None,
                    model: None,
                    session_id: None,
                    event_parser: RealtimeEventParser::V1,
                    session_mode: RealtimeSessionMode::Conversational,
                    output_modality: RealtimeOutputModality::Audio,
                    voice: RealtimeVoice::Cove,
                },
                HeaderMap::new(),
                HeaderMap::new(),
            ),
        )
        .await?;
        assert!(result.is_err());
        return Ok(());
    }

    let origin = TcpListener::bind("127.0.0.1:0").await?;
    let proxy = TcpListener::bind("127.0.0.1:0").await?;
    let proxy_url = format!("http://{}", proxy.local_addr()?);
    let origin_addr = origin.local_addr()?;
    let proxy_task = tokio::spawn(async move {
        let (mut stream, _) =
            tokio::time::timeout(Duration::from_secs(/*secs*/ 5), proxy.accept()).await??;
        let mut request = Vec::new();
        while !request.ends_with(b"\r\n\r\n") && request.len() < 8192 {
            request.push(stream.read_u8().await?);
        }
        stream
            .write_all(b"HTTP/1.1 407 Proxy Authentication Required\r\nContent-Length: 0\r\n\r\n")
            .await?;
        Ok::<_, anyhow::Error>(String::from_utf8(request)?)
    });
    let mut command = tokio::process::Command::new(std::env::current_exe()?);
    for key in [
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "NO_PROXY",
        "http_proxy",
        "https_proxy",
        "all_proxy",
        "no_proxy",
    ] {
        command.env_remove(key);
    }
    let output = command
        .args(["--exact", "realtime_proxy_rejection_never_reaches_origin"])
        .env(ORIGIN, format!("http://{origin_addr}/v1"))
        .env("HTTPS_PROXY", proxy_url)
        .output()
        .await?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let request = proxy_task.await??;
    assert_eq!(
        request.lines().next(),
        Some(format!("CONNECT {origin_addr} HTTP/1.1").as_str())
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(/*millis*/ 50), origin.accept())
            .await
            .is_err()
    );
    Ok(())
}
