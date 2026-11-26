# Security Policy

## Local Execution Boundaries
- **No MCP-controlled binary execution**: The `colab` CLI path is resolved strictly from trusted startup configuration (`COLAB_MCP_COLAB_BIN` or standard system `PATH` / `~/.local/bin` / Homebrew directories) and validated by executable basename. MCP tool calls cannot overwrite `colab_path`.
- **Strict plugin directory boundary**: Custom plugin deletion validates `plugin_id` (`^[a-z0-9][a-z0-9._-]{0,63}$`), rejects symlinks and built-in IDs, and requires the canonical target to be a direct child of `plugins_dir`.
- **Remote-only VM plugins**: Plugin worker scripts execute exclusively on the remote Colab VM, never on the local host.
- **Per-connection SEP-1865 gating**: App-only tools (`colab_app_*`) are hidden from `tools/list` and rejected (`-32602`) in `tools/call` unless the client negotiates `"text/html;profile=mcp-app"`.

## Secrets & Network Hardening
- Secrets (`hf_token`, plugin `secret` inputs) are kept in memory only, transmitted via private `0600`/stdin channels (never CLI argv), and recursively redacted across logs, errors, plugin outputs, and MCP snapshots.
- `~/.ssh/config` is never read or modified; VM port `8080` is blocked; dynamic `ProxyCommand` paths escape OpenSSH `%` tokens.
- External Hugging Face requests enforce an HTTPS domain allowlist with hop-by-hop redirect validation rejecting private/loopback/reserved SSRF targets.
- VM worker downloads verify archive bounds and integrity before execution.

## Zero-Billing Test Guarantee
- `Host::run` and real subprocess/network spawners enforce compile-time `#[cfg(test)]` refusals alongside integration-test executor guards so `cargo test` can never spawn real `colab`/`ssh`/`git`/`curl` or consume Compute Units.
