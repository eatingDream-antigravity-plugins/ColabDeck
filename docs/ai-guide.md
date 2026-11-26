# ColabDeck (`colab-mcp`) — AI Agent Guide & Technical Reference

This document contains the complete CLI, environment variable, MCP tool, protocol, and cost-safety reference for AI agents and automation.

## 1. CLI Flags & Environment Variables

```sh
colab-mcp serve [--data-dir <DIR>] [--panel-html <PATH>] [--mock|--real]
```

| Flag / Env Var | Default | Description |
| :--- | :--- | :--- |
| `--data-dir <DIR>` | Platform data dir (`colab-mcp`) | Stores settings, real `ledger.json`, per-account CLI session configs, and installed custom plugins (`0700`/`0600`). |
| `--panel-html <PATH>` | Embedded compiled `panel/dist/panel.html` | Optional development override. Fails loudly if missing, unreadable, oversized, or lacking `/*__COLAB_MCP_BOOT__*/`. |
| `--mock` / `--real` | Saved backend mode | Fix the backend for the process. Mock mode uses an in-memory account registry and ledger. It disables real Colab, Git and network operations. |
| `COLAB_MCP_DATA_DIR` | Platform data dir | Sets the data directory when `--data-dir` is absent. |
| `COLAB_MCP_PANEL_HTML` | Embedded bundle | Sets the panel override when `--panel-html` is absent. |
| `COLAB_MCP_BACKEND` | Saved backend mode | Accepts `mock` or `real`. The CLI flag takes precedence. |
| `COLAB_MCP_COLAB_BIN` | Standard PATH discovery | Trusted startup override for the `colab` binary path (basename must be `colab` or `colab.exe`). Never writable via MCP tools. |

The `doctor` subcommand prints an environment check. The `status` subcommand prints a status snapshot. Both accept `--data-dir`, `--mock` and `--real`.

For the default ADC account, set `GOOGLE_APPLICATION_CREDENTIALS`, use the gcloud ADC store, or supply `accounts/<id>/credentials.json` under the data directory. Other accounts use that account's credentials file. Use a self-contained ADC file. Impersonated service accounts can use inline authorized-user or service-account credentials. Profiles marked `oauth2` require an account-specific authorized-user file. Colab CLI's shared OS keyring cannot pin an identity across commands. The server verifies the email with `colab whoami` and checks it against the account profile and saved binding. When leaving an OAuth2 profile, release its active runtimes first.

## 2. Model Context Protocol (MCP) and Apps negotiation

- Supported protocol versions are `2025-11-25`, `2025-06-18`, `2025-03-26` and `2024-11-05`.
- Initialize with `protocolVersion`, `capabilities` and `clientInfo` (string `name` and `version`). Request IDs are strings or integers. Uncorrelated parse and envelope errors use `id: null`.
- MCP Apps tools require `capabilities.extensions["io.modelcontextprotocol/ui"].mimeTypes` to be a string array that contains `text/html;profile=mcp-app`. Other clients receive the five public tools.
- The `2025-03-26` version accepts batches of 1–64 items and up to 4 MiB of serialized replies. Initialize outside a batch. A notification-only batch produces no reply.
- `resources/read` at `ui://colab-mcp/panel` returns static HTML. Load current state from a tool result or `colab_app_poll`.
- Parse errors use `-32700`. Envelope errors use `-32600`. Unknown methods use `-32601`. Invalid tool names or arguments use `-32602`. An unknown resource uses `-32002`. A response over the size limit uses `-32003`. Backend errors use the tool result's `isError` field.

## 3. MCP tools

Tool arguments are JSON objects. The schemas reject undeclared fields. Parameters marked `?` are optional. Accelerator values are `NONE`, `T4`, `L4`, `G4`, `A100`, `H100`, `V5E1` and `V6E1`.

### Public tools

| Tool | Arguments and behavior |
| :--- | :--- |
| `colab_panel` | `{ refresh?: bool }`. Returns the cached snapshot and attaches the panel resource. `refresh: true` schedules one background refresh. |
| `colab_status` | `{ force?: bool }`. Returns balance, burn rate, estimated time to zero, runtimes, tunnels and deployments. `force: true` waits for a fresh sample or reports the refresh error. |
| `colab_session_action` | Requires `action`. See session actions below. |
| `colab_tunnel_action` | Requires `action`. `open` takes `session`, `local_port?` (1024–65535, default 8000), `remote_port?` (1–65535 excluding 8080, default local port), and `health_path?` (string or null). `close` and `dismiss` take integer `tunnel_id`. `dismiss` removes a stopped or failed tunnel entry and returns `dismissed`. `list` takes no other arguments. |
| `colab_deploy_action` | Requires `action`. `deploy` takes `plugin_id?`, `session?` (string or null), `accelerator?`, `inputs?` (object), and `stop_on_failure?` (boolean or null). `cancel` takes `run_id`. `search_hf` takes `query`. `inspect_hf` takes `repo` and `accelerator?`. |

Public snapshots omit account email fields. The panel can read emails through the app-only tools. Paths under the user's home directory display with `~`.

### Session actions

| `action` | Arguments |
| :--- | :--- |
| `provision` | `name?` (string or null), `accelerator?`, `high_mem?` (bool), `min_vram_mib?` (integer or null), `max_attempts?` (integer 1–8), `max_runtime_secs?`, `idle_timeout_secs?`, `max_spend_cu?`, `min_balance_cu?`. |
| `cancel_provision` | `job_id`. |
| `stop`, `restart_kernel`, `url` | `session` or its alias `name`. |
| `reap_orphan` | `endpoint` for a `[?]` runtime. Optionally include `session` or `name` when checking a stale name. |

Session, deploy, app plugin and watchdog tools accept optional `expected_backend_mode` (`real`, `mock` or `null`) and `account_id` (string or `null`). Copy both values from the latest snapshot before issuing a lifecycle action, including cancellation. For an existing runtime, also pass its exact `endpoint` when supported. The server rejects a change in backend mode, a mismatch in the pinned account, or a replaced session name. A pending stop without an original endpoint requires manual verification.

For `min_vram_mib`, omit the field to inherit the default. Pass `min_vram_mib: null` to clear the VRAM minimum for this request. VRAM is video memory, measured in MiB.

A provision request inherits the guard default when it omits a field. For `max_runtime_secs` and `idle_timeout_secs` (integer seconds) and `max_spend_cu` (number), use `null` or `0` to remove the limit. For `min_balance_cu` (number), `null` removes the floor and `0` sets it to zero.

### App-only tools

| Tool | Arguments and behavior |
| :--- | :--- |
| `colab_app_poll` | `{ since_rev?: integer \| null, force?: bool }`. Returns `{ unchanged: true, rev, server_ms }` when the app revision matches. Otherwise returns a full snapshot. `force: true` waits for a fresh sample. |
| `colab_app_watchdog` | `{ session, action, hours?: number, endpoint?: string \| null, account_id?: string \| null }`. Actions are `arm`, `extend` and `cancel`. Hours range from 0.25 to 24. `extend` adds 2 hours by default and re-arms an exited watchdog. `cancel` clears runtime and idle limits. |
| `colab_app_accounts` | Requires `action`. `list` and `preview_switch` take no other arguments. `switch` takes `account_id` and `confirm: true`. `add` takes `account_id`, `label?`, `email?` (string or null) and `auth_mode?` (`adc` or `oauth2`, default `oauth2`). `remove` takes `account_id`. `set_hf_token` takes `token` (string stored in memory). |
| `colab_app_plugins` | Requires `action`. `list` takes no other arguments. `search_hf`, `inspect_hf` and `deploy` take the same fields as their public counterparts. `install_dir` takes `path`. `install_git` takes `url` (HTTPS, SSH or `git@host:owner/repo.git`). `remove` takes `plugin_id`. `cancel_run` takes `run_id`. |
| `colab_app_settings` | Requires `action`. `get` reads settings. `doctor` checks the environment. `update` takes a `patch` object. |

`preview_switch` returns the `open_tunnels` and `active_sessions` counts, plus these arrays.

- `closing_tunnels` lists active forwards with `id`, `local_url`, `session` and `remote_port`.
- `watched_sessions` lists managed runtimes with an enforced deadline or idle timeout. Each row has `name`, `hardware`, `endpoint` and `remaining_secs`. An idle-only watchdog has `remaining_secs: null`.
- `unwatched_sessions` lists managed runtimes without an enforced VM auto-stop. Each row has `name`, `hardware` and `endpoint`.
- `orphan_sessions` lists unmanaged runtimes, including named runtimes outside the ledger, with `endpoint`, `hardware` and `external_name` (string or null).

The settings patch accepts `auth_mode`, `backend_mode` (`real` or `mock`), `locale` (`en` or `zh-CN`), `accelerator`, `high_mem`, `min_vram_mib`, `max_attempts`, `retry_delay_secs`, `max_concurrent_sessions`, `max_runtime_secs`, `idle_timeout_secs`, `max_spend_cu`, `min_balance_cu`, `poll_active_secs` and `poll_idle_secs`. Clear guard defaults using the same values as a provision request. The server chooses its CLI executable at startup. Settings edited in mock mode stay in memory. The server refuses to save mock as the startup mode while real runtimes, reservations, or pending stops remain.

For both deploy tools, `deploy` uses the bundled `llama-cpp` when `plugin_id` is omitted. Pass a session name to use an existing runtime. Omit `session` or pass `null` to provision a runtime. `stop_on_failure` defaults to `false` when omitted or `null`. Set it to `true` to stop a runtime created by this deploy if it fails before `ready` and no other active deploy uses the runtime. The server records a `PendingStop` if it cannot verify release. `cancel` and `cancel_run` can find persisted deployment runs after a restart. Use inputs declared by the plugin manifest. Number inputs accept JSON numbers and numeric strings from forms. Omit an optional `api_key` or pass a string with no whitespace, control characters or leading `#`.

### Snapshot fields

- `sessions` includes `started_at_epoch`, `uptime_secs`, `spend_cu`, `spend_estimated`, `external_name` and `start_source`. The source is `tracked`, `cli_history`, or `first_seen`. For `[?]` runtimes, the server reads local CLI history or uses the first observation. Their spend is an estimate. The observer does not execute code on the runtime.
- `hardware_rates` maps each accelerator to `cu_per_hour` and `measured`. The server uses a settled standard-memory rate if available, then a settled high-memory rate, then the published standard-memory rate. Rates use compute units per hour. The rate for a sole runtime must stay stable for 120 seconds. If the concurrent rate was known, the server first waits for the rate to fall.
- `pending_stops` includes the original `session`, `endpoint`, `is_orphan` flag and retry `status`. `plugin_runs` includes the run's original `endpoint` and a `created` flag that identifies deployments which provisioned a runtime.
- `ended_sessions` lists up to ten ended runtimes for the active account, most recent first. Each row has `name`, `hardware`, `started_at_epoch`, `ended_at_epoch`, `duration_secs`, `spend_cu` (number or null) and `stop_reason` (`user`, `guard`, `provision` or `vanished`).

## 4. Cost-safety workflow

1. Check `colab_status` before allocating a runtime. Verify the current balance and assignments.
2. Pass `min_vram_mib: 70000` for an A100 with 80 GB. The allocation loop verifies release of a smaller assignment before retrying. An ambiguous allocation stays pending so the server does not start a second runtime.
3. Check the deploy state. `ready` means the VM worker passed startup and the required SSH forwards opened. A configured health path must first return a successful HTTP response through its forward.
4. Release a named runtime with `stop`, or a `[?]` runtime with `reap_orphan`. Confirm that its endpoint disappears. A `PendingStop` entry means that release verification is still outstanding. Closing a tunnel or the panel leaves runtime billing active.
5. The host cost guard monitors tracked runtimes across accounts. For a ledger recovery error, keep the existing `ledger.json` and restore a valid copy before provisioning. A confirmed live endpoint can still be stopped. The response reports `ledger_warning` separately. The VM watchdog enforces confirmed runtime and idle limits while the host is offline.
6. Use `colab-mcp serve --mock` to verify workflows without Colab assignments or Compute Unit charges.
