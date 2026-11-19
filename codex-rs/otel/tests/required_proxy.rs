//! All three telemetry signals must reject a transport that cannot honor a required proxy.
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::error::Error;
use std::io::ErrorKind;
use std::net::TcpListener;
use std::process::Command;

use codex_http_client::HttpClientFactory;
use codex_http_client::OutboundProxyPolicy;
use codex_otel::OtelExporter;
use codex_otel::OtelProvider;
use codex_otel::OtelSettings;
use pretty_assertions::assert_eq;

#[derive(Debug)]
enum Signal {
    Logs,
    Traces,
    Metrics,
}

#[tokio::test]
async fn grpc_exporters_never_bypass_env_proxy() -> Result<(), Box<dyn Error>> {
    const ORIGIN: &str = "CODEX_TEST_OTEL_PROXY_ORIGIN";
    const KEYS: [&str; 6] = [
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "http_proxy",
        "https_proxy",
        "all_proxy",
    ];
    if let Ok(endpoint) = std::env::var(ORIGIN) {
        for signal in [Signal::Logs, Signal::Traces, Signal::Metrics] {
            let mut settings = OtelSettings {
                headers_helper: None,
                environment: "test".into(),
                service_name: "proxy-test".into(),
                service_version: "1".into(),
                codex_home: std::env::temp_dir(),
                http_client_factory: HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault),
                exporter: OtelExporter::None,
                trace_exporter: OtelExporter::None,
                metrics_exporter: OtelExporter::None,
                runtime_metrics: false,
                span_attributes: HashMap::new(),
                tracestate: BTreeMap::new(),
            };
            let exporter = OtelExporter::OtlpGrpc {
                endpoint: endpoint.clone(),
                headers: HashMap::new(),
                tls: None,
            };
            match signal {
                Signal::Logs => settings.exporter = exporter,
                Signal::Traces => settings.trace_exporter = exporter,
                Signal::Metrics => settings.metrics_exporter = exporter,
            }
            let error = OtelProvider::try_new(&settings)
                .err()
                .expect("gRPC must not build a direct exporter with a proxy configured")
                .to_string();
            assert!(
                error.contains("OTLP/gRPC cannot honor the configured proxy"),
                "{signal:?}: {error}"
            );
            assert!(!error.contains("secret"));
        }
        return Ok(());
    }

    let origin = TcpListener::bind("127.0.0.1:0")?;
    origin.set_nonblocking(true)?;
    for key in KEYS {
        let mut command = Command::new(std::env::current_exe()?);
        for name in KEYS {
            command.env_remove(name);
        }
        let output = command
            .arg("--exact")
            .arg("grpc_exporters_never_bypass_env_proxy")
            .env(ORIGIN, format!("http://{}", origin.local_addr()?))
            .env(key, "http://secret:password@127.0.0.1:9")
            .output()?;
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    assert_eq!(origin.accept().unwrap_err().kind(), ErrorKind::WouldBlock);
    Ok(())
}

/// A reachable collector makes a silent direct retry observable, even when the proxy fails.
#[cfg(unix)]
#[test]
fn http_signals_and_helpers_never_bypass_the_required_proxy() -> Result<(), Box<dyn Error>> {
    use codex_otel::OtelHeadersHelper;
    use codex_otel::OtelHttpProtocol;
    use opentelemetry::logs::LogRecord as _;
    use opentelemetry::logs::Logger as _;
    use opentelemetry::logs::LoggerProvider as _;
    use opentelemetry::trace::Span as _;
    use opentelemetry::trace::Tracer as _;
    use std::io::Read;
    use std::io::Write;
    use std::net::SocketAddr;
    use std::net::TcpStream;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    const MODE: &str = "CODEX_TEST_OTEL_HTTP_PROXY_MODE";
    const PROXY: &str = "CODEX_TEST_OTEL_HTTP_PROXY_ADDRESS";
    const ORIGIN: &str = "CODEX_TEST_OTEL_HTTP_ORIGIN_ADDRESS";
    const RUNTIME: &str = "CODEX_TEST_OTEL_HTTP_RUNTIME";
    if let Ok(mode) = std::env::var(MODE) {
        let proxy: SocketAddr = std::env::var(PROXY)?.parse()?;
        let origin: SocketAddr = std::env::var(ORIGIN)?.parse()?;
        let scheme = if mode == "tls-failure" {
            "https"
        } else {
            "http"
        };
        let required = codex_required_proxy::RequiredProxy::new(
            &format!("{scheme}://fixture:fixture-secret@{proxy}"),
            &[],
        )?;
        assert!(!required.bypasses_host(&origin.ip().to_string()));
        codex_required_proxy::install(required, None)?;
        let mut builder = if std::env::var(RUNTIME)? == "multi" {
            let mut builder = tokio::runtime::Builder::new_multi_thread();
            builder.worker_threads(2);
            builder
        } else {
            tokio::runtime::Builder::new_current_thread()
        };
        let runtime = builder.enable_all().build()?;
        return runtime.block_on(async {
            let gateway = codex_command_proxy::spawn(
                tokio::net::TcpListener::bind("127.0.0.1:0").await?,
                codex_required_proxy::current,
            )?;
            codex_required_proxy::set_command_proxy(gateway)?;
            let exporter = |path| OtelExporter::OtlpHttp {
                endpoint: format!("http://{origin}/v1/{path}"),
                headers: HashMap::new(), protocol: OtelHttpProtocol::Binary, tls: None,
            };
            let helper = (mode == "helper").then(|| OtelHeadersHelper {
                command: format!("test \"$HTTPS_PROXY\" = 'http://codex:codex-local@{gateway}' && test \"$pip_proxy\" = 'http://codex:codex-local@{gateway}' && printf '%s' '{{\"x-proxy-helper\":\"ok\"}}'"),
                refresh_interval: Duration::from_secs(3600),
                apply_to_logs: true, apply_to_traces: true, apply_to_metrics: true,
            });
            let provider = OtelProvider::try_new(&OtelSettings {
                headers_helper: helper, environment: "test".into(),
                service_name: "required-proxy-probe".into(), service_version: "1".into(),
                codex_home: std::env::temp_dir(),
                http_client_factory: HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault),
                exporter: exporter("logs"), trace_exporter: exporter("traces"),
                metrics_exporter: exporter("metrics"), runtime_metrics: false,
                span_attributes: HashMap::new(), tracestate: BTreeMap::new(),
            })?.ok_or("no telemetry provider")?;
            let logger = provider.logger.as_ref().ok_or("no logger")?.logger("proxy-probe");
            let mut record = logger.create_log_record();
            record.set_body("required-proxy-probe".into());
            logger.emit(record);
            let mut span = provider.tracer.as_ref().ok_or("no tracer")?.start("proxy-probe");
            span.end();
            provider.metrics.as_ref().ok_or("no metrics")?.counter("codex.proxy_probe", 1, &[])?;
            let result = provider.shutdown_with_timeout(Duration::from_secs(10)).await;
            if mode == "ok" || mode == "helper" { result?; }
            Ok(())
        });
    }

    for (mode, runtime) in [
        ("ok", "current"),
        ("ok", "multi"),
        ("helper", "multi"),
        ("407", "current"),
        ("reset", "current"),
        ("unavailable", "current"),
        ("tls-failure", "current"),
    ] {
        // 0.0.0.0 is locally reachable on the supported Unix runners and is not
        // in the required proxy's implicit direct list. No external service or DNS is used.
        let origin = TcpListener::bind("0.0.0.0:0")?;
        let origin_address = origin.local_addr()?;
        let probe = TcpStream::connect(origin_address)?;
        drop(origin.accept()?);
        drop(probe);
        origin.set_nonblocking(true)?;
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let proxy_address = listener.local_addr()?;
        let stopped = Arc::new(AtomicBool::new(false));
        let worker = if mode == "unavailable" {
            drop(listener);
            None
        } else {
            let stopped = Arc::clone(&stopped);
            Some(std::thread::spawn(
                move || -> std::io::Result<Vec<String>> {
                    let mut requests = Vec::new();
                    loop {
                        let (mut stream, _) = listener.accept()?;
                        if stopped.load(Ordering::Acquire) {
                            break;
                        }
                        stream.set_read_timeout(Some(Duration::from_secs(3)))?;
                        if mode == "tls-failure" {
                            requests.push("TLS handshake".to_string());
                            stream.write_all(b"HTTP/1.1 400 Bad Request\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")?;
                            continue;
                        }
                        let mut header = Vec::new();
                        while !header.ends_with(b"\r\n\r\n") && header.len() < 32 * 1024 {
                            let mut byte = [0];
                            if stream.read(&mut byte)? == 0 {
                                break;
                            }
                            header.push(byte[0]);
                        }
                        let header = String::from_utf8_lossy(&header).to_ascii_lowercase();
                        let length = header
                            .lines()
                            .find_map(|line| {
                                line.strip_prefix("content-length:")
                                    .and_then(|value| value.trim().parse::<usize>().ok())
                            })
                            .unwrap_or(0);
                        if length > 1024 * 1024 {
                            return Err(std::io::ErrorKind::InvalidData.into());
                        }
                        stream.read_exact(&mut vec![0; length])?;
                        requests.push(header);
                        if mode == "reset" {
                            continue;
                        }
                        let response: &[u8] = if mode == "407" {
                            b"HTTP/1.1 407 Proxy Authentication Required\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                        } else {
                            b"HTTP/1.1 200 OK\r\ncontent-type: application/x-protobuf\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                        };
                        stream.write_all(response)?;
                    }
                    Ok(requests)
                },
            ))
        };
        let mut command = Command::new(std::env::current_exe()?);
        command.args([
            "--exact",
            "http_signals_and_helpers_never_bypass_the_required_proxy",
        ]);
        for (key, _) in std::env::vars_os() {
            if key.to_str().is_some_and(|key| {
                codex_required_proxy::is_command_proxy_env_key(key) || key.starts_with("OTEL_")
            }) {
                command.env_remove(key);
            }
        }
        let output = command
            .env(MODE, mode)
            .env(RUNTIME, runtime)
            .env(PROXY, proxy_address.to_string())
            .env(ORIGIN, origin_address.to_string())
            .env("OTEL_EXPORTER_OTLP_TIMEOUT", "1000")
            .output()?;
        stopped.store(true, Ordering::Release);
        let requests = if let Some(worker) = worker {
            let _ = TcpStream::connect(proxy_address);
            worker.join().map_err(|_| "proxy worker panicked")??
        } else {
            Vec::new()
        };
        assert!(
            output.status.success(),
            "{mode}/{runtime}: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            origin.accept().unwrap_err().kind(),
            ErrorKind::WouldBlock,
            "{mode}/{runtime}: direct collector connection"
        );
        if mode == "ok" || mode == "helper" {
            for signal in ["logs", "traces", "metrics"] {
                assert!(
                    requests.iter().any(|request| request
                        .starts_with(&format!("post http://{origin_address}/v1/{signal} "))),
                    "{mode}/{runtime}: missing proxied {signal}"
                );
            }
            if mode == "helper" {
                assert!(
                    requests
                        .iter()
                        .all(|request| request.contains("x-proxy-helper: ok\r\n"))
                );
            }
        } else if mode != "unavailable" {
            assert!(!requests.is_empty(), "{mode}: proxy was never contacted");
        }
        assert!(!String::from_utf8_lossy(&output.stderr).contains("fixture-secret"));
    }
    Ok(())
}
