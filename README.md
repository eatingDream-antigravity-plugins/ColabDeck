<div align="center">

<img src="assets/logo.svg" alt="ColabDeck" width="80" height="80">

# ColabDeck (`colab-mcp`)

**Manage Google Colab GPUs from Antigravity with usage monitoring and host and VM cost guards.**

English | [简体中文](README.zh-CN.md) · [Releases](https://github.com/eatingDream-antigravity-plugins/ColabDeck/releases) · [AI Guide](docs/ai-guide.md) · [`llms.txt`](llms.txt)

[![CI](https://github.com/eatingDream-antigravity-plugins/ColabDeck/actions/workflows/ci.yml/badge.svg)](https://github.com/eatingDream-antigravity-plugins/ColabDeck/actions/workflows/ci.yml)
![Built with Rust](https://img.shields.io/badge/built%20with-Rust-1A1916?logo=rust&logoColor=white)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue)](LICENSE)

</div>

## What It Is

A native Rust MCP server and SEP-1865 interactive control panel for the official Google Colab CLI (`google-colab-cli`), designed for Antigravity and MCP-compatible agents.

The local `colab` CLI handles authentication and runtime management. ColabDeck adds cost guards, SSH port forwarding, and declarative VM deployments.

## Features

- **Cached Panel & Non-Blocking Stdio**: Panel tools return cached snapshots while the server refreshes usage and sessions in the background. The panel HTML is a static resource.
- **ADC & Multi-Account Isolation**: Application Default Credentials (ADC) and account-specific OAuth2 user credentials. Runtime actions pin the account and endpoint.
- **Hardware-Targeted GPUs & Orphan Cleanup**: Provision CPU, GPU (`T4`, `L4`, `G4`, `A100`, `H100`), or TPU (`v5e1`, `v6e1`) runtimes with `min_vram_mib` retry loops and one-click `[?]` browser orphan termination.
- **Dual-Layer Cost Guard**: Detached VM-side `watchdog.py` (absolute deadline + idle timeout, survives panel close) paired with cross-account host-side runtime, spend-cap, and balance-floor enforcement with `PendingStop` retry tracking.
- **Exclusive SSH Port Forwarding**: Local `127.0.0.1` tunnels via `colab ssh --proxy-mode` without modifying `~/.ssh/config`, rejecting VM port `8080`, escaping OpenSSH `%` tokens, and refusing blind auto-reconnects that could silently create new runtimes.
- **Declarative VM Plugins & Built-in GGUF Deploy**: Bundled `llama-cpp` plugin with Hugging Face GGUF search, GPU VRAM quantization fitting, integrity-checked archive extraction, and live log streaming.

## Quick Start

Requires Rust 1.89+ (to build from source), Node.js 20+, and the Google Colab CLI (`colab`) on your `PATH` (or via `COLAB_MCP_COLAB_BIN`).

```sh
git clone https://github.com/eatingDream-antigravity-plugins/ColabDeck.git
cd ColabDeck
chmod +x scripts/install.sh scripts/check_sanitized.sh
./scripts/install.sh
```

Try every workflow with **zero billing** using Demo/Mock mode:

```sh
colab-mcp serve --mock
```

## For AI Agents

All CLI flags, environment variables, MCP tool schemas, SEP-1865 negotiation rules, and cost-safety workflows live in the agent docs:

```sh
npx skills add eatingDream-antigravity-plugins/ColabDeck
```

> Tell your agent: *"Read [`llms.txt`](llms.txt) and [`docs/ai-guide.md`](docs/ai-guide.md) to configure `colab-mcp` and manage Colab GPU sessions safely with cost guards."*

## Safety, Privacy & Disclaimer

- **Billing Honesty**: Closing a tunnel or panel never stops VM billing. Only confirmed `stop` or `reap_orphan` ends compute-unit consumption; failed stops remain tracked as `PendingStop`.
- **Least-Privilege Local Boundaries**: MCP tool calls cannot mutate the `colab` binary path (`COLAB_MCP_COLAB_BIN` / trusted PATH only), plugin scripts execute **only** on the remote Colab VM, and secrets never pass through CLI argv or persist to disk.
- **Zero-Billing Tests**: Unit and integration tests enforce compile-time and runtime refusals on real subprocess execution so `cargo test` can never spawn `colab` or bill Compute Units.
- **Google Colab Terms of Service**: `colab-mcp` is an independent open-source project not affiliated with Google. You are solely responsible for complying with Google's Terms of Service and Colab Pro/Pro+ usage policies.

## Documentation

- [AI Agent Guide & Reference (`docs/ai-guide.md`)](docs/ai-guide.md) — MCP tools, CLI flags, env vars, and cost-guard workflows
- [Portable Agent Skill (`skills/colab-panel/SKILL.md`)](skills/colab-panel/SKILL.md) · [LLM Index (`llms.txt`)](llms.txt)
- [Security Policy](SECURITY.md) · [Contributing](CONTRIBUTING.md) · [Third-Party Notices](THIRD_PARTY_NOTICES.md)

## Acknowledgments & License

Inspired by and incorporating MIT-licensed components (VM watchdog and plugin manifest conventions) from [scarletkc/colab-playground](https://github.com/scarletkc/colab-playground) (`Copyright (c) 2026 ScarletKc`). See [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md) for full attribution and license text.

Licensed under the [MIT License](LICENSE).
