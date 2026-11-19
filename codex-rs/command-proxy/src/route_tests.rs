use super::*;
use pretty_assertions::assert_eq;

fn basic(credentials: &str) -> String {
    format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(credentials)
    )
}

#[test]
fn identity_comes_from_the_proxy_username() {
    assert_eq!(
        Identity::from_proxy_authorization(Some(&basic("thread-abc:"))),
        Identity::Thread("abc".to_string())
    );
    for value in [
        None,
        Some(basic("codex:")),
        Some(basic("thread-:")),
        Some("Bearer x".into()),
    ] {
        assert_eq!(
            Identity::from_proxy_authorization(value.as_deref()),
            Identity::Codex
        );
    }
}

#[test]
fn threads_may_go_direct_except_for_pinned_hosts_and_codex_never_does() {
    let required = RequiredProxy::new("http://proxy.example", &["10.0.0.0/8".to_string()])
        .expect("valid proxy");
    let policies = Policies::default();
    let thread = Identity::Thread("t1".to_string());
    let other = Identity::Thread("t2".to_string());

    // Package registries start direct for agent threads, never for Codex itself.
    assert_eq!(
        policies.route(&required, &thread, "registry.npmjs.org"),
        Route::Direct
    );
    assert_eq!(
        policies.route(&required, &Identity::Codex, "registry.npmjs.org"),
        Route::Upstream
    );
    assert_eq!(
        policies.route(&required, &thread, "example.com"),
        Route::Upstream
    );

    policies
        .update(
            "t1",
            RoutingUpdate {
                add: vec!["example.com".to_string()],
                remove: vec!["registry.npmjs.org".to_string()],
                ..RoutingUpdate::default()
            },
        )
        .expect("update");
    assert_eq!(
        policies.route(&required, &thread, "api.example.com"),
        Route::Direct
    );
    assert_eq!(
        policies.route(&required, &thread, "registry.npmjs.org"),
        Route::Upstream
    );
    assert_eq!(
        policies.route(&required, &other, "example.com"),
        Route::Upstream
    );
    assert_eq!(
        policies.route(&required, &other, "registry.npmjs.org"),
        Route::Direct
    );

    policies
        .update(
            "t1",
            RoutingUpdate {
                default: Some(DefaultRoute::Direct),
                ..RoutingUpdate::default()
            },
        )
        .expect("update");
    assert_eq!(
        policies.route(&required, &thread, "anything.example"),
        Route::Direct
    );
    assert_eq!(
        policies.route(&required, &thread, "api.openai.com"),
        Route::Upstream
    );
    assert_eq!(
        policies.route(&required, &Identity::Codex, "10.1.2.3"),
        Route::Direct
    );

    let mut direct: Vec<String> = PACKAGE_REGISTRIES
        .iter()
        .filter(|registry| **registry != "registry.npmjs.org")
        .map(ToString::to_string)
        .collect();
    direct.push("example.com".to_string());
    assert_eq!(
        policies.routing(&required, "t1"),
        ThreadRouting {
            thread: "t1".to_string(),
            default: DefaultRoute::Direct,
            direct,
            always_direct: ["localhost", "127.0.0.1", "::1", "10.0.0.0/8"]
                .map(String::from)
                .to_vec(),
            always_proxied: codex_required_proxy::PINNED_DOMAINS
                .iter()
                .map(ToString::to_string)
                .collect(),
        }
    );
}

#[test]
fn remove_accepts_the_same_spelling_as_add() {
    let policies = Policies::default();
    let proxy = RequiredProxy::new("http://proxy.invalid:3128", &[]).expect("valid proxy");
    let add = vec!["EXAMPLE.invalid".to_string(), "*.OTHER.invalid".to_string()];
    let identity = Identity::Thread("test".to_string());
    policies
        .update(
            "test",
            RoutingUpdate {
                add: add.clone(),
                ..RoutingUpdate::default()
            },
        )
        .expect("add routes");
    assert_eq!(
        policies.route(&proxy, &identity, "example.invalid"),
        Route::Direct
    );
    assert_eq!(
        policies.route(&proxy, &identity, "a.other.invalid"),
        Route::Direct
    );
    policies
        .update(
            "test",
            RoutingUpdate {
                remove: add,
                ..RoutingUpdate::default()
            },
        )
        .expect("remove routes");
    assert_eq!(
        policies.route(&proxy, &identity, "example.invalid"),
        Route::Upstream
    );
    assert_eq!(
        policies.route(&proxy, &identity, "a.other.invalid"),
        Route::Upstream
    );
}

#[test]
fn threads_cannot_send_openai_direct() {
    let policies = Policies::default();
    let error = policies
        .update(
            "t1",
            RoutingUpdate {
                add: vec!["chatgpt.com".to_string()],
                ..RoutingUpdate::default()
            },
        )
        .expect_err("pinned destination");
    assert_eq!(
        error.to_string(),
        "invalid [proxy] direct entry `chatgpt.com`: OpenAI destinations always use the proxy"
    );
}
