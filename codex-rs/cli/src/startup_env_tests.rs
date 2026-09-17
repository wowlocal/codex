use super::*;
use pretty_assertions::assert_eq;

#[test]
fn parses_toml_and_covers_http_and_https_without_overriding_inherited_proxy() {
    let values = resolve(
        "[env]\nHTTPS_PROXY = 'https://configured.example:443' # comment\nCUSTOM = 'value'\n",
        BTreeMap::from([("https_proxy".into(), "inherited.example:8080".into())]),
    )
    .unwrap();
    assert_eq!(
        values,
        BTreeMap::from([
            ("CUSTOM".into(), "value".into()),
            ("HTTP_PROXY".into(), "http://inherited.example:8080".into()),
            ("http_proxy".into(), "http://inherited.example:8080".into()),
            ("HTTPS_PROXY".into(), "http://inherited.example:8080".into()),
            ("https_proxy".into(), "http://inherited.example:8080".into()),
        ])
    );
}

#[test]
fn invalid_proxy_fails_without_disclosing_credentials() {
    for value in [
        "https://secret@",
        "ftp://user:secret@proxy.example",
        "https://proxy.example/secret",
    ] {
        let error = resolve("", BTreeMap::from([("HTTPS_PROXY".into(), value.into())]))
            .unwrap_err()
            .to_string();
        assert!(!error.contains("secret"));
    }
}

#[test]
fn proxy_credentials_are_normalized_for_uri_based_clients() {
    let values = resolve(
        "",
        BTreeMap::from([(
            "HTTPS_PROXY".into(),
            "http://usér:päss@proxy.example:8080".into(),
        )]),
    )
    .unwrap();
    let normalized = "http://us%C3%A9r:p%C3%A4ss@proxy.example:8080";
    assert_eq!(
        values,
        BTreeMap::from([
            ("HTTP_PROXY".into(), normalized.into()),
            ("http_proxy".into(), normalized.into()),
            ("HTTPS_PROXY".into(), normalized.into()),
            ("https_proxy".into(), normalized.into()),
        ])
    );
}

#[test]
fn explicit_no_proxy_is_preserved_and_not_invented() {
    let configured = resolve(
        "[env]\nALL_PROXY = 'socks5h://proxy.example:1080'\nNO_PROXY = 'localhost,127.0.0.1'",
        BTreeMap::new(),
    )
    .unwrap();
    assert_eq!(
        configured.get(&OsString::from("no_proxy")),
        Some(&OsString::from("localhost,127.0.0.1"))
    );
    assert_eq!(resolve("", BTreeMap::new()).unwrap(), BTreeMap::new());
}

#[test]
fn empty_uppercase_no_proxy_cannot_leave_a_lowercase_wildcard_active() {
    let values = resolve(
        "",
        BTreeMap::from([
            ("HTTPS_PROXY".into(), "http://proxy.example:8080".into()),
            ("NO_PROXY".into(), "".into()),
            ("no_proxy".into(), "*".into()),
        ]),
    )
    .unwrap();
    assert_eq!(
        values,
        BTreeMap::from([
            ("HTTP_PROXY".into(), "http://proxy.example:8080".into()),
            ("http_proxy".into(), "http://proxy.example:8080".into()),
            ("HTTPS_PROXY".into(), "http://proxy.example:8080".into()),
            ("https_proxy".into(), "http://proxy.example:8080".into()),
            ("NO_PROXY".into(), "".into()),
            ("no_proxy".into(), "".into()),
        ])
    );
}
