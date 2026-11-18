use super::*;
use crate::config::NetworkProxyConfig;
use crate::state::network_proxy_state_for_policy;
use pretty_assertions::assert_eq;
use rama_http::StatusCode;
use rama_http::Version;
use rama_net::address::Host;
use rama_net::address::HostWithPort;
use rama_tls_rustls::client::client_root_certs;
use rama_tls_rustls::dep::pki_types::CertificateDer;
use rama_tls_rustls::dep::pki_types::PrivateKeyDer;
use rama_tls_rustls::dep::pki_types::pem::PemObject;
use rama_tls_rustls::dep::rcgen::BasicConstraints;
use rama_tls_rustls::dep::rcgen::CertificateParams;
use rama_tls_rustls::dep::rcgen::DistinguishedName;
use rama_tls_rustls::dep::rcgen::DnType;
use rama_tls_rustls::dep::rcgen::ExtendedKeyUsagePurpose;
use rama_tls_rustls::dep::rcgen::IsCa;
use rama_tls_rustls::dep::rcgen::Issuer;
use rama_tls_rustls::dep::rcgen::KeyPair;
use rama_tls_rustls::dep::rcgen::KeyUsagePurpose;
use rama_tls_rustls::dep::rcgen::PKCS_ECDSA_P256_SHA256;
use rama_tls_rustls::dep::tokio_rustls::TlsAcceptor;
use std::collections::HashMap;
use std::fs;
use std::sync::Arc;
use tempfile::tempdir;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;

async fn read_proxy_head(stream: &mut (impl tokio::io::AsyncRead + Unpin)) -> String {
    let mut head = Vec::new();
    while !head.ends_with(b"\r\n\r\n") {
        assert!(head.len() < 16_384);
        head.push(stream.read_u8().await.unwrap());
    }
    String::from_utf8(head).unwrap()
}

#[tokio::test]
async fn required_proxy_is_authoritative_for_managed_http_and_connect() {
    if run_required_proxy_test_in_child(
        "upstream::tests::required_proxy_is_authoritative_for_managed_http_and_connect",
    ) {
        return;
    }
    let proxy = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let required = RequiredProxy::new(
        &format!("http://user:secret@{}", proxy.local_addr().unwrap()),
        &["direct.example".into(), "192.168.0.0/16".into()],
    )
    .unwrap();
    codex_required_proxy::install(required.clone(), None).unwrap();

    // Managed listeners may overwrite the environment with themselves. Neither an absent
    // environment proxy nor an invalid one may replace the separately installed Keychain route.
    let config = ProxyConfig {
        configuration_error: Some("invalid environment proxy"),
        ..Default::default()
    };
    let destination = HostWithPort::new(Host::EXAMPLE_NAME, 443);
    for secure in [false, true] {
        let selected = config
            .proxy_for_target(&destination, secure, true)
            .unwrap()
            .unwrap();
        assert_eq!(selected.address.port, proxy.local_addr().unwrap().port());
        assert!(selected.credential.is_some());
        assert!(
            config
                .proxy_for_target(&destination, secure, false)
                .is_err()
        );
        for direct in [
            "localhost",
            "127.0.0.1",
            "::1",
            "direct.example",
            "192.168.1.2",
        ] {
            assert!(
                config
                    .proxy_for_target(
                        &HostWithPort::new(direct.parse::<Host>().unwrap(), 443),
                        secure,
                        false
                    )
                    .unwrap()
                    .is_none(),
                "{direct}"
            );
        }
    }
    assert!(
        required_proxy_for_target(
            &required,
            &HostWithPort::new("10.0.0.1".parse::<Host>().unwrap(), 443),
            true,
            false,
        )
        .is_err()
    );

    let state = Arc::new(network_proxy_state_for_policy(NetworkProxyConfig {
        allow_upstream_proxy: true,
        allow_local_binding: Some(true),
        ..Default::default()
    }));
    let roots = client_root_certs();
    // These four constructors cover plain HTTP and both MITM construction branches. Required
    // proxy selection also wins for legacy callers asking for a nominally direct client.
    for client in [
        UpstreamClient::from_env_proxy(state.clone()),
        UpstreamClient::direct(state.clone()),
        UpstreamClient::from_env_proxy_with_tls_root_store(state.clone(), roots.clone()),
        UpstreamClient::direct_with_tls_root_store(state.clone(), roots.clone()),
    ] {
        let peer = async {
            let (mut stream, _) = proxy.accept().await.unwrap();
            let request = read_proxy_head(&mut stream).await;
            assert!(request.starts_with("GET http://managed-required.invalid/probe HTTP/1.1\r\n"));
            assert!(
                request
                    .to_ascii_lowercase()
                    .contains("proxy-authorization: basic dxnlcjpzzwnyzxq=\r\n")
            );
            stream
                .write_all(b"HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n")
                .await
                .unwrap();
        };
        let request = client.serve(
            Request::builder()
                .uri("http://managed-required.invalid/probe")
                .body(Body::empty())
                .unwrap(),
        );
        let (response, ()) = tokio::join!(request, peer);
        assert_eq!(response.unwrap().status(), StatusCode::NO_CONTENT);
    }

    let connect = async {
        let selected = proxy_for_connect(&destination, &state, true).unwrap();
        let mut tunnel = connect_tunnel(
            state.clone(),
            TcpRequest::new(destination.clone()),
            selected,
        )
        .await
        .unwrap()
        .conn;
        let mut greeting = [0; 5];
        tunnel.read_exact(&mut greeting).await.unwrap();
        assert_eq!(&greeting, b"hello");
        tunnel.write_all(b"private").await.unwrap();
        tunnel.shutdown().await.unwrap();
    };
    let peer = async {
        let (mut stream, _) = proxy.accept().await.unwrap();
        let request = read_proxy_head(&mut stream).await;
        assert!(request.starts_with("CONNECT example.com:443 HTTP/1.1\r\n"));
        assert!(
            request
                .to_ascii_lowercase()
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
    tokio::join!(connect, peer);

    let origin = std::net::TcpListener::bind("0.0.0.0:0").unwrap();
    origin.set_nonblocking(true).unwrap();
    let state = Arc::new(network_proxy_state_for_policy(NetworkProxyConfig {
        allow_upstream_proxy: false,
        allow_local_binding: Some(true),
        ..Default::default()
    }));
    let origin_url = format!("http://{}/private", origin.local_addr().unwrap());
    for client in [
        UpstreamClient::direct(state.clone()),
        UpstreamClient::from_env_proxy(state.clone()),
        UpstreamClient::direct_with_tls_root_store(state.clone(), roots.clone()),
        UpstreamClient::from_env_proxy_with_tls_root_store(state.clone(), roots),
    ] {
        let error = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            client.serve(
                Request::builder()
                    .uri(&origin_url)
                    .body(Body::empty())
                    .unwrap(),
            ),
        )
        .await
        .unwrap()
        .unwrap_err();
        assert!(error.to_string().contains("disabled by network policy"));
    }
    let error = proxy_for_connect(
        &HostWithPort::from(origin.local_addr().unwrap()),
        &state,
        false,
    )
    .unwrap_err();
    assert!(error.to_string().contains("disabled by network policy"));
    assert!(!error.to_string().contains("secret"));
    assert_eq!(
        origin.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
}

#[tokio::test]
async fn https_upstream_encrypts_http_and_connect_authentication() {
    ensure_rustls_crypto_provider();
    let fixture = tempdir().unwrap();
    let (ca_pem, ca_key) = generate_ca("upstream proxy CA");
    let issuer = Issuer::from_ca_cert_pem(&ca_pem, ca_key).unwrap();
    let params = CertificateParams::new(vec![
        "localhost".to_string(),
        "secure-origin.invalid".to_string(),
    ])
    .unwrap();
    let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).unwrap();
    let certificate = params.signed_by(&key, &issuer).unwrap();
    fs::write(fixture.path().join("ca.pem"), ca_pem).unwrap();
    fs::write(fixture.path().join("server.pem"), certificate.pem()).unwrap();
    fs::write(fixture.path().join("server-key.pem"), key.serialize_pem()).unwrap();
    if run_required_proxy_test_in_child_with_env(
        "upstream::tests::https_upstream_encrypts_http_and_connect_authentication",
        &[
            (
                "SSL_CERT_FILE",
                fixture.path().join("ca.pem").to_string_lossy().into_owned(),
            ),
            (
                "CODEX_NETWORK_PROXY_TLS_FIXTURE",
                fixture.path().to_string_lossy().into_owned(),
            ),
        ],
    ) {
        return;
    }
    // The child receives only the production CA environment; callers do not inject TLS
    // connector extensions or supply a root store manually.
    let fixture =
        std::path::PathBuf::from(std::env::var("CODEX_NETWORK_PROXY_TLS_FIXTURE").unwrap());
    let server_config = rustls::ServerConfig::builder_with_protocol_versions(rustls::ALL_VERSIONS)
        .with_no_client_auth()
        .with_single_cert(
            vec![
                CertificateDer::from_pem_slice(&fs::read(fixture.join("server.pem")).unwrap())
                    .unwrap(),
            ],
            PrivateKeyDer::from_pem_slice(&fs::read(fixture.join("server-key.pem")).unwrap())
                .unwrap(),
        )
        .unwrap();
    let acceptor = TlsAcceptor::from(Arc::new(server_config));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let required = RequiredProxy::new(
        &format!(
            "https://user:secret@localhost:{}",
            listener.local_addr().unwrap().port()
        ),
        &[],
    )
    .unwrap();
    codex_required_proxy::install(required, None).unwrap();
    let state = Arc::new(network_proxy_state_for_policy(NetworkProxyConfig {
        allow_local_binding: Some(true),
        ..Default::default()
    }));
    for client in [
        UpstreamClient::from_env_proxy(state.clone()),
        UpstreamClient::direct(state.clone()),
    ] {
        let peer = async {
            let (stream, _) = listener.accept().await.unwrap();
            let mut stream = acceptor.accept(stream).await.unwrap();
            let head = read_proxy_head(&mut stream).await;
            assert!(head.starts_with("GET http://encrypted-proxy.invalid/ HTTP/1.1\r\n"));
            assert!(
                head.to_ascii_lowercase()
                    .contains("proxy-authorization: basic dxnlcjpzzwnyzxq=\r\n")
            );
            stream
                .write_all(b"HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n")
                .await
                .unwrap();
        };
        let request = client.serve(
            Request::builder()
                .uri("http://encrypted-proxy.invalid/")
                .body(Body::empty())
                .unwrap(),
        );
        let (response, ()) = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            tokio::join!(request, peer)
        })
        .await
        .unwrap();
        assert_eq!(response.unwrap().status(), StatusCode::NO_CONTENT);
    }

    // HTTPS origin authentication is distinct from proxy authentication, even when both TLS
    // layers happen to use the same test CA. Proxy credentials must stop at CONNECT.
    let client = UpstreamClient::from_env_proxy(state.clone());
    let request = client.serve(
        Request::builder()
            .uri("https://secure-origin.invalid/private")
            .header("Proxy-Authorization", "Basic downstream-credentials")
            .body(Body::empty())
            .unwrap(),
    );
    let peer = async {
        let (stream, _) = listener.accept().await.unwrap();
        let mut proxy_tls = acceptor.accept(stream).await.unwrap();
        let connect = read_proxy_head(&mut proxy_tls).await;
        assert!(connect.starts_with("CONNECT secure-origin.invalid:443 HTTP/1.1\r\n"));
        assert!(
            connect
                .to_ascii_lowercase()
                .contains("proxy-authorization: basic dxnlcjpzzwnyzxq=\r\n")
        );
        proxy_tls
            .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            .await
            .unwrap();
        let mut origin_tls = acceptor.accept(proxy_tls).await.unwrap();
        let origin_request = read_proxy_head(&mut origin_tls).await;
        assert!(origin_request.starts_with("GET /private HTTP/1.1\r\n"));
        assert!(
            !origin_request
                .to_ascii_lowercase()
                .contains("proxy-authorization:")
        );
        origin_tls
            .write_all(b"HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
    };
    let (response, ()) = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        tokio::join!(request, peer)
    })
    .await
    .unwrap();
    assert_eq!(response.unwrap().status(), StatusCode::NO_CONTENT);

    let target = "opaque.invalid:5432".parse().unwrap();
    let address = proxy_for_connect(&target, &state, true).unwrap();
    let caller = async {
        let mut tunnel = connect_tunnel(state, TcpRequest::new(target), address)
            .await
            .unwrap()
            .conn;
        let mut greeting = [0; 5];
        tunnel.read_exact(&mut greeting).await.unwrap();
        assert_eq!(&greeting, b"hello");
        tunnel.shutdown().await.unwrap();
    };
    let peer = async {
        let (stream, _) = listener.accept().await.unwrap();
        let mut stream = acceptor.accept(stream).await.unwrap();
        let head = read_proxy_head(&mut stream).await;
        assert!(head.starts_with("CONNECT opaque.invalid:5432 HTTP/1.1\r\n"));
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
        assert!(payload.is_empty());
    };
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        tokio::join!(caller, peer);
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn invalid_custom_ca_never_falls_back_to_platform_upstream_trust() {
    let fixture = tempdir().unwrap();
    let ca = fixture.path().join("invalid-ca.pem");
    fs::write(
        &ca,
        "-----BEGIN CERTIFICATE-----\nAQID\n-----END CERTIFICATE-----\n",
    )
    .unwrap();
    if run_required_proxy_test_in_child_with_env(
        "upstream::tests::invalid_custom_ca_never_falls_back_to_platform_upstream_trust",
        &[("SSL_CERT_FILE", ca.to_string_lossy().into_owned())],
    ) {
        return;
    }
    let proxy = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    proxy.set_nonblocking(true).unwrap();
    let required = RequiredProxy::new(
        &format!(
            "https://user:secret@localhost:{}",
            proxy.local_addr().unwrap().port()
        ),
        &[],
    )
    .unwrap();
    codex_required_proxy::install(required, None).unwrap();
    let state = Arc::new(network_proxy_state_for_policy(NetworkProxyConfig {
        allow_local_binding: Some(true),
        ..Default::default()
    }));
    for client in [
        UpstreamClient::direct(state.clone()),
        UpstreamClient::from_env_proxy(state.clone()),
    ] {
        let error = client
            .serve(
                Request::builder()
                    .uri("http://invalid-ca-target.invalid/")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("TLS trust configuration unavailable")
        );
    }
    let target = "invalid-ca-target.invalid:443".parse().unwrap();
    let route = proxy_for_connect(&target, &state, true).unwrap();
    let error = connect_tunnel(state, TcpRequest::new(target), route)
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("TLS trust configuration unavailable")
    );
    assert_eq!(
        proxy.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
}

#[tokio::test]
async fn required_proxy_preserves_ip_and_remote_dns_policy_before_any_dial() {
    if run_required_proxy_test_in_child(
        "upstream::tests::required_proxy_preserves_ip_and_remote_dns_policy_before_any_dial",
    ) {
        return;
    }
    let proxy = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    proxy.set_nonblocking(true).unwrap();
    codex_required_proxy::install(
        RequiredProxy::new(
            &format!("http://user:secret@{}", proxy.local_addr().unwrap()),
            &[],
        )
        .unwrap(),
        None,
    )
    .unwrap();
    let roots = client_root_certs();
    for (local_binding, deny, review, host, expected) in [
        (
            false,
            false,
            false,
            "policy-target.invalid",
            "local address enforcement",
        ),
        (
            true,
            true,
            false,
            "policy-target.invalid",
            "local address enforcement",
        ),
        (
            true,
            false,
            true,
            "policy-target.invalid",
            "local address enforcement",
        ),
        (true, true, false, "1.1.1.1", "IP destination cannot use"),
        (true, false, true, "1.1.1.1", "IP destination cannot use"),
    ] {
        let mut config = NetworkProxyConfig {
            allow_local_binding: Some(local_binding),
            ..Default::default()
        };
        if deny {
            config.set_denied_domains(vec!["1.1.1.1".into()]);
        }
        if review {
            config.review_domains = vec!["1.1.1.1".into()];
        }
        let state = Arc::new(network_proxy_state_for_policy(config));
        let url = format!("http://{host}:443/private");
        for client in [
            UpstreamClient::from_env_proxy(state.clone()),
            UpstreamClient::direct(state.clone()),
            UpstreamClient::from_env_proxy_with_tls_root_store(state.clone(), roots.clone()),
            UpstreamClient::direct_with_tls_root_store(state.clone(), roots.clone()),
        ] {
            let error = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                client.serve(Request::builder().uri(&url).body(Body::empty()).unwrap()),
            )
            .await
            .unwrap()
            .unwrap_err();
            assert!(error.to_string().contains(expected), "{host}: {error}");
            assert!(!error.to_string().contains("secret"));
        }
        let target = HostWithPort::new(host.parse::<Host>().unwrap(), 443);
        let route = proxy_for_connect(&target, &state, true).unwrap();
        let error = connect_tunnel(state, TcpRequest::new(target), route)
            .await
            .unwrap_err();
        assert!(error.to_string().contains(expected), "{host}: {error}");
    }
    assert_eq!(
        proxy.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );

    // A numeric review approval remains scoped to the admitted target; a later denial wins.
    let target: HostWithPort = "1.1.1.1:443".parse().unwrap();
    let config = NetworkProxyConfig {
        review_domains: vec!["1.1.1.1".into()],
        ..Default::default()
    };
    let state = network_proxy_state_for_policy(config);
    let policy_request = crate::network_policy::NetworkPolicyRequest::new(
        crate::network_policy::NetworkPolicyRequestArgs {
            protocol: crate::network_policy::NetworkProtocol::Http,
            host: "1.1.1.1".into(),
            port: 443,
            environment_id: None,
            client_addr: None,
            method: Some("GET".into()),
            command: None,
            exec_policy_hint: None,
        },
    );
    let mut extensions = rama_core::extensions::Extensions::new();
    extensions.insert(DialPolicy::new(
        &target,
        crate::runtime::HostAuthorization::Approved,
        &policy_request,
    ));
    check_required_upstream_destination(&state, &target, &extensions)
        .await
        .unwrap();
    let other: HostWithPort = "1.1.1.1:8443".parse().unwrap();
    assert!(
        check_required_upstream_destination(&state, &other, &extensions)
            .await
            .is_err()
    );
    state.add_denied_domain("1.1.1.1").await.unwrap();
    assert!(
        check_required_upstream_destination(&state, &target, &extensions)
            .await
            .is_err()
    );
}

fn generate_ca(common_name: &str) -> (String, KeyPair) {
    let mut params = CertificateParams::default();
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.key_usages = vec![
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::DigitalSignature,
        KeyUsagePurpose::KeyEncipherment,
    ];
    let mut distinguished_name = DistinguishedName::new();
    distinguished_name.push(DnType::CommonName, common_name);
    params.distinguished_name = distinguished_name;
    let key_pair = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).unwrap();
    let cert = params.self_signed(&key_pair).unwrap();
    (cert.pem(), key_pair)
}

#[tokio::test]
async fn mitm_upstream_client_trusts_startup_custom_ca() {
    ensure_rustls_crypto_provider();
    let temp_dir = tempdir().unwrap();
    let startup_ca_path = temp_dir.path().join("startup-ca.pem");
    let managed_ca_path = temp_dir.path().join("managed-ca.pem");
    let (startup_ca_pem, startup_ca_key) = generate_ca("startup CA");
    let (managed_ca_pem, _) = generate_ca("managed MITM CA");
    fs::write(&startup_ca_path, &startup_ca_pem).unwrap();
    fs::write(&managed_ca_path, managed_ca_pem).unwrap();

    let issuer = Issuer::from_ca_cert_pem(&startup_ca_pem, startup_ca_key).unwrap();
    let mut server_params = CertificateParams::new(vec!["localhost".to_string()]).unwrap();
    server_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    server_params.key_usages = vec![
        KeyUsagePurpose::DigitalSignature,
        KeyUsagePurpose::KeyEncipherment,
    ];
    let server_key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).unwrap();
    let server_cert = server_params.signed_by(&server_key, &issuer).unwrap();
    let server_cert = CertificateDer::from_pem_slice(server_cert.pem().as_bytes()).unwrap();
    let server_key = PrivateKeyDer::from_pem_slice(server_key.serialize_pem().as_bytes()).unwrap();
    let mut server_config =
        rustls::ServerConfig::builder_with_protocol_versions(rustls::ALL_VERSIONS)
            .with_no_client_auth()
            .with_single_cert(vec![server_cert], server_key)
            .unwrap();
    server_config.alpn_protocols = vec![b"http/1.1".to_vec()];

    let env = HashMap::from([(
        "SSL_CERT_FILE",
        startup_ca_path.to_string_lossy().into_owned(),
    )]);
    let roots =
        crate::certs::upstream_tls_root_store_for_cert_path(&managed_ca_path, &env).unwrap();
    let baseline_roots =
        crate::certs::upstream_tls_root_store_for_cert_path(&managed_ca_path, &HashMap::new())
            .unwrap();
    assert_eq!(roots.len(), baseline_roots.len() + 1);

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let acceptor = TlsAcceptor::from(Arc::new(server_config));
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut stream = acceptor.accept(stream).await.unwrap();
        let mut request = [0; 4096];
        let bytes_read = stream.read(&mut request).await.unwrap();
        assert!(bytes_read > 0);
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
    });

    let mut config = NetworkProxyConfig::default();
    config.set_allowed_domains(vec!["localhost".to_string()]);
    let state = Arc::new(network_proxy_state_for_policy(config));
    let client = UpstreamClient::direct_with_tls_root_store(state, roots);
    let response = client
        .serve(
            Request::builder()
                .version(Version::HTTP_2)
                .uri(format!("https://localhost:{}/", address.port()))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    server.await.unwrap();
}

#[test]
fn private_ip_upstream_routing_is_opt_in() {
    let proxy = ProxyAddress::try_from("http://127.0.0.1:43128").unwrap();
    for enabled in [false, true] {
        let config = ProxyConfig {
            http: Some(proxy.clone()),
            https: Some(proxy.clone()),
            proxy_private_ips_via_upstream: enabled,
            ..ProxyConfig::default()
        };
        for (host, use_proxy) in [
            ("10.0.0.1", enabled),
            ("172.16.0.1", enabled),
            ("192.168.0.1", enabled),
            ("100.68.58.50", enabled),
            ("fd00::1", enabled),
            ("::ffff:100.68.58.50", enabled),
            ("localhost", false),
            ("127.0.0.1", false),
            ("::1", false),
            ("169.254.169.254", false),
            ("fe80::1", false),
            ("0.0.0.0", false),
            ("224.0.0.1", false),
            ("192.0.2.1", false),
            ("240.0.0.1", false),
            ("example.com", true),
            ("100.63.255.255", true),
            ("100.128.0.0", true),
        ] {
            let target = HostWithPort::new(host.parse::<Host>().unwrap(), /*port*/ 443);
            for is_secure in [false, true] {
                assert_eq!(
                    config
                        .proxy_for_target(&target, is_secure, /*allow_upstream_proxy*/ true)
                        .unwrap(),
                    use_proxy.then(|| proxy.clone()),
                    "host={host}, enabled={enabled}, secure={is_secure}"
                );
            }
        }
    }
}

#[test]
fn invalid_upstream_proxy_is_an_error_without_trying_another_spelling() {
    for value in ["http://[invalid", "socks5://secret:password@localhost:1080"] {
        let error = read_proxy_env(&["HTTPS_PROXY", "https_proxy"], |key| {
            assert_eq!(key, "HTTPS_PROXY");
            Some(value.to_string())
        })
        .unwrap_err();
        assert!(!error.contains(value));
        let config = ProxyConfig {
            configuration_error: Some(error),
            ..ProxyConfig::default()
        };
        let target = HostWithPort::new(Host::EXAMPLE_NAME, /*port*/ 443);
        assert!(
            config
                .proxy_for_target(
                    &target, /*is_secure*/ true, /*allow_upstream_proxy*/ true
                )
                .is_err()
        );
        assert!(
            config
                .proxy_for_target(
                    &target, /*is_secure*/ false, /*allow_upstream_proxy*/ true
                )
                .is_err()
        );
    }
}

#[test]
fn a_single_upstream_proxy_covers_both_protocols() {
    let proxy = ProxyAddress::try_from("http://127.0.0.1:43128").unwrap();
    let target = HostWithPort::new(Host::EXAMPLE_NAME, /*port*/ 443);
    for config in [
        ProxyConfig {
            http: Some(proxy.clone()),
            ..ProxyConfig::default()
        },
        ProxyConfig {
            https: Some(proxy.clone()),
            ..ProxyConfig::default()
        },
        ProxyConfig {
            all: Some(proxy.clone()),
            ..ProxyConfig::default()
        },
    ] {
        assert_eq!(
            config
                .proxy_for_target(
                    &target, /*is_secure*/ true, /*allow_upstream_proxy*/ true
                )
                .unwrap(),
            Some(proxy.clone())
        );
        assert_eq!(
            config
                .proxy_for_target(
                    &target, /*is_secure*/ false, /*allow_upstream_proxy*/ true
                )
                .unwrap(),
            Some(proxy.clone())
        );
    }
}
