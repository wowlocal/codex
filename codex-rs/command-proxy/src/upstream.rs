//! Connections to the required upstream proxy.

use std::time::Duration;

use base64::Engine as _;
use rustls::pki_types::ServerName;
use tokio::io::AsyncRead;
use tokio::io::AsyncWrite;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;

use crate::head;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

pub(crate) trait Stream: AsyncRead + AsyncWrite + Unpin + Send {}

impl<T: AsyncRead + AsyncWrite + Unpin + Send> Stream for T {}

/// A connection to the upstream proxy and the `Proxy-Authorization` value it expects.
pub(crate) struct Upstream {
    pub(crate) stream: Box<dyn Stream>,
    pub(crate) authorization: Option<String>,
}

/// Why a destination could not be reached. Messages never include credentials.
#[derive(Debug)]
pub(crate) struct Failure {
    pub(crate) tag: &'static str,
    pub(crate) message: String,
}

impl Failure {
    pub(crate) fn upstream(message: impl Into<String>) -> Self {
        Self {
            tag: "upstream-failed",
            message: message.into(),
        }
    }

    pub(crate) fn direct(message: impl Into<String>) -> Self {
        Self {
            tag: "direct-failed",
            message: message.into(),
        }
    }
}

pub(crate) async fn connect(proxy_url: &str) -> Result<Upstream, Failure> {
    let url = url::Url::parse(proxy_url)
        .map_err(|_| Failure::upstream("the upstream proxy URL is invalid"))?;
    let host = url
        .host_str()
        .ok_or_else(|| Failure::upstream("the upstream proxy URL has no host"))?
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_string();
    let port = url.port_or_known_default().unwrap_or(80);
    let tcp = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect((host.as_str(), port)))
        .await
        .map_err(|_| Failure::upstream("timed out connecting to the upstream proxy"))?
        .map_err(|error| Failure::upstream(format!("cannot reach the upstream proxy: {error}")))?;
    let stream: Box<dyn Stream> = if url.scheme() == "https" {
        let config = codex_http_client::build_rustls_client_config_with_custom_ca()
            .map_err(|error| Failure::upstream(format!("TLS configuration failed: {error}")))?;
        let server_name = ServerName::try_from(host)
            .map_err(|_| Failure::upstream("the upstream proxy host is not a valid TLS name"))?;
        let tls = TlsConnector::from(config)
            .connect(server_name, tcp)
            .await
            .map_err(|error| {
                Failure::upstream(format!("TLS to the upstream proxy failed: {error}"))
            })?;
        Box::new(tls)
    } else {
        Box::new(tcp)
    };
    Ok(Upstream {
        stream,
        authorization: basic_authorization(&url),
    })
}

/// Opens a tunnel to `authority` through the upstream proxy.
pub(crate) async fn tunnel(authority: &str, proxy_url: &str) -> Result<Box<dyn Stream>, Failure> {
    let Upstream {
        mut stream,
        authorization,
    } = connect(proxy_url).await?;
    let mut request = format!("CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n");
    if let Some(authorization) = authorization {
        request.push_str(&format!("Proxy-Authorization: {authorization}\r\n"));
    }
    request.push_str("\r\n");
    stream
        .write_all(request.as_bytes())
        .await
        .map_err(|error| Failure::upstream(format!("upstream proxy write failed: {error}")))?;
    let (response, rest) = head::read_head(&mut stream)
        .await
        .map_err(|error| Failure::upstream(format!("upstream proxy read failed: {error}")))?;
    match head::status_code(&response) {
        Some(status) if (200..300).contains(&status) && rest.is_empty() => Ok(stream),
        Some(status) => Err(Failure::upstream(format!(
            "the upstream proxy answered {status} for {authority}"
        ))),
        None => Err(Failure::upstream(
            "the upstream proxy sent an invalid response",
        )),
    }
}

fn basic_authorization(url: &url::Url) -> Option<String> {
    if url.username().is_empty() && url.password().is_none() {
        return None;
    }
    let decode = |value: &str| {
        urlencoding::decode(value)
            .map(std::borrow::Cow::into_owned)
            .unwrap_or_else(|_| value.to_string())
    };
    let credentials = format!(
        "{}:{}",
        decode(url.username()),
        decode(url.password().unwrap_or_default())
    );
    let encoded = base64::engine::general_purpose::STANDARD.encode(credentials);
    Some(format!("Basic {encoded}"))
}

pub(crate) async fn connect_direct(host: &str, port: u16) -> Result<TcpStream, Failure> {
    tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect((host, port)))
        .await
        .map_err(|_| Failure::direct(format!("timed out connecting to {host}:{port}")))?
        .map_err(|error| Failure::direct(format!("cannot reach {host}:{port}: {error}")))
}
