#!/usr/bin/env bash
set -euo pipefail

version="${1:-0.1.0}"
mode="${2:---dry-run}"

if [[ "$mode" != "--dry-run" && "$mode" != "--execute" ]]; then
  echo "usage: $0 [version] [--dry-run|--execute]" >&2
  exit 2
fi

if ! grep -q "^version = \"${version}\"$" Cargo.toml; then
  echo "workspace version does not match ${version}" >&2
  exit 2
fi

if [[ "$mode" == "--execute" ]]; then
  if [[ -n "$(git status --porcelain --untracked-files=normal)" ]]; then
    echo "refusing to publish from a dirty worktree" >&2
    exit 2
  fi
  if [[ "$(git describe --tags --exact-match 2>/dev/null || true)" != "v${version}" ]]; then
    echo "refusing to publish without an exact v${version} tag at HEAD" >&2
    exit 2
  fi
fi

published() {
  cargo info "${1}@${version}" --registry crates-io >/dev/null 2>&1
}

publish_one() {
  local crate="$1"
  if [[ "$mode" == "--execute" ]] && published "$crate"; then
    echo "already published: ${crate}@${version}"
    return
  fi
  if [[ "$mode" == "--execute" ]]; then
    cargo publish -p "$crate" --locked
  elif [[ "$crate" == "slop-graph" || "$crate" == "slop-llm" || "$crate" == "slop-parse" || "$crate" == "slop-resolve" ]]; then
    cargo package -p "$crate" --locked --allow-dirty
  else
    cargo package -p "$crate" --allow-dirty --list >/dev/null
    echo "archive file list checked: ${crate}@${version} (full registry verification waits for dependency waves)"
  fi
}

wait_for_registry() {
  local crate="$1"
  [[ "$mode" != "--execute" ]] && return
  for _attempt in {1..30}; do
    published "$crate" && return
    sleep 10
  done
  echo "crates.io did not index ${crate}@${version} within five minutes" >&2
  exit 1
}

for crate in slop-graph slop-llm slop-parse slop-resolve; do publish_one "$crate"; done
for crate in slop-graph slop-llm slop-parse slop-resolve; do wait_for_registry "$crate"; done
publish_one slop-analyze
wait_for_registry slop-analyze
for crate in slop-lsp slop-mcp slop-proxy slop-tui; do publish_one "$crate"; done
for crate in slop-lsp slop-mcp slop-proxy slop-tui; do wait_for_registry "$crate"; done
publish_one slop-cli
