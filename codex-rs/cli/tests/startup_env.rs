use std::process::Command;

use codex_utils_cargo_bin::cargo_bin;
use pretty_assertions::assert_eq;

#[test]
fn doctor_rejects_malformed_startup_configuration_without_exposing_credentials()
-> anyhow::Result<()> {
    let home = tempfile::tempdir()?;
    std::fs::write(
        home.path().join("config.toml"),
        "[env]\nHTTPS_PROXY = 'https://user:secret@proxy.invalid' trailing\n",
    )?;
    for args in [
        vec!["doctor", "--json"],
        vec!["doctor", "--probe-filesystem-path", "."],
    ] {
        let output = Command::new(cargo_bin("codex")?)
            .args(args)
            .env("CODEX_HOME", home.path())
            .env_remove("CODEX_FORK_CONFIG")
            .output()?;
        assert_eq!(output.status.code(), Some(1));
        assert!(output.stdout.is_empty());
        let error = String::from_utf8(output.stderr)?;
        assert!(
            error.contains("invalid startup [env] configuration"),
            "{error}"
        );
        assert!(!error.contains("secret"));
    }
    Ok(())
}

#[test]
fn explicitly_selected_missing_config_cannot_silently_disable_proxy_configuration()
-> anyhow::Result<()> {
    let home = tempfile::tempdir()?;
    let output = Command::new(cargo_bin("codex")?)
        .arg("--version")
        .env("CODEX_HOME", home.path())
        .env("CODEX_FORK_CONFIG", home.path().join("missing.toml"))
        .output()?;
    assert!(!output.status.success());
    assert!(
        String::from_utf8(output.stderr)?.contains("cannot read startup environment configuration")
    );
    Ok(())
}

fn codex(home: &std::path::Path) -> anyhow::Result<Command> {
    let mut command = Command::new(cargo_bin("codex")?);
    command
        .env("CODEX_HOME", home)
        .env_remove("CODEX_FORK_CONFIG")
        .env_remove("CODEX_FORK_DANGEROUSLY_ALLOW_DIRECT")
        .env_remove("CODEX_THREAD_ID")
        .env_remove("CODEX_SESSION_ID");
    Ok(command)
}

#[test]
fn proxy_variables_in_env_are_rejected_with_migration_hint() -> anyhow::Result<()> {
    let home = tempfile::tempdir()?;
    std::fs::write(
        home.path().join("config.toml"),
        "[env]\nHTTPS_PROXY = 'https://user:secret@proxy.invalid'\nNO_PROXY = 'localhost'\n",
    )?;
    let output = codex(home.path())?.args(["login", "status"]).output()?;
    assert!(!output.status.success());
    let error = String::from_utf8(output.stderr)?;
    assert!(
        error.contains("sets HTTPS_PROXY, NO_PROXY in [env]")
            && error.contains("Run `codex proxy import`"),
        "{error}"
    );
    assert!(!error.contains("secret"));
    Ok(())
}

#[test]
fn kill_switch_refuses_to_start_without_a_stored_proxy() -> anyhow::Result<()> {
    let home = tempfile::tempdir()?;
    let output = codex(home.path())?
        .args(["login", "status"])
        .env("CODEX_FORK_REQUIRE_PROXY", "1")
        .output()?;
    assert!(!output.status.success());
    let error = String::from_utf8(output.stderr)?;
    assert!(
        error.contains("Refusing to start without the proxy (kill switch)"),
        "{error}"
    );

    // Managing the proxy itself must still work.
    let status = codex(home.path())?
        .args(["proxy", "status"])
        .env("CODEX_FORK_REQUIRE_PROXY", "1")
        .output()?;
    assert!(status.status.success());
    assert!(String::from_utf8(status.stdout)?.contains("Kill switch: on"));
    Ok(())
}

#[test]
fn agent_sessions_cannot_change_the_proxy() -> anyhow::Result<()> {
    let home = tempfile::tempdir()?;
    for action in ["set", "clear", "import"] {
        let output = codex(home.path())?
            .args(["proxy", action])
            .env("CODEX_THREAD_ID", "thread")
            .output()?;
        assert!(!output.status.success());
        assert!(
            String::from_utf8(output.stderr)?.contains("cannot run inside a Codex agent session"),
            "{action}"
        );
    }
    Ok(())
}
