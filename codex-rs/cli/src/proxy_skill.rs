//! The agent skill that explains this fork's proxy routing.

use std::path::Path;
use std::path::PathBuf;

const SKILL_NAME: &str = "codex-proxy-routing";

const SKILL: &str = r#"---
name: codex-proxy-routing
description: How network access works in this Codex. Use when a shell command's network request fails, hangs, is slow or returns a response with an X-Codex-Proxy header, before installing packages or calling external hosts from the shell, or when asked about the proxy.
---

# Codex proxy routing

Shell commands you run get `HTTP_PROXY`/`HTTPS_PROXY` pointing at Codex's local proxy
(`http://thread-<id>@127.0.0.1:<port>`). It forwards traffic to the user's upstream proxy,
whose address and credentials live in the system credential store and are never available
to you. Do not look for them, do not unset or override proxy variables, and do not bypass the
proxy with `--noproxy`, `NO_PROXY` or tools that ignore proxy variables.

## Rules

- OpenAI destinations (openai.com, chatgpt.com, oaistatic.com, oaiusercontent.com,
  openai.org, sentry.io and their subdomains) always use the upstream proxy. Nothing changes
  that.
- Loopback and the user's `[proxy] direct` list always connect directly.
- Package registries (npm, Yarn, PyPI, crates.io, Go, Homebrew, ghcr.io, RubyGems, Maven
  Central) connect directly for your commands by default.
- Everything else from your commands uses the upstream proxy unless you change this thread's
  routing. Codex's own traffic and MCP servers are not affected by your changes.

## Diagnosing failures

A response header `X-Codex-Proxy: upstream-failed` means the upstream proxy could not be
reached or refused the destination; the body says why. `direct-failed` means a direct
connection failed. Inspect with `curl -sS -D - -o /dev/null https://host/`.

## Changing this thread's routing

Only for programs you run in this thread (curl, gh, git, package managers), until Codex
restarts:

```shell
codex proxy route status
codex proxy route packages          # restore direct package registries
codex proxy route add example.com   # a domain (and subdomains) direct
codex proxy route remove example.com
codex proxy route default direct    # everything except OpenAI direct
codex proxy route default proxy
```

Prefer the narrowest change: a host before `default direct`. Whenever you
change routing, tell the user what you changed and why. Never run `codex proxy set`, `clear`
or `import`, and never edit `[proxy]` in config.toml unless the user explicitly asks.
"#;

/// Writes the skill into the Codex home, replacing an older copy.
pub(crate) fn install(codex_home: &Path) -> std::io::Result<PathBuf> {
    let directory = codex_home.join("skills").join(SKILL_NAME);
    std::fs::create_dir_all(&directory)?;
    let path = directory.join("SKILL.md");
    std::fs::write(&path, SKILL)?;
    Ok(path)
}
