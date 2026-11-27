//! MCP Apps static resource loader and model/app tool registry.
//!
//! Model-visible tools stay lean and semantic; high-frequency polling, account, settings,
//! watchdog, and plugin management tools are strictly hidden from the model with
//! `_meta.ui.visibility: ["app"]`.

use crate::accounts::{validate_account_id, AccountProfile};
use crate::cli::RESERVED_VM_PORT;
use crate::error::{Error, Result as ColabResult};
use crate::mcp::coordinator::{AccountContext, ColabCoordinator, ProvisionRequest, StopResult};
use crate::settings::{Accelerator, AuthMode, BackendMode, SettingsPatch};
use crate::util::{display_path, now_secs};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fs::{self, File, Metadata};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Instant, SystemTime};

pub const APP_URI: &str = "ui://colab-mcp/panel";
pub const APP_MIME: &str = "text/html;profile=mcp-app";
pub const SERVER_NAME: &str = "colab-mcp";
pub const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");
const BOOT_PLACEHOLDER: &str = "/*__COLAB_MCP_BOOT__*/";

const EMBEDDED_PANEL_HTML: &str = include_str!("../../panel/dist/panel.html");

pub struct CompiledTemplate {
    prefix: String,
    suffix: String,
    has_placeholder: bool,
}

impl CompiledTemplate {
    pub fn from_html(html: &str) -> Self {
        if let Some((prefix, suffix)) = html.split_once(BOOT_PLACEHOLDER) {
            Self {
                prefix: prefix.to_owned(),
                suffix: suffix.to_owned(),
                has_placeholder: true,
            }
        } else {
            Self {
                prefix: html.to_owned(),
                suffix: String::new(),
                has_placeholder: false,
            }
        }
    }

    pub fn render_with_boot(&self, boot_json: &str) -> String {
        if !self.has_placeholder {
            return self.prefix.clone();
        }
        let mut out =
            String::with_capacity(self.prefix.len() + boot_json.len() + self.suffix.len());
        out.push_str(&self.prefix);
        out.push_str(boot_json);
        out.push_str(&self.suffix);
        out
    }
}

const MAX_HTML_BYTES: u64 = 1024 * 1024;

#[derive(Clone, PartialEq, Eq)]
struct FileStamp {
    modified: SystemTime,
    len: u64,
    #[cfg(unix)]
    identity: (u64, u64, i64, i64),
}

impl FileStamp {
    fn from_meta(meta: Metadata) -> std::io::Result<Self> {
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;
        Ok(Self {
            modified: meta.modified()?,
            len: meta.len(),
            #[cfg(unix)]
            identity: (meta.dev(), meta.ino(), meta.ctime(), meta.ctime_nsec()),
        })
    }
}

pub struct HtmlLoader {
    path: Option<PathBuf>,
    embedded: Arc<CompiledTemplate>,
    cache: Mutex<Option<(FileStamp, Arc<CompiledTemplate>)>>,
}

impl HtmlLoader {
    pub fn new(override_path: Option<PathBuf>) -> Self {
        Self {
            path: override_path.or_else(|| {
                std::env::var_os("COLAB_MCP_PANEL_HTML")
                    .filter(|path| !path.is_empty())
                    .map(PathBuf::from)
            }),
            embedded: Arc::new(CompiledTemplate::from_html(EMBEDDED_PANEL_HTML)),
            cache: Mutex::new(None),
        }
    }

    pub fn load(&self) -> Result<Arc<CompiledTemplate>, String> {
        let Some(path) = &self.path else {
            return Ok(self.embedded.clone());
        };
        let fail = |detail: &str| format!("Panel HTML override {}: {detail}", display_path(path));
        let stamp = |meta| FileStamp::from_meta(meta).map_err(|e| fail(&e.to_string()));
        let meta =
            fs::metadata(path).map_err(|e| fail(&format!("cannot read file metadata: {e}")))?;
        if !meta.is_file() {
            return Err(fail("expected a regular HTML file"));
        }
        let observed = stamp(meta)?;
        let previous = {
            let cache = self.cache.lock().unwrap();
            if let Some((key, template)) = cache.as_ref() {
                if *key == observed {
                    return Ok(template.clone());
                }
            }
            cache.as_ref().map(|(key, _)| key.clone())
        };
        if observed.len > MAX_HTML_BYTES {
            return Err(fail("exceeds the 1 MiB limit"));
        }
        let file = File::open(path).map_err(|e| fail(&format!("cannot open file: {e}")))?;
        let from_open = stamp(file.metadata().map_err(|e| fail(&e.to_string()))?)?;
        if from_open != observed {
            return Err(fail("file changed while opening; read the resource again"));
        }
        let mut html = String::new();
        file.take(MAX_HTML_BYTES + 1)
            .read_to_string(&mut html)
            .map_err(|e| fail(&format!("cannot read UTF-8 HTML: {e}")))?;
        if html.len() as u64 > MAX_HTML_BYTES {
            return Err(fail("exceeds the 1 MiB limit"));
        }
        if html.trim().is_empty() {
            return Err(fail("file is empty"));
        }
        if !html.contains(BOOT_PLACEHOLDER) {
            return Err(fail("add the /*__COLAB_MCP_BOOT__*/ placeholder"));
        }
        if stamp(fs::metadata(path).map_err(|e| fail(&e.to_string()))?)? != observed {
            return Err(fail("file changed while reading; read the resource again"));
        }
        let template = Arc::new(CompiledTemplate::from_html(&html));
        let mut cache = self.cache.lock().unwrap();
        if cache.as_ref().map(|(key, _)| key) == previous.as_ref() {
            *cache = Some((observed, template.clone()));
        }
        Ok(template)
    }
}

pub fn list_resources_json() -> Value {
    json!({
        "resources": [
            {
                "uri": APP_URI,
                "name": "Colab Deck",
                "title": "Colab Deck",
                "description": "Interactive Google Colab control panel: compute units, GPU sessions with VRAM retry, dual-layer cost guards, SSH port forwarding, and declarative VM plugins.",
                "mimeType": APP_MIME,
                "_meta": {
                    "ui": { "prefersBorder": false }
                }
            }
        ]
    })
}

pub fn read_resource_json(uri: &str, html_loader: &HtmlLoader) -> Result<Value, String> {
    if uri != APP_URI {
        return Err(format!("unknown resource URI: {uri}"));
    }
    let html = html_loader.load()?.render_with_boot("null");
    let mut result = json!({
        "contents": [{
            "uri": APP_URI,
            "mimeType": APP_MIME,
            "_meta": { "ui": { "prefersBorder": false } }
        }]
    });
    result["contents"][0]["text"] = Value::String(html);
    Ok(result)
}

fn build_tool_registry() -> Value {
    let panel_meta = json!({
        "ui": { "resourceUri": APP_URI },
        "com.google.antigravity/ui": {
            "entrypoints": [
                { "type": "aux_pane", "title": "Colab Deck" },
                { "type": "global", "title": "Colab Deck" }
            ]
        }
    });
    let app_only = json!({ "ui": { "visibility": ["app"] } });

    json!({
        "tools": [
            // --- 5 Model-visible tools ---
            {
                "name": "colab_panel",
                "title": "Colab Deck",
                "description": "Open or refresh the interactive Google Colab control panel (balance, GPU sessions, cost guards, SSH port forwarding, GGUF deploy).",
                "inputSchema": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "refresh": { "type": "boolean", "description": "Schedule a live CLI refresh in the background." }
                    }
                },
                "_meta": panel_meta
            },
            {
                "name": "colab_status",
                "title": "Colab Status",
                "description": "Return current Colab compute unit balance, burn rate, active runtimes (including [?] browser orphans), tunnels, and deployments.",
                "inputSchema": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "force": { "type": "boolean" }
                    }
                }
            },
            {
                "name": "colab_session_action",
                "title": "Colab Session Action",
                "description": "Provision a GPU/TPU/CPU runtime (default G4 with VM watchdog & optional min_vram_mib retry), stop a session, reap an unmanaged [?] orphan, restart kernel, or get browser URL.",
                "inputSchema": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["action"],
                    "properties": {
                        "expected_backend_mode": { "type": ["string", "null"], "enum": ["real", "mock", null] },
                        "action": { "type": "string", "enum": ["provision", "cancel_provision", "stop", "reap_orphan", "restart_kernel", "url"] },
                        "session": { "type": "string" },
                        "name": { "type": ["string", "null"] },
                        "endpoint": { "type": ["string", "null"] },
                        "account_id": { "type": ["string", "null"] },
                        "job_id": { "type": "string" },
                        "accelerator": { "type": "string", "enum": ["NONE", "T4", "L4", "G4", "A100", "H100", "V5E1", "V6E1"] },
                        "high_mem": { "type": "boolean" },
                        "min_vram_mib": { "type": ["integer", "null"], "maximum": 4294967295_u64 },
                        "max_attempts": { "type": "integer", "minimum": 1, "maximum": 8 },
                        "max_runtime_secs": { "type": ["integer", "null"], "minimum": 0 },
                        "idle_timeout_secs": { "type": ["integer", "null"], "minimum": 0 },
                        "max_spend_cu": { "type": ["number", "null"], "minimum": 0 },
                        "min_balance_cu": { "type": ["number", "null"], "minimum": 0 }
                    }
                }
            },
            {
                "name": "colab_tunnel_action",
                "title": "Colab Port Forward",
                "description": "Manage local 127.0.0.1 SSH port forwards to Colab runtimes (port 8080 forbidden). Dismiss removes stopped or failed entries.",
                "inputSchema": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["action"],
                    "properties": {
                        "action": { "type": "string", "enum": ["open", "close", "dismiss", "list"] },
                        "expected_backend_mode": { "type": ["string", "null"], "enum": ["real", "mock", null] },
                        "account_id": { "type": ["string", "null"] },
                        "endpoint": { "type": ["string", "null"] },
                        "session": { "type": "string" },
                        "local_port": { "type": "integer", "minimum": 1024, "maximum": 65535 },
                        "remote_port": { "type": "integer", "minimum": 1, "maximum": 65535, "not": { "const": RESERVED_VM_PORT } },
                        "health_path": { "type": ["string", "null"] },
                        "tunnel_id": { "type": "integer" }
                    }
                }
            },
            {
                "name": "colab_deploy_action",
                "title": "Colab Model & Plugin Deploy",
                "description": "Search Hugging Face GGUF models, estimate GPU VRAM quantisation fit, or deploy/cancel a declarative VM plugin (default: llama-cpp).",
                "inputSchema": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["action"],
                    "properties": {
                        "expected_backend_mode": { "type": ["string", "null"], "enum": ["real", "mock", null] },
                        "account_id": { "type": ["string", "null"] },
                        "endpoint": { "type": ["string", "null"] },
                        "action": { "type": "string", "enum": ["deploy", "cancel", "search_hf", "inspect_hf"] },
                        "plugin_id": { "type": "string" },
                        "session": { "type": ["string", "null"] },
                        "stop_on_failure": { "type": ["boolean", "null"], "description": "Stop a newly created runtime on deploy failure if no other active deploy uses it. Defaults to false." },
                        "accelerator": { "type": "string" },
                        "inputs": { "type": "object" },
                        "query": { "type": "string" },
                        "repo": { "type": "string" },
                        "run_id": { "type": "string" }
                    }
                }
            },
            // --- 5 App-only tools (hidden from model) ---
            {
                "name": "colab_app_poll",
                "description": "App-only delta poll returning {unchanged:true} when state revision has not changed.",
                "inputSchema": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "since_rev": { "type": ["integer", "null"] },
                        "force": { "type": "boolean" }
                    }
                },
                "_meta": app_only
            },
            {
                "name": "colab_app_accounts",
                "description": "App-only account registry, switch impact preview, and in-memory HF token operations.",
                "inputSchema": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["action"],
                    "properties": {
                        "action": { "type": "string", "enum": ["list", "preview_switch", "switch", "add", "remove", "set_hf_token"] },
                        "account_id": { "type": "string" },
                        "label": { "type": "string" },
                        "email": { "type": ["string", "null"] },
                        "auth_mode": { "type": "string" },
                        "confirm": { "type": "boolean" },
                        "token": { "type": "string" }
                    }
                },
                "_meta": app_only
            },
            {
                "name": "colab_app_settings",
                "description": "App-only settings and environment diagnostics.",
                "inputSchema": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["action"],
                    "properties": {
                        "action": { "type": "string", "enum": ["get", "update", "doctor"] },
                        "patch": { "type": "object" }
                    }
                },
                "_meta": app_only
            },
            {
                "name": "colab_app_plugins",
                "description": "App-only plugin catalog, HF GGUF search/fit, install, deploy, and cancel.",
                "inputSchema": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["action"],
                    "properties": {
                        "expected_backend_mode": { "type": ["string", "null"], "enum": ["real", "mock", null] },
                        "account_id": { "type": ["string", "null"] },
                        "endpoint": { "type": ["string", "null"] },
                        "action": { "type": "string", "enum": ["list", "search_hf", "inspect_hf", "install_dir", "install_git", "remove", "deploy", "cancel_run"] },
                        "plugin_id": { "type": "string" },
                        "session": { "type": ["string", "null"] },
                        "stop_on_failure": { "type": ["boolean", "null"], "description": "Stop a newly created runtime on deploy failure if no other active deploy uses it. Defaults to false." },
                        "accelerator": { "type": "string" },
                        "inputs": { "type": "object" },
                        "query": { "type": "string" },
                        "repo": { "type": "string" },
                        "path": { "type": "string" },
                        "url": { "type": "string" },
                        "run_id": { "type": "string" }
                    }
                },
                "_meta": app_only
            },
            {
                "name": "colab_app_watchdog",
                "description": "App-only VM watchdog control (extend, cancel, arm).",
                "inputSchema": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["session", "action"],
                    "properties": {
                        "expected_backend_mode": { "type": ["string", "null"], "enum": ["real", "mock", null] },
                        "session": { "type": "string" },
                        "action": { "type": "string", "enum": ["extend", "cancel", "arm"] },
                        "endpoint": { "type": ["string", "null"] },
                        "account_id": { "type": ["string", "null"] },
                        "hours": { "type": "number", "minimum": 0.25, "maximum": 24 }
                    }
                },
                "_meta": app_only
            }
        ]
    })
}

static TOOL_REGISTRY: LazyLock<Value> = LazyLock::new(build_tool_registry);

fn registry() -> &'static [Value] {
    TOOL_REGISTRY["tools"].as_array().unwrap()
}

pub fn tool_allowed(name: &str, ui: bool) -> bool {
    registry().iter().any(|tool| {
        tool["name"] == name && (ui || !tool["name"].as_str().unwrap().starts_with("colab_app_"))
    })
}

pub fn list_tools_json(ui: bool) -> Value {
    let tools = registry()
        .iter()
        .filter(|tool| tool_allowed(tool["name"].as_str().unwrap(), ui))
        .cloned()
        .map(|mut tool| {
            if ui {
                if tool["_meta"].is_null() {
                    tool["_meta"] = json!({ "ui": { "visibility": ["model", "app"] } });
                }
            } else {
                tool.as_object_mut().unwrap().remove("_meta");
            }
            tool
        })
        .collect::<Vec<_>>();
    json!({ "tools": tools })
}

pub fn validate_arguments(name: &str, args: &Value) -> Result<(), String> {
    let def = registry()
        .iter()
        .find(|desc| desc["name"] == name)
        .ok_or("unknown tool")?;
    let args = args.as_object().ok_or("tool arguments must be an object")?;
    let schema = &def["inputSchema"];
    if let Some(required) = schema["required"].as_array() {
        for key in required {
            let key = key.as_str().unwrap();
            if !args.contains_key(key) {
                return Err(format!("missing {key}"));
            }
        }
    }
    for (key, value) in args {
        let field = schema["properties"]
            .get(key)
            .ok_or("unsupported tool argument")?;
        let matches_type = |kind: &str| match kind {
            "string" => value.is_string(),
            "boolean" => value.is_boolean(),
            "integer" => value.as_u64().is_some(),
            "number" => value.as_f64().is_some_and(f64::is_finite),
            "null" => value.is_null(),
            "object" => value.is_object(),
            _ => false,
        };
        let kind = &field["type"];
        if !kind.as_str().is_some_and(matches_type)
            && !kind
                .as_array()
                .is_some_and(|kinds| kinds.iter().filter_map(Value::as_str).any(matches_type))
        {
            return Err(format!("invalid type for {key}"));
        }
        if let Some(n) = value.as_f64() {
            if field["minimum"].as_f64().is_some_and(|min| n < min)
                || field["maximum"].as_f64().is_some_and(|max| n > max)
                || field["not"].get("const") == Some(value)
            {
                return Err(match key.as_str() {
                    "local_port" => "local_port must be in 1024..=65535".into(),
                    "remote_port" => "remote_port must be in 1..=65535, excluding 8080".into(),
                    _ => format!("{key} is out of range"),
                });
            }
        }
        if let Some(variants) = field["enum"].as_array() {
            if !variants.contains(value) {
                return Err(format!("unsupported {key}"));
            }
        }
    }
    if schema["properties"].get("expected_backend_mode").is_some() {
        if let Some(id) = args.get("account_id").and_then(Value::as_str) {
            validate_account_id(id).map_err(|_| "invalid account_id")?;
        }
    }
    if let Some(value) = args.get("accelerator") {
        if value.as_str().and_then(Accelerator::from_label).is_none() {
            return Err("unknown accelerator".into());
        }
    }
    Ok(())
}

fn ok_result(text: String, structured: Value) -> Value {
    let mut result = json!({
        "content": [{ "type": "text", "text": text }],
        "isError": false
    });
    result["structuredContent"] = structured;
    result
}

pub(crate) fn err_result(msg: impl Into<String>) -> Value {
    let err = Error::new(crate::error::ErrorKind::Other, msg.into());
    err_from(&err)
}

pub(crate) fn err_from(error: &Error) -> Value {
    let m = error.message.clone();
    json!({
        "content": [{ "type": "text", "text": m }],
        "structuredContent": {
            "error": m,
            "kind": error.ui_kind(),
            "localized": error.localized_json(),
        },
        "isError": true
    })
}

fn parse_acc(v: Option<&Value>) -> Option<Accelerator> {
    v.and_then(|x| x.as_str()).and_then(Accelerator::from_label)
}

fn stop_result(
    result: ColabResult<StopResult>,
    verb: &str,
    target: &str,
    field: &str,
    snapshot: impl FnOnce() -> Value,
) -> Value {
    let pending = || format!("Stop pending confirmation for {target}.");
    let (mut text, confirmed, ledger_warning) = match result {
        Ok(StopResult {
            confirmed: true,
            ledger_warning,
        }) => (format!("{verb} {target}"), true, ledger_warning),
        Ok(StopResult {
            confirmed: false,
            ledger_warning,
        }) => (pending(), false, ledger_warning),
        Err(error) if error.context.get("pending_stop").and_then(Value::as_bool) == Some(true) => {
            (format!("{} {}", pending(), error.message), false, None)
        }
        Err(error) => {
            let mut output = err_from(&error);
            if let Some(warning) = error.context.get("ledger_warning") {
                output["structuredContent"]["ledger_warning"] = warning.clone();
            }
            return output;
        }
    };
    if let Some(warning) = &ledger_warning {
        text.push_str(&format!(" Ledger persistence warning: {warning}"));
    } else if !confirmed {
        text.push_str(" PendingStop retry scheduled.");
    }
    let mut output = json!({ "snapshot": snapshot() });
    output[field] = json!(confirmed);
    if let Some(warning) = ledger_warning {
        output["ledger_warning"] = json!(warning);
    }
    ok_result(text, output)
}

fn expected_mode(args: &Value) -> Option<BackendMode> {
    match args["expected_backend_mode"].as_str() {
        Some("real") => Some(BackendMode::Real),
        Some("mock") => Some(BackendMode::Mock),
        _ => None,
    }
}

pub(crate) fn pin_tool_context(
    name: &str,
    args: &Value,
    coord: &ColabCoordinator,
) -> Option<ColabResult<AccountContext>> {
    let action = args["action"].as_str().unwrap_or_default();
    let needs_account = matches!(
        (name, action),
        ("colab_session_action" | "colab_app_watchdog", _)
            | ("colab_tunnel_action", "open")
            | (
                "colab_deploy_action" | "colab_app_plugins",
                "deploy" | "cancel" | "cancel_run"
            )
    );
    needs_account.then(|| {
        let session = if action == "provision" {
            None
        } else {
            args["session"].as_str().or_else(|| args["name"].as_str())
        };
        coord.pin_action_context(
            session,
            args["endpoint"].as_str(),
            args["account_id"].as_str(),
            expected_mode(args),
            name == "colab_session_action" && matches!(action, "stop" | "reap_orphan"),
        )
    })
}

pub(crate) fn call_tool_json(
    name: &str,
    args: &Value,
    coord: &Arc<ColabCoordinator>,
    entered: Instant,
    context: Option<AccountContext>,
) -> Value {
    if let Err(error) = validate_arguments(name, args) {
        return err_result(error);
    }
    let context = if let Some(context) = context {
        Some(context)
    } else {
        match pin_tool_context(name, args, coord).transpose() {
            Ok(context) => context,
            Err(error) => return err_from(&error),
        }
    };
    if let Some(context) = &context {
        if let Err(error) =
            coord.verify_action_context(context, expected_mode(args), args["account_id"].as_str())
        {
            return err_from(&error);
        }
    }
    let account = || {
        context
            .clone()
            .ok_or_else(|| Error::invalid("account_id", "account context is unavailable"))
    };
    let snapshot_json = |snapshot: &crate::mcp::coordinator::PanelSnapshot| {
        if name.starts_with("colab_app_") {
            snapshot.json(None, entered)
        } else {
            snapshot.public_json(entered)
        }
    };
    let snapshot = || snapshot_json(&coord.cached_snapshot());
    let session = || {
        args["session"]
            .as_str()
            .or_else(|| args["name"].as_str())
            .unwrap_or_default()
    };
    match name {
        "colab_panel" | "colab_status" | "colab_app_poll" => {
            let force = args.get("force").and_then(Value::as_bool).unwrap_or(false);
            if force {
                if let Err(error) = coord.refresh(true) {
                    return err_from(&error);
                }
            }
            let snapshot = if name == "colab_panel" && args["refresh"] == true {
                let cached = coord.cached_snapshot();
                coord.request_refresh(true);
                cached
            } else {
                coord.panel_snapshot()
            };
            let since = if !force && name == "colab_app_poll" {
                args.get("since_rev").and_then(Value::as_u64)
            } else {
                None
            };
            ok_result(
                if name == "colab_app_poll" {
                    "ok".into()
                } else {
                    snapshot.summary.clone()
                },
                if name == "colab_app_poll" {
                    snapshot.json(since, entered)
                } else {
                    snapshot.public_json(entered)
                },
            )
        }
        "colab_session_action" => {
            let action = args.get("action").and_then(|x| x.as_str()).unwrap_or("");
            match action {
                "provision" => {
                    let mut fields = args.clone();
                    let fields_map = fields.as_object_mut().unwrap();
                    fields_map.remove("action");
                    fields_map.remove("account_id");
                    fields_map.remove("expected_backend_mode");
                    fields_map.remove("endpoint");
                    let req: ProvisionRequest = match serde_json::from_value(fields) {
                        Ok(req) => req,
                        Err(_) => return err_result("invalid provision fields or numbers"),
                    };
                    match account().and_then(|ctx| coord.start_provision_in(ctx, req)) {
                        Ok(job) => ok_result(
                            format!("Provisioning {} ({:?})", job.name, job.accelerator),
                            json!({ "job": job, "snapshot": snapshot() }),
                        ),
                        Err(e) => err_from(&e),
                    }
                }
                "cancel_provision" => {
                    let jid = args.get("job_id").and_then(|x| x.as_str()).unwrap_or("");
                    let cancelled = match coord.cancel_provision_checked(
                        jid,
                        expected_mode(args),
                        args["account_id"].as_str(),
                    ) {
                        Ok(cancelled) => cancelled,
                        Err(error) => return err_from(&error),
                    };
                    ok_result(
                        format!("cancel_provision({jid}) = {cancelled}"),
                        json!({ "cancelled": cancelled, "snapshot": snapshot() }),
                    )
                }
                "stop" => stop_result(
                    account().and_then(|ctx| coord.stop_session_in(ctx, session())),
                    "Stopped session",
                    session(),
                    "stopped",
                    snapshot,
                ),
                "reap_orphan" => {
                    let endpoint = args["endpoint"].as_str().unwrap_or_default();
                    let name = Some(session()).filter(|name| !name.is_empty());
                    stop_result(
                        account().and_then(|ctx| coord.reap_orphan_in(ctx, name, endpoint)),
                        "Reaped orphan",
                        endpoint,
                        "released",
                        snapshot,
                    )
                }
                "restart_kernel" => {
                    let sess = session();
                    match account().and_then(|ctx| coord.restart_kernel_in(ctx, sess)) {
                        Ok(()) => ok_result(
                            format!("Restarted kernel on {sess}"),
                            json!({ "ok": true, "snapshot": snapshot() }),
                        ),
                        Err(e) => err_from(&e),
                    }
                }
                "url" => {
                    let sess = session();
                    match account().and_then(|ctx| coord.session_url_in(ctx, sess)) {
                        Ok(url) => ok_result(url.clone(), json!({ "url": url })),
                        Err(e) => err_from(&e),
                    }
                }
                _ => err_result(format!("unknown session action {action:?}")),
            }
        }
        "colab_tunnel_action" => {
            let action = args.get("action").and_then(|x| x.as_str()).unwrap_or("");
            match action {
                "open" => {
                    let sess = args.get("session").and_then(|x| x.as_str()).unwrap_or("");
                    let lp = args
                        .get("local_port")
                        .and_then(Value::as_u64)
                        .and_then(|p| u16::try_from(p).ok())
                        .unwrap_or(8000);
                    let rp = args
                        .get("remote_port")
                        .and_then(Value::as_u64)
                        .and_then(|p| u16::try_from(p).ok())
                        .unwrap_or(lp);
                    let hp = args
                        .get("health_path")
                        .and_then(|x| x.as_str())
                        .filter(|s| !s.is_empty())
                        .map(ToOwned::to_owned);
                    match account().and_then(|ctx| coord.open_tunnel_in(ctx, sess, lp, rp, hp)) {
                        Ok(info) => ok_result(
                            format!(
                                "Tunnel {} -> {}:{} ({:?})",
                                info.local_url, sess, rp, info.state
                            ),
                            json!({ "tunnel": info, "snapshot": snapshot() }),
                        ),
                        Err(e) => err_from(&e),
                    }
                }
                "close" => {
                    let tid = args.get("tunnel_id").and_then(|x| x.as_u64()).unwrap_or(0);
                    let closed = coord.close_tunnel(tid);
                    ok_result(
                        format!("Closed tunnel {tid} (ok={closed}). Note: VM billing continues until stopped."),
                        json!({ "closed": closed, "snapshot": snapshot() }),
                    )
                }
                "dismiss" => {
                    let Some(id) = args["tunnel_id"].as_u64() else {
                        return err_result("missing tunnel_id");
                    };
                    let dismissed = coord.dismiss_tunnel(id);
                    ok_result(
                        format!("Tunnel {id} dismissed={dismissed}"),
                        json!({ "dismissed": dismissed, "snapshot": snapshot() }),
                    )
                }
                "list" => {
                    let list = coord.list_tunnels();
                    ok_result(
                        format!("{} tunnel(s)", list.len()),
                        json!({ "tunnels": list }),
                    )
                }
                _ => err_result(format!("unknown tunnel action {action:?}")),
            }
        }
        "colab_app_watchdog" => {
            let sess = args.get("session").and_then(|x| x.as_str()).unwrap_or("");
            let action = args.get("action").and_then(|x| x.as_str()).unwrap_or("");
            let hours = args.get("hours").and_then(|x| x.as_f64());
            match account().and_then(|ctx| coord.watchdog_action_in(ctx, sess, action, hours)) {
                Ok(wd) => ok_result(
                    format!("Watchdog {action} on {sess}: armed={}", wd.armed),
                    json!({ "watchdog": wd, "snapshot": snapshot() }),
                ),
                Err(e) => err_result(e.message),
            }
        }
        "colab_app_accounts" => {
            let action = args.get("action").and_then(|x| x.as_str()).unwrap_or("");
            match action {
                "list" => ok_result("ok".into(), snapshot()),
                "preview_switch" => ok_result("ok".into(), coord.preview_account_switch()),
                "switch" => {
                    if args["confirm"] != true {
                        return err_result("account switch requires confirm: true");
                    }
                    let id = args
                        .get("account_id")
                        .and_then(|x| x.as_str())
                        .unwrap_or("");
                    match coord.switch_account(id) {
                        Ok(reg) => ok_result(
                            format!("Switched to {}", reg.active_id),
                            json!({ "accounts": reg, "snapshot": snapshot() }),
                        ),
                        Err(e) => err_from(&e),
                    }
                }
                "add" => {
                    let id = args
                        .get("account_id")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_owned();
                    let label = args
                        .get("label")
                        .and_then(|x| x.as_str())
                        .unwrap_or(&id)
                        .to_owned();
                    let email = args
                        .get("email")
                        .and_then(|x| x.as_str())
                        .map(ToOwned::to_owned);
                    let auth_mode = match args.get("auth_mode").and_then(|x| x.as_str()) {
                        Some("adc") => AuthMode::Adc,
                        Some("oauth2") | None => AuthMode::OAuth2,
                        _ => return err_result("invalid authentication mode"),
                    };
                    match coord.add_account(AccountProfile {
                        id,
                        label,
                        email,
                        auth_mode,
                        added_at: now_secs(),
                    }) {
                        Ok(reg) => ok_result(
                            "Account saved".into(),
                            json!({ "accounts": reg, "snapshot": snapshot() }),
                        ),
                        Err(e) => err_from(&e),
                    }
                }
                "remove" => {
                    let id = args
                        .get("account_id")
                        .and_then(|x| x.as_str())
                        .unwrap_or("");
                    match coord.remove_account(id) {
                        Ok(reg) => ok_result(
                            format!("Removed {id}"),
                            json!({ "accounts": reg, "snapshot": snapshot() }),
                        ),
                        Err(e) => err_from(&e),
                    }
                }
                "set_hf_token" => {
                    let tok = args
                        .get("token")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_owned();
                    coord.set_hf_token(tok);
                    ok_result(
                        "HF token updated in memory".into(),
                        json!({ "snapshot": snapshot() }),
                    )
                }
                _ => err_result(format!("unknown account action {action:?}")),
            }
        }
        "colab_app_settings" => {
            let action = args.get("action").and_then(|x| x.as_str()).unwrap_or("get");
            match action {
                "get" => ok_result("ok".into(), snapshot()),
                "doctor" => match coord.refresh_env() {
                    Ok(env) => {
                        ok_result("ok".into(), json!({ "env": env, "snapshot": snapshot() }))
                    }
                    Err(error) => err_from(&error),
                },
                "update" => {
                    let patch_val = args.get("patch").cloned().unwrap_or_else(|| json!({}));
                    match serde_json::from_value::<SettingsPatch>(patch_val) {
                        Ok(patch) => match coord.update_settings(patch) {
                            Ok(s) => ok_result(
                                "Settings saved".into(),
                                json!({ "settings": s, "snapshot": snapshot() }),
                            ),
                            Err(e) => err_from(&e),
                        },
                        Err(e) => err_result(format!("invalid settings patch: {e}")),
                    }
                }
                _ => err_result(format!("unknown settings action {action:?}")),
            }
        }
        "colab_app_plugins" | "colab_deploy_action" => {
            let action = args.get("action").and_then(|x| x.as_str()).unwrap_or("");
            match action {
                "list" => ok_result(
                    "ok".into(),
                    json!({ "plugins": coord.list_plugins().as_ref() }),
                ),
                "search_hf" => {
                    let q = args.get("query").and_then(|x| x.as_str()).unwrap_or("");
                    match coord.search_hf(q) {
                        Ok(models) => ok_result(
                            format!("Found {} GGUF repo(s)", models.len()),
                            json!({ "models": models }),
                        ),
                        Err(e) => err_from(&e),
                    }
                }
                "inspect_hf" => {
                    let repo = args.get("repo").and_then(|x| x.as_str()).unwrap_or("");
                    let acc = parse_acc(args.get("accelerator")).unwrap_or(Accelerator::G4);
                    match coord.inspect_hf(repo, acc) {
                        Ok((fits, listing)) => ok_result(
                            format!("VRAM fit computed for {repo}"),
                            json!({ "repo": repo, "accelerator": acc, "fits": fits, "listing": listing }),
                        ),
                        Err(e) => err_from(&e),
                    }
                }
                "install_dir" => {
                    let p = args.get("path").and_then(|x| x.as_str()).unwrap_or("");
                    match coord.install_plugin_dir(Path::new(p)) {
                        Ok(plg) => ok_result(
                            format!("Installed plugin {}", plg.manifest.id),
                            json!({ "plugin": plg, "snapshot": snapshot() }),
                        ),
                        Err(e) => err_from(&e),
                    }
                }
                "install_git" => {
                    let u = args.get("url").and_then(|x| x.as_str()).unwrap_or("");
                    match coord.install_plugin_git(u) {
                        Ok(plg) => ok_result(
                            format!("Installed plugin {} from git", plg.manifest.id),
                            json!({ "plugin": plg, "snapshot": snapshot() }),
                        ),
                        Err(e) => err_from(&e),
                    }
                }
                "remove" => {
                    let id = args.get("plugin_id").and_then(|x| x.as_str()).unwrap_or("");
                    match coord.remove_plugin(id) {
                        Ok(rm) => ok_result(
                            format!("Removed {id} ({rm})"),
                            json!({ "removed": rm, "snapshot": snapshot() }),
                        ),
                        Err(e) => err_from(&e),
                    }
                }
                "deploy" => {
                    let pid = args
                        .get("plugin_id")
                        .and_then(|x| x.as_str())
                        .unwrap_or("llama-cpp");
                    let sess = args
                        .get("session")
                        .and_then(|x| x.as_str())
                        .map(ToOwned::to_owned);
                    let acc = parse_acc(args.get("accelerator"));
                    let inputs: BTreeMap<String, Value> = args
                        .get("inputs")
                        .and_then(|x| x.as_object())
                        .map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
                        .unwrap_or_default();
                    let stop_on_failure = args["stop_on_failure"].as_bool().unwrap_or(false);
                    match account().and_then(|ctx| {
                        coord.deploy_plugin_in(ctx, pid, sess, acc, inputs, stop_on_failure)
                    }) {
                        Ok(run) => ok_result(
                            format!(
                                "Started plugin {pid} (run {}) on {}",
                                run.run_id, run.session
                            ),
                            json!({ "run": run, "snapshot": snapshot() }),
                        ),
                        Err(e) => err_from(&e),
                    }
                }
                "cancel_run" | "cancel" => {
                    let rid = args.get("run_id").and_then(|x| x.as_str()).unwrap_or("");
                    let (ok, updated) = match coord.cancel_run_snapshot(
                        rid,
                        expected_mode(args),
                        args["account_id"].as_str(),
                    ) {
                        Ok(result) => result,
                        Err(error) => return err_from(&error),
                    };
                    ok_result(
                        if ok {
                            format!("Cancellation requested for run {rid}")
                        } else {
                            format!("Run {rid} cannot be cancelled")
                        },
                        json!({ "cancelled": ok, "snapshot": snapshot_json(&updated) }),
                    )
                }
                _ => err_result(format!("unknown plugin action {action:?}")),
            }
        }
        _ => err_result(format!("unknown tool: {name}")),
    }
}
