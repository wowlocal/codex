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
                environment: "test".into(),
                service_name: "proxy-test".into(),
                service_version: "1".into(),
                codex_home: std::env::temp_dir(),
                http_client_factory: HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault),
                exporter: OtelExporter::None,
                trace_exporter: OtelExporter::None,
                metrics_exporter: OtelExporter::None,
                runtime_metrics: false,
                span_attributes: BTreeMap::new(),
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
