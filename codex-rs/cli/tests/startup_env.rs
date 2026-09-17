use std::process::Command;

use codex_utils_cargo_bin::cargo_bin;

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

#[test]
fn direct_binary_rejects_invalid_configured_proxy_before_starting() -> anyhow::Result<()> {
    let home = tempfile::tempdir()?;
    std::fs::write(
        home.path().join("config.toml"),
        "[env]\nHTTPS_PROXY = 'ftp://user:secret@proxy.invalid'\n",
    )?;
    let mut command = Command::new(cargo_bin("codex")?);
    command
        .arg("--version")
        .env("CODEX_HOME", home.path())
        .env_remove("CODEX_FORK_CONFIG");
    for key in [
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "NO_PROXY",
        "http_proxy",
        "https_proxy",
        "all_proxy",
        "no_proxy",
    ] {
        command.env_remove(key);
    }
    let output = command.output()?;
    assert!(!output.status.success());
    let error = String::from_utf8(output.stderr)?;
    assert!(error.contains("unsupported HTTPS_PROXY URL"), "{error}");
    assert!(!error.contains("secret"));
    Ok(())
}

#[test]
fn inherited_proxy_overrides_config_even_when_binary_is_launched_directly() -> anyhow::Result<()> {
    let home = tempfile::tempdir()?;
    std::fs::write(
        home.path().join("config.toml"),
        "[env]\nHTTPS_PROXY = 'ftp://invalid' # overridden\n",
    )?;
    let output = Command::new(cargo_bin("codex")?)
        .arg("--version")
        .env("CODEX_HOME", home.path())
        .env_remove("CODEX_FORK_CONFIG")
        .env("HTTPS_PROXY", "http://127.0.0.1:9")
        .output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}
