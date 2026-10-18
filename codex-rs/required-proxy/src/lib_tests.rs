use super::*;
use pretty_assertions::assert_eq;

fn direct(entries: &[&str]) -> Vec<String> {
    entries.iter().map(ToString::to_string).collect()
}

#[test]
fn proxy_keeps_loopback_and_private_destinations_direct() {
    let proxy = RequiredProxy::new(
        "https://user:p%40ss@proxy.example:443/",
        &direct(&[
            "192.168.0.0/16",
            "100.64.0.0/10",
            "fd7a:115c:a1e0::/48",
            ".ts.net",
            "*.corp.example",
            "registry.npmjs.org",
            "10.1.2.3",
            "[fe80::1]",
            "localhost",
        ]),
    )
    .expect("valid proxy");

    assert_eq!(proxy.url(), "https://user:p%40ss@proxy.example");
    assert_eq!(
        proxy.no_proxy(),
        "localhost,127.0.0.1,::1,192.168.0.0/16,100.64.0.0/10,fd7a:115c:a1e0::/48,.ts.net,\
         .corp.example,registry.npmjs.org,10.1.2.3,fe80::1"
    );
}

#[test]
fn diagnostics_never_expose_credentials() {
    let proxy = RequiredProxy::new("http://user:secret@proxy.example:3128", &[]).expect("valid");

    assert_eq!(proxy.redacted(), "http://[REDACTED]@proxy.example:3128");
    assert!(!format!("{proxy:?}").contains("secret"));
    assert_eq!(
        RequiredProxy::new("ftp://user:secret@proxy.example", &[]),
        Err(RequiredProxyError::UnsupportedUrl)
    );
    assert_eq!(
        RequiredProxy::new("https://user:secret@proxy.example/path", &[])
            .unwrap_err()
            .to_string(),
        "the upstream proxy must be an http:// or https:// URL without a path or query"
    );
}

#[test]
fn scheme_less_proxy_defaults_to_http() {
    assert_eq!(
        normalize_proxy_url("proxy.example:8080"),
        Ok("http://proxy.example:8080".to_string())
    );
}

#[test]
fn direct_entries_cannot_bypass_openai_or_public_networks() {
    for (entry, reason) in [
        (
            "*",
            "a wildcard would bypass the proxy for every destination",
        ),
        ("com", "OpenAI destinations always use the proxy"),
        (".openai.com", "OpenAI destinations always use the proxy"),
        ("api.openai.com", "OpenAI destinations always use the proxy"),
        ("*.chatgpt.com", "OpenAI destinations always use the proxy"),
        (
            "o33249.ingest.us.sentry.io",
            "OpenAI destinations always use the proxy",
        ),
        (
            "8.8.8.8",
            "only loopback, private and link-local addresses may bypass the proxy",
        ),
        (
            "2001:db8::1",
            "only loopback, private and link-local addresses may bypass the proxy",
        ),
        (
            "0.0.0.0/0",
            "only loopback, private and link-local ranges may bypass the proxy",
        ),
        (
            "192.0.0.0/8",
            "only loopback, private and link-local ranges may bypass the proxy",
        ),
        (
            "::/0",
            "only loopback, private and link-local ranges may bypass the proxy",
        ),
        ("example.com:443", "expected a domain, IP address or CIDR"),
    ] {
        assert_eq!(
            RequiredProxy::new("http://proxy.example", &direct(&[entry])),
            Err(RequiredProxyError::InvalidDirectEntry {
                entry: entry.to_string(),
                reason,
            }),
            "{entry}"
        );
    }
}

#[test]
fn pinned_hosts_include_subdomains_only() {
    assert!(is_pinned_host("chatgpt.com"));
    assert!(is_pinned_host("API.OpenAI.com."));
    assert!(!is_pinned_host("notopenai.com"));
    assert!(!is_pinned_host("openai.com.evil.example"));
}

#[test]
fn url_credentials_are_redacted() {
    assert_eq!(
        redact_url_credentials("https://user:secret@proxy.example:443"),
        "https://[REDACTED]@proxy.example:443"
    );
    assert_eq!(
        redact_url_credentials("user:secret@proxy.example:3128"),
        "http://[REDACTED]@proxy.example:3128"
    );
    assert_eq!(
        redact_url_credentials("http://127.0.0.1:3128"),
        "http://127.0.0.1:3128"
    );
    assert!(carries_credentials("https://user@proxy.example"));
    assert!(!carries_credentials("localhost,127.0.0.1"));
}

#[test]
fn bypass_matches_direct_domains_networks_and_never_pinned_hosts() {
    let proxy = RequiredProxy::new(
        "http://proxy.example:3128",
        &direct(&[
            "192.168.0.0/16",
            "fd7a:115c:a1e0::/48",
            ".ts.net",
            "npmjs.org",
        ]),
    )
    .expect("valid");

    for host in [
        "localhost",
        "127.0.0.1",
        "[::1]",
        "192.168.1.20",
        "fd7a:115c:a1e0::5",
        "box.tail1234.ts.net",
        "registry.npmjs.org",
        "npmjs.org",
    ] {
        assert!(proxy.bypasses_host(host), "{host}");
    }
    for host in [
        "api.openai.com",
        "192.169.0.1",
        "127.0.0.2",
        "evilnpmjs.org",
        "ts.net.example",
    ] {
        assert!(!proxy.bypasses_host(host), "{host}");
    }
    let destination = |url: &str| url::Url::parse(url).expect("url");
    assert_eq!(
        proxy.proxy_for(&destination("https://chatgpt.com/backend-api")),
        Some("http://proxy.example:3128")
    );
    assert_eq!(
        proxy.proxy_for(&destination("http://192.168.0.7:8080/mcp")),
        None
    );
}

#[test]
fn reloader_swaps_on_change_and_keeps_previous_proxy_on_failure() {
    let dir = tempfile::tempdir().expect("tempdir");
    let marker = dir.path().join("revision");
    std::fs::write(&marker, "").expect("marker");
    let first = RequiredProxy::new("http://first.example", &[]).expect("valid");
    let second = RequiredProxy::new("http://second.example", &[]).expect("valid");
    let next = Arc::new(Mutex::new(Ok(second)));
    let reloader = Reloader::new(vec![marker.clone()], Duration::ZERO, {
        let next = Arc::clone(&next);
        move || next.lock().expect("next").clone()
    });
    let current = RwLock::new(Arc::new(first));
    let touch = |seconds| {
        std::fs::File::options()
            .write(true)
            .open(&marker)
            .and_then(|file| {
                file.set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(seconds))
            })
            .expect("touch");
    };
    let current_url =
        |current: &RwLock<Arc<RequiredProxy>>| current.read().expect("current").url().to_string();

    reloader.refresh(&current);
    assert_eq!(current_url(&current), "http://first.example");

    touch(1);
    reloader.refresh(&current);
    assert_eq!(current_url(&current), "http://second.example");

    *next.lock().expect("next") = Err("credential store unavailable".to_string());
    touch(2);
    reloader.refresh(&current);
    assert_eq!(current_url(&current), "http://second.example");
}
