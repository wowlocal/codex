use crate::attribution::BindConnectionAttribution;
use crate::config::NetworkMode;
use crate::connect_policy::DialPolicy;
use crate::connect_policy::TargetCheckedTcpConnector;
use crate::connection_lifecycle::CancelOnShutdown;
use crate::mitm;
use crate::network_policy::BlockDecisionAuditEventArgs;
use crate::network_policy::NetworkDecision;
use crate::network_policy::NetworkDecisionSource;
use crate::network_policy::NetworkPolicyDecider;
use crate::network_policy::NetworkPolicyDecision;
use crate::network_policy::NetworkPolicyRequest;
use crate::network_policy::NetworkPolicyRequestArgs;
use crate::network_policy::NetworkProtocol;
use crate::network_policy::emit_block_decision_audit_event;
use crate::network_policy::evaluate_host_policy;
use crate::network_policy::evaluate_host_policy_for_dial;
use crate::policy::normalize_host;
use crate::policy::numeric_host_address;
use crate::reasons::REASON_METHOD_NOT_ALLOWED;
use crate::reasons::REASON_MITM_REQUIRED;
use crate::reasons::REASON_PROXY_DISABLED;
use crate::responses::PolicyDecisionDetails;
use crate::responses::blocked_message_with_policy;
use crate::runtime::HostMitmRequirement;
use crate::state::BlockedRequest;
use crate::state::BlockedRequestArgs;
use crate::state::NetworkProxyState;
use crate::upstream::UpstreamTunnel;
use crate::upstream::connect_tunnel;
use crate::upstream::required_proxy_for_target;
use anyhow::Context as _;
use anyhow::Result;
use rama_core::Service;
use rama_core::error::BoxError;
use rama_core::extensions::Extensions;
use rama_core::extensions::ExtensionsMut;
use rama_core::extensions::ExtensionsRef;
use rama_core::graceful::ShutdownGuard;
use rama_core::service::BoxService;
use rama_core::service::service_fn;
use rama_core::stream::rewind::Rewind;
use rama_net::address::HostWithPort;
use rama_net::client::EstablishedClientConnection;
use rama_net::proxy::ProxyRequest;
use rama_net::proxy::ProxyTarget;
use rama_net::proxy::StreamForwardService;
use rama_net::stream::ClientSocketInfo;
use rama_net::stream::Socket;
use rama_net::stream::SocketInfo;
use rama_socks5::Socks5Acceptor;
use rama_socks5::server::DefaultConnector;
use rama_socks5::server::DefaultUdpRelay;
use rama_socks5::server::udp::RelayRequest;
use rama_socks5::server::udp::RelayResponse;
use rama_tcp::TcpStream;
use rama_tcp::client::Request as TcpRequest;
use rama_tcp::server::TcpListener;
use std::io;
use std::net::SocketAddr;
use std::net::TcpListener as StdTcpListener;
use std::pin::Pin;
use std::sync::Arc;
use std::task::Context as TaskContext;
use std::task::Poll;
use std::time::Instant;
use tokio::io::AsyncRead;
use tokio::io::AsyncWrite;
use tokio::io::ReadBuf;
use tracing::error;
use tracing::info;
use tracing::warn;

pub async fn run_socks5(
    state: Arc<NetworkProxyState>,
    addr: SocketAddr,
    policy_decider: Option<Arc<dyn NetworkPolicyDecider>>,
    environment_id: Option<String>,
    enable_socks5_udp: bool,
    guard: ShutdownGuard,
) -> Result<()> {
    let listener = TcpListener::build()
        .bind(addr)
        .await
        // See `http_proxy.rs` for details on why we wrap `BoxError` before converting to anyhow.
        .map_err(rama_core::error::OpaqueError::from)
        .map_err(anyhow::Error::from)
        .with_context(|| format!("bind SOCKS5 proxy: {addr}"))?;

    run_socks5_with_listener(
        state,
        listener,
        policy_decider,
        environment_id,
        enable_socks5_udp,
        guard,
    )
    .await
}

pub async fn run_socks5_with_std_listener(
    state: Arc<NetworkProxyState>,
    listener: StdTcpListener,
    policy_decider: Option<Arc<dyn NetworkPolicyDecider>>,
    environment_id: Option<String>,
    enable_socks5_udp: bool,
    guard: ShutdownGuard,
) -> Result<()> {
    let listener =
        TcpListener::try_from(listener).context("convert std listener to SOCKS5 proxy listener")?;
    run_socks5_with_listener(
        state,
        listener,
        policy_decider,
        environment_id,
        enable_socks5_udp,
        guard,
    )
    .await
}

async fn run_socks5_with_listener(
    state: Arc<NetworkProxyState>,
    listener: TcpListener,
    policy_decider: Option<Arc<dyn NetworkPolicyDecider>>,
    environment_id: Option<String>,
    enable_socks5_udp: bool,
    guard: ShutdownGuard,
) -> Result<()> {
    let addr = listener
        .local_addr()
        .context("read SOCKS5 listener local addr")?;

    info!("SOCKS5 proxy listening on {addr}");

    match state.network_mode().await {
        Ok(NetworkMode::Limited) => {
            info!(
                "SOCKS5 UDP and non-HTTPS SOCKS5 TCP are blocked in limited mode; HTTPS SOCKS5 TCP requires MITM inspection"
            );
        }
        Ok(NetworkMode::Full) => {}
        Err(err) => {
            warn!("failed to read network mode: {err}");
        }
    }

    listener
        .serve_graceful(
            guard,
            CancelOnShutdown::new(socks5_proxy_service(
                state,
                policy_decider,
                environment_id,
                enable_socks5_udp,
            )),
        )
        .await;
    Ok(())
}

pub(crate) fn socks5_proxy_service(
    state: Arc<NetworkProxyState>,
    policy_decider: Option<Arc<dyn NetworkPolicyDecider>>,
    environment_id: Option<String>,
    enable_socks5_udp: bool,
) -> BoxService<TcpStream, (), BoxError> {
    let policy_tcp_connector = service_fn({
        let policy_decider = policy_decider.clone();
        let environment_id = environment_id.clone();
        move |req: TcpRequest| {
            let policy_decider = policy_decider.clone();
            let environment_id = environment_id.clone();
            async move { handle_socks5_tcp(req, policy_decider, environment_id).await }
        }
    });

    let socks_proxy = service_fn(|request| async move { proxy_socks5_tcp(request).await });
    let socks_connector = DefaultConnector::default()
        .with_connector(policy_tcp_connector)
        .with_service(socks_proxy);
    let base = Socks5Acceptor::new().with_connector(socks_connector);

    if enable_socks5_udp {
        let udp_state = state.clone();
        let udp_decider = policy_decider.clone();
        let udp_relay =
            DefaultUdpRelay::default().with_async_inspector(service_fn({
                let environment_id = environment_id.clone();
                move |request: RelayRequest| {
                    let udp_state = udp_state.clone();
                    let udp_decider = udp_decider.clone();
                    let environment_id = environment_id.clone();
                    async move {
                        inspect_socks5_udp(request, udp_state, udp_decider, environment_id).await
                    }
                }
            }));
        let socks_acceptor = base.with_udp_associator(udp_relay);
        BindConnectionAttribution::new(socks_acceptor, state, environment_id).boxed()
    } else {
        BindConnectionAttribution::new(base, state, environment_id).boxed()
    }
}

async fn handle_socks5_tcp(
    mut req: TcpRequest,
    policy_decider: Option<Arc<dyn NetworkPolicyDecider>>,
    environment_id: Option<String>,
) -> Result<EstablishedClientConnection<Socks5TcpConnection, TcpRequest>, BoxError> {
    let app_state = req
        .extensions()
        .get::<Arc<NetworkProxyState>>()
        .cloned()
        .ok_or_else(|| io::Error::other("missing state"))?;

    let host = normalize_host(&req.authority.host.to_string());
    let port = req.authority.port;
    let target = req.authority.clone();
    if host.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid host").into());
    }

    let client = req
        .extensions()
        .get::<SocketInfo>()
        .map(|info| info.peer_addr().to_string());

    match app_state.enabled().await {
        Ok(true) => {}
        Ok(false) => {
            emit_socks_block_decision_audit_event(
                &app_state,
                NetworkDecisionSource::ProxyState,
                REASON_PROXY_DISABLED,
                NetworkProtocol::Socks5Tcp,
                host.as_str(),
                port,
                client.as_deref(),
            );
            let details = PolicyDecisionDetails {
                decision: NetworkPolicyDecision::Deny,
                reason: REASON_PROXY_DISABLED,
                source: NetworkDecisionSource::ProxyState,
                protocol: NetworkProtocol::Socks5Tcp,
                host: &host,
                port,
            };
            let _ = app_state
                .record_blocked(BlockedRequest::new(BlockedRequestArgs {
                    host: host.clone(),
                    reason: REASON_PROXY_DISABLED.to_string(),
                    client: client.clone(),
                    method: None,
                    mode: None,
                    protocol: "socks5".to_string(),
                    decision: Some(details.decision.as_str().to_string()),
                    source: Some(details.source.as_str().to_string()),
                    port: Some(port),
                }))
                .await;
            let client = client.as_deref().unwrap_or_default();
            warn!("SOCKS blocked; proxy disabled (client={client}, host={host})");
            return Err(policy_denied_error(REASON_PROXY_DISABLED, &details).into());
        }
        Err(err) => {
            error!("failed to read enabled state: {err}");
            return Err(io::Error::other("proxy error").into());
        }
    }

    let mode = match app_state.network_mode().await {
        Ok(mode) => mode,
        Err(err) => {
            error!("failed to evaluate method policy: {err}");
            return Err(io::Error::other("proxy error").into());
        }
    };
    let host_mitm_requirement = match app_state.host_mitm_requirement(&host, port).await {
        Ok(requirement) => requirement,
        Err(err) => {
            error!("failed to inspect MITM requirements for {host}: {err}");
            return Err(io::Error::other("proxy error").into());
        }
    };
    // Otherwise retain the existing limited-mode restriction to the default HTTPS port.
    let brokered_http = matches!(host_mitm_requirement, HostMitmRequirement::Credential(protocols) if protocols.http);
    let socks5_tcp_target_is_https = port == 443;
    if mode == NetworkMode::Limited && !socks5_tcp_target_is_https && !brokered_http {
        emit_socks_block_decision_audit_event(
            &app_state,
            NetworkDecisionSource::ModeGuard,
            REASON_METHOD_NOT_ALLOWED,
            NetworkProtocol::Socks5Tcp,
            host.as_str(),
            port,
            client.as_deref(),
        );
        let details = PolicyDecisionDetails {
            decision: NetworkPolicyDecision::Deny,
            reason: REASON_METHOD_NOT_ALLOWED,
            source: NetworkDecisionSource::ModeGuard,
            protocol: NetworkProtocol::Socks5Tcp,
            host: &host,
            port,
        };
        let _ = app_state
            .record_blocked(BlockedRequest::new(BlockedRequestArgs {
                host: host.clone(),
                reason: REASON_METHOD_NOT_ALLOWED.to_string(),
                client: client.clone(),
                method: None,
                mode: Some(NetworkMode::Limited),
                protocol: "socks5".to_string(),
                decision: Some(details.decision.as_str().to_string()),
                source: Some(details.source.as_str().to_string()),
                port: Some(port),
            }))
            .await;
        let client = client.as_deref().unwrap_or_default();
        warn!(
            "SOCKS blocked; limited mode only supports HTTPS MITM (client={client}, host={host}, port={port})"
        );
        return Err(policy_denied_error(REASON_METHOD_NOT_ALLOWED, &details).into());
    }

    let request = NetworkPolicyRequest::new(NetworkPolicyRequestArgs {
        protocol: NetworkProtocol::Socks5Tcp,
        host: host.clone(),
        port,
        environment_id,
        client_addr: client.clone(),
        method: None,
        command: None,
        exec_policy_hint: None,
    });

    match evaluate_host_policy_for_dial(
        &app_state,
        policy_decider.as_ref(),
        &request,
        Some((&target, req.extensions_mut())),
    )
    .await
    {
        Ok(NetworkDecision::Deny {
            reason,
            source,
            decision,
        }) => {
            let details = PolicyDecisionDetails {
                decision,
                reason: &reason,
                source,
                protocol: NetworkProtocol::Socks5Tcp,
                host: &host,
                port,
            };
            let _ = app_state
                .record_blocked(BlockedRequest::new(BlockedRequestArgs {
                    host: host.clone(),
                    reason: reason.clone(),
                    client: client.clone(),
                    method: None,
                    mode: None,
                    protocol: "socks5".to_string(),
                    decision: Some(details.decision.as_str().to_string()),
                    source: Some(details.source.as_str().to_string()),
                    port: Some(port),
                }))
                .await;
            let client = client.as_deref().unwrap_or_default();
            warn!("SOCKS blocked (client={client}, host={host}, reason={reason})");
            return Err(policy_denied_error(&reason, &details).into());
        }
        Ok(NetworkDecision::Allow) => {
            let client = client.as_deref().unwrap_or_default();
            info!("SOCKS allowed (client={client}, host={host}, port={port})");
        }
        Err(err) => {
            error!("failed to evaluate host: {err}");
            return Err(io::Error::other("proxy error").into());
        }
    }

    let mitm_state = match app_state.mitm_state().await {
        Ok(state) => state,
        Err(err) => {
            error!("failed to load MITM state: {err}");
            return Err(io::Error::other("proxy error").into());
        }
    };
    let socks_mitm_mode = if mode == NetworkMode::Limited && !brokered_http {
        SocksMitmMode::Enabled
    } else {
        match host_mitm_requirement {
            HostMitmRequirement::None => SocksMitmMode::Disabled,
            HostMitmRequirement::Credential(protocols) => SocksMitmMode::DetectProtocol(protocols),
            HostMitmRequirement::Always => SocksMitmMode::Enabled,
        }
    };
    let unsupported_hook_protocol =
        host_mitm_requirement == HostMitmRequirement::Always && !socks5_tcp_target_is_https;
    if unsupported_hook_protocol
        || (socks_mitm_mode != SocksMitmMode::Disabled && mitm_state.is_none())
    {
        emit_socks_block_decision_audit_event(
            &app_state,
            NetworkDecisionSource::ModeGuard,
            REASON_MITM_REQUIRED,
            NetworkProtocol::Socks5Tcp,
            host.as_str(),
            port,
            client.as_deref(),
        );
        let details = PolicyDecisionDetails {
            decision: NetworkPolicyDecision::Deny,
            reason: REASON_MITM_REQUIRED,
            source: NetworkDecisionSource::ModeGuard,
            protocol: NetworkProtocol::Socks5Tcp,
            host: &host,
            port,
        };
        let _ = app_state
            .record_blocked(BlockedRequest::new(BlockedRequestArgs {
                host: host.clone(),
                reason: REASON_MITM_REQUIRED.to_string(),
                client: client.clone(),
                method: None,
                mode: Some(mode),
                protocol: "socks5".to_string(),
                decision: Some(details.decision.as_str().to_string()),
                source: Some(details.source.as_str().to_string()),
                port: Some(port),
            }))
            .await;
        let client = client.as_deref().unwrap_or_default();
        warn!(
            "SOCKS blocked; MITM required to enforce HTTPS policy (client={client}, host={host}, mode={mode:?}, host_mitm_requirement={host_mitm_requirement:?}, https_target={socks5_tcp_target_is_https})"
        );
        return Err(policy_denied_error(REASON_MITM_REQUIRED, &details).into());
    }

    if let Some(mitm_state) = mitm_state {
        let client = client.as_deref().unwrap_or_default();
        let mut extensions = Extensions::new();
        if let Some(policy) = req.extensions().get::<DialPolicy>() {
            extensions.insert(policy.clone());
        }
        let conn = match socks_mitm_mode {
            SocksMitmMode::Disabled => None,
            SocksMitmMode::Enabled => Some(Socks5TcpConnection::Mitm {
                target,
                mode,
                mitm: mitm_state,
                extensions,
            }),
            SocksMitmMode::DetectProtocol(protocols) => Some(Socks5TcpConnection::DetectProtocol {
                protocols,
                target,
                mode,
                mitm: mitm_state,
                state: app_state.clone(),
                extensions,
            }),
        };
        if let Some(conn) = conn {
            info!(
                "SOCKS MITM selected (client={client}, host={host}, port={port}, mode={mode:?}, mitm_mode={socks_mitm_mode:?})"
            );
            return Ok(EstablishedClientConnection { input: req, conn });
        }
    }

    info!("SOCKS upstream dial started (host={host}, port={port})");
    let connect_started_at = Instant::now();
    let result = connect_opaque_tcp(app_state, req).await;
    match &result {
        Ok(_) => info!(
            "SOCKS upstream dial established (host={host}, port={port}, elapsed_ms={})",
            connect_started_at.elapsed().as_millis()
        ),
        Err(_) => warn!(
            "SOCKS upstream dial failed (host={host}, port={port}, elapsed_ms={})",
            connect_started_at.elapsed().as_millis()
        ),
    }
    result
}

async fn connect_opaque_tcp(
    state: Arc<NetworkProxyState>,
    req: TcpRequest,
) -> Result<EstablishedClientConnection<Socks5TcpConnection, TcpRequest>, BoxError> {
    if let Some(required) = codex_required_proxy::current() {
        if state.tcp_upstream_proxy.is_some() {
            // The executor-provisioned next hop is mandatory, including direct-list targets.
            // Chaining two independently mandatory HTTP proxies is not supported here.
            return Err("required proxy is incompatible with the executor's mandatory TCP upstream; refusing connection".into());
        }
        let proxy = required_proxy_for_target(
            &required,
            &req.authority,
            state.allow_upstream_proxy().await?,
            state.proxy_private_ips_via_upstream,
        )?;
        if let Some(proxy) = proxy {
            let host = normalize_host(&req.authority.host.to_string());
            let target_ip = numeric_host_address(&host);
            let same_port = req.authority.port == proxy.address.port;
            if same_port && host == normalize_host(&proxy.address.host.to_string()) {
                return Err("cannot tunnel back into the required upstream proxy".into());
            }
            let EstablishedClientConnection { input, conn } =
                connect_tunnel(state, req, Some(proxy)).await?;
            if same_port && let Some(ip) = target_ip {
                let peer = conn
                    .extensions()
                    .get::<ClientSocketInfo>()
                    .ok_or("missing required upstream peer address")?
                    .0
                    .peer_addr();
                if ip.to_canonical() == peer.ip().to_canonical() {
                    return Err("cannot tunnel back into the required upstream proxy".into());
                }
            }
            return Ok(EstablishedClientConnection {
                input,
                conn: Socks5TcpConnection::RequiredProxy(Box::new(RequiredProxyTunnel::new(conn))),
            });
        }
        // An explicit [proxy] direct destination (including implicit loopback) must not be
        // diverted to the environment or executor's alternative TCP upstream.
        return connect_direct_opaque_tcp(state, req).await;
    }
    if let Some(proxy) = &state.tcp_upstream_proxy {
        let tunnel = proxy
            .connect(
                &state,
                &req.authority,
                DialPolicy::authorization(req.extensions(), &req.authority),
            )
            .await?;
        return Ok(EstablishedClientConnection {
            input: req,
            conn: Socks5TcpConnection::Tunnel(tunnel),
        });
    }
    connect_direct_opaque_tcp(state, req).await
}

async fn connect_direct_opaque_tcp(
    state: Arc<NetworkProxyState>,
    req: TcpRequest,
) -> Result<EstablishedClientConnection<Socks5TcpConnection, TcpRequest>, BoxError> {
    let EstablishedClientConnection { input, conn } =
        TargetCheckedTcpConnector::new(state).serve(req).await?;
    Ok(EstablishedClientConnection {
        input,
        conn: Socks5TcpConnection::Direct(conn),
    })
}

/// Internal connector output for SOCKS5 TCP. MITM requests do not dial upstream before the
/// inner HTTPS request is inspected, so they carry the target metadata instead of a socket.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SocksMitmMode {
    Disabled,
    Enabled,
    DetectProtocol(crate::brokered_tunnel::BrokeredProtocols),
}

#[derive(Debug)]
struct RequiredProxyTunnel {
    // Rama's upgraded HTTP stream is Send, but not Sync. The Socket contract requires Sync;
    // shared access exposes only a snapshot of metadata, while I/O always has exclusive &mut
    // access. Mutex supplies that contract without unsafe or a lock held across polling/await.
    stream: std::sync::Mutex<UpstreamTunnel>,
    extensions: Extensions,
}

impl RequiredProxyTunnel {
    fn new(stream: UpstreamTunnel) -> Self {
        let extensions = stream.extensions().clone();
        Self {
            stream: std::sync::Mutex::new(stream),
            extensions,
        }
    }

    fn stream_mut(&mut self) -> &mut UpstreamTunnel {
        self.stream
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[derive(Debug)]
enum Socks5TcpConnection {
    Direct(TcpStream),
    Tunnel(Rewind<TcpStream>),
    RequiredProxy(Box<RequiredProxyTunnel>),
    Mitm {
        target: HostWithPort,
        mode: NetworkMode,
        mitm: Arc<mitm::MitmState>,
        extensions: Extensions,
    },
    DetectProtocol {
        protocols: crate::brokered_tunnel::BrokeredProtocols,
        target: HostWithPort,
        mode: NetworkMode,
        mitm: Arc<mitm::MitmState>,
        state: Arc<NetworkProxyState>,
        extensions: Extensions,
    },
}

impl AsyncRead for Socks5TcpConnection {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Direct(stream) => Pin::new(stream).poll_read(cx, buf),
            Self::Tunnel(stream) => Pin::new(stream).poll_read(cx, buf),
            Self::RequiredProxy(stream) => Pin::new(stream.stream_mut()).poll_read(cx, buf),
            Self::Mitm { .. } | Self::DetectProtocol { .. } => Poll::Ready(Ok(())),
        }
    }
}

impl AsyncWrite for Socks5TcpConnection {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            Self::Direct(stream) => Pin::new(stream).poll_write(cx, buf),
            Self::Tunnel(stream) => Pin::new(stream).poll_write(cx, buf),
            Self::RequiredProxy(stream) => Pin::new(stream.stream_mut()).poll_write(cx, buf),
            Self::Mitm { .. } | Self::DetectProtocol { .. } => Poll::Ready(Ok(buf.len())),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Direct(stream) => Pin::new(stream).poll_flush(cx),
            Self::Tunnel(stream) => Pin::new(stream).poll_flush(cx),
            Self::RequiredProxy(stream) => Pin::new(stream.stream_mut()).poll_flush(cx),
            Self::Mitm { .. } | Self::DetectProtocol { .. } => Poll::Ready(Ok(())),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Direct(stream) => Pin::new(stream).poll_shutdown(cx),
            Self::Tunnel(stream) => Pin::new(stream).poll_shutdown(cx),
            Self::RequiredProxy(stream) => Pin::new(stream.stream_mut()).poll_shutdown(cx),
            Self::Mitm { .. } | Self::DetectProtocol { .. } => Poll::Ready(Ok(())),
        }
    }
}

impl Socket for Socks5TcpConnection {
    fn local_addr(&self) -> io::Result<SocketAddr> {
        match self {
            Self::Direct(stream) => stream.local_addr(),
            Self::Tunnel(_)
            | Self::RequiredProxy(_)
            | Self::Mitm { .. }
            | Self::DetectProtocol { .. } => Ok(SocketAddr::from(([0, 0, 0, 0], 0))),
        }
    }

    fn peer_addr(&self) -> io::Result<SocketAddr> {
        match self {
            Self::Direct(stream) => stream.peer_addr(),
            Self::Tunnel(_)
            | Self::RequiredProxy(_)
            | Self::Mitm { .. }
            | Self::DetectProtocol { .. } => Ok(SocketAddr::from(([0, 0, 0, 0], 0))),
        }
    }
}

impl ExtensionsRef for Socks5TcpConnection {
    fn extensions(&self) -> &Extensions {
        match self {
            Self::Direct(stream) => stream.extensions(),
            Self::Tunnel(stream) => stream.extensions(),
            Self::RequiredProxy(stream) => &stream.extensions,
            Self::Mitm { extensions, .. } | Self::DetectProtocol { extensions, .. } => extensions,
        }
    }
}

impl ExtensionsMut for Socks5TcpConnection {
    fn extensions_mut(&mut self) -> &mut Extensions {
        match self {
            Self::Direct(stream) => stream.extensions_mut(),
            Self::Tunnel(stream) => stream.extensions_mut(),
            Self::RequiredProxy(stream) => &mut stream.extensions,
            Self::Mitm { extensions, .. } | Self::DetectProtocol { extensions, .. } => extensions,
        }
    }
}

async fn proxy_socks5_tcp(
    request: ProxyRequest<TcpStream, Socks5TcpConnection>,
) -> Result<(), BoxError> {
    let ProxyRequest { mut source, target } = request;
    match target {
        target @ (Socks5TcpConnection::Direct(_)
        | Socks5TcpConnection::Tunnel(_)
        | Socks5TcpConnection::RequiredProxy(_)) => StreamForwardService::default()
            .serve(ProxyRequest { source, target })
            .await
            .map_err(Into::into),
        Socks5TcpConnection::Mitm {
            target,
            mode,
            mitm,
            extensions,
        } => {
            if let Some(policy) = extensions.get::<DialPolicy>() {
                source.extensions_mut().insert(policy.clone());
            }
            source.extensions_mut().insert(ProxyTarget(target));
            source.extensions_mut().insert(mode);
            source.extensions_mut().insert(mitm);
            mitm::mitm_stream(source, rama_http::uri::Scheme::HTTPS)
                .await
                .map_err(Into::into)
        }
        Socks5TcpConnection::DetectProtocol {
            protocols,
            target,
            mode,
            mitm,
            state,
            extensions,
        } => {
            if let Some(policy) = extensions.get::<DialPolicy>() {
                source.extensions_mut().insert(policy.clone());
            }
            source.extensions_mut().insert(ProxyTarget(target.clone()));
            source.extensions_mut().insert(mode);
            source.extensions_mut().insert(mitm);
            let (protocol, source) = crate::brokered_tunnel::peek_protocol(source, protocols)
                .await
                .map_err(|err| -> BoxError { err.into() })?;
            match protocol {
                crate::brokered_tunnel::TunnelProtocol::Tls => {
                    mitm::mitm_stream(source, rama_http::uri::Scheme::HTTPS)
                        .await
                        .map_err(Into::into)
                }
                crate::brokered_tunnel::TunnelProtocol::Http => {
                    mitm::mitm_stream(source, rama_http::uri::Scheme::HTTP)
                        .await
                        .map_err(Into::into)
                }
                crate::brokered_tunnel::TunnelProtocol::Opaque => {
                    if mode == NetworkMode::Limited {
                        return Err(io::Error::new(
                            io::ErrorKind::PermissionDenied,
                            "opaque tunnels are not allowed in limited mode",
                        )
                        .into());
                    }
                    info!("SOCKS opaque upstream dial started (target={target})");
                    let connect_started_at = Instant::now();
                    let EstablishedClientConnection { conn: upstream, .. } = connect_opaque_tcp(
                        state,
                        TcpRequest::new_with_extensions(target.clone(), extensions),
                    )
                    .await?;
                    info!(
                        "SOCKS opaque upstream dial established (target={target}, elapsed_ms={})",
                        connect_started_at.elapsed().as_millis()
                    );
                    StreamForwardService::default()
                        .serve(ProxyRequest {
                            source,
                            target: upstream,
                        })
                        .await
                        .map_err(Into::into)
                }
            }
        }
    }
}

async fn inspect_socks5_udp(
    request: RelayRequest,
    state: Arc<NetworkProxyState>,
    policy_decider: Option<Arc<dyn NetworkPolicyDecider>>,
    environment_id: Option<String>,
) -> io::Result<RelayResponse> {
    let RelayRequest {
        server_address,
        payload,
        extensions,
        ..
    } = request;

    let host = normalize_host(&server_address.ip_addr.to_string());
    let port = server_address.port;
    if host.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid host"));
    }

    let client = extensions
        .get::<SocketInfo>()
        .map(|info| info.peer_addr().to_string());

    match state.enabled().await {
        Ok(true) => {}
        Ok(false) => {
            emit_socks_block_decision_audit_event(
                &state,
                NetworkDecisionSource::ProxyState,
                REASON_PROXY_DISABLED,
                NetworkProtocol::Socks5Udp,
                host.as_str(),
                port,
                client.as_deref(),
            );
            let details = PolicyDecisionDetails {
                decision: NetworkPolicyDecision::Deny,
                reason: REASON_PROXY_DISABLED,
                source: NetworkDecisionSource::ProxyState,
                protocol: NetworkProtocol::Socks5Udp,
                host: &host,
                port,
            };
            let _ = state
                .record_blocked(BlockedRequest::new(BlockedRequestArgs {
                    host: host.clone(),
                    reason: REASON_PROXY_DISABLED.to_string(),
                    client: client.clone(),
                    method: None,
                    mode: None,
                    protocol: "socks5-udp".to_string(),
                    decision: Some(details.decision.as_str().to_string()),
                    source: Some(details.source.as_str().to_string()),
                    port: Some(port),
                }))
                .await;
            let client = client.as_deref().unwrap_or_default();
            warn!("SOCKS UDP blocked; proxy disabled (client={client}, host={host})");
            return Err(policy_denied_error(REASON_PROXY_DISABLED, &details));
        }
        Err(err) => {
            error!("failed to read enabled state: {err}");
            return Err(io::Error::other("proxy error"));
        }
    }

    match state.network_mode().await {
        Ok(NetworkMode::Limited) => {
            emit_socks_block_decision_audit_event(
                &state,
                NetworkDecisionSource::ModeGuard,
                REASON_METHOD_NOT_ALLOWED,
                NetworkProtocol::Socks5Udp,
                host.as_str(),
                port,
                client.as_deref(),
            );
            let details = PolicyDecisionDetails {
                decision: NetworkPolicyDecision::Deny,
                reason: REASON_METHOD_NOT_ALLOWED,
                source: NetworkDecisionSource::ModeGuard,
                protocol: NetworkProtocol::Socks5Udp,
                host: &host,
                port,
            };
            let _ = state
                .record_blocked(BlockedRequest::new(BlockedRequestArgs {
                    host: host.clone(),
                    reason: REASON_METHOD_NOT_ALLOWED.to_string(),
                    client: client.clone(),
                    method: None,
                    mode: Some(NetworkMode::Limited),
                    protocol: "socks5-udp".to_string(),
                    decision: Some(details.decision.as_str().to_string()),
                    source: Some(details.source.as_str().to_string()),
                    port: Some(port),
                }))
                .await;
            return Err(policy_denied_error(REASON_METHOD_NOT_ALLOWED, &details));
        }
        Ok(NetworkMode::Full) => {}
        Err(err) => {
            error!("failed to evaluate method policy: {err}");
            return Err(io::Error::other("proxy error"));
        }
    }

    let request = NetworkPolicyRequest::new(NetworkPolicyRequestArgs {
        protocol: NetworkProtocol::Socks5Udp,
        host: host.clone(),
        port,
        environment_id,
        client_addr: client.clone(),
        method: None,
        command: None,
        exec_policy_hint: None,
    });

    match evaluate_host_policy(&state, policy_decider.as_ref(), &request).await {
        Ok(NetworkDecision::Deny {
            reason,
            source,
            decision,
        }) => {
            let details = PolicyDecisionDetails {
                decision,
                reason: &reason,
                source,
                protocol: NetworkProtocol::Socks5Udp,
                host: &host,
                port,
            };
            let _ = state
                .record_blocked(BlockedRequest::new(BlockedRequestArgs {
                    host: host.clone(),
                    reason: reason.clone(),
                    client: client.clone(),
                    method: None,
                    mode: None,
                    protocol: "socks5-udp".to_string(),
                    decision: Some(details.decision.as_str().to_string()),
                    source: Some(details.source.as_str().to_string()),
                    port: Some(port),
                }))
                .await;
            let client = client.as_deref().unwrap_or_default();
            warn!("SOCKS UDP blocked (client={client}, host={host}, reason={reason})");
            Err(policy_denied_error(&reason, &details))
        }
        Ok(NetworkDecision::Allow) => {
            if codex_required_proxy::current()
                .is_some_and(|required| !required.bypasses_host(&host))
            {
                // HTTP(S) CONNECT cannot carry UDP. Never let a sandbox's SOCKS UDP request
                // turn the mandatory TCP upstream into a direct datagram route.
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "SOCKS UDP cannot use the required upstream proxy; refusing direct datagram",
                ));
            }
            Ok(RelayResponse {
                maybe_payload: Some(payload),
                extensions,
            })
        }
        Err(err) => {
            error!("failed to evaluate UDP host: {err}");
            Err(io::Error::other("proxy error"))
        }
    }
}

fn emit_socks_block_decision_audit_event(
    state: &NetworkProxyState,
    source: NetworkDecisionSource,
    reason: &str,
    protocol: NetworkProtocol,
    host: &str,
    port: u16,
    client_addr: Option<&str>,
) {
    emit_block_decision_audit_event(
        state,
        BlockDecisionAuditEventArgs {
            source,
            reason,
            protocol,
            server_address: host,
            server_port: port,
            method: None,
            client_addr,
        },
    );
}

fn policy_denied_error(reason: &str, details: &PolicyDecisionDetails<'_>) -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        blocked_message_with_policy(reason, details),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::NetworkMode;
    use crate::config::NetworkProxyConfig;
    use crate::mitm_hook::MitmHookConfig;
    use crate::mitm_hook::MitmHookMatchConfig;
    use crate::network_policy::test_support::POLICY_DECISION_EVENT_NAME;
    use crate::network_policy::test_support::capture_events;
    use crate::network_policy::test_support::find_event_by_name;
    use crate::runtime::ConfigReloader;
    use crate::runtime::ConfigReloaderFuture;
    use crate::runtime::ConfigState;
    use crate::state::NetworkProxyConstraints;
    use crate::state::build_config_state;
    use crate::state::network_proxy_state_for_policy;
    use pretty_assertions::assert_eq;
    use rama_core::extensions::Extensions;
    use rama_core::extensions::ExtensionsMut;
    use rama_net::address::HostWithPort;
    use rama_net::address::SocketAddress;
    use rama_socks5::server::udp::RelayDirection;
    use std::collections::HashMap;
    use std::net::IpAddr;
    use std::net::Ipv4Addr;
    use std::sync::Arc;
    use std::sync::Mutex;

    // Managed MITM CA files live under the shared test CODEX_HOME, so MITM-enabled config state
    // must be materialized one test at a time.
    static MITM_CONFIG_STATE_LOCK: Mutex<()> = Mutex::new(());

    #[tokio::test]
    async fn required_proxy_never_overrides_executor_mandatory_tcp_upstream() {
        if crate::upstream::run_required_proxy_test_in_child(
            "socks5::tests::required_proxy_never_overrides_executor_mandatory_tcp_upstream",
        ) {
            return;
        }
        let required_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let executor_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let origin_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        for listener in [&required_listener, &executor_listener, &origin_listener] {
            listener.set_nonblocking(true).unwrap();
        }
        codex_required_proxy::install(
            codex_required_proxy::RequiredProxy::new(
                &format!(
                    "http://user:secret@{}",
                    required_listener.local_addr().unwrap()
                ),
                &["direct.example".into()],
            )
            .unwrap(),
            None,
        )
        .unwrap();
        let mut state = network_proxy_state_for_policy(NetworkProxyConfig {
            mode: NetworkMode::Full,
            allow_local_binding: Some(true),
            ..Default::default()
        });
        state.set_tcp_upstream_proxy(
            executor_listener
                .local_addr()
                .unwrap()
                .to_string()
                .parse()
                .unwrap(),
        );
        let state = Arc::new(state);
        for target in [
            HostWithPort::from(origin_listener.local_addr().unwrap()),
            "direct.example:5432".parse().unwrap(),
            "required.example:5432".parse().unwrap(),
        ] {
            let error = connect_opaque_tcp(state.clone(), TcpRequest::new(target))
                .await
                .unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("incompatible with the executor's mandatory TCP upstream")
            );
        }
        for listener in [&required_listener, &executor_listener, &origin_listener] {
            assert_eq!(
                listener.accept().unwrap_err().kind(),
                io::ErrorKind::WouldBlock
            );
        }
    }

    #[tokio::test]
    async fn required_proxy_covers_socks_tcp_and_refuses_unproxyable_udp() {
        if crate::upstream::run_required_proxy_test_in_child(
            "socks5::tests::required_proxy_covers_socks_tcp_and_refuses_unproxyable_udp",
        ) {
            return;
        }
        use codex_required_proxy::RequiredProxy;
        use tokio::io::AsyncReadExt;
        use tokio::io::AsyncWriteExt;

        let proxy = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let required = RequiredProxy::new(
            &format!("http://user:secret@{}", proxy.local_addr().unwrap()),
            &[],
        )
        .unwrap();
        codex_required_proxy::install(required, None).unwrap();
        let state = Arc::new(network_proxy_state_for_policy(NetworkProxyConfig {
            mode: NetworkMode::Full,
            allow_upstream_proxy: true,
            allow_local_binding: Some(true),
            ..Default::default()
        }));
        let target: HostWithPort = "db.invalid:5432".parse().unwrap();
        let caller = async {
            let connection = connect_opaque_tcp(state.clone(), TcpRequest::new(target.clone()))
                .await
                .unwrap();
            assert!(matches!(
                connection.conn,
                Socks5TcpConnection::RequiredProxy(_)
            ));
            let mut tunnel = connection.conn;
            let mut greeting = [0; 5];
            tunnel.read_exact(&mut greeting).await.unwrap();
            assert_eq!(&greeting, b"hello");
            tunnel.write_all(b"private").await.unwrap();
            tunnel.shutdown().await.unwrap();
        };
        let peer = async {
            let (mut stream, _) = proxy.accept().await.unwrap();
            let mut head = Vec::new();
            while !head.ends_with(b"\r\n\r\n") {
                assert!(head.len() < 16_384);
                head.push(stream.read_u8().await.unwrap());
            }
            let head = String::from_utf8(head).unwrap();
            assert!(head.starts_with("CONNECT db.invalid:5432 HTTP/1.1\r\n"));
            assert!(
                head.to_ascii_lowercase()
                    .contains("proxy-authorization: basic dxnlcjpzzwnyzxq=\r\n")
            );
            stream
                .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\nhello")
                .await
                .unwrap();
            let mut payload = Vec::new();
            stream.read_to_end(&mut payload).await.unwrap();
            assert_eq!(payload, b"private");
        };
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            tokio::join!(caller, peer);
        })
        .await
        .unwrap();

        let origin = std::net::TcpListener::bind("0.0.0.0:0").unwrap();
        origin.set_nonblocking(true).unwrap();
        let origin_target = HostWithPort::from(origin.local_addr().unwrap());
        let state = Arc::new(network_proxy_state_for_policy(NetworkProxyConfig {
            mode: NetworkMode::Full,
            allow_upstream_proxy: false,
            allow_local_binding: Some(true),
            ..Default::default()
        }));
        let error = connect_opaque_tcp(state.clone(), TcpRequest::new(origin_target))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("disabled by network policy"));
        assert!(!error.to_string().contains("secret"));
        assert_eq!(
            origin.accept().unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );

        let mut config = NetworkProxyConfig {
            mode: NetworkMode::Full,
            allow_local_binding: Some(true),
            ..Default::default()
        };
        config.set_allowed_domains(vec!["1.1.1.1".into(), "127.0.0.1".into()]);
        let state = Arc::new(network_proxy_state_for_policy(config));
        for (ip, permitted) in [("1.1.1.1", false), ("127.0.0.1", true)] {
            let result = inspect_socks5_udp(
                RelayRequest {
                    server_address: SocketAddress::new(ip.parse().unwrap(), 53),
                    payload: b"private".to_vec().into(),
                    extensions: Extensions::new(),
                    direction: RelayDirection::North,
                },
                state.clone(),
                None,
                None,
            )
            .await;
            assert_eq!(result.is_ok(), permitted, "{ip}");
            if !permitted {
                assert_eq!(result.unwrap_err().kind(), io::ErrorKind::PermissionDenied);
            }
        }
    }

    #[derive(Clone)]
    struct StaticReloader {
        state: ConfigState,
    }

    impl ConfigReloader for StaticReloader {
        fn maybe_reload(&self) -> ConfigReloaderFuture<'_, Option<ConfigState>> {
            Box::pin(async { Ok(None) })
        }

        fn reload_now(&self) -> ConfigReloaderFuture<'_, ConfigState> {
            Box::pin(async { Ok(self.state.clone()) })
        }

        fn source_label(&self) -> String {
            "static test reloader".to_string()
        }
    }

    fn state_for_settings(network: NetworkProxyConfig) -> Arc<NetworkProxyState> {
        let config = network;
        let _mitm_config_state_guard = config.mitm.then(|| MITM_CONFIG_STATE_LOCK.lock().unwrap());
        let state = build_config_state(
            config,
            NetworkProxyConstraints::default(),
            crate::Platform::native(),
        )
        .unwrap();
        let reloader = Arc::new(StaticReloader {
            state: state.clone(),
        });
        Arc::new(NetworkProxyState::with_reloader(state, reloader))
    }

    #[tokio::test(flavor = "current_thread")]
    async fn handle_socks5_tcp_emits_block_decision_for_proxy_disabled() {
        let state = state_for_settings(NetworkProxyConfig {
            enabled: false,
            mode: NetworkMode::Full,
            ..NetworkProxyConfig::default()
        });
        let mut request =
            TcpRequest::new(HostWithPort::try_from("example.com:443").expect("valid authority"));
        request.extensions_mut().insert(state.clone());

        let (result, events) = capture_events(|| async {
            handle_socks5_tcp(
                request, /*policy_decider*/ None, /*environment_id*/ None,
            )
            .await
        })
        .await;
        assert!(result.is_err(), "proxy-disabled request should be denied");

        let event = find_event_by_name(&events, POLICY_DECISION_EVENT_NAME)
            .expect("expected policy decision event");
        assert_eq!(event.field("network.policy.scope"), Some("non_domain"));
        assert_eq!(event.field("network.policy.decision"), Some("deny"));
        assert_eq!(event.field("network.policy.source"), Some("proxy_state"));
        assert_eq!(
            event.field("network.policy.reason"),
            Some(REASON_PROXY_DISABLED)
        );
        assert_eq!(
            event.field("network.transport.protocol"),
            Some("socks5_tcp")
        );
        assert_eq!(event.field("server.address"), Some("example.com"));
        assert_eq!(event.field("server.port"), Some("443"));
        assert_eq!(event.field("http.request.method"), Some("none"));
        assert_eq!(event.field("client.address"), Some("unknown"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn handle_socks5_tcp_uses_mitm_in_limited_mode() {
        let mut settings = NetworkProxyConfig {
            enabled: true,
            mode: NetworkMode::Limited,
            mitm: true,
            ..NetworkProxyConfig::default()
        };
        settings.set_allowed_domains(vec!["example.com".to_string()]);
        let state = state_for_settings(settings);
        let mut request =
            TcpRequest::new(HostWithPort::try_from("example.com:443").expect("valid authority"));
        request.extensions_mut().insert(state.clone());

        let result = handle_socks5_tcp(
            request, /*policy_decider*/ None, /*environment_id*/ None,
        )
        .await
        .expect("limited-mode HTTPS should use MITM");

        assert!(matches!(result.conn, Socks5TcpConnection::Mitm { .. }));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn handle_socks5_tcp_blocks_non_https_in_limited_mode() {
        let mut settings = NetworkProxyConfig {
            enabled: true,
            mode: NetworkMode::Limited,
            ..NetworkProxyConfig::default()
        };
        settings.set_allowed_domains(vec!["example.com".to_string()]);
        let state = state_for_settings(settings);
        let mut request =
            TcpRequest::new(HostWithPort::try_from("example.com:80").expect("valid authority"));
        request.extensions_mut().insert(state.clone());

        let (result, events) = capture_events(|| async {
            handle_socks5_tcp(
                request, /*policy_decider*/ None, /*environment_id*/ None,
            )
            .await
        })
        .await;
        assert!(
            result.is_err(),
            "limited-mode non-HTTPS SOCKS should be denied"
        );

        let event = find_event_by_name(&events, POLICY_DECISION_EVENT_NAME)
            .expect("expected policy decision event");
        assert_eq!(event.field("network.policy.scope"), Some("non_domain"));
        assert_eq!(event.field("network.policy.decision"), Some("deny"));
        assert_eq!(event.field("network.policy.source"), Some("mode_guard"));
        assert_eq!(
            event.field("network.policy.reason"),
            Some(REASON_METHOD_NOT_ALLOWED)
        );
        assert_eq!(
            event.field("network.transport.protocol"),
            Some("socks5_tcp")
        );
        assert_eq!(event.field("server.address"), Some("example.com"));
        assert_eq!(event.field("server.port"), Some("80"));
        assert_eq!(event.field("http.request.method"), Some("none"));
        assert_eq!(event.field("client.address"), Some("unknown"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn handle_socks5_tcp_detects_tls_for_brokered_nonstandard_port_in_full_mode() {
        let mut settings = NetworkProxyConfig {
            enabled: true,
            mode: NetworkMode::Full,
            mitm: true,
            credential_broker: true,
            ..NetworkProxyConfig::default()
        };
        settings.set_allowed_domains(vec!["api.openai.com".to_string()]);
        let state = state_for_settings(settings);
        let mut env = HashMap::from([("OPENAI_API_KEY".to_string(), "sk-real".to_string())]);
        state.virtualize_child_credentials(&mut env);
        let mut request = TcpRequest::new(
            HostWithPort::try_from("api.openai.com:8443").expect("valid authority"),
        );
        request.extensions_mut().insert(state.clone());

        let result = handle_socks5_tcp(
            request, /*policy_decider*/ None, /*environment_id*/ None,
        )
        .await
        .expect("brokered TLS should defer MITM until protocol detection");

        assert!(matches!(
            result.conn,
            Socks5TcpConnection::DetectProtocol { .. }
        ));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn handle_socks5_tcp_blocks_limited_mode_without_mitm_state() {
        let mut settings = NetworkProxyConfig {
            enabled: true,
            mode: NetworkMode::Limited,
            ..NetworkProxyConfig::default()
        };
        settings.set_allowed_domains(vec!["example.com".to_string()]);
        let state = state_for_settings(settings);
        let mut request =
            TcpRequest::new(HostWithPort::try_from("example.com:443").expect("valid authority"));
        request.extensions_mut().insert(state.clone());

        let err = handle_socks5_tcp(
            request, /*policy_decider*/ None, /*environment_id*/ None,
        )
        .await
        .expect_err("limited-mode HTTPS requires MITM");

        assert!(
            format!("{err:?}").contains("MITM required"),
            "unexpected error: {err:?}"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn handle_socks5_tcp_uses_mitm_for_hooked_host_in_full_mode() {
        let mut settings = NetworkProxyConfig {
            enabled: true,
            mode: NetworkMode::Full,
            mitm: true,
            mitm_hooks: vec![MitmHookConfig {
                host: "api.github.com".to_string(),
                matcher: MitmHookMatchConfig {
                    methods: vec!["POST".to_string()],
                    path_prefixes: vec!["/repos/openai/".to_string()],
                    ..MitmHookMatchConfig::default()
                },
                ..MitmHookConfig::default()
            }],
            ..NetworkProxyConfig::default()
        };
        settings.set_allowed_domains(vec!["api.github.com".to_string()]);
        let state = state_for_settings(settings);
        let mut request =
            TcpRequest::new(HostWithPort::try_from("api.github.com:443").expect("valid authority"));
        request.extensions_mut().insert(state.clone());

        let result = handle_socks5_tcp(
            request, /*policy_decider*/ None, /*environment_id*/ None,
        )
        .await
        .expect("hooked HTTPS should use MITM");

        assert!(matches!(result.conn, Socks5TcpConnection::Mitm { .. }));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn handle_socks5_tcp_blocks_hooked_non_https_host_in_full_mode() {
        let mut settings = NetworkProxyConfig {
            enabled: true,
            mode: NetworkMode::Full,
            mitm: true,
            mitm_hooks: vec![MitmHookConfig {
                host: "api.github.com".to_string(),
                matcher: MitmHookMatchConfig {
                    methods: vec!["POST".to_string()],
                    path_prefixes: vec!["/repos/openai/".to_string()],
                    ..MitmHookMatchConfig::default()
                },
                ..MitmHookConfig::default()
            }],
            ..NetworkProxyConfig::default()
        };
        settings.set_allowed_domains(vec!["api.github.com".to_string()]);
        let state = state_for_settings(settings);
        let mut request =
            TcpRequest::new(HostWithPort::try_from("api.github.com:80").expect("valid authority"));
        request.extensions_mut().insert(state.clone());

        let err = handle_socks5_tcp(
            request, /*policy_decider*/ None, /*environment_id*/ None,
        )
        .await
        .expect_err("hooked non-HTTPS SOCKS should require MITM");

        assert!(
            format!("{err:?}").contains("MITM required"),
            "unexpected error: {err:?}"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn inspect_socks5_udp_emits_block_decision_for_mode_guard_deny() {
        let state = state_for_settings(NetworkProxyConfig {
            enabled: true,
            mode: NetworkMode::Limited,
            ..NetworkProxyConfig::default()
        });
        let request = RelayRequest {
            direction: RelayDirection::South,
            server_address: SocketAddress::new(IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34)), 53),
            payload: Default::default(),
            extensions: Extensions::new(),
        };

        let (result, events) = capture_events(|| async {
            inspect_socks5_udp(
                request, state, /*policy_decider*/ None, /*environment_id*/ None,
            )
            .await
        })
        .await;
        assert!(result.is_err(), "limited-mode UDP request should be denied");

        let event = find_event_by_name(&events, POLICY_DECISION_EVENT_NAME)
            .expect("expected policy decision event");
        assert_eq!(event.field("network.policy.scope"), Some("non_domain"));
        assert_eq!(event.field("network.policy.decision"), Some("deny"));
        assert_eq!(event.field("network.policy.source"), Some("mode_guard"));
        assert_eq!(
            event.field("network.policy.reason"),
            Some(REASON_METHOD_NOT_ALLOWED)
        );
        assert_eq!(
            event.field("network.transport.protocol"),
            Some("socks5_udp")
        );
        assert_eq!(event.field("server.address"), Some("93.184.216.34"));
        assert_eq!(event.field("server.port"), Some("53"));
        assert_eq!(event.field("http.request.method"), Some("none"));
        assert_eq!(event.field("client.address"), Some("unknown"));
    }
}
