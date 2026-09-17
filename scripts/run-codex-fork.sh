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

if [[ -f "$repo_root/codex-package.json" ]]; then
  default_binary="$repo_root/bin/codex"
else
  default_binary="$repo_root/codex-rs/target/release/codex"
fi
codex_binary="${CODEX_FORK_BINARY:-$default_binary}"
# The fork binary loads [env] before startup and validates proxy configuration.
# Keep the launcher free of a partial TOML parser so both entrypoints agree.

if [[ ! -x "$codex_binary" ]]; then
  printf 'Codex fork binary is not executable: %s\n' "$codex_binary" >&2
  printf 'Build/install the release binary or set CODEX_FORK_BINARY.\n' >&2
  exit 1
fi

# This personal launcher opts into the fork's same-user native TUI controls.
# Set CODEX_TUI_CONTROL=0 to disable; the raw binary remains opt-in.
export CODEX_TUI_CONTROL="${CODEX_TUI_CONTROL:-1}"
exec "$codex_binary" "$@"
