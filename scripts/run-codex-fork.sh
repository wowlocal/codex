#!/usr/bin/env bash

set -euo pipefail

script_path="${BASH_SOURCE[0]}"
while [[ -L "$script_path" ]]; do
  script_dir="$(cd -P "$(dirname "$script_path")" >/dev/null 2>&1 && pwd)"
  link_target="$(readlink "$script_path")"
  if [[ "$link_target" == /* ]]; then
    script_path="$link_target"
  else
    script_path="$script_dir/$link_target"
  fi
done
script_dir="$(cd -P "$(dirname "$script_path")" >/dev/null 2>&1 && pwd)"
repo_root="$(cd "$script_dir/.." >/dev/null 2>&1 && pwd)"

codex_binary="${CODEX_FORK_BINARY:-$repo_root/codex-rs/target/release/codex}"
codex_config="${CODEX_FORK_CONFIG:-${CODEX_HOME:-$HOME/.codex}/config.toml}"

read_proxy_value() {
  local config_path="$1"
  local proxy_key="$2"

  /usr/bin/awk -v key="$proxy_key" '
    /^\[env\][[:space:]]*$/ { in_env = 1; next }
    /^\[/ { in_env = 0 }
    in_env && $0 ~ "^[[:space:]]*" key "[[:space:]]*=" {
      value = $0
      sub("^[[:space:]]*" key "[[:space:]]*=[[:space:]]*\\\"", "", value)
      sub("\\\"[[:space:]]*$", "", value)
      print value
      exit
    }
  ' "$config_path"
}

# `[env]` is a fork launcher convention. Codex itself does not import this
# table into its own process environment. Existing process variables win.
if [[ -r "$codex_config" ]]; then
  for proxy_key in HTTPS_PROXY HTTP_PROXY ALL_PROXY NO_PROXY; do
    if [[ -n "${!proxy_key:-}" ]]; then
      continue
    fi
    proxy_value="$(read_proxy_value "$codex_config" "$proxy_key")"
    if [[ -n "$proxy_value" ]]; then
      export "$proxy_key=$proxy_value"
    fi
  done
fi

if [[ ! -x "$codex_binary" ]]; then
  printf 'Codex fork binary is not executable: %s\n' "$codex_binary" >&2
  printf 'Build/install the release binary or set CODEX_FORK_BINARY.\n' >&2
  exit 1
fi

exec "$codex_binary" "$@"
