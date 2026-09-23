# Fork notes

This branch keeps its product changes narrow and uses the upstream release pipeline wherever
possible. The current fork-specific behavior is:

- the MCP Apps browser bridge, including reconnect recovery;
- required proxy routing for HTTP, Responses and Realtime WebSocket clients;
- startup `[env]` loading in the CLI binary; and
- native TUI settings control compatible with BUSY Codex protocol version 1.

The current base is upstream `rust-v0.156.1`; the fork version is `0.156.1-fork.1`.
The 2026-09-23 port rebases all 19 commits from `8822eed9c7` onto that release,
preserving the MCP Apps bridge, required proxy routing, startup environment loading,
and BUSY native TUI controls.
The port starts from `codex/native-tui-control` at `a16413c46f`, with native `[env]`
loading restored from `c4e774a392` and Realtime routing restored from `b66720601b`.

## Proxy-aware launcher

The CLI loads the top-level `[env]` table in the user config before starting its runtime,
including when the binary is launched directly. Inherited variables take precedence. Proxy
variables accept uppercase or lowercase names; uppercase wins when both are inherited. Both
spellings are synchronized before clients start. Invalid configuration stops startup without
printing proxy credentials. TOML strings and comments are parsed by the TOML parser.
An unreadable explicitly selected `CODEX_FORK_CONFIG` or malformed TOML stops every CLI
subcommand, including `exec-server`: ignoring it could silently discard the configured proxy.

[`scripts/run-codex-fork.sh`](scripts/run-codex-fork.sh) provides a Codex-only launch boundary. It:

1. enables native TUI control unless explicitly disabled; and
2. executes `bin/codex` in a release package or `codex-rs/target/release/codex` in a source
   checkout. Environment loading and validation belong to the binary.

For example:

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

Use `CODEX_FORK_CONFIG` to select another file for startup environment loading and
`CODEX_FORK_BINARY` to select another binary in the launcher. Neither is a config.toml key.

Explicit environment proxies take precedence over system routing, including a system `DIRECT`
decision, even when the upstream `respect_system_proxy` feature is disabled. That feature still
controls system/PAC discovery when no environment proxy is set. If only one HTTP(S) proxy is
configured, it covers both schemes; `ALL_PROXY` is the
fallback for a missing scheme-specific entry. Connection failures, authentication failures,
TLS errors and unavailable system proxy discovery fail without retrying a direct route.
Switching from Responses WebSocket to HTTP still uses the configured proxy. Explicit
`NO_PROXY` destinations and dedicated loopback IPC remain direct. This is application routing,
not a system firewall: browsers and independently launched programs own their network policy.

OTLP/gRPC telemetry refuses to initialize when an environment proxy is configured, because
its Tonic transport cannot honor that proxy. Use OTLP/HTTP for logs, traces and metrics instead;
this conservative gRPC restriction also applies to destinations listed in `NO_PROXY`.

The managed command proxy also refuses direct fallback when its enabled HTTP(S) upstream is
invalid or unavailable. Its existing local/private-target bypass and explicit
`allow_upstream_proxy` policy remain unchanged. Unsupported upstream schemes fail closed.

Realtime uses the same connector and session HTTP factory as Responses, including TLS-to-proxy
and custom certificate authorities. This restores the functionality from `b66720601b`.

## Native TUI settings control (Unix)

The raw binary opts in with `CODEX_TUI_CONTROL=1`. This fork's launcher enables
it by default; `CODEX_TUI_CONTROL=0` disables it. Each TUI publishes a private
`$CODEX_HOME/tui-control/<pid>-<instance>/session.json` rendezvous record and a
same-user Unix socket. The endpoint belongs to the displayed TUI, including
cached task switches; no title parsing, keyboard injection, or external
app-server proxy is involved. The initial implementation is Unix-only.

The JSONL protocol is version 1. `status/read` returns the current task ID,
effective model/effort, service tier, supported effort choices, visible model choices, collaboration mode, focus,
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

Submit `model/set` with the same request identity/selection fields and explicit
`model` and `effort` values. The `models` snapshot array advertises each visible
picker entry as `model`, `name`, `levels`, and `default`. The endpoint rejects
models or effort values outside this catalog, preserves the visible Plan mask,
and confirms both settings through the ordinary native settings event. This
command does not start a turn or persist global defaults.

`request/read` with the same `requestId` reports `pending`, `applied`,
`rejected`, or `unconfirmed`. Applied means a matching native settings event
was observed (or the requested setting already held). Queued does not mean
applied. Terminal outcomes include an `outcome` object. A disconnect/lost event
stream or a 30-second confirmation deadline is unconfirmed, not success.
The last 64 requests are retained for the lifetime of that TUI instance.
Repeating an identical retained request returns its result without another
write; reusing its ID with different parameters is rejected. Unknown/expired
IDs must not be blindly resubmitted. One settings request can be pending at once across all controls.

Rendezvous directories are private and owner-checked; peers must have the same
UID. Frames are bounded to 4096 bytes, connections to eight, and writes have a
deadline. Clean exit removes only that instance's own files. Conversation,
approval, prompt, tool, and account-credential APIs are not exposed here.

## Local release identity

Local release builds should use a nonzero, fork-qualified SemVer version such as
`0.156.1-fork.1`. Shipping the workspace placeholder `0.0.0` can activate fixtures and
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

## Port validation (2026-09-23, macOS Apple Silicon)

- Rebased the 19 fork commits onto stable upstream `rust-v0.156.1` and adapted new
  daemon connection and test call sites to retain MCP Apps capabilities.
- Preserved Windows platform TLS validation when Realtime uses the shared
  proxy-aware connector. An invalid custom CA still fails without falling back
  to system trust; that path has regression coverage.
- The affected-crate run covered 7,253 tests. After adapting diagnostic fixtures,
  reviewing three TUI snapshots, and removing inherited proxy variables from the
  test runner, all 164 selected rechecks passed, including every initial failure.
  The additional diagnostic regression also passed. Production proxy handling
  was not relaxed.
- The diagnostic tests use a valid explicit startup configuration to exercise
  upstream doctor's handling of malformed regular configuration. A separate CLI
  regression verifies that malformed startup configuration still blocks both
  doctor modes without exposing credentials.
- Scoped `just fix` completed without warnings; `just fmt` completed successfully.
- `just write-config-schema` and `just bazel-lock-update` produced no schema or
  Bazel lock drift.
- macOS 27 rejects some Rust proc-macro dylibs stripped during compilation. Local
  test builds used `CARGO_PROFILE_DEV_STRIP=none` and
  `CARGO_PROFILE_TEST_STRIP=none`, with debug information and incremental builds
  disabled to limit disk usage. Release uses the unchanged upstream profile and
  strips binaries only after archiving their symbols.

The full workspace suite and native Windows/Linux checks were not run. Signing,
notarization, and final artifact checks are recorded with the release artifacts.

## Port validation (2026-09-17, macOS Apple Silicon)

- The affected-crate run covered config, network-proxy, API, MCP, TUI, CLI and
  app-server-client: 5,728 tests ran. The two CLI failures were fixed and rechecked;
  four unchanged terminal-color tests passed after removing the test runner's
  `NO_COLOR` override. Their snapshots did not need changes.
- The subsequent CLI, HTTP/WebSocket and targeted core Realtime/system-proxy run
  passed all 710 selected tests. Regression tests verify that a reachable origin
  receives no connection when a configured proxy is unavailable or rejects it.
- The remaining 22 network-proxy failures are DNS-environment dependent: this Mac
  resolves public test names such as `example.com` to `240.0.2.17`, so existing
  destination-policy checks reject them before forwarding. The destination policy
  was not weakened to make these tests pass.
- BUSY Codex's native client/model-menu/effort suites passed all 35 tests. An isolated
  live TUI also passed effort, Fast and model roundtrips, Plan/config preservation,
  focus checks, reconnect/replay recovery and stale-task rejection. No model prompt
  was submitted and no physical Bar workers were restarted.
- All 63 OTEL tests passed, including a subprocess regression verifying that gRPC
  logs, traces and metrics refuse every supported spelling of the proxy environment
  variables without connecting to an otherwise reachable collector.
- Config schema generation and `just bazel-lock-update` completed; the Bazel lock
  remained unchanged. The ordinary CLI binary reports `0.154.0-fork.1`.

The full workspace suite and native Windows/Linux checks were not run. This port
does not replace the installed binary or publish the branch automatically.
