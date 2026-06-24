# Configuration

For basic configuration instructions, see [this documentation](https://developers.openai.com/codex/config-basic).

For advanced configuration instructions, see [this documentation](https://developers.openai.com/codex/config-advanced).

For a full configuration reference, see [this documentation](https://developers.openai.com/codex/config-reference).

## Lifecycle hooks

Admins can set top-level `allow_managed_hooks_only = true` in
`requirements.toml` to ignore user, project, and session hook configs while
still allowing managed hooks from requirements and managed config layers. This
setting is only supported in `requirements.toml`; putting it in `config.toml`
does not enable managed-hooks-only mode.

## Process environment (`[env]`)

The `[env]` table sets environment variables in the Codex process itself at
startup, before any network client is built. This is the supported way to pin
proxy variables such as `HTTPS_PROXY`, `HTTP_PROXY`, and `NO_PROXY` without
exporting them in your shell:

```toml
[env]
HTTPS_PROXY = "https://user:pass@proxy.example.com:443"
HTTP_PROXY = "https://user:pass@proxy.example.com:443"
NO_PROXY = "localhost,127.0.0.1"
```

Notes:

- Each entry is applied only when the variable is **not already present** in the
  environment, so an inherited/shell value takes precedence (e.g.
  `HTTPS_PROXY=... codex` overrides the `[env]` value).
- `[env]` configures Codex's own process — including its model/API/WebSocket and
  auth traffic. It is distinct from `shell_environment_policy`, which governs the
  environment passed to shell tools the model runs.
- Values are read from the top-level `config.toml` and applied during early
  startup; they are stored in plaintext, so treat the file as sensitive when it
  contains credentials.
- Secure WebSocket (`wss`) traffic honors these proxy variables, including
  `https://` (TLS-to-proxy) proxies. Set `respect_system_proxy = true` under
  `[features]` to additionally honor the OS system proxy (macOS/Windows).
