# AGENTS.md

Guidance for AI coding agents working in `colab-mcp` (**Colab Deck**). Runtime workflows: [skills/colab-panel/SKILL.md](skills/colab-panel/SKILL.md) and [docs/ai-guide.md](docs/ai-guide.md). Humans: [README.md](README.md).

## Architecture

- `src/` (`main.rs`, `mcp/`, `accounts.rs`, `provision.rs`, `guard.rs`, `watchdog.rs`, `tunnel.rs`, `hf.rs`, `quant.rs`, `mock.rs`, `plugins.rs`) — Rust MCP server, ADC/OAuth2 routing, VRAM provisioning, VM/host cost watchdogs, SSH tunnels, and GGUF deploy.
- `panel/` (`pack.ts`, `app.ts`, `i18n.ts`, `kit/`, `test/panel.test.ts`) — Dark-first SEP-1865 UI bundled to `panel/dist/panel.html`.
- `tests/` (`mcp_integration.rs`, `test_vm_workers.py`) — Zero-billing Rust tests and Python VM worker suite.
- `plugin/` (`plugin.json`, `mcp_config.json`, `rules/AGENTS.md`) — Release plugin bundle; keep `plugin/rules/AGENTS.md` (`<= 400 B`) strictly separate from this root `AGENTS.md`.

## Build, Test & Security Gates (run before every PR)

```sh
(cd panel && npm ci --ignore-scripts --no-audit --no-fund)
npm --prefix panel run typecheck && npm --prefix panel run build && npm --prefix panel test
cargo fmt --check && cargo clippy --locked --all-targets -- -D warnings && cargo test --locked
python3 -m unittest discover -s tests -p 'test_vm_workers.py'
./scripts/check_sanitized.sh
```

CI also checks `git diff --exit-code -- panel/dist/panel.html`, and release runs `./scripts/check_sanitized.sh --stage <dir>`.

## Invariants

- Never invoke real `colab` binaries or spend Compute Units in tests; keep `--mock` / compile-time test guards active.
- Keep `panel/dist/panel.html` `<= 64 KiB` (`65536 B`) with `panel/kit/*` synced; keep `plugin/rules/AGENTS.md` plain markdown `<= 400 B`.
- Bind SSH forwards to `127.0.0.1` without touching `~/.ssh/config`; never treat tunnel/panel close as stopping VM billing.
- Never commit personal paths, tokens, emails, or account/session IDs (`./scripts/check_sanitized.sh` must exit 0).
