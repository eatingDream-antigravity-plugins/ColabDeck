//! [DP2.3] Typed `colab` CLI wrapper, output parsers, failure classifier, and Backend trait.
//!
//! Supports both `--auth=adc` and `--auth=oauth2` with isolated `--config` files,
//! intercepts unexpected interactive OAuth prompts on stderr, and classifies errors
//! reliably even when `colab stop` or `colab exec` exits 0 on errors.

use crate::accounts::{validate_account_id, AccountStore, ADC_ACCOUNT_ID};
use crate::error::{Error, ErrorKind, Result};
use crate::host::{ExecRequest, ExecResult, Host, LineAction, Stream};
use crate::settings::{Accelerator, AuthMode};
use crate::util::{
    b64_encode, display_path, is_env_key, lock_file, redact_json, redact_secrets, shell_quote,
    tail, write_atomic_private, CancelToken, PrivateDir,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

pub const RESULT_MARKER: &str = "__CMCP_RESULT__ ";
pub const EXEC_PREAMBLE: &str = "# colab-mcp\n";
pub const AUTH_PROMPT_MARKER: &str = "To authorize colab-cli";
pub const RESERVED_VM_PORT: u16 = 8080;

/// One parsed line of `colab sessions`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerSession {
    /// Local name; `None` means `[?]` unmanaged/browser orphan.
    pub name: Option<String>,
    pub endpoint: String,
    pub hardware: String,
    pub accelerator: Option<Accelerator>,
    pub shape: String,
    pub variant: String,
    pub status: Option<String>,
}

impl ServerSession {
    pub fn high_mem(&self) -> bool {
        self.shape.eq_ignore_ascii_case("High-RAM") || self.shape.eq_ignore_ascii_case("HIGH_RAM")
    }
}

/// Raw usage reading parsed from `colab usage`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct UsageReading {
    pub balance_cu: f64,
    pub rate_cu_per_hour: f64,
    pub assignments: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopOutcome {
    Terminated,
    NotFound,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ExecOutput {
    pub stdout: String,
    pub stderr: String,
    pub result: Option<Value>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HardwareProbe {
    pub gpu: Option<String>,
    pub vram_mib: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnvironmentCheck {
    pub colab_found: bool,
    pub colab_path: Option<String>,
    pub colab_version: Option<String>,
    pub ssh_key_ready: bool,
    pub adc_file_present: bool,
    pub auth_mode: AuthMode,
    pub backend: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CliCommand {
    Sessions,
    Usage,
    Version,
    Whoami,
    Url {
        name: String,
    },
    RestartKernel {
        name: String,
    },
    New {
        name: String,
        accelerator: Accelerator,
        high_mem: bool,
    },
    Exec {
        name: String,
        timeout_secs: u32,
    },
    Stop {
        name: String,
    },
}

pub fn validate_session_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 63
        && name
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._-".contains(c));
    if ok {
        Ok(())
    } else {
        Err(Error::invalid(
            "session",
            format!("invalid session name {name:?}"),
        ))
    }
}

pub(crate) fn valid_endpoint(endpoint: &str) -> bool {
    !endpoint.is_empty()
        && endpoint.len() <= 255
        && !endpoint
            .chars()
            .any(|c| c.is_whitespace() || c.is_control())
}

/// Build `colab` argv with `--auth=<mode>` and optional `--config <state>` BEFORE the subcommand.
pub fn build_argv(
    auth_mode: AuthMode,
    state_file: Option<&Path>,
    cmd: &CliCommand,
) -> Result<Vec<String>> {
    let mut argv = Vec::new();
    if !matches!(cmd, CliCommand::Version) {
        argv.push(format!("--auth={}", auth_mode.cli_flag()));
    }
    if let Some(state) = state_file {
        if !matches!(cmd, CliCommand::Version) {
            argv.push("--config".into());
            argv.push(state.to_string_lossy().into_owned());
        }
    }
    match cmd {
        CliCommand::Sessions => argv.push("sessions".into()),
        CliCommand::Usage => argv.push("usage".into()),
        CliCommand::Version => argv.push("version".into()),
        CliCommand::Whoami => argv.push("whoami".into()),
        CliCommand::Url { name } => {
            validate_session_name(name)?;
            argv.extend(["url".into(), "-s".into(), name.clone()]);
        }
        CliCommand::RestartKernel { name } => {
            validate_session_name(name)?;
            argv.extend(["restart-kernel".into(), "-s".into(), name.clone()]);
        }
        CliCommand::New {
            name,
            accelerator,
            high_mem,
        } => {
            validate_session_name(name)?;
            argv.extend(["new".into(), "-s".into(), name.clone()]);
            argv.extend(accelerator.new_flags());
            if *high_mem && accelerator.supports_high_mem() {
                argv.push("--high-mem".into());
            }
        }
        CliCommand::Exec { name, timeout_secs } => {
            validate_session_name(name)?;
            argv.extend([
                "exec".into(),
                "-s".into(),
                name.clone(),
                "--timeout".into(),
                timeout_secs.to_string(),
            ]);
        }
        CliCommand::Stop { name } => {
            validate_session_name(name)?;
            argv.extend(["stop".into(), "-s".into(), name.clone()]);
        }
    }
    Ok(argv)
}

/// Parse `colab sessions` output lines.
pub fn parse_sessions(stdout: &str) -> Result<Vec<ServerSession>> {
    let mut list = Vec::new();
    let mut endpoints = BTreeSet::new();
    let mut names = BTreeSet::new();
    let mut empty = false;
    let unexpected = || {
        Error::new(
            ErrorKind::Parse,
            "unrecognized or ambiguous colab sessions output",
        )
    };
    for line in stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        let diagnostic = line.strip_prefix("[colab] ").unwrap_or(line);
        if matches!(
            diagnostic,
            "You can run 'colab update --install' to upgrade in place."
                | "Run 'uv tool install -U google-colab-cli' to update."
                | "Run 'pip install --upgrade google-colab-cli' to update."
                | "To silence this check, set \"enable_update_check\": false in ~/.config/colab-cli/settings.json"
        ) || diagnostic.strip_prefix("A new version of Colab CLI is available: ")
            .and_then(|text| text.strip_suffix(')'))
            .and_then(|text| text.split_once(" (current: "))
            .is_some_and(|(latest, current)| [latest, current].into_iter().all(|v|
                !v.is_empty() && v.bytes().all(|b| b.is_ascii_alphanumeric() || b".+-".contains(&b))))
        {
            continue;
        }
        if diagnostic
            .strip_prefix("Pruned ")
            .and_then(|text| text.strip_suffix(" stale local session(s)."))
            .is_some_and(|count| count.parse::<usize>().is_ok())
        {
            continue;
        }
        if diagnostic == "No active sessions found on server." && !empty && list.is_empty() {
            empty = true;
            continue;
        }
        let entry = parse_session_line(line).ok_or_else(unexpected)?;
        if empty
            || !endpoints.insert(entry.endpoint.clone())
            || entry
                .name
                .as_ref()
                .is_some_and(|name| !names.insert(name.clone()))
        {
            return Err(unexpected());
        }
        list.push(entry);
    }
    if !empty && list.is_empty() {
        return Err(unexpected());
    }
    Ok(list)
}

fn parse_session_line(line: &str) -> Option<ServerSession> {
    let line = line.trim();
    let rest = line.strip_prefix('[')?;
    let (name, rest) = rest.split_once("] ")?;
    let mut parts = rest.split(" | ");
    let endpoint = parts.next()?.trim();
    if !valid_endpoint(endpoint) || (name != "?" && validate_session_name(name).is_err()) {
        return None;
    }
    let mut s = ServerSession {
        name: if name == "?" {
            None
        } else {
            Some(name.to_owned())
        },
        endpoint: endpoint.to_owned(),
        hardware: String::new(),
        accelerator: None,
        shape: String::new(),
        variant: String::new(),
        status: None,
    };
    let mut saw_hw = false;
    for part in parts {
        let (k, v) = part.split_once(": ")?;
        let v = v.trim().to_owned();
        match k.trim() {
            "Hardware" => {
                saw_hw = true;
                s.accelerator = Accelerator::from_label(&v);
                s.hardware = v;
            }
            "Shape" => s.shape = v,
            "Variant" => s.variant = v,
            "Status" => s.status = Some(v),
            _ => return None,
        }
    }
    (saw_hw && !s.hardware.is_empty() && !s.shape.is_empty() && !s.variant.is_empty()).then_some(s)
}

/// Parse `colab usage` output.
pub fn parse_usage(stdout: &str) -> Result<UsageReading> {
    let mut balance = None;
    let mut rate = None;
    let mut count = None;
    for line in stdout.lines() {
        let line = line.trim();
        if let Some(v) = line.strip_prefix("Current balance:") {
            balance = v
                .trim()
                .trim_end_matches("compute units")
                .trim()
                .parse::<f64>()
                .ok();
        } else if let Some(v) = line.strip_prefix("Usage rate:") {
            rate = v.trim().trim_end_matches("/hr").trim().parse::<f64>().ok();
        } else if let Some(v) = line.strip_prefix("Active assignments:") {
            count = v.trim().parse::<u32>().ok();
        }
    }
    match (balance, rate, count) {
        (Some(b), Some(r), Some(c)) if b.is_finite() && r.is_finite() => Ok(UsageReading {
            balance_cu: b,
            rate_cu_per_hour: r,
            assignments: c,
        }),
        _ => Err(Error::new(
            ErrorKind::Parse,
            format!(
                "unexpected `colab usage` output: {}",
                tail(&redact_secrets(stdout, &[]), 300)
            ),
        )),
    }
}

pub fn find_result(stdout: &str) -> Option<Value> {
    stdout
        .lines()
        .rev()
        .find_map(|l| l.trim_start().strip_prefix(RESULT_MARKER))
        .and_then(|j| serde_json::from_str(j.trim()).ok())
}

pub fn classify_failure(out: &ExecResult) -> Error {
    let all = format!("{}\n{}", out.stdout, out.stderr);
    let has = |s: &str| all.contains(s);
    let kind = if matches!(out.code, Some(126) | Some(127))
        && (has("No such file")
            || has("not found")
            || has("Permission denied")
            || has("cannot execute"))
    {
        ErrorKind::NotInstalled
    } else if has("Could not read the OAuth token from the OS keyring") {
        ErrorKind::KeyringUnavailable
    } else if has(AUTH_PROMPT_MARKER)
        || has("Enter the authorization code")
        || has("Reauthentication is needed")
        || has("DefaultCredentialsError")
        || has("invalid_grant")
        || has("HTTP 401")
    {
        ErrorKind::AuthRequired
    } else if has("Backend rejected accelerator") || has("HTTP 400") {
        ErrorKind::NotEntitled
    } else if has("Allocation refused") || has("HTTP 412") || has("HTTP 503") {
        ErrorKind::CapacityRefused
    } else if all.lines().any(|l| {
        l.trim_start().starts_with("ReadTimeout:")
            || l.trim_start()
                .starts_with("requests.exceptions.ReadTimeout:")
    }) {
        ErrorKind::AllocationTimeout
    } else if has("(HTTP 429)") {
        ErrorKind::SshBusy
    } else if has("appears to be lost")
        || (has("ession '") && has("' not found"))
        || has("(HTTP 404)")
    {
        ErrorKind::SessionGone
    } else if has("Connection was lost") {
        ErrorKind::ConnectionLost
    } else if has("Timeout waiting for") {
        ErrorKind::ExecTimeout
    } else {
        ErrorKind::Other
    };
    let detail = if out.stderr.trim().is_empty() {
        &out.stdout
    } else {
        &out.stderr
    };
    let mut e = Error::new(kind, tail(&redact_secrets(detail, &[]), 600));
    if let Some(code) = out.code {
        e = e.with("exit_code", code);
    }
    e
}

/// Unified Backend trait shared by `RealCliBackend` and `MockBackend`.
pub trait ColabBackend: Send + Sync {
    fn is_mock(&self) -> bool;
    fn check_env(&self, auth_mode: AuthMode) -> EnvironmentCheck;
    fn sessions(
        &self,
        account_id: &str,
        auth_mode: AuthMode,
        state_file: &Path,
        cancel: &CancelToken,
    ) -> Result<Vec<ServerSession>>;
    fn usage(
        &self,
        account_id: &str,
        auth_mode: AuthMode,
        state_file: &Path,
        cancel: &CancelToken,
    ) -> Result<UsageReading>;
    #[allow(clippy::too_many_arguments)]
    fn create_session(
        &self,
        account_id: &str,
        auth_mode: AuthMode,
        state_file: &Path,
        name: &str,
        accelerator: Accelerator,
        high_mem: bool,
        cancel: &CancelToken,
    ) -> Result<String>;
    #[allow(clippy::too_many_arguments)]
    fn create_session_with_receipt(
        &self,
        account_id: &str,
        auth_mode: AuthMode,
        state_file: &Path,
        _receipt: &Path,
        name: &str,
        accelerator: Accelerator,
        high_mem: bool,
        cancel: &CancelToken,
    ) -> Result<String> {
        self.create_session(
            account_id,
            auth_mode,
            state_file,
            name,
            accelerator,
            high_mem,
            cancel,
        )
    }
    fn probe_hardware(
        &self,
        account_id: &str,
        auth_mode: AuthMode,
        state_file: &Path,
        name: &str,
        cancel: &CancelToken,
    ) -> Result<HardwareProbe>;
    fn stop_session(
        &self,
        account_id: &str,
        auth_mode: AuthMode,
        state_file: &Path,
        name: &str,
        cancel: &CancelToken,
    ) -> Result<StopOutcome>;
    fn reap_endpoint(
        &self,
        account_id: &str,
        auth_mode: AuthMode,
        scratch_state: &Path,
        endpoint: &str,
        cancel: &CancelToken,
    ) -> Result<bool>;
    fn restart_kernel(
        &self,
        account_id: &str,
        auth_mode: AuthMode,
        state_file: &Path,
        name: &str,
        cancel: &CancelToken,
    ) -> Result<()>;
    fn session_url(
        &self,
        account_id: &str,
        auth_mode: AuthMode,
        state_file: &Path,
        name: &str,
        cancel: &CancelToken,
    ) -> Result<String>;
    #[allow(clippy::too_many_arguments)]
    fn exec_code(
        &self,
        account_id: &str,
        auth_mode: AuthMode,
        state_file: &Path,
        name: &str,
        code: &str,
        env: &[(String, String)],
        timeout_secs: u32,
        cancel: &CancelToken,
    ) -> Result<ExecOutput>;
    fn colab_binary(&self) -> Option<PathBuf>;
}

/// Real `colab` CLI backend routed through `Host`.
pub struct RealCliBackend {
    host: Host,
    colab: Option<PathBuf>,
    accounts: AccountStore,
    credentials: Mutex<BTreeMap<String, CredentialSlot>>,
}

const CREDENTIAL_TTL: Duration = Duration::from_secs(60);

type CredentialSlot = Arc<Mutex<Option<VerifiedCredentials>>>;

#[derive(Clone, PartialEq, Eq)]
struct CredentialSource {
    path: PathBuf,
    modified: SystemTime,
    len: u64,
}

impl CredentialSource {
    fn read(path: PathBuf) -> Result<Self> {
        let unavailable = || {
            Error::new(ErrorKind::AuthRequired,
            "configure a private credentials.json for this account or use the local gcloud ADC store for the default ADC account")
        };
        let meta = fs::metadata(&path).map_err(|_| unavailable())?;
        if !meta.is_file() {
            return Err(unavailable());
        }
        Ok(Self {
            path,
            modified: meta.modified().map_err(|_| unavailable())?,
            len: meta.len(),
        })
    }
}

#[derive(Clone)]
pub(crate) struct VerifiedCredentials {
    source: CredentialSource,
    authorized_user: bool,
    valid: Arc<AtomicBool>,
    verified_at: Instant,
    directory: Arc<PrivateDir>,
    pub(crate) identity: String,
}

impl VerifiedCredentials {
    pub(crate) fn env(&self) -> Vec<(String, String)> {
        vec![(
            "GOOGLE_APPLICATION_CREDENTIALS".into(),
            self.directory
                .path()
                .join("credentials.json")
                .to_string_lossy()
                .into_owned(),
        )]
    }

    pub(crate) fn invalidate(&self) {
        self.valid.store(false, Ordering::Relaxed);
    }
}

fn bound_identity(path: &Path) -> Result<Option<String>> {
    match fs::read(path) {
        Ok(raw) => {
            let (_, email): (AuthMode, String) = serde_json::from_slice(&raw).map_err(|_| {
                Error::new(
                    ErrorKind::AuthRequired,
                    "account credential binding is invalid",
                )
            })?;
            Ok(Some(email))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(_) => Err(Error::new(
            ErrorKind::AuthRequired,
            "cannot read account credential binding",
        )),
    }
}

fn whoami_identity(out: &ExecResult) -> Result<String> {
    if out.code != Some(0) || !out.stderr.trim().is_empty() {
        return Err(Error::new(
            classify_failure(out).kind,
            "Colab CLI could not verify the account credentials",
        ));
    }
    let mut identities = out
        .stdout
        .lines()
        .filter_map(|line| line.strip_prefix("Email:"))
        .map(str::trim);
    match (identities.next(), identities.next()) {
        (Some(email), None)
            if email.contains('@') && email.bytes().all(|b| b.is_ascii_graphic()) =>
        {
            Ok(email.to_ascii_lowercase())
        }
        _ => Err(Error::new(
            ErrorKind::AuthRequired,
            "Colab CLI could not verify a single credential email; check the userinfo.email scope",
        )),
    }
}

impl RealCliBackend {
    pub fn new(host: Host, data_dir: &Path) -> Self {
        Self {
            colab: host.discover_colab(),
            host,
            accounts: AccountStore::new(data_dir),
            credentials: Mutex::new(BTreeMap::new()),
        }
    }

    fn resolve_colab(&self) -> Result<String> {
        self.colab.as_ref().and_then(|p| p.to_str()).map(str::to_owned).ok_or_else(|| {
            Error::new(ErrorKind::NotInstalled,
                "colab CLI not found; install google-colab-cli or set COLAB_MCP_COLAB_BIN to its executable")
        })
    }

    pub(crate) fn invalidate_credentials(&self, account_id: Option<&str>) {
        let mut cache = self.credentials.lock().unwrap();
        if let Some(account_id) = account_id {
            cache.remove(account_id);
        } else {
            cache.clear();
        }
    }

    pub(crate) fn binding_identity(&self, account_id: &str) -> Result<Option<String>> {
        let file = self
            .accounts
            .cli_state_file(account_id)
            .with_file_name("credential-binding.json");
        Ok(bound_identity(&file)?.map(|email| email.to_ascii_lowercase()))
    }

    pub(crate) fn verified_env(
        &self,
        account_id: &str,
        auth_mode: AuthMode,
        cancel: &CancelToken,
    ) -> Result<VerifiedCredentials> {
        self.host.require_external_io()?;
        cancel.check()?;
        self.verified_env_with(account_id, auth_mode, |credentials| {
            let out = self.run_command(
                AuthMode::Adc,
                None,
                &CliCommand::Whoami,
                None,
                15,
                cancel,
                credentials.env(),
            )?;
            whoami_identity(&out)
        })
    }

    fn verified_env_with(
        &self,
        account_id: &str,
        auth_mode: AuthMode,
        verify: impl FnOnce(&VerifiedCredentials) -> Result<String>,
    ) -> Result<VerifiedCredentials> {
        validate_account_id(account_id)?;
        let slot = self
            .credentials
            .lock()
            .unwrap()
            .entry(account_id.to_owned())
            .or_default()
            .clone();
        // Coalesce concurrent whoami calls for this account only.
        let mut cache = slot.lock().unwrap();
        let previous = cache.take();
        let state = self.accounts.cli_state_file(account_id);
        let profile = self
            .accounts
            .load()?
            .get(account_id)
            .cloned()
            .ok_or_else(|| Error::new(ErrorKind::AuthRequired, "account profile is missing"))?;
        if account_id != ADC_ACCOUNT_ID && auth_mode != profile.auth_mode {
            return Err(Error::new(
                ErrorKind::AuthRequired,
                "account authentication mode changed",
            ));
        }
        let profile_file = state.with_file_name("credentials.json");
        let source = if auth_mode == AuthMode::OAuth2 {
            // The Colab CLI uses one OS keyring entry for OAuth2. An authorized-user
            // ADC file pins the OAuth identity through both whoami and the actual command.
            CredentialSource::read(profile_file).map_err(|_| Error::new(ErrorKind::AuthRequired,
                "OAuth2 requires an account-specific authorized-user credentials.json; the shared OS keyring cannot pin the account"))?
        } else if account_id != ADC_ACCOUNT_ID
            || !fs::symlink_metadata(&profile_file)
                .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
        {
            CredentialSource::read(profile_file)?
        } else {
            let path = if let Some(path) = std::env::var_os("GOOGLE_APPLICATION_CREDENTIALS") {
                PathBuf::from(path)
            } else if let Some(path) = std::env::var_os("CLOUDSDK_CONFIG") {
                PathBuf::from(path).join("application_default_credentials.json")
            } else {
                std::env::var_os("HOME")
                    .map(PathBuf::from)
                    .unwrap_or_default()
                    .join(".config/gcloud/application_default_credentials.json")
            };
            CredentialSource::read(path)?
        };
        let binding = state.with_file_name("credential-binding.json");
        let matches_profile = |email: &str| {
            profile
                .email
                .as_ref()
                .is_none_or(|e| e.eq_ignore_ascii_case(email))
        };
        let bound = bound_identity(&binding)?;
        if let Some(previous) = previous.filter(|previous| {
            previous.source == source
                && previous.valid.load(Ordering::Relaxed)
                && previous.verified_at.elapsed() < CREDENTIAL_TTL
                && matches_profile(&previous.identity)
                && bound
                    .as_ref()
                    .is_some_and(|email| email.eq_ignore_ascii_case(&previous.identity))
        }) {
            if auth_mode != AuthMode::OAuth2 || previous.authorized_user {
                *cache = Some(previous.clone());
                return Ok(previous);
            }
        }
        let creds = fs::read(&source.path)
            .map_err(|_| Error::new(ErrorKind::AuthRequired, "cannot read account credentials"))?;
        if source != CredentialSource::read(source.path.clone())? {
            return Err(Error::new(
                ErrorKind::AuthRequired,
                "credentials changed during verification; retry",
            ));
        }
        let config: Value = serde_json::from_slice(&creds).map_err(|_| {
            Error::new(
                ErrorKind::AuthRequired,
                "account credentials are not valid JSON",
            )
        })?;
        let credential_type = config["type"].as_str();
        let authorized_user = credential_type == Some("authorized_user");
        let self_contained = authorized_user
            || credential_type == Some("service_account")
            || (credential_type == Some("impersonated_service_account")
                && matches!(
                    config["source_credentials"]["type"].as_str(),
                    Some("authorized_user" | "service_account")
                ));
        if !self_contained || (auth_mode == AuthMode::OAuth2 && !authorized_user) {
            return Err(Error::new(ErrorKind::AuthRequired, "ADC requires a self-contained user or service-account file. OAuth2 requires authorized-user credentials."));
        }
        let directory = Arc::new(PrivateDir::new(&std::env::temp_dir(), "adc")?);
        write_atomic_private(&directory.path().join("credentials.json"), &creds)?;
        let mut verified = VerifiedCredentials {
            source,
            authorized_user,
            valid: Arc::new(AtomicBool::new(true)),
            verified_at: Instant::now(),
            directory,
            identity: String::new(),
        };
        let identity = verify(&verified)?;
        if !matches_profile(&identity) {
            return Err(Error::new(
                ErrorKind::AuthRequired,
                "Colab credential email differs from the account profile",
            ));
        }
        let _binding_lock = lock_file(&binding.with_extension("lock"))?;
        if verified.source != CredentialSource::read(verified.source.path.clone())? {
            return Err(Error::new(
                ErrorKind::AuthRequired,
                "credentials changed during verification; retry",
            ));
        }
        match bound_identity(&binding)? {
            Some(email) if !email.eq_ignore_ascii_case(&identity) => {
                return Err(Error::new(
                    ErrorKind::AuthRequired,
                    "Colab credential differs from the account's saved credential binding",
                ));
            }
            Some(_) => {}
            None => write_atomic_private(&binding, &serde_json::to_vec(&(auth_mode, &identity))?)?,
        }
        verified.verified_at = Instant::now();
        verified.identity = identity;
        *cache = Some(verified.clone());
        Ok(verified)
    }

    #[allow(clippy::too_many_arguments)]
    fn run_cli(
        &self,
        account_id: &str,
        auth_mode: AuthMode,
        state_file: &Path,
        cmd: &CliCommand,
        stdin: Option<Vec<u8>>,
        timeout_secs: u64,
        cancel: &CancelToken,
    ) -> Result<ExecResult> {
        let credentials = self
            .verified_env(account_id, auth_mode, cancel)
            .map_err(|error| {
                if matches!(cmd, CliCommand::New { .. }) {
                    error.with("allocation_not_started", true)
                } else {
                    error
                }
            })?;
        let result = self
            .run_command(
                AuthMode::Adc,
                Some(state_file),
                cmd,
                stdin,
                timeout_secs,
                cancel,
                credentials.env(),
            )
            .and_then(|out| {
                if out.code != Some(0) || !out.stderr.is_empty() {
                    let error = classify_failure(&out);
                    if error.kind == ErrorKind::AuthRequired {
                        return Err(error);
                    }
                }
                Ok(out)
            });
        if result
            .as_ref()
            .is_err_and(|e| e.kind == ErrorKind::AuthRequired)
        {
            credentials.invalidate();
            self.invalidate_credentials(Some(account_id));
        }
        result
    }

    #[allow(clippy::too_many_arguments)]
    fn run_command(
        &self,
        auth_mode: AuthMode,
        state_file: Option<&Path>,
        cmd: &CliCommand,
        stdin: Option<Vec<u8>>,
        timeout_secs: u64,
        cancel: &CancelToken,
        env: Vec<(String, String)>,
    ) -> Result<ExecResult> {
        let colab = self.resolve_colab()?;
        if let Some(parent) = state_file.and_then(|p| p.parent()) {
            let _ = fs::create_dir_all(parent);
        }
        let argv = build_argv(auth_mode, state_file, cmd)?;
        let mut req = ExecRequest::new(colab, argv);
        req.stdin = stdin;
        req.timeout = Some(Duration::from_secs(timeout_secs));
        req.env = env;
        let out = self.host.run(req, cancel, &mut |line| {
            if line.stream == Stream::Stderr && line.text.contains(AUTH_PROMPT_MARKER) {
                LineAction::Kill
            } else {
                LineAction::Continue
            }
        })?;
        if out.killed {
            let mut e = classify_failure(&out);
            if e.kind != ErrorKind::KeyringUnavailable {
                e = e.with_kind(ErrorKind::AuthRequired);
            }
            return Err(e);
        }
        Ok(out)
    }
}

/// OpenSSH expands percent tokens even inside shell quotes.
pub fn proxy_command(
    colab: &str,
    auth_mode: AuthMode,
    state_file: &Path,
    session: &str,
) -> Result<String> {
    validate_session_name(session)?;
    let path = |s: &str| {
        if s.chars().any(char::is_control) {
            Err(Error::invalid(
                "path",
                "control characters are forbidden in SSH paths",
            ))
        } else {
            Ok(shell_quote(&s.replace('%', "%%")))
        }
    };
    Ok(format!(
        "{} --auth={} --config {} ssh --proxy-mode --colab-internal-mode cmd -s {}",
        path(colab)?,
        auth_mode.cli_flag(),
        path(&state_file.to_string_lossy())?,
        shell_quote(session),
    ))
}

pub(crate) fn pin_session(
    state_file: &Path,
    session: &str,
    endpoint: Option<&str>,
) -> Result<PrivateDir> {
    validate_session_name(session)?;
    let row = read_session(state_file, session)
        .filter(|row| {
            row["endpoint"]
                .as_str()
                .is_some_and(|ep| endpoint.is_none_or(|expected| ep == expected))
        })
        .ok_or_else(|| {
            Error::new(
                ErrorKind::SessionGone,
                "session endpoint is no longer recorded",
            )
        })?;
    let pinned = PrivateDir::new(&std::env::temp_dir(), "session")?;
    let data = serde_json::to_vec(&serde_json::json!({session: row}))?;
    write_atomic_private(&pinned.path().join("sessions.json"), &data)?;
    Ok(pinned)
}

pub(crate) fn read_session(state_file: &Path, session: &str) -> Option<serde_json::Value> {
    let raw = fs::read(state_file).ok()?;
    serde_json::from_slice::<serde_json::Value>(&raw)
        .ok()?
        .get(session)
        .cloned()
}

fn update_session_file(
    state_file: &Path,
    change: impl FnOnce(&mut serde_json::Map<String, Value>) -> Result<()>,
) -> Result<()> {
    let _lock = lock_file(&state_file.with_extension("lock"))?;
    let mut sessions = match fs::read(state_file) {
        Ok(raw) => serde_json::from_slice(&raw)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => serde_json::Map::new(),
        Err(error) => return Err(error.into()),
    };
    change(&mut sessions)?;
    write_atomic_private(state_file, &serde_json::to_vec(&sessions)?)
}

/// Persist the new kernel metadata only if the name still refers to this endpoint.
pub(crate) fn merge_pinned_session(
    state_file: &Path,
    pinned: &Path,
    name: &str,
    endpoint: &str,
) -> Result<()> {
    let gone = || {
        Error::new(
            ErrorKind::SessionGone,
            "session changed before saving kernel state",
        )
    };
    let row = read_session(pinned, name)
        .filter(|row| row["endpoint"].as_str() == Some(endpoint))
        .ok_or_else(gone)?;
    update_session_file(state_file, |sessions| {
        if sessions.get(name).and_then(|row| row["endpoint"].as_str()) != Some(endpoint) {
            return Err(gone());
        }
        sessions.insert(name.to_owned(), row);
        Ok(())
    })
}

/// Ensure `~/.ssh/id_ed25519` or `~/.ssh/id_ecdsa` exists (Colab SSH rejects RSA).
pub(crate) fn ensure_ssh_key(host: &Host) -> Result<()> {
    host.require_external_io()?;
    static KEY_LOCK: Mutex<()> = Mutex::new(());
    let _guard = KEY_LOCK.lock().unwrap();
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return Ok(());
    };
    let ssh_dir = home.join(".ssh");
    if ssh_dir.join("id_ed25519.pub").is_file() || ssh_dir.join("id_ecdsa.pub").is_file() {
        return Ok(());
    }
    let key_path = ssh_dir.join("id_ed25519");
    if key_path.exists() {
        return Ok(());
    }
    fs::create_dir_all(&ssh_dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&ssh_dir, fs::Permissions::from_mode(0o700));
    }
    let req = ExecRequest::new(
        "ssh-keygen",
        [
            "-q",
            "-t",
            "ed25519",
            "-N",
            "",
            "-C",
            "colab-mcp",
            "-f",
            &key_path.to_string_lossy(),
        ]
        .map(str::to_owned)
        .to_vec(),
    );
    let status = host.run(req, &CancelToken::new(), &mut |_| LineAction::Continue)?;
    if status.code != Some(0) {
        return Err(Error::new(
            ErrorKind::Io,
            "failed to generate ed25519 SSH key for Colab port forwarding",
        ));
    }
    Ok(())
}

const PROBE_HARDWARE_SNIPPET: &str = r#"import json, subprocess
gpu = None
vram = None
try:
    out = subprocess.check_output(
        ["nvidia-smi", "--query-gpu=name,memory.total", "--format=csv,noheader,nounits"],
        text=True, timeout=15
    ).strip()
    if out:
        parts = [p.strip() for p in out.splitlines()[0].split(",")]
        if parts:
            gpu = parts[0] or None
        if len(parts) > 1:
            vram = int(float(parts[1]))
except Exception:
    pass
print("__CMCP_RESULT__ " + json.dumps({"gpu": gpu, "vram_mib": vram}))
"#;

impl ColabBackend for RealCliBackend {
    fn is_mock(&self) -> bool {
        false
    }

    fn colab_binary(&self) -> Option<PathBuf> {
        self.colab.clone()
    }

    fn check_env(&self, auth_mode: AuthMode) -> EnvironmentCheck {
        let bin = self.colab_binary();
        let colab_version = bin.as_ref().and_then(|p| {
            let mut req = ExecRequest::new(p.to_string_lossy(), vec!["version".into()]);
            req.timeout = Some(Duration::from_secs(5));
            self.host
                .run(req, &CancelToken::new(), &mut |_| LineAction::Continue)
                .ok()
                .filter(|r| r.code == Some(0))
                .map(|r| redact_secrets(r.stdout.trim(), &[]))
        });
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let ssh_key_ready = home.as_ref().is_some_and(|h| {
            h.join(".ssh/id_ed25519.pub").is_file() || h.join(".ssh/id_ecdsa.pub").is_file()
        });
        let adc_file_present = std::env::var_os("GOOGLE_APPLICATION_CREDENTIALS")
            .map(PathBuf::from)
            .is_some_and(|p| p.is_file())
            || home.as_ref().is_some_and(|h| {
                h.join(".config/gcloud/application_default_credentials.json")
                    .is_file()
            });
        EnvironmentCheck {
            colab_found: bin.is_some(),
            colab_path: bin.as_deref().map(display_path),
            colab_version,
            ssh_key_ready,
            adc_file_present,
            auth_mode,
            backend: "real".into(),
        }
    }

    fn sessions(
        &self,
        account_id: &str,
        auth_mode: AuthMode,
        state_file: &Path,
        cancel: &CancelToken,
    ) -> Result<Vec<ServerSession>> {
        // `colab sessions` refreshes and prunes its state. Give each invocation
        // a snapshot so it cannot overwrite concurrent allocations.
        let snapshot = PrivateDir::new(&std::env::temp_dir(), "list")?;
        let snapshot_file = snapshot.path().join("sessions.json");
        match fs::read(state_file) {
            Ok(raw) => write_atomic_private(&snapshot_file, &raw)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        let out = self.run_cli(
            account_id,
            auth_mode,
            &snapshot_file,
            &CliCommand::Sessions,
            None,
            35,
            cancel,
        )?;
        if out.code != Some(0) {
            return Err(classify_failure(&out));
        }
        if !out.stderr.trim().is_empty() {
            return Err(classify_failure(&out));
        }
        parse_sessions(&out.stdout)
    }

    fn usage(
        &self,
        account_id: &str,
        auth_mode: AuthMode,
        state_file: &Path,
        cancel: &CancelToken,
    ) -> Result<UsageReading> {
        let out = self.run_cli(
            account_id,
            auth_mode,
            state_file,
            &CliCommand::Usage,
            None,
            35,
            cancel,
        )?;
        if out.code != Some(0) {
            return Err(classify_failure(&out));
        }
        parse_usage(&out.stdout)
    }

    fn create_session(
        &self,
        account_id: &str,
        auth_mode: AuthMode,
        state_file: &Path,
        name: &str,
        accelerator: Accelerator,
        high_mem: bool,
        cancel: &CancelToken,
    ) -> Result<String> {
        let staging = PrivateDir::new(state_file.parent().unwrap_or(Path::new(".")), "new")?;
        self.create_session_with_receipt(
            account_id,
            auth_mode,
            state_file,
            &staging.path().join("sessions.json"),
            name,
            accelerator,
            high_mem,
            cancel,
        )
    }

    fn create_session_with_receipt(
        &self,
        account_id: &str,
        auth_mode: AuthMode,
        state_file: &Path,
        operation_file: &Path,
        name: &str,
        accelerator: Accelerator,
        high_mem: bool,
        cancel: &CancelToken,
    ) -> Result<String> {
        let cmd = CliCommand::New {
            name: name.to_owned(),
            accelerator,
            high_mem,
        };
        let previous = read_session(state_file, name);
        let result = self.run_cli(
            account_id,
            auth_mode,
            operation_file,
            &cmd,
            None,
            95,
            cancel,
        );
        let row = read_session(operation_file, name);
        let endpoint = row
            .as_ref()
            .and_then(|row| row["endpoint"].as_str())
            .filter(|ep| valid_endpoint(ep));
        let result = result.and_then(|out| {
            if out.code == Some(0) {
                Ok(())
            } else {
                let mut error = classify_failure(&out);
                // Only the CLI's assignment rejection is proof. A generic HTTP
                // status can also come from a request made after assignment.
                if out.stderr.lines().chain(out.stdout.lines()).any(|line| {
                    line.trim_start()
                        .starts_with("[colab] Allocation refused (precondition failed).")
                        || line
                            .trim_start()
                            .starts_with("[colab] Backend rejected accelerator ")
                }) {
                    error = error.with("allocation_rejected", true);
                }
                Err(error)
            }
        });
        if let Err(error) = result {
            return Err(match endpoint {
                Some(endpoint) => error.with("provisioned_endpoint", endpoint),
                None => error,
            });
        }
        let endpoint = endpoint
            .ok_or_else(|| {
                Error::new(
                    ErrorKind::OrphanSuspected,
                    "allocation returned no endpoint in its private state",
                )
            })?
            .to_owned();
        let import = update_session_file(state_file, |sessions| {
            if sessions.get(name) != previous.as_ref() {
                return Err(Error::invalid(
                    "session",
                    "session state changed during allocation",
                ));
            }
            sessions.insert(
                name.to_owned(),
                row.expect("the endpoint came from this row"),
            );
            Ok(())
        });
        import.map_err(|error| error.with("provisioned_endpoint", endpoint.clone()))?;
        Ok(endpoint)
    }

    fn probe_hardware(
        &self,
        account_id: &str,
        auth_mode: AuthMode,
        state_file: &Path,
        name: &str,
        cancel: &CancelToken,
    ) -> Result<HardwareProbe> {
        let out = self.exec_code(
            account_id,
            auth_mode,
            state_file,
            name,
            PROBE_HARDWARE_SNIPPET,
            &[],
            30,
            cancel,
        )?;
        let Some(v) = out.result else {
            return Ok(HardwareProbe::default());
        };
        Ok(HardwareProbe {
            gpu: v.get("gpu").and_then(|x| x.as_str()).map(ToOwned::to_owned),
            vram_mib: v.get("vram_mib").and_then(|x| x.as_u64()).map(|n| n as u32),
        })
    }

    fn stop_session(
        &self,
        account_id: &str,
        auth_mode: AuthMode,
        state_file: &Path,
        name: &str,
        cancel: &CancelToken,
    ) -> Result<StopOutcome> {
        let cmd = CliCommand::Stop {
            name: name.to_owned(),
        };
        let out = self.run_cli(account_id, auth_mode, state_file, &cmd, None, 45, cancel)?;
        if out.code != Some(0) {
            return Err(classify_failure(&out));
        }
        if out.stdout.to_ascii_lowercase().contains("not found") {
            Ok(StopOutcome::NotFound)
        } else {
            Ok(StopOutcome::Terminated)
        }
    }

    /// Release an unnamed `[?]` orphan runtime by writing a throw-away state record
    /// mapping a placeholder session name to `endpoint` and invoking `colab stop`.
    fn reap_endpoint(
        &self,
        account_id: &str,
        auth_mode: AuthMode,
        scratch_state: &Path,
        endpoint: &str,
        cancel: &CancelToken,
    ) -> Result<bool> {
        if !valid_endpoint(endpoint) {
            return Err(Error::invalid("endpoint", "invalid endpoint"));
        }
        let temp = PrivateDir::new(scratch_state, "reap")?;
        let scratch_state = &temp.path().join("sessions.json");
        let placeholder = "orphan-reap";
        let payload = serde_json::json!({
            placeholder: {
                "endpoint": endpoint,
                "name": placeholder,
                "url": "",
                "token": ""
            }
        });
        write_atomic_private(scratch_state, payload.to_string().as_bytes())?;
        let res = self.stop_session(account_id, auth_mode, scratch_state, placeholder, cancel);
        match res {
            Ok(StopOutcome::Terminated) => Ok(true),
            Ok(StopOutcome::NotFound) => Ok(false),
            Err(e) if e.kind == ErrorKind::SessionGone => Ok(false),
            Err(e) => Err(e),
        }
    }

    fn restart_kernel(
        &self,
        account_id: &str,
        auth_mode: AuthMode,
        state_file: &Path,
        name: &str,
        cancel: &CancelToken,
    ) -> Result<()> {
        let cmd = CliCommand::RestartKernel {
            name: name.to_owned(),
        };
        let out = self.run_cli(account_id, auth_mode, state_file, &cmd, None, 40, cancel)?;
        if out.code != Some(0) {
            return Err(classify_failure(&out));
        }
        Ok(())
    }

    fn session_url(
        &self,
        account_id: &str,
        auth_mode: AuthMode,
        state_file: &Path,
        name: &str,
        cancel: &CancelToken,
    ) -> Result<String> {
        let cmd = CliCommand::Url {
            name: name.to_owned(),
        };
        let out = self.run_cli(account_id, auth_mode, state_file, &cmd, None, 25, cancel)?;
        if out.code != Some(0) {
            return Err(classify_failure(&out));
        }
        let url = out
            .stdout
            .lines()
            .map(str::trim)
            .find(|l| l.starts_with("https://"))
            .ok_or_else(|| Error::new(ErrorKind::Parse, "no URL printed by `colab url`"))?;
        Ok(url.to_owned())
    }

    fn exec_code(
        &self,
        account_id: &str,
        auth_mode: AuthMode,
        state_file: &Path,
        name: &str,
        code: &str,
        env: &[(String, String)],
        timeout_secs: u32,
        cancel: &CancelToken,
    ) -> Result<ExecOutput> {
        let cmd = CliCommand::Exec {
            name: name.to_owned(),
            timeout_secs,
        };
        let mut payload = String::from(EXEC_PREAMBLE);
        let mut secrets = Vec::new();
        if !env.is_empty() {
            for (key, value) in env {
                if !is_env_key(key) || value.contains('\0') {
                    return Err(Error::invalid("env", "invalid environment entry"));
                }
            }
            let encoded = b64_encode(&serde_json::to_vec(&env)?);
            if env.iter().any(|(key, _)| !key.starts_with("CMCP_WD_")) {
                secrets.push(encoded.clone());
            }
            payload.push_str(&format!("import base64, json, os\nos.environ.update(json.loads(base64.b64decode('{encoded}')))\n"));
        }
        payload.push_str(code);
        let wait = if timeout_secs == 0 {
            120
        } else {
            u64::from(timeout_secs) + 20
        };
        let private = env.iter().any(|(key, _)| !key.starts_with("CMCP_WD_"));
        let mut out = if !private {
            self.run_cli(
                account_id,
                auth_mode,
                state_file,
                &cmd,
                Some(payload.into_bytes()),
                wait,
                cancel,
            )?
        } else {
            // The CLI's exec command records submitted input in history. SSH command
            // mode carries the private payload on stdin and can only connect to an existing assignment.
            let credentials = self.verified_env(account_id, auth_mode, cancel)?;
            let pinned = pin_session(state_file, name, None)?;
            let state = pinned.path().join("sessions.json");
            ensure_ssh_key(&self.host)?;
            let proxy = proxy_command(&self.resolve_colab()?, AuthMode::Adc, &state, name)?;
            let args = vec![
                "-T".into(),
                "-F".into(),
                "/dev/null".into(),
                "-o".into(),
                "BatchMode=yes".into(),
                "-o".into(),
                "StrictHostKeyChecking=no".into(),
                "-o".into(),
                "UserKnownHostsFile=/dev/null".into(),
                "-o".into(),
                format!("ProxyCommand={proxy}"),
                "root@colab-runtime".into(),
                "cd /content && exec python3 -".into(),
            ];
            let mut req = ExecRequest::new("ssh", args);
            req.stdin = Some(payload.into_bytes());
            req.redact.clone_from(&secrets);
            req.env = credentials.env();
            req.timeout = Some(Duration::from_secs(wait));
            self.host
                .run(req, cancel, &mut |_| LineAction::Continue)
                .map_err(|mut e| {
                    e.message = redact_secrets(&e.message, &secrets);
                    e
                })?
        };
        let mut result = find_result(&out.stdout);
        out.stdout = redact_secrets(&out.stdout, &secrets);
        out.stderr = redact_secrets(&out.stderr, &secrets);
        if out.code != Some(0) {
            let error = classify_failure(&out);
            if error.kind == ErrorKind::AuthRequired {
                self.invalidate_credentials(Some(account_id));
            }
            return Err(error);
        }
        if let Some(result) = &mut result {
            redact_json(result, &secrets);
        }
        Ok(ExecOutput {
            stdout: out.stdout,
            stderr: out.stderr,
            result,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_failure_text_masks_authentication_fields() {
        let pem = ["-----BEGIN", " PRIVATE KEY-----"].concat();
        let trace = format!(
            r#"{pem}
synthetic-private-body
-----END PRIVATE KEY-----
Authorization: Bearer
synthetic-access
adc {{"refresh_token":
 "dummy-refresh", "credentials": {{
 "unlabelled": "synthetic-json-secret"
 }}}}
diagnostic remains visible"#
        );
        let mut redactor = crate::util::DiagnosticRedactor::new(&[]);
        let stderr = trace
            .split_inclusive('\n')
            .map(|line| redactor.line(line))
            .collect();
        let error = classify_failure(&ExecResult {
            stderr,
            ..Default::default()
        });
        let malformed_usage = format!("{pem}\n{}\n{trace}", "synthetic-private-body".repeat(30));
        for error in [error, parse_usage(&malformed_usage).unwrap_err()] {
            for secret in [
                "dummy-refresh",
                "synthetic-private-body",
                "synthetic-access",
                "synthetic-json-secret",
            ] {
                assert!(!error.message.contains(secret));
            }
            assert!(error.message.contains("***"));
            assert!(error.message.contains("diagnostic remains visible"));
        }
    }

    #[test]
    fn credential_cache_accepts_only_verified_identity_and_rechecks_changes() {
        let temp = PrivateDir::new(&std::env::temp_dir(), "verified-identity").unwrap();
        let backend = RealCliBackend::new(Host::denied(), temp.path());
        let identity = ["fixture", "@", "example.invalid"].concat();
        let other = ["different", "@", "example.invalid"].concat();
        let account = "oauth-fixture";
        backend
            .accounts
            .upsert(
                crate::accounts::AccountProfile {
                    id: account.into(),
                    label: "OAuth fixture".into(),
                    email: Some(identity.clone()),
                    auth_mode: AuthMode::OAuth2,
                    added_at: 0,
                },
                false,
            )
            .unwrap();
        let state = backend.accounts.cli_state_file(account);
        let source = state.with_file_name("credentials.json");
        let binding = state.with_file_name("credential-binding.json");
        let invoke = |email: &str| {
            backend.verified_env_with(account, AuthMode::OAuth2, |_| Ok(email.into()))
        };
        write_atomic_private(&source, br#"{"type":"authorized_user"}"#).unwrap();
        assert_eq!(invoke(&other).err().unwrap().kind, ErrorKind::AuthRequired);
        assert!(!binding.exists());

        let first = invoke(&identity).unwrap();
        let cached = backend
            .verified_env_with(account, AuthMode::OAuth2, |_| panic!("whoami was repeated"))
            .unwrap();
        assert_eq!(first.env(), cached.env());
        write_atomic_private(&source, br#"{"type":"authorized_user","fixture":2}"#).unwrap();
        assert_eq!(invoke(&other).err().unwrap().kind, ErrorKind::AuthRequired);
        assert_ne!(first.env(), invoke(&identity).unwrap().env());

        write_atomic_private(
            &binding,
            &serde_json::to_vec(&(AuthMode::OAuth2, &other)).unwrap(),
        )
        .unwrap();
        assert_eq!(
            invoke(&identity).err().unwrap().kind,
            ErrorKind::AuthRequired
        );
        write_atomic_private(
            &binding,
            &serde_json::to_vec(&(AuthMode::OAuth2, &identity)).unwrap(),
        )
        .unwrap();
        let credentials = invoke(&identity).unwrap();
        credentials.invalidate();
        assert_ne!(credentials.env(), invoke(&identity).unwrap().env());
        backend.invalidate_credentials(Some(account));
        assert_eq!(invoke(&other).err().unwrap().kind, ErrorKind::AuthRequired);
    }

    #[test]
    fn parser_requires_complete_recognized_session_output() {
        let empty = "[colab] A new version of Colab CLI is available: 1.2.3 (current: 1.2.2)\n[colab] Run 'uv tool install -U google-colab-cli' to update.\n[colab] Pruned 1 stale local session(s).\n[colab] No active sessions found on server.\n";
        assert!(parse_sessions(empty).unwrap().is_empty());
        let complete = "[test] endpoint-1 | Hardware: L4 | Shape: STANDARD | Variant: GPU";
        for bad in [
            "".into(),
            "No sessions".into(),
            format!("{complete}\nunrecognized session output"),
            format!("{complete}\n{complete}"),
            format!("{complete}\n{empty}"),
        ] {
            assert_eq!(parse_sessions(&bad).unwrap_err().kind, ErrorKind::Parse);
        }
    }

    #[test]
    fn parse_sessions_handles_named_and_orphans() {
        let sample = "\
Pruned 1 stale local session(s).
[train-g4] m-s-abc123 | Hardware: G4 | Shape: Standard | Variant: GPU | Status: Connected
[?] m-s-browser9 | Hardware: A100 | Shape: High-RAM | Variant: GPU
[cpu-box] m-s-cpu1 | Hardware: CPU | Shape: Standard | Variant: DEFAULT
";
        let list = parse_sessions(sample).unwrap();
        assert_eq!(list.len(), 3);
        assert_eq!(list[0].name.as_deref(), Some("train-g4"));
        assert_eq!(list[0].accelerator, Some(Accelerator::G4));
        assert!(!list[0].high_mem());
        assert_eq!(list[1].name, None);
        assert_eq!(list[1].endpoint, "m-s-browser9");
        assert_eq!(list[1].accelerator, Some(Accelerator::A100));
        assert!(list[1].high_mem());
        assert_eq!(list[2].accelerator, Some(Accelerator::None));
    }

    #[test]
    fn parse_usage_and_argv_order() {
        let sample = "\
Current balance: 482.50 compute units
Usage rate: 8.90/hr
Active assignments: 1
";
        let u = parse_usage(sample).unwrap();
        assert!((u.balance_cu - 482.5).abs() < 1e-6);
        assert!((u.rate_cu_per_hour - 8.9).abs() < 1e-6);
        assert_eq!(u.assignments, 1);

        let argv = build_argv(
            AuthMode::Adc,
            Some(Path::new("/tmp/s.json")),
            &CliCommand::New {
                name: "s1".into(),
                accelerator: Accelerator::G4,
                high_mem: true,
            },
        )
        .unwrap();
        assert_eq!(
            argv,
            vec![
                "--auth=adc",
                "--config",
                "/tmp/s.json",
                "new",
                "-s",
                "s1",
                "--gpu",
                "G4",
                "--high-mem"
            ]
        );
    }
}
