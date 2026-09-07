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
4. executes `bin/codex` in a release package or `codex-rs/target/release/codex` in a source
   checkout without adding another long-lived process.

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

## Native TUI settings control (Unix)

The raw binary opts in with `CODEX_TUI_CONTROL=1`. This fork's launcher enables
it by default; `CODEX_TUI_CONTROL=0` disables it. Each TUI publishes a private
`$CODEX_HOME/tui-control/<pid>-<instance>/session.json` rendezvous record and a
same-user Unix socket. The endpoint belongs to the displayed TUI, including
cached task switches; no title parsing, keyboard injection, or external
app-server proxy is involved. The initial implementation is Unix-only.

The JSONL protocol is version 1. `status/read` returns the current task ID,
effective model/effort, service tier, supported effort choices, collaboration mode, focus,
readiness, activity and context usage. `status/subscribe` sends an initial
snapshot and subsequent changes. Responses are wrapped in `{"result": ...}`.
The `revision` changes when the control target or its settings/readiness/focus
change; activity and context-only updates do not invalidate a settings request.

Submit `effort/set` with a UUID `requestId`, `expectedThreadId`,
`expectedRevision`, and an explicit `effort`. The TUI checks selection, focus,
readiness, model support and native task ownership, then uses the existing
thread settings operation. Explicit supported Max/Ultra values are permitted;
keyboard shortcut behavior is unchanged. This changes next-turn settings and
preserves Plan mode and unrelated settings without persisting global defaults.

Submit `fast/set` with the same request identity/selection fields and an explicit
`enabled` boolean. `status/read` advertises `serviceTier` and `fastServiceTier`;
the latter is null when the current model/account cannot select Fast. Enabling
uses the advertised tier ID; disabling sends explicit `default` routing, so a
Fast-by-default model stays disabled. The ordinary settings notification confirms
the change. Both commands preserve the visible Plan mask, including before its
first turn, without writing config.toml. Existing CLI processes must restart to
load this endpoint.

`request/read` with the same `requestId` reports `pending`, `applied`,
`rejected`, or `unconfirmed`. Applied means a matching native settings event
was observed (or the requested setting already held). Queued does not mean
applied. Terminal outcomes include an `outcome` object. A disconnect/lost event
stream or a 30-second confirmation deadline is unconfirmed, not success.
The last 64 requests are retained for the lifetime of that TUI instance.
Repeating an identical retained request returns its result without another
write; reusing its ID with different parameters is rejected. Unknown/expired
IDs must not be blindly resubmitted. One settings request can be pending at once, shared by both controls.

Rendezvous directories are private and owner-checked; peers must have the same
UID. Frames are bounded to 4096 bytes, connections to eight, and writes have a
deadline. Clean exit removes only that instance's own files. Conversation,
approval, prompt, tool, and account-credential APIs are not exposed here.

## Local release identity

Local release builds should use a nonzero, fork-qualified SemVer version such as
`0.153.4-fork.1`. Shipping the workspace placeholder `0.0.0` can activate fixtures and
announcements intended only for test builds.

For a release-like macOS Apple Silicon artifact, use the upstream Cargo `release` profile for
`aarch64-apple-darwin`, archive dSYMs, strip the binaries with the upstream release script, and sign
the final artifacts. Local ad-hoc signing validates binary integrity but is not equivalent to the
Developer ID signing and notarization used by an official OpenAI release.

## Local build resource limits

This fork defaults to one Cargo build job in `codex-rs/.cargo/config.toml`.
Run compilation and tests sequentially on the 24 GB development Mac; do not
start a release build while a test build or Clippy is still running. Use
`nice -n 10 cargo build --release --target aarch64-apple-darwin -p codex-cli --bin codex`
for the release and `NEXTEST_TEST_THREADS=2 nice -n 10 just test -p codex-tui`
for TUI checks. A larger machine can explicitly override the build limit with
`-j N`. These settings limit concurrency and scheduling priority, not the
memory allocation of a single compiler or linker process.
