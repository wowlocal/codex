use codex_utils_absolute_path::AbsolutePathBuf;
use std::path::Path;
use std::process::Command;
use tempfile::TempDir;

// These variables can redirect Git to an untrusted repository or inject command-scoped settings.
pub(crate) const REPOSITORY_LOCAL_GIT_ENVIRONMENT_VARIABLES: &[&str] = &[
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_CEILING_DIRECTORIES",
    "GIT_COMMON_DIR",
    "GIT_CONFIG",
    "GIT_CONFIG_COUNT",
    "GIT_CONFIG_PARAMETERS",
    "GIT_DIR",
    "GIT_DISCOVERY_ACROSS_FILESYSTEM",
    "GIT_GRAFT_FILE",
    "GIT_IMPLICIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_NAMESPACE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_PREFIX",
    "GIT_REPLACE_REF_BASE",
    "GIT_SHALLOW_FILE",
    "GIT_WORK_TREE",
];

/// Distinguishes isolated background Git operations from explicitly requested user operations.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum PluginGitMode {
    /// Ignores repository-scoped Git configuration inherited from the launch project.
    Automatic,
    /// Preserves the configuration explicitly available to user-requested Git commands.
    Manual,
}

impl PluginGitMode {
    /// Creates a Git command with the repository-environment policy for this operation.
    /// Automatic remote lookups and initial clones must also select a trusted repository.
    pub(crate) fn command(self, git_binary: &Path) -> Command {
        let mut command = codex_utils_process::background_command(git_binary);
        command.args(["-c", codex_git_utils::SAFE_BARE_REPOSITORY_CONFIG]);
        if matches!(self, Self::Automatic) {
            command.env("GIT_OPTIONAL_LOCKS", "0");
            for name in REPOSITORY_LOCAL_GIT_ENVIRONMENT_VARIABLES {
                command.env_remove(name);
            }
        }
        let proxy_env = codex_required_proxy::command_proxy_env(None);
        if !proxy_env.is_empty() {
            // Unsupported transports must never become an implicit escape through a URL
            // rewrite or a remote helper. Individual direct SSH/git remotes opt in below.
            command.env("GIT_ALLOW_PROTOCOL", "http:https:file");
            command.env("GIT_PROXY_COMMAND", "");
            if let Some((_, gateway)) = proxy_env.iter().find(|(name, _)| name == "HTTPS_PROXY") {
                command.args([
                    "-c",
                    &format!("http.proxy={gateway}"),
                    "-c",
                    &format!("remote.origin.proxy={gateway}"),
                ]);
            }
            command.envs(proxy_env);
        }
        command
    }
}

/// Validates the effective remote and pins Git's HTTP transport to Codex's gateway.
/// Git's SSH/native transports ignore HTTP_PROXY, so they require an explicit direct host.
pub(crate) fn configure_git_remote(command: &mut Command, source: &str) -> Result<(), String> {
    let Some(proxy) = codex_required_proxy::current() else {
        return Ok(());
    };
    let proxy_env =
        codex_required_proxy::command_proxy_env_checked(None).map_err(|err| err.to_string())?;
    configure_git_remote_with_proxy(command, source, &proxy, proxy_env)
}

/// Revalidates every remote before checkout can lazily fetch missing blobs.
/// Checking only origin would leave a second promisor remote outside the proxy policy.
/// The command must contain only global Git options and its repository selection so far.
pub(crate) fn configure_git_repository_origin(command: &mut Command) -> Result<(), String> {
    let Some(proxy) = codex_required_proxy::current() else {
        return Ok(());
    };
    let proxy_env =
        codex_required_proxy::command_proxy_env_checked(None).map_err(|err| err.to_string())?;
    configure_git_repository_origin_with_proxy(command, &proxy, proxy_env)
}

fn configure_git_repository_origin_with_proxy(
    command: &mut Command,
    proxy: &codex_required_proxy::RequiredProxy,
    proxy_env: Vec<(String, String)>,
) -> Result<(), String> {
    let output = git_configuration_command(command)
        .args(["config", "--null", "--get-regexp", r"^remote\..*\.url$"])
        .output()
        .map_err(|_| "failed to read Git marketplace remotes".to_string())?;
    if output.status.code() == Some(1) {
        // Repositories without remotes have nothing to fetch. The default transport
        // policy still disallows SSH/native remotes and pins HTTP to the gateway.
        return Ok(());
    }
    if !output.status.success() {
        return Err("failed to read Git marketplace remotes".to_string());
    }
    let mut seen = std::collections::HashSet::new();
    let mut allow_ssh = false;
    let mut allow_git = false;
    for record in output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty())
    {
        let record = std::str::from_utf8(record)
            .map_err(|_| "Git marketplace remote is not UTF-8".to_string())?;
        let (key, source) = record
            .split_once('\n')
            .ok_or_else(|| "invalid Git marketplace remote configuration".to_string())?;
        // Fetch uses the first configured URL for each remote. NUL separation
        // preserves its complete value and prevents later push URLs from hiding it.
        if !seen.insert(key) {
            continue;
        }
        if source.is_empty() {
            return Err("Git marketplace remote URL is empty".to_string());
        }
        configure_git_remote_with_proxy(command, source, proxy, proxy_env.clone())?;
        for (name, value) in command.get_envs() {
            if name == "GIT_ALLOW_PROTOCOL"
                && let Some(value) = value
            {
                allow_ssh |= value
                    .to_string_lossy()
                    .split(':')
                    .any(|value| value == "ssh");
                allow_git |= value
                    .to_string_lossy()
                    .split(':')
                    .any(|value| value == "git");
            }
        }
    }
    // Each remote has passed validation. Keep the union so an HTTP origin does not
    // disable a separately checked direct SSH promisor (or vice versa).
    let mut protocols = "http:https:file".to_string();
    if allow_ssh {
        protocols.push_str(":ssh");
    }
    if allow_git {
        protocols.push_str(":git");
    }
    command.env("GIT_ALLOW_PROTOCOL", protocols);
    Ok(())
}

// Configuration-only Git subprocesses use exactly the same executable, global options,
// environment policy and repository as the eventual command; neither operation connects.
fn git_configuration_command(command: &Command) -> Command {
    let mut reader = Command::new(command.get_program());
    reader.args(command.get_args());
    for (key, value) in command.get_envs() {
        match value {
            Some(value) => {
                reader.env(key, value);
            }
            None => {
                reader.env_remove(key);
            }
        }
    }
    if let Some(cwd) = command.get_current_dir() {
        reader.current_dir(cwd);
    }
    reader
}

fn configure_git_remote_with_proxy(
    command: &mut Command,
    source: &str,
    proxy: &codex_required_proxy::RequiredProxy,
    proxy_env: Vec<(String, String)>,
) -> Result<(), String> {
    command.env("GIT_ALLOW_PROTOCOL", "http:https:file");
    // Resolve url.*.insteadOf before checking the route. This Git operation only expands
    // configuration; --get-url performs no connection, including for SSH and helpers.
    let output = git_configuration_command(command)
        .args(["ls-remote", "--get-url", "--", source])
        .output()
        .map_err(|_| "failed to resolve Git marketplace transport".to_string())?;
    if !output.status.success() {
        return Err("failed to resolve Git marketplace transport".to_string());
    }
    let effective = std::str::from_utf8(&output.stdout)
        .map_err(|_| "Git marketplace URL is not UTF-8".to_string())?
        .trim();
    let protocol = approved_git_protocol(proxy, effective)?;
    if let Some(protocol) = protocol {
        command.env("GIT_ALLOW_PROTOCOL", format!("http:https:file:{protocol}"));
        command.env("GIT_PROXY_COMMAND", "");
        if protocol == "ssh" {
            // An SSH config alias, ProxyJump or ProxyCommand could replace the checked
            // direct destination. Use OpenSSH's direct transport with no config rewriting.
            command.env(
                "GIT_SSH_COMMAND",
                "ssh -F none -o ProxyCommand=none -o ProxyJump=none -o CanonicalizeHostname=no",
            );
            command.env("GIT_SSH_VARIANT", "ssh");
        }
    }
    if let Some((_, gateway)) = proxy_env.iter().find(|(name, _)| name == "HTTPS_PROXY") {
        // URL-specific Git config otherwise outranks http.proxy, even when environment
        // proxy variables are set. Pin both the original and rewritten destination.
        for url in [source, effective] {
            if url.starts_with("http://") || url.starts_with("https://") {
                command.args(["-c", &format!("http.{url}.proxy={gateway}")]);
            }
        }
    }
    command.envs(proxy_env);
    Ok(())
}

fn approved_git_protocol(
    proxy: &codex_required_proxy::RequiredProxy,
    source: &str,
) -> Result<Option<&'static str>, String> {
    if source.starts_with("\\\\") || source.starts_with("//") {
        let host = source
            .trim_start_matches(['/', '\\'])
            .split(['/', '\\'])
            .next()
            .unwrap_or_default();
        return if proxy.bypasses_host(host) {
            Ok(None)
        } else {
            Err("Git network filesystem source must be explicitly direct".to_string())
        };
    }
    let remote = if source.contains("://") {
        let url = url::Url::parse(source).map_err(|_| "invalid Git marketplace URL".to_string())?;
        match url.scheme() {
            "http" | "https" => return Ok(None),
            "file" if url.host_str().is_none_or(|host| proxy.bypasses_host(host)) => {
                return Ok(None);
            }
            "ssh" | "git+ssh" | "ssh+git" => (url.host_str().map(str::to_owned), "ssh"),
            "git" => (url.host_str().map(str::to_owned), "git"),
            _ => {
                return Err(
                    "Git marketplace transport cannot enforce the required proxy".to_string(),
                );
            }
        }
    } else if let Some((authority, path)) = source.split_once(':') {
        // A drive-letter path is local on Windows. A slash before ':' is local on Unix.
        if authority.contains('/')
            || authority.contains('\\')
            || (cfg!(windows) && authority.len() == 1 && path.starts_with(['/', '\\']))
        {
            return Ok(None);
        }
        if authority.is_empty() || path.starts_with(':') {
            return Err("Git remote helpers cannot enforce the required proxy".to_string());
        }
        (
            Some(authority.rsplit('@').next().unwrap_or(authority).to_owned()),
            "ssh",
        )
    } else {
        return Ok(None);
    };
    if remote
        .0
        .as_deref()
        .is_some_and(|host| proxy.bypasses_host(host))
    {
        Ok(Some(remote.1))
    } else {
        Err("Git SSH/native transport cannot use the required proxy; destination must be explicitly direct".to_string())
    }
}

/// Isolates an automatic Git command from repository discovery in the launch project.
///
/// The command must already have repository-scoped environment variables removed. Keep the
/// returned guard alive until the Git process exits because it owns the selected repository.
pub(crate) fn configure_trusted_git_repository(
    command: &mut Command,
    codex_home: &Path,
) -> Result<TempDir, String> {
    let canonical_home = AbsolutePathBuf::from_absolute_path(codex_home)
        .and_then(|path| path.canonicalize())
        .map_err(|err| format!("failed to resolve trusted Codex home: {err}"))?;
    let staging_root = canonical_home.join(".tmp");
    std::fs::create_dir_all(staging_root.as_path())
        .map_err(|err| format!("failed to create trusted Git repository root: {err}"))?;
    let staging_root = staging_root
        .canonicalize()
        .map_err(|err| format!("failed to resolve trusted Git repository root: {err}"))?;
    if !staging_root.as_path().starts_with(canonical_home.as_path()) {
        return Err("trusted Git repository root escapes Codex home".to_string());
    }

    let repository = tempfile::Builder::new()
        .prefix("git-")
        .tempdir_in(staging_root.as_path())
        .map_err(|err| format!("failed to create trusted Git repository: {err}"))?;
    for directory in ["objects", "refs"] {
        std::fs::create_dir(repository.path().join(directory))
            .map_err(|err| format!("failed to initialize trusted Git repository: {err}"))?;
    }
    std::fs::write(repository.path().join("HEAD"), "ref: refs/heads/main\n")
        .map_err(|err| format!("failed to initialize trusted Git repository HEAD: {err}"))?;
    command.env("GIT_DIR", repository.path());
    Ok(repository)
}

#[cfg(test)]
mod proxy_tests {
    use super::approved_git_protocol;

    fn git_command_with_config(config: &str) -> (tempfile::TempDir, std::process::Command) {
        let root = tempfile::tempdir().expect("temporary Git repo");
        let git_dir = root.path().join(".git");
        std::fs::create_dir_all(git_dir.join("objects")).unwrap();
        std::fs::create_dir_all(git_dir.join("refs")).unwrap();
        std::fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::write(git_dir.join("config"), config).unwrap();
        let mut command = super::PluginGitMode::Automatic.command(std::path::Path::new("git"));
        command.current_dir(root.path());
        command.env("GIT_CONFIG_NOSYSTEM", "1");
        command.env(
            "GIT_CONFIG_GLOBAL",
            if cfg!(windows) { "NUL" } else { "/dev/null" },
        );
        (root, command)
    }

    #[test]
    fn partial_checkout_allows_only_explicit_direct_ssh_origin() {
        let proxy = codex_required_proxy::RequiredProxy::new(
            "http://upstream.invalid:8080",
            &["host.internal".to_owned()],
        )
        .unwrap();
        for (origin, allowed) in [
            ("ssh://git@host.internal/org/repo", true),
            ("git@host.internal:org/repo", true),
            ("ssh://git@github.com/org/repo", false),
        ] {
            let (_root, mut command) = git_command_with_config(&format!(
                "[remote \"origin\"]\n    url = {origin}\n    promisor = true\n    partialclonefilter = blob:none\n"
            ));
            let result = super::configure_git_repository_origin_with_proxy(
                &mut command,
                &proxy,
                vec![("HTTPS_PROXY".into(), "http://codex@127.0.0.1:1234".into())],
            );
            assert_eq!(result.is_ok(), allowed, "{origin}: {result:?}");
            if allowed {
                command.args(["checkout", "HEAD"]);
                assert!(
                    command
                        .get_envs()
                        .any(|(key, value)| key == "GIT_ALLOW_PROTOCOL"
                            && value == Some(std::ffi::OsStr::new("http:https:file:ssh")))
                );
                assert!(
                    command
                        .get_envs()
                        .any(|(key, value)| key == "GIT_SSH_COMMAND"
                            && value
                                .is_some_and(|value| value.to_string_lossy().contains("-F none")))
                );
            }
        }
    }

    #[test]
    fn partial_checkout_checks_second_promisor_even_with_local_origin() {
        let (_root, mut command) = git_command_with_config(
            "[remote \"origin\"]\n    url = /tmp/local-repo\n    promisor = true\n[remote \"extra\"]\n    url = ssh://git@github.com/org/repo\n    promisor = true\n",
        );
        let proxy =
            codex_required_proxy::RequiredProxy::new("http://upstream.invalid:8080", &[]).unwrap();
        let result = super::configure_git_repository_origin_with_proxy(
            &mut command,
            &proxy,
            vec![("HTTPS_PROXY".into(), "http://codex@127.0.0.1:1234".into())],
        );
        assert!(
            result.is_err(),
            "an unchecked second promisor must not reach checkout"
        );
    }

    #[test]
    fn partial_checkout_preserves_multiple_checked_direct_promisors() {
        let (_root, mut command) = git_command_with_config(
            "[remote \"origin\"]\n    url = ssh://git@one.internal/org/repo\n    promisor = true\n[remote \"extra\"]\n    url = ssh://git@two.internal/org/repo\n    promisor = true\n[remote \"http\"]\n    url = https://github.com/org/repo\n    promisor = true\n",
        );
        let proxy = codex_required_proxy::RequiredProxy::new(
            "http://upstream.invalid:8080",
            &["one.internal".to_owned(), "two.internal".to_owned()],
        )
        .unwrap();
        super::configure_git_repository_origin_with_proxy(
            &mut command,
            &proxy,
            vec![("HTTPS_PROXY".into(), "http://codex@127.0.0.1:1234".into())],
        )
        .unwrap();
        assert!(
            command
                .get_envs()
                .any(|(key, value)| key == "GIT_ALLOW_PROTOCOL"
                    && value == Some(std::ffi::OsStr::new("http:https:file:ssh")))
        );
        let ssh = command
            .get_envs()
            .find_map(|(key, value)| (key == "GIT_SSH_COMMAND").then_some(value).flatten())
            .unwrap();
        assert!(ssh.to_string_lossy().contains("-F none"));
        assert!(!ssh.to_string_lossy().contains("HostName="));
        assert!(
            command
                .get_args()
                .any(|arg| arg
                    == "http.https://github.com/org/repo.proxy=http://codex@127.0.0.1:1234")
        );
    }

    #[test]
    fn partial_checkout_checks_first_fetch_url_not_last_push_url() {
        let (_root, mut command) = git_command_with_config(
            "[remote \"origin\"]\n    url = ssh://git@github.com/org/repo\n    url = ssh://git@host.internal/org/repo\n",
        );
        let proxy = codex_required_proxy::RequiredProxy::new(
            "http://upstream.invalid:8080",
            &["host.internal".to_owned()],
        )
        .unwrap();
        assert!(
            super::configure_git_repository_origin_with_proxy(
                &mut command,
                &proxy,
                vec![("HTTPS_PROXY".into(), "http://codex@127.0.0.1:1234".into())],
            )
            .is_err()
        );
    }

    #[test]
    fn partial_checkout_revalidates_rewritten_origin() {
        let (_root, mut command) = git_command_with_config(
            "[remote \"origin\"]\n    url = ssh://git@host.internal/org/repo\n[url \"ssh://git@github.com/\"]\n    insteadOf = ssh://git@host.internal/\n",
        );
        let proxy = codex_required_proxy::RequiredProxy::new(
            "http://upstream.invalid:8080",
            &["host.internal".to_owned()],
        )
        .unwrap();
        assert!(
            super::configure_git_repository_origin_with_proxy(
                &mut command,
                &proxy,
                vec![("HTTPS_PROXY".into(), "http://codex@127.0.0.1:1234".into())],
            )
            .is_err()
        );
    }

    #[test]
    fn git_url_rewrite_cannot_escape_to_ssh() {
        let (_root, mut command) = git_command_with_config(
            "[url \"ssh://git@github.com/\"]\n    insteadOf = https://github.com/\n",
        );
        let proxy =
            codex_required_proxy::RequiredProxy::new("http://upstream.invalid:8080", &[]).unwrap();
        let error = super::configure_git_remote_with_proxy(
            &mut command,
            "https://github.com/org/repo",
            &proxy,
            vec![("HTTPS_PROXY".into(), "http://codex@127.0.0.1:1234".into())],
        )
        .expect_err("SSH rewrite must be blocked before network access");
        assert!(error.contains("SSH/native transport"));
    }

    #[test]
    fn git_url_specific_config_cannot_disable_gateway() {
        let (_root, mut command) =
            git_command_with_config("[http \"https://github.com/org/repo\"]\n    proxy =\n");
        let proxy =
            codex_required_proxy::RequiredProxy::new("http://upstream.invalid:8080", &[]).unwrap();
        super::configure_git_remote_with_proxy(
            &mut command,
            "https://github.com/org/repo",
            &proxy,
            vec![("HTTPS_PROXY".into(), "http://codex@127.0.0.1:1234".into())],
        )
        .unwrap();
        let output = command
            .args([
                "config",
                "--get-urlmatch",
                "http.proxy",
                "https://github.com/org/repo",
            ])
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(
            String::from_utf8(output.stdout).unwrap().trim(),
            "http://codex@127.0.0.1:1234"
        );
    }

    #[test]
    fn unsupported_git_transports_cannot_bypass_proxy() {
        let proxy = codex_required_proxy::RequiredProxy::new(
            "http://proxy.invalid:8080",
            &["host.internal".to_owned()],
        )
        .expect("proxy");
        for source in [
            "git@github.com:org/repo.git",
            "ssh://git@github.com/org/repo",
            "git://github.com/org/repo",
            "ext::sh -c bad",
            "custom://host/repo",
            "file://outside.invalid/share",
            r"\\outside.invalid\share",
        ] {
            assert!(approved_git_protocol(&proxy, source).is_err(), "{source}");
        }
        for source in [
            "git@host.internal:org/repo.git",
            "ssh://git@localhost/org/repo",
        ] {
            assert_eq!(approved_git_protocol(&proxy, source).unwrap(), Some("ssh"));
        }
        assert_eq!(
            approved_git_protocol(&proxy, "git://127.0.0.1/repo").unwrap(),
            Some("git")
        );
        for source in [
            "https://github.com/org/repo",
            "http://localhost/repo",
            "file:///tmp/repo",
            "/tmp/repo",
            "./local/repo",
        ] {
            assert_eq!(
                approved_git_protocol(&proxy, source).unwrap(),
                None,
                "{source}"
            );
        }
    }
}
