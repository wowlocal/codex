use super::*;
use pretty_assertions::assert_eq;

fn pairs(values: &[(&str, &str)]) -> Vec<(OsString, OsString)> {
    values
        .iter()
        .map(|(key, value)| ((*key).into(), (*value).into()))
        .collect()
}

#[test]
fn proxy_settings_are_never_exported_and_inherited_credentials_are_removed() {
    let resolved = resolve(
        r#"
[env]
HTTPS_PROXY = "https://user:secret@proxy.example:443"
no_proxy = "localhost"
NPM_CONFIG_PROXY = "http://npm.example"
EDITOR = "vim"
PAGER = "less"

[proxy]
direct = ["192.168.0.0/16", ".ts.net"]
"#,
        pairs(&[
            ("PAGER", "more"),
            ("http_proxy", "http://user:secret@inherited.example"),
            ("ALL_PROXY", "http://127.0.0.1:3128"),
            ("PATH", "/usr/bin"),
        ]),
    )
    .expect("valid startup configuration");

    assert_eq!(
        resolved,
        Resolved {
            set: pairs(&[("EDITOR", "vim")]),
            remove: vec!["http_proxy".into()],
            direct: vec!["192.168.0.0/16".to_string(), ".ts.net".to_string()],
            env_proxy_keys: vec![
                "HTTPS_PROXY".to_string(),
                "NPM_CONFIG_PROXY".to_string(),
                "no_proxy".to_string(),
            ],
        }
    );
}

#[test]
fn malformed_configuration_fails_without_disclosing_its_contents() {
    let error = resolve(
        "[env]\nHTTPS_PROXY = \"https://user:secret@proxy.example\n",
        std::iter::empty(),
    )
    .expect_err("malformed TOML");

    assert_eq!(
        error.to_string(),
        "Error loading config.toml: invalid startup [env] configuration"
    );
}
