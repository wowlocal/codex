//! Agent commands reach the network through Codex's command proxy, tagged with their thread.
//! This binary holds a single test because the command proxy address is process-wide.

use codex_protocol::config_types::ShellEnvironmentPolicy;
use codex_protocol::shell_environment::create_env_from_vars;
use pretty_assertions::assert_eq;

#[test]
fn command_env_replaces_inherited_proxies_with_the_thread_command_proxy() {
    let inherited = [
        ("PATH".to_string(), "/usr/bin".to_string()),
        (
            "HTTPS_PROXY".to_string(),
            "http://127.0.0.1:9999".to_string(),
        ),
    ];
    let policy = ShellEnvironmentPolicy::default();
    let before = create_env_from_vars(inherited.clone(), &policy, Some("t1"));
    assert_eq!(
        before.get("HTTPS_PROXY").map(String::as_str),
        Some("http://127.0.0.1:9999")
    );

    codex_required_proxy::set_command_proxy("127.0.0.1:4242".parse().expect("address"))
        .expect("publish command proxy");
    let env = create_env_from_vars(inherited, &policy, Some("t1"));
    let proxy_vars: Vec<(&str, &str)> = [
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "http_proxy",
        "https_proxy",
        "all_proxy",
        "NO_PROXY",
        "no_proxy",
        "NODE_USE_ENV_PROXY",
    ]
    .into_iter()
    .map(|key| (key, env.get(key).map(String::as_str).unwrap_or_default()))
    .collect();
    let url = "http://thread-t1@127.0.0.1:4242";
    let no_proxy = "localhost,127.0.0.1,::1";
    assert_eq!(
        proxy_vars,
        vec![
            ("HTTP_PROXY", url),
            ("HTTPS_PROXY", url),
            ("ALL_PROXY", url),
            ("http_proxy", url),
            ("https_proxy", url),
            ("all_proxy", url),
            ("NO_PROXY", no_proxy),
            ("no_proxy", no_proxy),
            ("NODE_USE_ENV_PROXY", "1"),
        ]
    );
    assert_eq!(
        codex_required_proxy::command_proxy_env(/*thread_id*/ None)[0],
        (
            "HTTP_PROXY".to_string(),
            "http://codex@127.0.0.1:4242".to_string()
        )
    );
}
