#!/bin/sh
# Build and install colab-mcp binary, single-file panel, notices, and Antigravity plugin bundle.
# Usage: ./scripts/install.sh [--dest ~/.gemini/config/plugins/colab-mcp]

set -eu

SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
REPO_ROOT="$(CDPATH= cd -- "${SCRIPT_DIR}/.." && pwd)"
DEST="${COLAB_MCP_PLUGIN_DIR:-${ANTIGRAVITY_PLUGINS_DIR:-${HOME}/.gemini/config/plugins}/colab-mcp}"

while [ $# -gt 0 ]; do
  case "$1" in
    --dest)
      DEST="$2"
      shift 2
      ;;
    -h|--help)
      printf 'Usage: %s [--dest DIR]\n' "$0"
      exit 0
      ;;
    *)
      printf 'Unknown argument: %s\n' "$1" >&2
      exit 2
      ;;
  esac
done

(cd "${REPO_ROOT}/panel" && npm ci --no-audit --no-fund && node pack.mjs)
(
  cd "${REPO_ROOT}"
  CARGO_HOME_DIR="${CARGO_HOME:-${HOME}/.cargo}"
  set -- "--remap-path-prefix=${HOME}=/build" \
    "--remap-path-prefix=${CARGO_HOME_DIR}=/cargo" \
    "--remap-path-prefix=${CARGO_HOME_DIR}/registry/src=/cargo-src" \
    "--remap-path-prefix=${PWD}=/src" \
    "--remap-path-prefix=$(pwd -P)=/src"
  if [ -d "$CARGO_HOME_DIR" ]; then
    CARGO_HOME_PHYSICAL="$(CDPATH= cd -- "$CARGO_HOME_DIR" && pwd -P)"
    set -- "$@" "--remap-path-prefix=${CARGO_HOME_PHYSICAL}=/cargo" \
      "--remap-path-prefix=${CARGO_HOME_PHYSICAL}/registry/src=/cargo-src"
  fi
  for registry_src in "${CARGO_HOME_DIR}"/registry/src/* "${CARGO_HOME_PHYSICAL:-$CARGO_HOME_DIR}"/registry/src/*; do
    [ -d "$registry_src" ] || continue
    set -- "$@" "--remap-path-prefix=${registry_src}=/cargo-src" \
      "--remap-path-prefix=$(CDPATH= cd -- "$registry_src" && pwd -P)=/cargo-src"
  done
  RUSTFLAGS="$*"
  # Preserve each remap as one compiler argument when a path contains spaces.
  CARGO_ENCODED_RUSTFLAGS="$(printf '%s\037' "$@")"
  CARGO_ENCODED_RUSTFLAGS="${CARGO_ENCODED_RUSTFLAGS%?}"
  export RUSTFLAGS CARGO_ENCODED_RUSTFLAGS
  cargo build --release --locked
)

BIN_SRC="${REPO_ROOT}/target/release/colab-mcp"
PANEL_SRC="${REPO_ROOT}/panel/dist/panel.html"

mkdir -p "${DEST}/bin" "${DEST}/assets" "${DEST}/rules" "${DEST}/skills/colab-panel" "${DEST}/panel/dist" "${DEST}/docs"

cp "$BIN_SRC" "${DEST}/bin/colab-mcp.new"
chmod 0755 "${DEST}/bin/colab-mcp.new"
mv -f "${DEST}/bin/colab-mcp.new" "${DEST}/bin/colab-mcp"

cp "$PANEL_SRC" "${DEST}/panel/dist/panel.html"
cp "${REPO_ROOT}/plugin/plugin.json" "${DEST}/plugin.json"
cp "${REPO_ROOT}/plugin/mcp_config.json" "${DEST}/mcp_config.json"
cp "${REPO_ROOT}/plugin/rules/AGENTS.md" "${DEST}/rules/AGENTS.md"
cp "${REPO_ROOT}/skills/colab-panel/SKILL.md" "${DEST}/skills/colab-panel/SKILL.md"
cp "${REPO_ROOT}/assets/logo.svg" "${DEST}/assets/logo.svg"
cp "${REPO_ROOT}/LICENSE" "${DEST}/LICENSE"
cp "${REPO_ROOT}/THIRD_PARTY_NOTICES.md" "${DEST}/THIRD_PARTY_NOTICES.md"
cp "${REPO_ROOT}/README.md" "${DEST}/README.md"
cp "${REPO_ROOT}/README.zh-CN.md" "${DEST}/README.zh-CN.md"
cp "${REPO_ROOT}/docs/ai-guide.md" "${DEST}/docs/ai-guide.md"
cp "${REPO_ROOT}/llms.txt" "${DEST}/llms.txt"

printf 'Installed colab-mcp plugin into %s\n' "$DEST"

if [ -n "${ANTIGRAVITY_CSRF_TOKEN:-}" ]; then
  curl -sf -H "Content-Type: application/json" \
    -H "x-codeium-csrf-token: ${ANTIGRAVITY_CSRF_TOKEN}" \
    -d '{"serverName": "colab-mcp_colab-mcp"}' \
    "http://${ANTIGRAVITY_LS_ADDRESS:-localhost:5387}/exa.language_server_pb.LanguageServerService/RefreshMcpServers" >/dev/null 2>&1 || true
fi
