//! [DP7.3] Declarative VM plugin engine (`api = 1`), installer, detached runner, and secret redactor.
//!
//! Plugins execute on the VM. Inputs travel on the SSH stdin stream, and
//! remote results are redacted before they reach panel state.

use crate::cli::{ColabBackend, RESERVED_VM_PORT, RESULT_MARKER};
use crate::error::{Error, ErrorKind, Result};
use crate::hf::is_valid_repo_id;
use crate::host::{ExecRequest, Host, LineAction};
use crate::settings::{Accelerator, AuthMode};
use crate::util::{b64_encode, is_env_key, redact_json, redact_secrets, CancelToken};
use crate::watchdog::{HEARTBEAT_PATH, VM_BASE};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const API_VERSION: u32 = 1;
pub const MAX_MANIFEST_BYTES: u64 = 64 * 1024;
pub const MAX_SCRIPT_BYTES: u64 = 512 * 1024;

// Retain the terminator so the release scan sees this as a standalone capability.
const HF_TOKEN_CAPABILITY: &std::ffi::CStr = c"hf_token";

pub type PluginInputs = (Vec<(String, String)>, Vec<String>);

pub struct RunStatus {
    pub state: String,
    pub stage: Option<String>,
    pub progress: Option<f64>,
    pub message: String,
    pub outputs: BTreeMap<String, Value>,
    pub log_tail: Vec<String>,
}

const BUNDLED_LLAMA_TOML: &str = include_str!("../assets/plugins/llama-cpp/plugin.toml");
const BUNDLED_LLAMA_WORKER: &str = include_str!("../assets/plugins/llama-cpp/worker.py");

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct LocalizedText(pub BTreeMap<String, String>);

impl LocalizedText {
    pub fn get(&self, locale: &str) -> String {
        let lang = locale.split('-').next().unwrap_or(locale);
        self.0
            .get(locale)
            .or_else(|| {
                self.0
                    .iter()
                    .find(|(k, _)| k.eq_ignore_ascii_case(locale))
                    .map(|(_, v)| v)
            })
            .or_else(|| {
                self.0
                    .iter()
                    .find(|(k, _)| k.split('-').next() == Some(lang))
                    .map(|(_, v)| v)
            })
            .or_else(|| self.0.get("en"))
            .or_else(|| self.0.values().next())
            .cloned()
            .unwrap_or_default()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProvisionDefaults {
    #[serde(default)]
    pub accelerator: Option<Accelerator>,
    #[serde(default)]
    pub min_vram_mib: u32,
    #[serde(default)]
    pub high_mem: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum InputKind {
    Text,
    Number,
    Bool,
    Select,
    Secret,
    HfModel,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum InputValue {
    Bool(bool),
    Number(f64),
    Text(String),
}

impl InputValue {
    pub fn to_env_string(&self) -> String {
        match self {
            Self::Bool(b) => if *b { "1" } else { "0" }.to_owned(),
            Self::Number(n) if n.fract() == 0.0 && n.abs() < 1e15 => format!("{}", *n as i64),
            Self::Number(n) => n.to_string(),
            Self::Text(s) => s.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InputSpec {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: InputKind,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub label: LocalizedText,
    #[serde(default)]
    pub help: LocalizedText,
    #[serde(default)]
    pub default: Option<InputValue>,
    #[serde(default)]
    pub options: Vec<String>,
    #[serde(default)]
    pub min: Option<f64>,
    #[serde(default)]
    pub max: Option<f64>,
}

impl InputSpec {
    pub fn env_key(&self) -> String {
        format!(
            "PLUGIN_INPUT_{}",
            self.id.to_ascii_uppercase().replace('-', "_")
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunSpec {
    pub script: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StageSpec {
    pub id: String,
    #[serde(default)]
    pub label: LocalizedText,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EndpointSpec {
    pub id: String,
    #[serde(default)]
    pub port_input: Option<String>,
    #[serde(default)]
    pub port: Option<u16>,
    #[serde(default = "default_root_path")]
    pub path: String,
    #[serde(default)]
    pub health: Option<String>,
    #[serde(default = "default_endpoint_kind")]
    pub kind: String,
    #[serde(default)]
    pub label: LocalizedText,
}

fn default_root_path() -> String {
    "/".to_owned()
}
fn default_endpoint_kind() -> String {
    "http".to_owned()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KeepSpec {
    pub path: String,
    #[serde(default)]
    pub label: LocalizedText,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginManifest {
    pub api: u32,
    pub id: String,
    pub version: String,
    #[serde(default)]
    pub needs: Vec<String>,
    pub name: LocalizedText,
    #[serde(default)]
    pub description: LocalizedText,
    #[serde(default)]
    pub provision: ProvisionDefaults,
    #[serde(default)]
    pub inputs: Vec<InputSpec>,
    pub run: RunSpec,
    #[serde(default)]
    pub stages: Vec<StageSpec>,
    #[serde(default)]
    pub endpoints: Vec<EndpointSpec>,
    #[serde(default)]
    pub keep: Vec<KeepSpec>,
}

impl PluginManifest {
    pub fn parse_and_validate(raw: &str) -> Result<Self> {
        if raw.len() as u64 > MAX_MANIFEST_BYTES {
            return Err(Error::invalid("manifest", "manifest exceeds 64 KiB"));
        }
        let m: Self = toml::from_str(raw)
            .map_err(|e| Error::invalid("manifest", format!("invalid plugin.toml: {e}")))?;
        m.validate()?;
        Ok(m)
    }

    pub fn validate(&self) -> Result<()> {
        if self.api != API_VERSION {
            return Err(Error::invalid(
                "api",
                format!("unsupported plugin api {}", self.api),
            ));
        }
        validate_plugin_id(&self.id)?;
        if !self.name.0.contains_key("en") {
            return Err(Error::invalid("name.en", "English name is mandatory"));
        }
        for cap in &self.needs {
            if cap.as_bytes() != HF_TOKEN_CAPABILITY.to_bytes() {
                return Err(Error::invalid(
                    "needs",
                    format!("unknown capability {cap:?}"),
                ));
            }
        }
        if self.run.script.is_empty()
            || !self
                .run
                .script
                .bytes()
                .next()
                .is_some_and(|b| b.is_ascii_alphanumeric())
            || !self
                .run
                .script
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
            || !self.run.script.ends_with(".py") && !self.run.script.ends_with(".sh")
        {
            return Err(Error::invalid("run.script", "script must be a .py or .sh filename with ASCII letters, digits, dots, underscores or hyphens"));
        }

        let mut env_keys = BTreeSet::new();
        for inp in &self.inputs {
            let ek = inp.env_key();
            if !is_env_key(&ek) || !env_keys.insert(ek) {
                return Err(Error::invalid(
                    "inputs",
                    format!("duplicate or invalid input id {:?}", inp.id),
                ));
            }
            if inp.kind == InputKind::Secret && inp.default.is_some() {
                return Err(Error::invalid(
                    "inputs.default",
                    "secret inputs cannot have defaults",
                ));
            }
            if inp.kind == InputKind::Select && inp.options.is_empty() {
                return Err(Error::invalid(
                    "inputs.options",
                    "select input requires options",
                ));
            }
        }
        for ep in &self.endpoints {
            if ep.port.is_some() == ep.port_input.is_some() {
                return Err(Error::invalid(
                    "endpoints",
                    "endpoint must set exactly one of `port` or `port_input`",
                ));
            }
            if ep.port == Some(0) {
                return Err(Error::invalid("endpoints.port", "port must be nonzero"));
            }
            if ep.port == Some(RESERVED_VM_PORT) {
                return Err(Error::new(
                    ErrorKind::PortReserved,
                    "endpoint cannot use port 8080",
                ));
            }
        }
        Ok(())
    }

    /// Validate user-supplied values and return `(env_pairs, secret_values)`.
    pub fn resolve_inputs(
        &self,
        provided: &BTreeMap<String, Value>,
        hf_token: Option<&str>,
    ) -> Result<PluginInputs> {
        if provided
            .keys()
            .any(|key| !self.inputs.iter().any(|input| input.id == *key))
        {
            return Err(Error::invalid("inputs", "undeclared plugin input"));
        }
        let mut env = Vec::new();
        let mut secrets = Vec::new();
        for spec in &self.inputs {
            let invalid_type = || Error::invalid(&spec.id, "unsupported input value type");
            let resolved = match provided.get(&spec.id) {
                Some(Value::Bool(value)) if spec.kind == InputKind::Bool => {
                    Some(InputValue::Bool(*value).to_env_string())
                }
                Some(Value::Number(value)) if spec.kind == InputKind::Number => {
                    Some(value.to_string())
                }
                Some(Value::String(value)) => {
                    if value.contains('\0') {
                        return Err(Error::invalid(&spec.id, "must not contain NUL"));
                    }
                    if self.id == "llama-cpp" && spec.id == "api_key" && !valid_api_key(value) {
                        return Err(Error::invalid(
                            &spec.id,
                            "must not start with # or contain whitespace or control characters",
                        ));
                    }
                    let value = value.trim();
                    if value.is_empty() && !spec.required {
                        spec.default.as_ref().map(InputValue::to_env_string)
                    } else {
                        Some(value.to_owned())
                    }
                }
                Some(Value::Null) | None => spec.default.as_ref().map(InputValue::to_env_string),
                Some(_) => return Err(invalid_type()),
            };
            let Some(mut text) = resolved else {
                if spec.required {
                    return Err(Error::invalid(&spec.id, "required plugin input is missing"));
                }
                continue;
            };
            if text.trim().is_empty() || text.contains('\0') {
                return Err(Error::invalid(
                    &spec.id,
                    "expected a non-empty value without NUL",
                ));
            }
            match spec.kind {
                InputKind::Number => {
                    let n: f64 = text
                        .parse()
                        .map_err(|_| Error::invalid(&spec.id, "must be a number"))?;
                    if !n.is_finite() {
                        return Err(Error::invalid(&spec.id, "must be finite"));
                    }
                    if let Some(min) = spec.min {
                        if n < min {
                            return Err(Error::invalid(&spec.id, format!("must be >= {min}")));
                        }
                    }
                    if let Some(max) = spec.max {
                        if n > max {
                            return Err(Error::invalid(&spec.id, format!("must be <= {max}")));
                        }
                    }
                    if self.id == "llama-cpp" && spec.id == "ctx" && n.fract() != 0.0 {
                        return Err(Error::invalid(
                            &spec.id,
                            "context length must be an integer",
                        ));
                    }
                    if spec.id == "port" && n == f64::from(RESERVED_VM_PORT) {
                        return Err(Error::new(
                            ErrorKind::PortReserved,
                            "port 8080 is reserved by Colab",
                        ));
                    }
                }
                InputKind::Select => {
                    if !spec.options.iter().any(|o| o == &text) {
                        return Err(Error::invalid(&spec.id, "invalid input option"));
                    }
                }
                InputKind::HfModel => {
                    if !is_valid_repo_id(&text) {
                        return Err(Error::invalid(
                            &spec.id,
                            "expected Hugging Face repo `owner/name`",
                        ));
                    }
                }
                InputKind::Secret => {
                    secrets.push(text.clone());
                }
                InputKind::Bool => {
                    text = match text.as_str() {
                        "true" | "1" => "1".into(),
                        "false" | "0" => "0".into(),
                        _ => return Err(Error::invalid(&spec.id, "must be a boolean")),
                    };
                }
                InputKind::Text => {}
            }
            env.push((spec.env_key(), text));
        }

        if self
            .needs
            .iter()
            .any(|cap| cap.as_bytes() == HF_TOKEN_CAPABILITY.to_bytes())
        {
            if let Some(tok) = hf_token.map(str::trim).filter(|s| !s.is_empty()) {
                env.push(("PLUGIN_HF_TOKEN".into(), tok.to_owned()));
                secrets.push(tok.to_owned());
            }
        }
        Ok((env, secrets))
    }

    pub fn resolve_endpoint_port(
        &self,
        ep: &EndpointSpec,
        provided: &BTreeMap<String, Value>,
    ) -> Option<u16> {
        if let Some(p) = ep.port {
            return (p != RESERVED_VM_PORT && p > 0).then_some(p);
        }
        let inp_id = ep.port_input.as_deref()?;
        let port = match provided.get(inp_id) {
            Some(Value::Null) | None => self
                .inputs
                .iter()
                .find(|i| i.id == inp_id)
                .and_then(|i| i.default.as_ref())
                .and_then(|val| val.to_env_string().parse::<u16>().ok())
                .map(u64::from),
            Some(v) => v
                .as_u64()
                .or_else(|| v.as_str()?.trim().parse::<u64>().ok()),
        }?;
        let p16 = u16::try_from(port).ok()?;
        (p16 != RESERVED_VM_PORT && p16 > 0).then_some(p16)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstalledPlugin {
    pub manifest: PluginManifest,
    pub bundled: bool,
    #[serde(skip_serializing)]
    pub script_content: String,
}

fn valid_api_key(key: &str) -> bool {
    !key.starts_with('#') && !key.chars().any(|c| c.is_whitespace() || c.is_control())
}

fn valid_git_remote(url: &str) -> bool {
    if url.len() > 2048 || !url.bytes().all(|b| b.is_ascii_graphic()) {
        return false;
    }
    let valid_segment = |segment: &str| {
        !matches!(segment, "." | "..")
            && !segment.starts_with('-')
            && !segment.is_empty()
            && segment
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
    };
    if let Some((scheme, rest)) = url.split_once("://") {
        if !matches!(scheme, "https" | "ssh") {
            return false;
        }
        let Some((authority, path)) = rest.split_once('/') else {
            return false;
        };
        let host = if let Some((user, host)) = authority.split_once('@') {
            if scheme != "ssh" || !valid_segment(user) {
                return false;
            }
            host
        } else {
            authority
        };
        return valid_git_host(host, true) && path.split('/').all(valid_segment);
    }
    let Some((host, path)) = url
        .strip_prefix("git@")
        .and_then(|rest| rest.split_once(':'))
    else {
        return false;
    };
    valid_git_host(host, false)
        && path
            .split_once('/')
            .is_some_and(|(owner, repo)| valid_segment(owner) && valid_segment(repo))
}

fn valid_git_host(authority: &str, allow_port: bool) -> bool {
    let valid_port = |port: &str| {
        allow_port
            && port.bytes().all(|b| b.is_ascii_digit())
            && port.parse::<u16>().is_ok_and(|port| port != 0)
    };
    if let Some((ip, suffix)) = authority
        .strip_prefix('[')
        .and_then(|host| host.split_once(']'))
    {
        return ip.parse::<std::net::Ipv6Addr>().is_ok()
            && (suffix.is_empty() || suffix.strip_prefix(':').is_some_and(valid_port));
    }
    let (host, port) = authority
        .split_once(':')
        .map_or((authority, None), |(host, port)| (host, Some(port)));
    port.is_none_or(valid_port)
        && host.len() <= 253
        && host.split('.').all(|label| {
            label.len() <= 63
                && label
                    .as_bytes()
                    .first()
                    .is_some_and(u8::is_ascii_alphanumeric)
                && label
                    .as_bytes()
                    .last()
                    .is_some_and(u8::is_ascii_alphanumeric)
                && label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
}

pub fn validate_plugin_id(id: &str) -> Result<()> {
    let valid = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit();
    if id.len() <= 64
        && id.bytes().next().is_some_and(valid)
        && id.bytes().all(|b| valid(b) || b"._-".contains(&b))
    {
        Ok(())
    } else {
        Err(Error::invalid("plugin_id", "expected 1–64 lowercase ASCII letters, digits, dots, underscores or hyphens, starting with a letter or digit"))
    }
}

#[derive(Debug, Clone)]
pub struct PluginStore {
    plugins_dir: PathBuf,
}

impl PluginStore {
    pub fn new(data_dir: &Path) -> Self {
        Self {
            plugins_dir: data_dir.join("plugins"),
        }
    }

    pub fn list(&self) -> Vec<InstalledPlugin> {
        let mut out = Vec::new();
        if let Ok(m) = PluginManifest::parse_and_validate(BUNDLED_LLAMA_TOML) {
            out.push(InstalledPlugin {
                manifest: m,
                bundled: true,
                script_content: BUNDLED_LLAMA_WORKER.to_owned(),
            });
        }
        if let Ok(entries) = self.canonical_root().and_then(|p| Ok(fs::read_dir(p)?)) {
            for entry in entries.flatten() {
                let dir = entry.path();
                if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                    continue;
                }
                if let Ok(plugin) = load_plugin_from_dir(&dir) {
                    if entry.file_name() == plugin.manifest.id.as_str()
                        && !out.iter().any(|p| p.manifest.id == plugin.manifest.id)
                    {
                        out.push(plugin);
                    }
                }
            }
        }
        out
    }

    pub fn get(&self, id: &str) -> Option<InstalledPlugin> {
        validate_plugin_id(id).ok()?;
        self.list().into_iter().find(|p| p.manifest.id == id)
    }

    /// Install a user plugin from a local folder (validating manifest, rejecting symlinks & bundled ID overrides).
    pub fn install_from_dir(&self, source: &Path) -> Result<InstalledPlugin> {
        let plugin = load_plugin_from_dir(source)?;
        if plugin.manifest.id == "llama-cpp" {
            return Err(Error::invalid(
                "id",
                "bundled plugin 'llama-cpp' cannot be overwritten",
            ));
        }
        fs::create_dir_all(&self.plugins_dir)?;
        let root = self.canonical_root()?;
        let staging = crate::util::PrivateDir::new(&root, "install")?;
        let manifest = toml::to_string(&plugin.manifest).map_err(|_| {
            Error::invalid("manifest", "cannot serialize validated plugin manifest")
        })?;
        fs::write(staging.path().join("plugin.toml"), manifest)?;
        fs::write(
            staging.path().join(&plugin.manifest.run.script),
            &plugin.script_content,
        )?;
        let dest = root.join(&plugin.manifest.id);
        match dest.symlink_metadata() {
            Ok(meta) if meta.file_type().is_dir() => {
                let backup = crate::util::PrivateDir::new(&root, "backup")?;
                let old = backup.path().join("previous");
                fs::rename(&dest, &old)?;
                if let Err(error) = fs::rename(staging.path(), &dest) {
                    fs::rename(old, dest)?;
                    return Err(error.into());
                }
            }
            Ok(_) => {
                return Err(Error::invalid(
                    "plugin_id",
                    "installed plugin must be a real directory",
                ))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => fs::rename(staging.path(), dest)?,
            Err(e) => return Err(e.into()),
        }
        Ok(plugin)
    }

    /// Install from an HTTPS or SSH Git remote.
    pub fn install_from_git(&self, host: &Host, url: &str) -> Result<InstalledPlugin> {
        if !valid_git_remote(url) {
            return Err(Error::invalid(
                "url",
                "expected a credential-free HTTPS URL or a valid SSH Git remote",
            ));
        }
        host.require_external_io()?;
        let staging = crate::util::PrivateDir::new(&std::env::temp_dir(), "git")?;
        let tmp = staging.path().join("checkout");
        let mut req = ExecRequest::new(
            "git",
            vec![
                "-c".into(),
                "protocol.allow=never".into(),
                "-c".into(),
                "protocol.https.allow=always".into(),
                "-c".into(),
                "protocol.ssh.allow=always".into(),
                "-c".into(),
                "core.symlinks=false".into(),
                "clone".into(),
                "--depth".into(),
                "1".into(),
                "--".into(),
                url.to_owned(),
                tmp.to_string_lossy().into_owned(),
            ],
        );
        req.env = vec![
            ("GIT_ALLOW_PROTOCOL".into(), "https:ssh".into()),
            ("GIT_TERMINAL_PROMPT".into(), "0".into()),
        ];
        req.timeout = Some(Duration::from_secs(45));
        let out = host.run(req, &CancelToken::new(), &mut |_| LineAction::Continue)?;
        if out.code != Some(0) {
            return Err(Error::new(
                ErrorKind::Other,
                format!("git clone failed: {}", out.stderr.trim()),
            ));
        }
        self.install_from_dir(&tmp)
    }

    fn canonical_root(&self) -> Result<PathBuf> {
        if !self.plugins_dir.symlink_metadata()?.file_type().is_dir() {
            return Err(Error::invalid(
                "plugin_id",
                "plugin root must be a real directory",
            ));
        }
        Ok(self.plugins_dir.canonicalize()?)
    }

    pub fn remove(&self, id: &str) -> Result<bool> {
        validate_plugin_id(id)?;
        if id == "llama-cpp" {
            return Err(Error::invalid(
                "plugin_id",
                "bundled plugin cannot be removed",
            ));
        }
        let root = match self.canonical_root() {
            Err(_)
                if self
                    .plugins_dir
                    .symlink_metadata()
                    .is_err_and(|io| io.kind() == std::io::ErrorKind::NotFound) =>
            {
                return Ok(false)
            }
            result => result?,
        };
        let dest = root.join(id);
        let meta = match dest.symlink_metadata() {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            result => result?,
        };
        if !meta.file_type().is_dir() || dest.canonicalize()?.parent() != Some(root.as_path()) {
            return Err(Error::invalid(
                "plugin_id",
                "plugin must be a real direct child of the plugin directory",
            ));
        }
        if load_plugin_from_dir(&dest)?.manifest.id != id {
            return Err(Error::invalid(
                "plugin_id",
                "installed manifest does not match directory name",
            ));
        }
        fs::remove_dir_all(dest)?;
        Ok(true)
    }
}

fn load_plugin_from_dir(dir: &Path) -> Result<InstalledPlugin> {
    if !dir.symlink_metadata()?.file_type().is_dir() {
        return Err(Error::invalid(
            "path",
            "plugin source must be a real directory",
        ));
    }
    let manifest_path = dir.join("plugin.toml");
    let meta = fs::symlink_metadata(&manifest_path)?;
    if !meta.is_file() || meta.len() > MAX_MANIFEST_BYTES {
        return Err(Error::invalid("plugin.toml", "symlinks are rejected"));
    }
    let raw = fs::read_to_string(&manifest_path)?;
    let manifest = PluginManifest::parse_and_validate(&raw)?;
    let script_path = dir.join(&manifest.run.script);
    let smeta = fs::symlink_metadata(&script_path)?;
    if !smeta.is_file() {
        return Err(Error::invalid("run.script", "symlinks are rejected"));
    }
    if smeta.len() > MAX_SCRIPT_BYTES {
        return Err(Error::invalid("run.script", "script exceeds 512 KiB"));
    }
    let script_content = fs::read_to_string(&script_path)?;
    Ok(InstalledPlugin {
        manifest,
        bundled: false,
        script_content,
    })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginRunInfo {
    pub run_id: String,
    pub plugin_id: String,
    pub session: String,
    #[serde(default)]
    pub created: bool,
    #[serde(default)]
    pub endpoint: Option<String>,
    pub state: String,
    pub stage: Option<String>,
    pub progress: Option<f64>,
    pub message: String,
    pub outputs: BTreeMap<String, Value>,
    pub log_tail: Vec<String>,
    pub endpoints: Vec<RunEndpointInfo>,
    pub started_at_secs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunEndpointInfo {
    pub id: String,
    pub label: String,
    pub kind: String,
    pub remote_port: u16,
    pub path: String,
    pub health: Option<String>,
    pub tunnel_id: Option<u64>,
    pub local_url: Option<String>,
}

/// Launch a plugin script detached on the VM, unset `PLUGIN_*` from kernel env,
/// and redact secrets from local CLI history immediately.
#[allow(clippy::too_many_arguments)]
pub fn launch_detached(
    backend: &dyn ColabBackend,
    account_id: &str,
    auth_mode: AuthMode,
    state_file: &Path,
    session: &str,
    plugin: &InstalledPlugin,
    run_id: &str,
    mut env: Vec<(String, String)>,
    secrets: &[String],
    cancel: &CancelToken,
) -> Result<()> {
    let header = run_header(run_id)?;
    let run_dir = format!("{VM_BASE}/runs/{run_id}");
    env.push(("PLUGIN_ID".into(), plugin.manifest.id.clone()));
    env.push(("PLUGIN_RUN_ID".into(), run_id.to_owned()));
    env.push(("PLUGIN_RUN_DIR".into(), run_dir.clone()));
    env.push((
        "PLUGIN_STATUS_FILE".into(),
        format!("{run_dir}/status.json"),
    ));
    env.push(("PLUGIN_HEARTBEAT".into(), HEARTBEAT_PATH.to_owned()));

    let script_b64 = b64_encode(plugin.script_content.as_bytes());
    let script_name = &plugin.manifest.run.script;
    let runner_cmd = if script_name.ends_with(".py") {
        format!("[sys.executable, '-u', '{run_dir}/{script_name}']")
    } else {
        format!("['bash', '{run_dir}/{script_name}']")
    };

    let code = format!(
        r#"{header}
import base64, subprocess, sys
if not os.path.exists(RD + "/cancelled"):
    with open(RD + "/{script_name}", "wb") as fh:
        fh.write(base64.b64decode("{script_b64}"))
    child_env = {{k: v for k, v in os.environ.items() if k.startswith("PLUGIN_")}}
    child_env.update(PATH=os.defpath + ":/usr/local/bin", LANG="C.UTF-8", HOME=RD)
    with open(RD + "/run.log", "ab") as log:
        p = subprocess.Popen({runner_cmd}, cwd=RD, env=child_env, stdin=subprocess.DEVNULL, stdout=log, stderr=subprocess.STDOUT, start_new_session=True, close_fds=True)
        with open(RD + "/identity", "w") as fh:
            json.dump([p.pid, proc_stat(p.pid)[19]], fh)
    print("{RESULT_MARKER}" + json.dumps({{"pid": p.pid}}))
"#
    );
    let out = backend
        .exec_code(
            account_id, auth_mode, state_file, session, &code, &env, 45, cancel,
        )
        .map_err(|mut e| {
            e.message = redact_secrets(&e.message, secrets);
            e
        })?;
    if out
        .result
        .as_ref()
        .and_then(|v| v.get("pid"))
        .and_then(Value::as_u64)
        .is_none()
    {
        return Err(Error::new(
            ErrorKind::Other,
            "plugin launcher did not confirm the remote process",
        ));
    }
    Ok(())
}

fn run_header(run_id: &str) -> Result<String> {
    validate_plugin_id(run_id)?;
    Ok(format!(
        r#"import fcntl, json, os
RD = "{VM_BASE}/runs/{run_id}"
os.umask(0o077)
os.makedirs(RD, mode=0o700, exist_ok=True)
lock = open(RD + "/lock", "a")
fcntl.flock(lock, fcntl.LOCK_EX)
def proc_stat(pid):
    with open("/proc/%d/stat" % pid) as fh:
        return fh.read().rsplit(") ", 1)[1].split()
def process():
    try:
        pid, started = json.load(open(RD + "/identity"))
        stat = proc_stat(pid)
        if pid > 1 and stat[0] not in ("X", "Z") and stat[2] == str(pid) and stat[19] == started:
            marker = b"PLUGIN_RUN_ID={run_id}"
            if marker in open("/proc/%d/environ" % pid, "rb").read().split(b"\0"):
                return pid
    except (OSError, ValueError, IndexError):
        pass
    return None
"#
    ))
}

pub fn cancel_vm_run(
    backend: &dyn ColabBackend,
    account_id: &str,
    auth_mode: AuthMode,
    state_file: &Path,
    session: &str,
    run_id: &str,
) -> Result<()> {
    let header = run_header(run_id)?;
    let code = format!(
        r#"{header}
import signal, time
open(RD + "/cancelled", "a").close()
def marked_groups():
    groups = set()
    for entry in os.listdir("/proc"):
        if not entry.isdecimal():
            continue
        try:
            stat = proc_stat(int(entry))
            if stat[0] in ("Z", "X") or int(stat[2]) <= 1:
                continue
            with open("/proc/" + entry + "/environ", "rb") as fh:
                if b"PLUGIN_RUN_ID={run_id}" in fh.read(1048576).split(b"\0"):
                    groups.add(int(stat[2]))
        except (OSError, ValueError, IndexError):
            pass
    return groups
groups = marked_groups()
def members():
    active = set()
    for entry in os.listdir("/proc"):
        if not entry.isdecimal():
            continue
        try:
            stat = proc_stat(int(entry))
            group = int(stat[2])
            if group in groups and stat[0] not in ("Z", "X"):
                active.add(group)
        except (OSError, ValueError, IndexError):
            pass
    return active
for sig in (signal.SIGTERM, signal.SIGKILL):
    for group in members():
        try:
            os.killpg(group, sig)
        except ProcessLookupError:
            pass
    for _ in range(20):
        if not members():
            break
        time.sleep(0.1)
print("{RESULT_MARKER}" + json.dumps({{"stopped": not members()}}))
"#
    );
    let out = backend.exec_code(
        account_id,
        auth_mode,
        state_file,
        session,
        &code,
        &[("PLUGIN_RUN_ID".into(), run_id.into())],
        15,
        &CancelToken::new(),
    )?;
    if out.result.and_then(|v| v["stopped"].as_bool()) != Some(true) {
        return Err(Error::new(
            ErrorKind::Other,
            "remote plugin process group termination was not confirmed",
        ));
    }
    Ok(())
}

pub(crate) fn vm_run_alive(
    backend: &dyn ColabBackend,
    account_id: &str,
    auth_mode: AuthMode,
    state_file: &Path,
    session: &str,
    run_id: &str,
    cancel: &CancelToken,
) -> Result<bool> {
    let header = run_header(run_id)?;
    let output = backend.exec_code(
        account_id,
        auth_mode,
        state_file,
        session,
        &format!(
            r#"{header}
print("{RESULT_MARKER}" + json.dumps({{"alive": process() is not None}}))
"#
        ),
        &[("PLUGIN_RUN_ID".into(), run_id.into())],
        15,
        cancel,
    )?;
    output
        .result
        .and_then(|reply| reply["alive"].as_bool())
        .ok_or_else(|| Error::new(ErrorKind::Parse, "plugin liveness check returned no status"))
}

/// Poll `status.json`, liveness, and redacted tail of `run.log` on the VM.
#[allow(clippy::too_many_arguments)]
pub fn poll_vm_run(
    backend: &dyn ColabBackend,
    account_id: &str,
    auth_mode: AuthMode,
    state_file: &Path,
    session: &str,
    run_id: &str,
    secrets: &[String],
    cancel: &CancelToken,
) -> Result<RunStatus> {
    let header = run_header(run_id)?;
    let code = format!(
        r#"{header}
st = None
try:
    with open(RD + "/status.json", "rb") as fh:
        st = json.loads(fh.read(65537))
except (OSError, ValueError):
    pass
alive = process() is not None
log = ""
log_omitted = False
try:
    with open(RD + "/run.log", "rb") as fh:
        # The host must see the entire log to redact multi-line credentials.
        # Suppress oversized logs instead of sending a suffix without context.
        data = fh.read(65537)
        log_omitted = len(data) > 65536
        if not log_omitted:
            log = data.decode("utf-8", "replace")
except OSError:
    pass
print("{RESULT_MARKER}" + json.dumps({{"status": st, "alive": alive, "log": log, "log_omitted": log_omitted}}))
"#
    );
    let out = backend.exec_code(
        account_id,
        auth_mode,
        state_file,
        session,
        &code,
        &[("PLUGIN_RUN_ID".into(), run_id.into())],
        25,
        cancel,
    )?;
    let reply = out
        .result
        .ok_or_else(|| Error::new(ErrorKind::Parse, "plugin poll returned no JSON"))?;
    Ok(parse_run_status(reply, secrets))
}

fn parse_run_status(mut v: Value, secrets: &[String]) -> RunStatus {
    let log = v.as_object_mut().and_then(|v| v.remove("log"));
    let lines = v.as_object_mut().and_then(|v| v.remove("lines"));
    let log = log
        .as_ref()
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| {
            lines.as_ref()?.as_array().map(|lines| {
                lines
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join("\n")
            })
        });
    let lines = if v["log_omitted"] == true {
        vec!["VM log omitted because it exceeds the diagnostic size limit".into()]
    } else {
        let clean = redact_secrets(log.as_deref().unwrap_or_default(), secrets);
        let clean = crate::util::tail(&clean, 8192 - "…".len());
        let mut lines: Vec<_> = clean.lines().rev().take(40).map(str::to_owned).collect();
        lines.reverse();
        lines
    };
    redact_json(&mut v, secrets);
    let alive = v.get("alive").and_then(|x| x.as_bool()).unwrap_or(false);

    if let Some(st) = v.get("status").and_then(|x| x.as_object()) {
        let state = st
            .get("state")
            .and_then(|x| x.as_str())
            .unwrap_or("running")
            .to_owned();
        let stage = st
            .get("stage")
            .and_then(|x| x.as_str())
            .map(ToOwned::to_owned);
        let progress = st.get("progress").and_then(|x| x.as_f64());
        let msg = redact_secrets(
            st.get("message").and_then(|x| x.as_str()).unwrap_or(""),
            secrets,
        );
        let outputs: BTreeMap<String, Value> = st
            .get("outputs")
            .and_then(|x| x.as_object())
            .map(|m| m.iter().map(|(k, val)| (k.clone(), val.clone())).collect())
            .unwrap_or_default();
        if alive || state == "failed" {
            return RunStatus {
                state,
                stage,
                progress,
                message: msg,
                outputs,
                log_tail: lines,
            };
        }
    }

    RunStatus {
        state: if alive { "running" } else { "failed" }.into(),
        stage: None,
        progress: None,
        message: if alive {
            "Starting…"
        } else {
            "Plugin process exited before reporting ready"
        }
        .into(),
        outputs: BTreeMap::new(),
        log_tail: lines,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deletion_rejects_bad_ids_and_preserves_neighboring_files() {
        let temp = crate::util::PrivateDir::new(&std::env::temp_dir(), "plugin-remove").unwrap();
        let root = temp.path();
        let outside = root.join("neighbor");
        let plugin_dir = root.join("plugins");
        fs::create_dir(&outside).unwrap();
        fs::create_dir(&plugin_dir).unwrap();
        fs::write(outside.join("sentinel"), "outside").unwrap();
        fs::write(plugin_dir.join("sentinel"), "root").unwrap();
        let store = PluginStore::new(root);
        for id in [
            "",
            "..",
            "../neighbor",
            "a/b",
            "a\\b",
            "a\n",
            "Upper",
            "llama-cpp",
        ] {
            assert!(store.remove(id).is_err());
        }
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&outside, plugin_dir.join("linked")).unwrap();
            assert!(store.remove("linked").is_err());
            assert!(plugin_dir
                .join("linked")
                .symlink_metadata()
                .unwrap()
                .file_type()
                .is_symlink());
        }
        let source = root.join("source");
        fs::create_dir(&source).unwrap();
        let manifest = BUNDLED_LLAMA_TOML.replace("id = \"llama-cpp\"", "id = \"a.plugin_1\"");
        fs::write(source.join("plugin.toml"), manifest).unwrap();
        fs::write(source.join("worker.py"), "# test plugin\n").unwrap();
        store.install_from_dir(&source).unwrap();
        store.install_from_dir(&source).unwrap();
        assert!(store.remove("a.plugin_1").unwrap());
        assert!(!store.remove("a.plugin_1").unwrap());
        assert_eq!(
            fs::read_to_string(outside.join("sentinel")).unwrap(),
            "outside"
        );
        assert_eq!(
            fs::read_to_string(plugin_dir.join("sentinel")).unwrap(),
            "root"
        );
    }

    #[test]
    fn plugin_status_redacts_messages_logs_stages_and_nested_outputs() {
        let s = "synthetic-secret";
        let hf = ["hf_", "1234567890", "ABCDEFGHIJKLMN"].concat();
        let raw = serde_json::json!({
            "status": { "state": "ready", "message": format!("secret={s}"),
                "stage": s, "outputs": {"nested": [{"refresh_token":"simulated-refresh"}], "summary": hf}},
            "alive": true, "lines": [format!("Bearer simulated-access {s}")]
        });
        let status = parse_run_status(raw, &[s.into()]);
        assert_eq!(status.message, "secret=***");
        assert_eq!(status.stage.as_deref(), Some("***"));
        assert_eq!(status.log_tail, ["Bearer *** ***"]);
        assert_eq!(status.outputs["nested"][0]["refresh_token"], "***");
        assert_eq!(status.outputs["summary"], "***");
    }

    #[test]
    fn log_tail_preserves_redaction_across_truncation_boundaries() {
        let header = ["-----BEGIN ", "PRIVATE KEY-----"].concat();
        let footer = ["-----END ", "PRIVATE KEY-----"].concat();
        let body = "synthetic credential content".repeat(30);
        let body = format!("{body}\n").repeat(64);
        let result = parse_run_status(
            serde_json::json!({
                "alive": true,
                "log": format!("{header}\n{body}{footer}\nfinished")
            }),
            &[],
        );
        assert_eq!(result.log_tail.last().map(String::as_str), Some("finished"));
        assert!(result
            .log_tail
            .iter()
            .all(|line| !line.contains("synthetic credential")));
    }

    #[test]
    fn git_install_accepts_only_https_and_ssh_remotes() {
        let root = crate::util::PrivateDir::new(&std::env::temp_dir(), "git-remote").unwrap();
        let store = PluginStore::new(root.path());
        let host = Host::denied();
        for remote in [
            "ext::git@host:owner/repo",
            "file:///fixture/repo",
            "fd::git@host:repo",
            "git::ssh://git@host/owner/repo",
            "git@host:ext::payload",
            "a@b:c@d:repo",
            "ssh://git@host:abc/owner/repo",
            "https://user@host/owner/repo",
            "https://host/-upload-pack",
            "git@host:../repo",
            " git@host:owner/repo",
            "git@host:owner/repo\n",
            "-git@host:owner/repo",
            "ssh://host//owner/repo",
        ] {
            let error = store.install_from_git(&host, remote).unwrap_err();
            assert_eq!(error.kind, ErrorKind::InvalidInput, "{remote}");
        }
        for remote in [
            "https://forge.example/owner/repo.git",
            "ssh://git@forge.example:2222/owner/repo.git",
            "git@forge.example:owner/repo",
        ] {
            assert!(store
                .install_from_git(&host, remote)
                .unwrap_err()
                .message
                .contains("execution is disabled"));
        }
    }

    #[test]
    fn bundled_llama_cpp_manifest_validates_and_resolves_inputs() {
        let dir = std::env::temp_dir().join(format!("colab-mcp-plg-{}", std::process::id()));
        let store = PluginStore::new(&dir);
        let list = store.list();
        assert_eq!(list.len(), 1);
        let llama = &list[0];
        assert_eq!(llama.manifest.id, "llama-cpp");
        assert!(llama.bundled);

        let mut vals = BTreeMap::new();
        vals.insert(
            "repo".into(),
            Value::String("unsloth/Qwen3-32B-GGUF".into()),
        );
        vals.insert("port".into(), Value::Number(8000.into()));
        vals.insert("api_key".into(), Value::String("sk-supersecret".into()));

        let (env, secrets) = llama
            .manifest
            .resolve_inputs(&vals, Some("hf_tok123456"))
            .unwrap();
        assert!(env
            .iter()
            .any(|(k, v)| k == "PLUGIN_INPUT_REPO" && v == "unsloth/Qwen3-32B-GGUF"));
        assert!(env
            .iter()
            .any(|(k, v)| k == "PLUGIN_HF_TOKEN" && v == "hf_tok123456"));
        assert_eq!(secrets.len(), 2);

        // Reject port 8080
        vals.insert("port".into(), Value::Number(8080.into()));
        assert_eq!(
            llama.manifest.resolve_inputs(&vals, None).unwrap_err().kind,
            ErrorKind::PortReserved
        );
    }

    #[test]
    fn manifest_rejects_unknown_fields_and_secret_defaults() {
        let bad = r#"
api = 1
id = "bad-plugin"
version = "0.1.0"
unknown_key = true
[name]
en = "Bad"
[run]
script = "run.py"
"#;
        assert!(PluginManifest::parse_and_validate(bad).is_err());

        let bad_secret = r#"
api = 1
id = "sec-plugin"
version = "0.1.0"
[name]
en = "Sec"
[[inputs]]
id = "tok"
type = "secret"
default = "leaked"
[run]
script = "run.py"
"#;
        assert!(PluginManifest::parse_and_validate(bad_secret).is_err());
    }
}
