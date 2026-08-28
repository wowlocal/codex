# Fork notes

This branch keeps its product changes narrow and uses the upstream release pipeline wherever
possible. The current fork-specific behavior is:

- the MCP Apps browser bridge;
- HTTPS environment-proxy support for Responses WebSocket connections; and
- a local launcher that gives the Codex process and its built-in MCP clients the same proxy
  environment.

## Proxy-aware launcher

Codex and Reqwest honor standard process environment variables such as `HTTPS_PROXY`,
`HTTP_PROXY`, `ALL_PROXY`, and `NO_PROXY`. A top-level `[env]` table in `config.toml` does not become
the environment of the Codex process by itself. This matters for `codex_apps`: if Codex starts
without the proxy variables, the MCP handshake can take a direct route even while a separately
launched test process works through a proxy.

[`scripts/run-codex-fork.sh`](scripts/run-codex-fork.sh) provides a Codex-only launch boundary. It:

1. preserves proxy variables already present in the process environment;
2. fills missing proxy variables from the `[env]` table in
   `${CODEX_HOME:-$HOME/.codex}/config.toml`;
3. never prints proxy values; and
4. executes `codex-rs/target/release/codex` without adding another long-lived process.

The expected local configuration uses one-line, double-quoted values:

```toml
[env]
HTTPS_PROXY = "https://user:password@proxy.example:443"
HTTP_PROXY = "https://user:password@proxy.example:443"
NO_PROXY = "localhost,127.0.0.1,::1,192.168.0.50"
```

The `NO_PROXY` entry keeps loopback MCP servers and other local development endpoints direct while
external Codex and `codex_apps` traffic continues through the configured proxy. Add the exact host
or IP address of each LAN MCP server because loopback entries do not cover private-network peers.

Run the tracked launcher directly:

```shell
./scripts/run-codex-fork.sh
```

To make it the global command while keeping the launcher in version control:

```shell
ln -s /absolute/path/to/codex/scripts/run-codex-fork.sh ~/.cargo/bin/codex
```

Use `CODEX_FORK_CONFIG` to test another config file and `CODEX_FORK_BINARY` to select another built
binary. These overrides are launcher-only and are not Codex configuration keys.

## Local release identity

Local release builds should use a nonzero, fork-qualified SemVer version such as
`0.151.0-fork.<short-sha>`. Shipping the workspace placeholder `0.0.0` can activate fixtures and
announcements intended only for test builds.

For a release-like macOS Apple Silicon artifact, use the upstream Cargo `release` profile for
`aarch64-apple-darwin`, archive dSYMs, strip the binaries with the upstream release script, and sign
the final artifacts. Local ad-hoc signing validates binary integrity but is not equivalent to the
Developer ID signing and notarization used by an official OpenAI release.
