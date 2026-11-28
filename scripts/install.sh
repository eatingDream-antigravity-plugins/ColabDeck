#!/bin/sh
# Install Colab Deck (colab-mcp) plugin from GitHub Releases with SHA-256 verification,
# or build from a local source checkout with --from-source.
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/eatingDream-antigravity-plugins/colab-deck/main/scripts/install.sh | sh
#   curl -fsSL https://raw.githubusercontent.com/eatingDream-antigravity-plugins/colab-deck/main/scripts/install.sh | sh -s -- --uninstall
#   ./scripts/install.sh --from-source [--dest ~/.gemini/config/plugins/colab-mcp]

set -eu

REPO="${COLAB_MCP_REPO:-eatingDream-antigravity-plugins/colab-deck}"
PLUGINS_DIR="${ANTIGRAVITY_PLUGINS_DIR:-${HOME}/.gemini/config/plugins}"
DEST="${COLAB_MCP_PLUGIN_DIR:-${PLUGINS_DIR}/colab-mcp}"
VERSION="${COLAB_MCP_VERSION:-}"
FROM_SOURCE=0
UNINSTALL=0
NO_RELOAD=0
FORCE_RELOAD=0

while [ $# -gt 0 ]; do
  case "$1" in
    --version|-v)
      [ $# -ge 2 ] || { printf 'error: %s requires a value\n' "$1" >&2; exit 2; }
      VERSION="$2"
      shift 2
      ;;
    --dest|-d|--dir)
      [ $# -ge 2 ] || { printf 'error: %s requires a directory path\n' "$1" >&2; exit 2; }
      DEST="$2"
      shift 2
      ;;
    --from-source)
      FROM_SOURCE=1
      shift
      ;;
    --no-reload)
      NO_RELOAD=1
      shift
      ;;
    --reload)
      FORCE_RELOAD=1
      shift
      ;;
    --uninstall)
      UNINSTALL=1
      shift
      ;;
    -h|--help)
      cat <<'EOF'
Colab Deck (colab-mcp) installer

Options:
  -v, --version <tag>   Install a specific release tag (e.g. v0.2.3). Defaults to latest.
  -d, --dest <path>     Target plugin directory (default: ${ANTIGRAVITY_PLUGINS_DIR:-$HOME/.gemini/config/plugins}/colab-mcp).
      --from-source     Build and install from a local repository checkout instead of GitHub Releases.
      --reload          Outside Antigravity, toggle the plugin via `agy plugin disable/enable`.
      --no-reload       Skip live MCP server refresh after install.
      --uninstall       Remove the installed plugin directory.
  -h, --help            Show this help message.
EOF
      exit 0
      ;;
    *)
      printf 'Unknown argument: %s\n' "$1" >&2
      exit 2
      ;;
  esac
done

if [ "$UNINSTALL" -eq 1 ]; then
  case "$DEST" in
    ""|"/"|"$HOME"|"$HOME/"|"$PLUGINS_DIR"|"$PLUGINS_DIR/")
      printf 'error: refusing to remove unsafe directory: %s\n' "$DEST" >&2
      exit 1
      ;;
  esac
  if [ -d "$DEST" ]; then
    rm -rf "$DEST"
    printf 'Removed Colab Deck (colab-mcp) from %s\n' "$DEST"
    printf 'Reload Antigravity to finish uninstalling.\n'
  else
    printf 'Colab Deck (colab-mcp) is not installed at %s\n' "$DEST"
  fi
  exit 0
fi

is_single_line_regex() {
  [ "$(printf '%s' "$1" | LC_ALL=C tr -d '\041-\176' | wc -c)" -eq 0 ] \
    && printf '%s\n' "$1" | grep -Eq "$2"
}

refresh_mcp_host() {
  if [ "$NO_RELOAD" -eq 1 ]; then
    return 0
  fi
  if [ -n "${ANTIGRAVITY_CSRF_TOKEN:-}" ]; then
    LS_ADDR="${ANTIGRAVITY_LS_ADDRESS:-localhost:5387}"
    if ! is_single_line_regex "$LS_ADDR" '^(localhost|127\.0\.0\.1|\[::1\]):[0-9]{1,5}$'; then
      printf 'Could not auto-refresh colab-mcp (non-loopback ANTIGRAVITY_LS_ADDRESS). Restart it in Customizations > Installed > colab-mcp > MCP > Restart.\n'
      return 0
    fi
    _ls_port="${LS_ADDR##*:}"
    if [ "$_ls_port" -lt 1 ] || [ "$_ls_port" -gt 65535 ]; then
      printf 'Could not auto-refresh colab-mcp (invalid ANTIGRAVITY_LS_ADDRESS port). Restart it in Customizations > Installed > colab-mcp > MCP > Restart.\n'
      return 0
    fi
    if [ "$(printf '%s' "$ANTIGRAVITY_CSRF_TOKEN" | LC_ALL=C tr -d '\040-\176' | wc -c)" -ne 0 ] \
      || case "$ANTIGRAVITY_CSRF_TOKEN" in *[\"\\]*) true ;; *) false ;; esac; then
      printf 'Could not auto-refresh colab-mcp (invalid CSRF token format). Restart it in Customizations > Installed > colab-mcp > MCP > Restart.\n'
      return 0
    fi
    code="$(
      printf 'header = "Content-Type: application/json"\nheader = "x-codeium-csrf-token: %s"\n' \
        "$ANTIGRAVITY_CSRF_TOKEN" |
      curl -sS -g --connect-timeout 2 --max-time 5 -o /dev/null -w '%{http_code}' \
        -d '{"serverName": "colab-mcp_colab-mcp"}' -K - \
        "http://${LS_ADDR}/exa.language_server_pb.LanguageServerService/RefreshMcpServers" 2>/dev/null || true
    )"
    if [ "$code" = "200" ]; then
      printf 'Refreshed colab-mcp in Antigravity (HTTP 200). Close and reopen the Colab Deck panel tab to load updated UI.\n'
    else
      printf 'Could not auto-refresh colab-mcp (HTTP %s). Restart it in Customizations > Installed > colab-mcp > MCP > Restart.\n' "${code:-err}"
    fi
    return 0
  fi
  if [ "$FORCE_RELOAD" -eq 1 ] && command -v agy >/dev/null 2>&1; then
    if agy plugin list --json 2>/dev/null | grep -q '"colab-mcp"'; then
      agy plugin disable colab-mcp >/dev/null 2>&1 && agy plugin enable colab-mcp >/dev/null 2>&1 \
        && printf 'Reloaded colab-mcp via agy CLI. Reopen the Colab Deck panel tab to load updated UI.\n' \
        || printf 'Reload failed. Run: agy plugin disable colab-mcp && agy plugin enable colab-mcp\n'
      return 0
    fi
  fi
  printf 'Note: reloading stops active local SSH port forwards (remote Colab sessions keep running).\n'
  printf 'To apply updates: run `agy plugin disable colab-mcp && agy plugin enable colab-mcp` or Customizations > Installed > colab-mcp > MCP > Restart.\n'
}

install_from_source() {
  SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
  REPO_ROOT="$(CDPATH= cd -- "${SCRIPT_DIR}/.." && pwd)"
  if [ ! -f "${REPO_ROOT}/Cargo.toml" ] || [ ! -d "${REPO_ROOT}/panel" ]; then
    printf 'error: --from-source must be run from a local colab-deck checkout\n' >&2
    exit 1
  fi

  (cd "${REPO_ROOT}/panel" && npm ci --ignore-scripts --no-audit --no-fund && node pack.ts)
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

  printf 'Installed Colab Deck (colab-mcp) from source into %s\n' "$DEST"
  refresh_mcp_host
}

if [ "$FROM_SOURCE" -eq 1 ]; then
  install_from_source
  exit 0
fi

need_cmd() {
  if ! command -v "$1" >/dev/null 2>&1; then
    printf "error: required command '%s' not found in PATH\n" "$1" >&2
    exit 1
  fi
}

need_cmd curl
need_cmd tar
need_cmd mktemp

OS="$(uname -s)"
ARCH="$(uname -m)"

case "${OS}:${ARCH}" in
  Darwin:arm64|Darwin:aarch64)
    TARGET="aarch64-apple-darwin"
    ;;
  Darwin:x86_64|Darwin:amd64)
    TARGET="x86_64-apple-darwin"
    ;;
  Linux:x86_64|Linux:amd64)
    TARGET="x86_64-unknown-linux-gnu"
    ;;
  *)
    printf 'error: unsupported prebuilt target %s/%s.\n' "$OS" "$ARCH" >&2
    printf 'Build from source instead: https://github.com/%s\n' "$REPO" >&2
    exit 1
    ;;
esac

if [ -z "$VERSION" ]; then
  EFFECTIVE_URL="$(curl -fsSLI -o /dev/null -w '%{url_effective}' "https://github.com/${REPO}/releases/latest")"
  VERSION="${EFFECTIVE_URL##*/}"
  if [ -z "$VERSION" ] || [ "$VERSION" = "latest" ] || [ "$VERSION" = "releases" ]; then
    printf 'error: could not determine latest release tag for %s.\n' "$REPO" >&2
    printf 'Pass COLAB_MCP_VERSION=vX.Y.Z or run ./scripts/install.sh --from-source.\n' >&2
    exit 1
  fi
else
  case "$VERSION" in
    v*) ;;
    *) VERSION="v${VERSION}" ;;
  esac
fi

if ! is_single_line_regex "$VERSION" '^v[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$'; then
  printf 'error: invalid release version tag: %s\n' "$VERSION" >&2
  exit 1
fi

ARCHIVE="colab-mcp-${VERSION}-${TARGET}.tar.gz"
BASE_URL="https://github.com/${REPO}/releases/download/${VERSION}"

TMP_DIR="$(mktemp -d)"
trap 'rm -rf "$TMP_DIR"' EXIT HUP INT TERM

printf 'Downloading %s (%s)...\n' "$ARCHIVE" "$VERSION"
curl -fsSL -o "${TMP_DIR}/${ARCHIVE}" "${BASE_URL}/${ARCHIVE}"
curl -fsSL -o "${TMP_DIR}/${ARCHIVE}.sha256" "${BASE_URL}/${ARCHIVE}.sha256"

EXPECTED_SHA="$(awk '{print $1; exit}' "${TMP_DIR}/${ARCHIVE}.sha256" | tr -d '\r\n')"
if [ -z "$EXPECTED_SHA" ]; then
  printf 'error: empty SHA-256 checksum file\n' >&2
  exit 1
fi
if ! is_single_line_regex "$EXPECTED_SHA" '^[0-9a-fA-F]{64}$'; then
  printf 'error: invalid SHA-256 checksum format\n' >&2
  exit 1
fi

if command -v sha256sum >/dev/null 2>&1; then
  ACTUAL_SHA="$(sha256sum "${TMP_DIR}/${ARCHIVE}" | awk '{print $1}')"
elif command -v shasum >/dev/null 2>&1; then
  ACTUAL_SHA="$(shasum -a 256 "${TMP_DIR}/${ARCHIVE}" | awk '{print $1}')"
elif command -v openssl >/dev/null 2>&1; then
  ACTUAL_SHA="$(openssl dgst -sha256 "${TMP_DIR}/${ARCHIVE}" | awk '{print $NF}')"
else
  printf 'error: no SHA-256 tool found (need sha256sum, shasum, or openssl)\n' >&2
  exit 1
fi

if [ "$ACTUAL_SHA" != "$EXPECTED_SHA" ]; then
  printf 'error: SHA-256 mismatch for %s\n  expected: %s\n  actual:   %s\n' "$ARCHIVE" "$EXPECTED_SHA" "$ACTUAL_SHA" >&2
  exit 1
fi

printf 'SHA-256 verified: %s\n' "$ACTUAL_SHA"

STAGE_DIR="${TMP_DIR}/stage"
mkdir -p "$STAGE_DIR"
tar -xzf "${TMP_DIR}/${ARCHIVE}" -C "$STAGE_DIR"

[ -f "${STAGE_DIR}/bin/colab-mcp" ] || { printf 'error: archive missing bin/colab-mcp\n' >&2; exit 1; }
[ -f "${STAGE_DIR}/plugin.json" ] || { printf 'error: archive missing plugin.json\n' >&2; exit 1; }

mkdir -p "${DEST}/bin" "${DEST}/assets" "${DEST}/rules" "${DEST}/skills/colab-panel" "${DEST}/panel/dist" "${DEST}/docs"

# Atomic binary swap so a running MCP server never sees a truncated executable.
cp "${STAGE_DIR}/bin/colab-mcp" "${DEST}/bin/colab-mcp.new"
chmod 0755 "${DEST}/bin/colab-mcp.new"
mv -f "${DEST}/bin/colab-mcp.new" "${DEST}/bin/colab-mcp"

cp "${STAGE_DIR}/panel/dist/panel.html" "${DEST}/panel/dist/panel.html"
cp "${STAGE_DIR}/plugin.json" "${DEST}/plugin.json"
cp "${STAGE_DIR}/mcp_config.json" "${DEST}/mcp_config.json"
cp "${STAGE_DIR}/rules/AGENTS.md" "${DEST}/rules/AGENTS.md"
cp "${STAGE_DIR}/skills/colab-panel/SKILL.md" "${DEST}/skills/colab-panel/SKILL.md"
cp "${STAGE_DIR}/assets/logo.svg" "${DEST}/assets/logo.svg"
for f in LICENSE THIRD_PARTY_NOTICES.md README.md README.zh-CN.md llms.txt; do
  if [ -f "${STAGE_DIR}/${f}" ]; then
    cp "${STAGE_DIR}/${f}" "${DEST}/${f}"
  fi
done
if [ -f "${STAGE_DIR}/docs/ai-guide.md" ]; then
  cp "${STAGE_DIR}/docs/ai-guide.md" "${DEST}/docs/ai-guide.md"
fi

printf '\nInstalled Colab Deck (colab-mcp) %s (%s) to:\n  %s\n\n' "$VERSION" "$TARGET" "$DEST"
printf 'Next steps:\n'
printf '  1. Make sure the Google Colab CLI (`colab`) is on your PATH\n'
printf '  2. Test with zero billing: %s/bin/colab-mcp serve --mock\n' "$DEST"
printf '  3. Reload Antigravity to open the Colab Deck side panel\n'
refresh_mcp_host
