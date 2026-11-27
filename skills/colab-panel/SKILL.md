---
name: colab-panel
description: >-
  ONLY use when operating the interactive ColabDeck MCP App UI panel or its
  MCP-managed Google Colab actions (`colab_panel`, `colab_status`,
  `colab_session_action`, `colab_tunnel_action`, `colab_deploy_action`) to
  monitor compute unit balance and burn rate, provision GPU/TPU runtimes with
  VRAM targeting and dual-layer cost watchdogs, forward local ports over SSH,
  or deploy declarative VM plugins such as Hugging Face GGUF models via
  llama.cpp. Do NOT use for standalone headless CLI scripts outside the
  ColabDeck MCP server.
---

# ColabDeck (`colab-mcp`) Workflow

## Start and check state

- Call `colab_panel { refresh?: bool }` to open `ui://colab-mcp/panel` and read cached state. Use `refresh: true` to schedule one background refresh.
- Call `colab_status { force?: bool }` to check the balance, burn rate and runtimes. Use `force: true` to wait for a fresh sample.
- For an exact schema and authentication setup, read the [agent guide](../../docs/ai-guide.md) and [tool index](../../llms.txt).

## Manage runtimes

`colab_session_action` requires `action`. Pass `expected_backend_mode` and `account_id` from the latest snapshot for session, deployment and watchdog actions. For an existing runtime, also pass its `endpoint` when supported.

- `provision` accepts `name?`, `accelerator?`, `high_mem?`, `min_vram_mib?`, `max_attempts?`, `max_runtime_secs?`, `idle_timeout_secs?`, `max_spend_cu?` and `min_balance_cu?`. Omit a guard field to inherit its default. Use `null` or `0` to clear the runtime, idle or spend limit. Use `null` to clear the balance floor.
- `cancel_provision` takes `job_id`.
- `stop`, `restart_kernel` and `url` take `session` or `name`.
- `reap_orphan` takes the `endpoint` of a `[?]` runtime.

Wait for the endpoint to disappear after a stop or reap. If the result reports `PendingStop`, the host guard will retry verification. Closing the panel or an SSH tunnel leaves the runtime active.

## Forward ports and deploy

- `colab_tunnel_action` supports `open`, `close`, `dismiss` and `list`. Use `session`, `local_port?` (1024–65535), `remote_port?` (1–65535 excluding 8080) and `health_path?` (string or null) for `open`. Use `tunnel_id` for `close` and `dismiss`. Use `dismiss` to remove a stopped or failed tunnel entry. Forwards bind to the loopback address.
- `colab_deploy_action` supports `deploy` (`plugin_id?`, `session?` string or null, `accelerator?`, `inputs?`, `stop_on_failure?` boolean or null), `cancel` (`run_id`), `search_hf` (`query`) and `inspect_hf` (`repo`, `accelerator?`). Use inputs declared by the selected manifest. Wait for `ready` before using the service.

For both deploy tools, pass a session name to use an existing runtime. Omit `session` or pass `null` to provision a runtime. `stop_on_failure` defaults to `false` when omitted or `null`. Set it to `true` to stop a runtime created by this deploy if it fails before `ready` and no other active deploy uses the runtime. Check `pending_stops` for unconfirmed release.

## Panel tools

The host must negotiate MCP Apps before these tools are available.

- `colab_app_poll { since_rev?: integer | null, force?: bool }` returns the latest app snapshot or an unchanged revision.
- `colab_app_watchdog { session, action, hours?: number, endpoint?: string | null, account_id?: string | null }` supports `extend`, `cancel` and `arm`. Hours range from 0.25 to 24. Successful cancellation clears the VM and host runtime and idle limits.
- `colab_app_accounts` supports `list`, `preview_switch`, `switch` (`account_id`, `confirm: true`), `add` (`account_id`, `label?`, `email?`, `auth_mode?`), `remove` (`account_id`) and `set_hf_token` (`token`).
- `colab_app_plugins` supports `list`, `search_hf` (`query`), `inspect_hf` (`repo`, `accelerator?`), `install_dir` (`path`), `install_git` (`url`), `remove` (`plugin_id`), `deploy` (`plugin_id?`, `session?` string or null, `accelerator?`, `inputs?`, `stop_on_failure?` boolean or null) and `cancel_run` (`run_id`).
- `colab_app_settings` supports `get`, `doctor` and `update` (`patch` object). The agent guide lists patch fields.

Before switching accounts, call `preview_switch`. It returns `open_tunnels` and `active_sessions` counts. Read `closing_tunnels` for forwards that close. Read `watched_sessions` for managed runtimes with enforced VM auto-stop. `remaining_secs` is null for an idle-only watchdog. Read `unwatched_sessions` for managed runtimes without enforced VM auto-stop and `orphan_sessions` for `[?]` runtimes with any recovered `external_name`.

Use `colab-mcp serve --mock` to test with zero billing.
