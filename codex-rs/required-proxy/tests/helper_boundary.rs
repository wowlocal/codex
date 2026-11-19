use std::error::Error;
use std::process::Command;

use codex_required_proxy::COMMAND_PROXY_ENV;
use codex_required_proxy::RequiredProxy;
use codex_required_proxy::RequiredProxyError;

#[test]
fn helper_boundary_never_silently_becomes_direct() -> Result<(), Box<dyn Error>> {
    const CASE: &str = "CODEX_TEST_HELPER_PROXY_CASE";
    if let Ok(case) = std::env::var(CASE) {
        match case.as_str() {
            "missing" => {
                assert!(!codex_required_proxy::bootstrap_local_helper_from_env()?);
                assert!(matches!(
                    codex_required_proxy::require_local_helper_proxy(),
                    Err(RequiredProxyError::MissingHelperBoundary)
                ));
                codex_required_proxy::install(
                    RequiredProxy::new("http://unreachable.invalid:3128", &[])?,
                    None,
                )?;
                assert!(codex_required_proxy::command_proxy_env_checked(None).is_err());
                let env: std::collections::HashMap<_, _> =
                    codex_required_proxy::command_proxy_env(None)
                        .into_iter()
                        .collect();
                assert_eq!(env["HTTPS_PROXY"], "http://codex:codex-local@127.0.0.1:0");
            }
            "valid" => {
                codex_required_proxy::require_local_helper_proxy()?;
                let proxy = codex_required_proxy::current().ok_or("missing delegated policy")?;
                assert_eq!(proxy.url(), "http://codex:codex-local@127.0.0.1:32123");
                let env: std::collections::HashMap<_, _> =
                    codex_required_proxy::command_proxy_env_checked(Some("test-thread"))?
                        .into_iter()
                        .collect();
                for key in [
                    "HTTPS_PROXY",
                    "pip_proxy",
                    "npm_config_proxy",
                    "YARN_HTTPS_PROXY",
                    "grpc_proxy",
                ] {
                    assert_eq!(
                        env[key],
                        "http://thread-test-thread:codex-local@127.0.0.1:32123"
                    );
                }
                assert_eq!(env[COMMAND_PROXY_ENV], "127.0.0.1:32123");
                assert!(!env["NO_PROXY"].contains('*'));
            }
            "invalid" => {
                assert!(matches!(
                    codex_required_proxy::require_local_helper_proxy(),
                    Err(RequiredProxyError::InvalidCommandProxy)
                ));
                assert!(!codex_required_proxy::is_active());
            }
            _ => panic!("unknown test case"),
        }
        return Ok(());
    }
    for (case, marker) in [
        ("missing", None),
        ("valid", Some("127.0.0.1:32123")),
        ("invalid", Some("192.0.2.1:3128")),
        ("invalid", Some("127.0.0.1:0")),
        ("invalid", Some("localhost:3128")),
        ("invalid", Some("http://user:fixture-secret@127.0.0.1:3128")),
    ] {
        let mut command = Command::new(std::env::current_exe()?);
        command
            .args(["--exact", "helper_boundary_never_silently_becomes_direct"])
            .env(CASE, case)
            .env("CODEX_FORK_REQUIRE_PROXY", "1")
            .env_remove("CODEX_FORK_DANGEROUSLY_ALLOW_DIRECT")
            .env_remove(COMMAND_PROXY_ENV);
        if let Some(marker) = marker {
            command.env(COMMAND_PROXY_ENV, marker);
        }
        let output = command.output()?;
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(())
}
