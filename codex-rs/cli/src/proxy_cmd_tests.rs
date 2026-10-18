use super::*;
use pretty_assertions::assert_eq;

#[test]
fn import_moves_proxy_variables_and_merges_no_proxy_into_direct() {
    let plan = import_plan(
        r#"
[env]
HTTPS_PROXY = "https://user:secret@proxy.example:443"
HTTP_PROXY = "https://user:secret@proxy.example"
NO_PROXY = "localhost,127.0.0.1,::1,192.168.0.0/16, .ts.net,192.168.0.0/16"
EDITOR = "vim"
"#,
        &["10.0.0.0/8".to_string()],
    )
    .expect("import plan");

    let direct: Vec<String> = ["10.0.0.0/8", "192.168.0.0/16", ".ts.net"]
        .map(String::from)
        .to_vec();
    assert_eq!(
        plan,
        ImportPlan {
            proxy: RequiredProxy::new("https://user:secret@proxy.example", &direct)
                .expect("valid proxy"),
            direct,
            env_keys: ["HTTPS_PROXY", "HTTP_PROXY", "NO_PROXY"]
                .map(String::from)
                .to_vec(),
            empties_env: false,
        }
    );
}

#[test]
fn import_refuses_ambiguous_or_unsafe_settings_without_disclosing_credentials() {
    for (config, message) in [
        (
            "[env]\nHTTPS_PROXY = \"https://user:secret@a.example\"\nHTTP_PROXY = \"http://user:secret@b.example\"\n",
            "[env] must set exactly one proxy URL in HTTP_PROXY, HTTPS_PROXY or ALL_PROXY; \
             store the proxy with `codex proxy set` instead",
        ),
        (
            "[env]\nHTTPS_PROXY = \"https://user:secret@a.example\"\nNO_PROXY = \"api.openai.com\"\n",
            "invalid [proxy] direct entry `api.openai.com`: OpenAI destinations always use the proxy",
        ),
        (
            "[env]\nEDITOR = \"vim\"\n",
            "[env] in config.toml does not set a proxy",
        ),
        (
            "[env]\nHTTPS_PROXY = \"https://user:secret@a.example\n",
            "config.toml is not valid TOML",
        ),
    ] {
        let error = import_plan(config, &[]).expect_err(config);
        assert_eq!(error.to_string(), message);
        assert!(!format!("{error:?}").contains("secret"));
    }
}
