use super::*;
use pretty_assertions::assert_eq;

#[test]
fn failed_pac_candidates_cannot_fall_through_to_direct() {
    for proxies in ["SOCKS proxy.internal:1080; DIRECT", "https=; DIRECT"] {
        assert_eq!(
            parse_proxy_list(proxies, "https"),
            ParsedProxyListDecision::Unavailable
        );
    }
    assert_eq!(
        parse_proxy_list("http=proxy.internal:8080; DIRECT", "https"),
        ParsedProxyListDecision::Direct
    );
}

#[test]
fn unavailable_system_proxy_cannot_build_a_direct_http_client() {
    let route = route_from_system_decision(
        &MapEnv {
            values: HashMap::new(),
        },
        EnvProxyKind::Https,
        SystemProxyDecision::Unavailable {
            failure: RouteFailureClass::ProxyResolutionUnavailable,
        },
    );
    assert!(matches!(
        configure_builder_for_resolved_route(
            reqwest::Client::builder(),
            ClientRouteClass::Api,
            &route,
        ),
        Err(BuildRouteAwareHttpClientError::ProxyUnavailable { .. })
    ));
}

#[test]
fn explicit_proxy_takes_precedence_over_system_direct_and_covers_both_schemes() {
    for key in ["HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY"] {
        let env = MapEnv {
            values: HashMap::from([(key.into(), "http://proxy.example:8080".into())]),
        };
        for url in [
            "http://example.com",
            "https://example.com",
            "ws://example.com",
            "wss://example.com",
        ] {
            for policy in [
                OutboundProxyPolicy::ReqwestDefault,
                OutboundProxyPolicy::RespectSystemProxy,
            ] {
                let expected = OutboundProxyRoute::Proxy {
                    url: "http://proxy.example:8080".into(),
                    no_proxy: None,
                };
                assert_eq!(
                    resolve_proxy_route(&env, url, policy, |_, _| panic!(
                        "explicit proxy must skip system discovery"
                    )),
                    expected
                );
                assert_eq!(
                    route_from_system_decision(
                        &env,
                        EnvProxyKind::from_request_url(url),
                        SystemProxyDecision::Direct
                    ),
                    expected
                );
            }
        }
    }
}

#[tokio::test]
async fn failed_proxy_never_connects_to_the_reachable_origin() {
    for response in [
        "",
        "HTTP/1.1 407 Proxy Authentication Required\r\nContent-Length: 0\r\n\r\n",
    ] {
        let origin = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        origin.set_nonblocking(true).unwrap();
        let url = format!("http://{}/private", origin.local_addr().unwrap());
        let (proxy, proxy_thread) = spawn_http_listener(vec![response.to_string()]);
        let env = MapEnv {
            values: HashMap::from([("HTTP_PROXY".into(), format!("http://{proxy}"))]),
        };
        let builder = configure_proxy_for_route(
            &env,
            reqwest::Client::builder().timeout(Duration::from_secs(/*secs*/ 2)),
            &url,
            ClientRouteClass::Api,
            OutboundProxyPolicy::RespectSystemProxy,
            |_, _| SystemProxyDecision::Direct,
        )
        .unwrap();
        let result = builder.build().unwrap().get(&url).send().await;
        assert!(
            result
                .and_then(reqwest::Response::error_for_status)
                .is_err()
        );
        assert!(only_request(proxy_thread, "configured proxy").contains("/private"));
        assert_eq!(
            origin.accept().unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
    }
}

#[tokio::test]
async fn unavailable_proxy_never_connects_to_the_reachable_origin() {
    let origin = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    origin.set_nonblocking(true).unwrap();
    let unavailable = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let proxy_url = format!("http://{}", unavailable.local_addr().unwrap());
    drop(unavailable);
    let url = format!("http://{}/private", origin.local_addr().unwrap());
    let env = MapEnv {
        values: HashMap::from([("HTTPS_PROXY".into(), proxy_url)]),
    };
    let builder = configure_proxy_for_route(
        &env,
        reqwest::Client::builder().timeout(Duration::from_secs(/*secs*/ 2)),
        &url,
        ClientRouteClass::Api,
        OutboundProxyPolicy::ReqwestDefault,
        |_, _| panic!("explicit proxy must skip discovery"),
    )
    .unwrap();
    assert!(builder.build().unwrap().get(url).send().await.is_err());
    assert_eq!(
        origin.accept().unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );
}
