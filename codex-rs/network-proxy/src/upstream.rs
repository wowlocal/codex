use crate::connect_policy::DialPolicy;
use crate::connect_policy::TargetCheckedTcpConnector;
use crate::connect_policy::is_non_public_target;
use crate::policy::is_private_network_ip;
use crate::policy::normalize_host;
use crate::policy::numeric_host_address;
use crate::state::NetworkProxyState;
use codex_required_proxy::RequiredProxy;
use codex_utils_rustls_provider::ensure_rustls_crypto_provider;
use rama_core::Layer;
use rama_core::Service;
use rama_core::error::BoxError;
use rama_core::error::ErrorExt as _;
use rama_core::error::OpaqueError;
use rama_core::extensions::ExtensionsMut;
use rama_core::extensions::ExtensionsRef;
use rama_core::service::BoxService;
use rama_http::Body;
use rama_http::Request;
use rama_http::Response;
use rama_http::Version;
use rama_http::layer::version_adapter::RequestVersionAdapter;
use rama_http_backend::client::HttpClientService;
use rama_http_backend::client::HttpConnector;
use rama_http_backend::client::proxy::layer::HttpProxyConnector;
use rama_http_backend::client::proxy::layer::HttpProxyConnectorLayer;
use rama_http_backend::client::proxy::layer::MaybeHttpProxiedConnection;
use rama_http_backend::client::proxy::layer::SetProxyAuthHttpHeaderLayer;
use rama_net::Protocol;
use rama_net::address::Host;
use rama_net::address::HostWithPort;
use rama_net::address::ProxyAddress;
use rama_net::client::EstablishedClientConnection;
use rama_net::http::RequestContext;
use rama_tcp::TcpStream;
use rama_tcp::client::Request as TcpRequest;
use rama_tls_rustls::client::AutoTlsStream;
use rama_tls_rustls::client::TlsConnectorDataBuilder;
use rama_tls_rustls::client::TlsConnectorLayer;
use rama_tls_rustls::dep::rustls;
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Instant;
use tracing::info;
use tracing::warn;

#[cfg(target_os = "macos")]
use rama_unix::client::UnixConnector;

#[derive(Clone, Default)]
struct ProxyConfig {
    http: Option<ProxyAddress>,
    https: Option<ProxyAddress>,
    all: Option<ProxyAddress>,
    proxy_private_ips_via_upstream: bool,
    configuration_error: Option<&'static str>,
}

impl ProxyConfig {
    fn from_env(proxy_private_ips_via_upstream: bool) -> Self {
        let read = |key: &str| std::env::var(key).ok();
        let parse = || -> Result<Self, &'static str> {
            Ok(Self {
                http: read_proxy_env(&["HTTP_PROXY", "http_proxy"], read)?,
                https: read_proxy_env(&["HTTPS_PROXY", "https_proxy"], read)?,
                all: read_proxy_env(&["ALL_PROXY", "all_proxy"], read)?,
                configuration_error: None,
                proxy_private_ips_via_upstream,
            })
        };
        parse().unwrap_or_else(|error| Self {
            configuration_error: Some(error),
            proxy_private_ips_via_upstream,
            ..Self::default()
        })
    }

    fn proxy_for_protocol(
        &self,
        is_secure: bool,
        allow_upstream_proxy: bool,
    ) -> Result<Option<ProxyAddress>, OpaqueError> {
        if let Some(required) = codex_required_proxy::current() {
            return required_proxy_address(&required, allow_upstream_proxy).map(Some);
        }
        self.environment_proxy_for_protocol(is_secure, allow_upstream_proxy)
    }

    fn environment_proxy_for_protocol(
        &self,
        is_secure: bool,
        allow_upstream_proxy: bool,
    ) -> Result<Option<ProxyAddress>, OpaqueError> {
        if !allow_upstream_proxy {
            return Ok(None);
        }
        if let Some(error) = self.configuration_error {
            return Err(OpaqueError::from_display(error));
        }
        Ok(if is_secure {
            self.https
                .clone()
                .or_else(|| self.all.clone())
                .or_else(|| self.http.clone())
        } else {
            self.http
                .clone()
                .or_else(|| self.all.clone())
                .or_else(|| self.https.clone())
        })
    }

    fn proxy_for_target(
        &self,
        target: &HostWithPort,
        is_secure: bool,
        allow_upstream_proxy: bool,
    ) -> Result<Option<ProxyAddress>, OpaqueError> {
        if let Some(required) = codex_required_proxy::current() {
            return required_proxy_for_target(
                &required,
                target,
                allow_upstream_proxy,
                self.proxy_private_ips_via_upstream,
            );
        }
        let proxy_private_ip = self.proxy_private_ips_via_upstream
            && matches!(&target.host, Host::Address(ip) if is_private_network_ip(*ip));
        if is_non_public_target(&target.host) && !proxy_private_ip {
            return Ok(None);
        }
        self.environment_proxy_for_protocol(is_secure, allow_upstream_proxy)
    }
}

fn required_proxy_address(
    required: &RequiredProxy,
    allow_upstream_proxy: bool,
) -> Result<ProxyAddress, OpaqueError> {
    if !allow_upstream_proxy {
        return Err(OpaqueError::from_display(
            "the required upstream proxy is disabled by network policy; refusing direct connection",
        ));
    }
    // RequiredProxy validates the URL first. Never attach the URL or parser error, which can
    // contain the Keychain credentials, to a transport or policy error.
    ProxyAddress::try_from(required.url()).map_err(|_| {
        OpaqueError::from_display("invalid required upstream proxy; refusing direct connection")
    })
}

pub(crate) fn required_proxy_for_target(
    required: &RequiredProxy,
    target: &HostWithPort,
    allow_upstream_proxy: bool,
    proxy_private_ips_via_upstream: bool,
) -> Result<Option<ProxyAddress>, OpaqueError> {
    if required.bypasses_host(&target.host.to_string()) {
        return Ok(None);
    }
    let proxy = required_proxy_address(required, allow_upstream_proxy)?;
    let proxy_private_ip = proxy_private_ips_via_upstream
        && matches!(&target.host, Host::Address(ip) if is_private_network_ip(*ip));
    if is_non_public_target(&target.host) && !proxy_private_ip {
        return Err(OpaqueError::from_display(
            "network target cannot use the required upstream proxy; refusing direct connection",
        ));
    }
    Ok(Some(proxy))
}

fn read_proxy_env(
    keys: &[&str],
    read: impl Fn(&str) -> Option<String>,
) -> Result<Option<ProxyAddress>, &'static str> {
    for key in keys {
        let Some(value) = read(key) else {
            continue;
        };
        let value = value.trim();
        if value.is_empty() {
            continue;
        }
        match ProxyAddress::try_from(value) {
            Ok(proxy) => {
                if proxy
                    .protocol
                    .as_ref()
                    .map(rama_net::Protocol::is_http)
                    .unwrap_or(true)
                {
                    return Ok(Some(proxy));
                }
                return Err("unsupported upstream proxy protocol; refusing direct connection");
            }
            Err(_) => {
                return Err("invalid upstream proxy address; refusing direct connection");
            }
        }
    }
    Ok(None)
}

pub(crate) fn proxy_for_connect(
    target: &HostWithPort,
    state: &NetworkProxyState,
    allow_upstream_proxy: bool,
) -> Result<Option<ProxyAddress>, OpaqueError> {
    ProxyConfig::from_env(state.proxy_private_ips_via_upstream).proxy_for_target(
        target,
        /*is_secure*/ true,
        allow_upstream_proxy,
    )
}

pub(crate) type UpstreamTunnel = MaybeHttpProxiedConnection<AutoTlsStream<TcpStream>>;

fn startup_upstream_tls_roots() -> Result<Arc<rustls::RootCertStore>, OpaqueError> {
    static ROOTS: OnceLock<Result<Arc<rustls::RootCertStore>, ()>> = OnceLock::new();
    ROOTS
        .get_or_init(|| {
            crate::certs::upstream_tls_root_store_for_env(&crate::certs::ca_env_from_process())
                .map_err(|_| ())
        })
        .clone()
        .map_err(|()| {
            OpaqueError::from_display(
                "upstream TLS trust configuration unavailable; refusing connection",
            )
        })
}

/// Remote DNS cannot enforce a local resolved-IP deny/review or private-address restriction.
/// Apply this before inserting ProxyAddress, which deliberately bypasses the direct-dial guard.
async fn check_required_upstream_destination(
    state: &NetworkProxyState,
    target: &HostWithPort,
    extensions: &rama_core::extensions::Extensions,
) -> Result<(), OpaqueError> {
    let host = normalize_host(&target.host.to_string());
    if let Some(ip) = numeric_host_address(&host) {
        let authorization = DialPolicy::authorization(extensions, target);
        if state
            .resolved_target_blocked(&host, ip, authorization)
            .await
            .map_err(|_| OpaqueError::from_display("required upstream target policy unavailable"))?
        {
            return Err(OpaqueError::from_display(
                "IP destination cannot use the required upstream proxy; refusing connection",
            ));
        }
    } else if !state
        .permits_remote_tcp_dns()
        .await
        .map_err(|_| OpaqueError::from_display("required upstream target policy unavailable"))?
    {
        return Err(OpaqueError::from_display(
            "destination requires local address enforcement; refusing required upstream remote DNS",
        ));
    }
    Ok(())
}

/// TLS belongs underneath the HTTP proxy handshake: an HTTPS proxy must authenticate its
/// own certificate before it receives CONNECT or its Proxy-Authorization header.
pub(crate) async fn connect_tunnel(
    state: Arc<NetworkProxyState>,
    mut req: TcpRequest,
    proxy: Option<ProxyAddress>,
) -> Result<EstablishedClientConnection<UpstreamTunnel, TcpRequest>, BoxError> {
    ensure_rustls_crypto_provider();
    if proxy.is_some() && codex_required_proxy::is_active() {
        check_required_upstream_destination(&state, &req.authority, req.extensions()).await?;
    }
    let roots = match state.mitm_state().await? {
        Some(mitm) => mitm.upstream_tls_root_store(),
        None => startup_upstream_tls_roots()?,
    };
    if let Some(proxy) = proxy {
        req.extensions_mut().insert(proxy);
    }
    // This marks the logical destination as opaque CONNECT; TLS terminates at the proxy only.
    req = req.with_protocol(Protocol::HTTPS);
    let tls_config = rustls::ClientConfig::builder_with_protocol_versions(rustls::ALL_VERSIONS)
        .with_root_certificates(roots)
        .with_no_client_auth();
    let tls = TlsConnectorLayer::tunnel(None)
        .with_connector_data(TlsConnectorDataBuilder::from(tls_config).build())
        .into_layer(TargetCheckedTcpConnector::new(state));
    HttpProxyConnector::optional(tls).serve(req).await
}

#[derive(Clone)]
pub(crate) struct UpstreamClient {
    connector: BoxService<
        Request<Body>,
        EstablishedClientConnection<HttpClientService<Body>, Request<Body>>,
        BoxError,
    >,
    proxy_config: ProxyConfig,
    state: Option<Arc<NetworkProxyState>>,
    tls_configuration_valid: bool,
}

impl UpstreamClient {
    pub(crate) fn direct(state: Arc<NetworkProxyState>) -> Self {
        Self::with_startup_roots(
            ProxyConfig {
                proxy_private_ips_via_upstream: state.proxy_private_ips_via_upstream,
                ..ProxyConfig::default()
            },
            state,
        )
    }

    pub(crate) fn from_env_proxy(state: Arc<NetworkProxyState>) -> Self {
        Self::with_startup_roots(
            ProxyConfig::from_env(state.proxy_private_ips_via_upstream),
            state,
        )
    }

    pub(crate) fn direct_with_tls_root_store(
        state: Arc<NetworkProxyState>,
        tls_root_store: Arc<rustls::RootCertStore>,
    ) -> Self {
        Self::new(
            ProxyConfig {
                proxy_private_ips_via_upstream: state.proxy_private_ips_via_upstream,
                ..ProxyConfig::default()
            },
            state,
            tls_root_store,
        )
    }

    pub(crate) fn from_env_proxy_with_tls_root_store(
        state: Arc<NetworkProxyState>,
        tls_root_store: Arc<rustls::RootCertStore>,
    ) -> Self {
        Self::new(
            ProxyConfig::from_env(state.proxy_private_ips_via_upstream),
            state,
            tls_root_store,
        )
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn unix_socket(path: &str) -> Self {
        let connector = build_unix_connector(path);
        Self {
            connector,
            proxy_config: ProxyConfig::default(),
            state: None,
            tls_configuration_valid: true,
        }
    }

    fn new(
        proxy_config: ProxyConfig,
        state: Arc<NetworkProxyState>,
        tls_root_store: Arc<rustls::RootCertStore>,
    ) -> Self {
        let connector = build_http_connector(
            TargetCheckedTcpConnector::new(state.clone()),
            tls_root_store,
        );
        Self {
            connector,
            proxy_config,
            state: Some(state),
            tls_configuration_valid: true,
        }
    }

    fn with_startup_roots(proxy_config: ProxyConfig, state: Arc<NetworkProxyState>) -> Self {
        let roots = startup_upstream_tls_roots().ok();
        let mut client = Self::new(
            proxy_config,
            state,
            roots
                .clone()
                .unwrap_or_else(|| Arc::new(rustls::RootCertStore::empty())),
        );
        client.tls_configuration_valid = roots.is_some();
        client
    }
}

impl Service<Request<Body>> for UpstreamClient {
    type Output = Response;
    type Error = OpaqueError;

    async fn serve(&self, mut req: Request<Body>) -> Result<Self::Output, Self::Error> {
        // Never forward the downstream listener's credentials or stale proxy auth to an origin.
        // The selected next hop gets fresh credentials immediately before its HTTP request.
        req.headers_mut()
            .remove(rama_http::header::PROXY_AUTHORIZATION);
        if !self.tls_configuration_valid {
            return Err(OpaqueError::from_display(
                "upstream TLS trust configuration unavailable; refusing connection",
            ));
        }
        let request_context = RequestContext::try_from(&req).ok();
        let authority = request_context
            .as_ref()
            .map(|ctx| ctx.host_with_port().to_string())
            .unwrap_or_else(|| "<unknown>".to_string());
        let proxy = if let Some(state) = &self.state {
            let allow_upstream_proxy = state
                .allow_upstream_proxy()
                .await
                .map_err(|_| OpaqueError::from_display("upstream proxy setting unavailable"))?;
            request_context.as_ref().map_or_else(
                || {
                    self.proxy_config
                        .proxy_for_protocol(/*is_secure*/ false, allow_upstream_proxy)
                },
                |ctx| {
                    self.proxy_config.proxy_for_target(
                        &ctx.host_with_port(),
                        ctx.protocol.is_secure(),
                        allow_upstream_proxy,
                    )
                },
            )?
        } else {
            // Unix sockets are local IPC, not network destinations.
            None
        };
        if proxy.is_some() && codex_required_proxy::is_active() {
            let target = request_context
                .as_ref()
                .ok_or_else(|| {
                    OpaqueError::from_display(
                        "required upstream target is unavailable; refusing connection",
                    )
                })?
                .host_with_port();
            check_required_upstream_destination(
                self.state
                    .as_ref()
                    .ok_or_else(|| OpaqueError::from_display("network state unavailable"))?,
                &target,
                req.extensions(),
            )
            .await?;
        }
        match proxy.as_ref() {
            Some(proxy) => info!(
                "HTTP upstream route selected (target={authority}, route=upstream_proxy, proxy={})",
                proxy.address
            ),
            None => info!("HTTP upstream route selected (target={authority}, route=direct)"),
        }
        if let Some(proxy) = proxy {
            req.extensions_mut().insert(proxy);
        }

        let connect_started_at = Instant::now();
        let EstablishedClientConnection {
            input: mut req,
            conn: http_connection,
        } = match self.connector.serve(req).await {
            Ok(connection) => {
                info!(
                    "HTTP upstream connection established (target={authority}, elapsed_ms={})",
                    connect_started_at.elapsed().as_millis()
                );
                connection
            }
            Err(err) => {
                warn!(
                    "HTTP upstream connection failed (target={authority}, elapsed_ms={})",
                    connect_started_at.elapsed().as_millis()
                );
                return Err(OpaqueError::from_boxed(err));
            }
        };

        req.extensions_mut()
            .extend(http_connection.extensions().clone());

        let request_started_at = Instant::now();
        // Rama's connector authenticates CONNECT itself. Plain HTTP through an HTTP(S) proxy
        // needs this additional layer; it deliberately omits Basic auth inside HTTPS tunnels.
        let http_connection = SetProxyAuthHttpHeaderLayer::new().into_layer(http_connection);
        match http_connection.serve(req).await {
            Ok(resp) => {
                info!(
                    "HTTP upstream response headers received (target={authority}, elapsed_ms={})",
                    request_started_at.elapsed().as_millis()
                );
                Ok(resp)
            }
            Err(err) => {
                warn!(
                    "HTTP upstream response headers failed (target={authority}, elapsed_ms={})",
                    request_started_at.elapsed().as_millis()
                );
                Err(OpaqueError::from_boxed(err).context("HTTP upstream request failed"))
            }
        }
    }
}

fn build_http_connector(
    transport: TargetCheckedTcpConnector,
    tls_root_store: Arc<rustls::RootCertStore>,
) -> BoxService<
    Request<Body>,
    EstablishedClientConnection<HttpClientService<Body>, Request<Body>>,
    BoxError,
> {
    ensure_rustls_crypto_provider();
    let client_config = rustls::ClientConfig::builder_with_protocol_versions(rustls::ALL_VERSIONS)
        .with_root_certificates(tls_root_store)
        .with_no_client_auth();
    let proxy_tls_config = TlsConnectorDataBuilder::from(client_config.clone()).build();
    let tls_config = TlsConnectorDataBuilder::from(client_config)
        .with_alpn_protocols_http_auto()
        .build();
    let proxy_tls = TlsConnectorLayer::tunnel(None)
        .with_connector_data(proxy_tls_config)
        .into_layer(transport);
    let proxy = HttpProxyConnectorLayer::optional().into_layer(proxy_tls);
    let tls = TlsConnectorLayer::auto()
        .with_connector_data(tls_config)
        .into_layer(proxy);
    let tls = RequestVersionAdapter::new(tls).with_default_version(Version::HTTP_11);
    let connector = HttpConnector::new(tls);
    connector.boxed()
}

#[cfg(test)]
#[path = "upstream_tests.rs"]
mod tests;

/// Each global-policy regression gets a fresh OnceLock without affecting parallel unit tests.
#[cfg(test)]
pub(crate) fn run_required_proxy_test_in_child(test_name: &str) -> bool {
    run_required_proxy_test_in_child_with_env(test_name, &[])
}

#[cfg(test)]
pub(crate) fn run_required_proxy_test_in_child_with_env(
    test_name: &str,
    env: &[(&str, String)],
) -> bool {
    const CHILD: &str = "CODEX_NETWORK_PROXY_REQUIRED_TEST_CHILD";
    if std::env::var(CHILD).as_deref() == Ok(test_name) {
        return false;
    }
    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", test_name, "--nocapture"])
        .env(CHILD, test_name);
    for key in crate::certs::CUSTOM_CA_ENV_KEYS
        .into_iter()
        .chain([crate::certs::SSL_CERT_DIR_ENV_KEY])
    {
        command.env_remove(key);
    }
    let output = command
        .envs(env.iter().map(|(key, value)| (*key, value)))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "isolated test failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    true
}

#[cfg(target_os = "macos")]
fn build_unix_connector(
    path: &str,
) -> BoxService<
    Request<Body>,
    EstablishedClientConnection<HttpClientService<Body>, Request<Body>>,
    BoxError,
> {
    let transport = UnixConnector::fixed(path);
    let connector = HttpConnector::new(transport);
    connector.boxed()
}
