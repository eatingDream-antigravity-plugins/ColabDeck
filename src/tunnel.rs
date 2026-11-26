//! [DP6] Exclusive local port forwarding over `colab ssh --proxy-mode`.
//!
//! A tunnel connects once to a pinned existing assignment using CLI command
//! mode. The transport never requests allocation or reconnects on exit.

use crate::cli::{
    ensure_ssh_key, pin_session, proxy_command, read_session, validate_session_name,
    VerifiedCredentials, AUTH_PROMPT_MARKER, RESERVED_VM_PORT,
};
use crate::error::{Error, ErrorKind, Result};
use crate::host::{isolate_process_group, kill_process_group, Host};
use crate::settings::AuthMode;
use crate::util::{tail, CancelToken};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
#[cfg(test)]
use std::fs;
use std::io::{ErrorKind as IoError, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TunnelSpec {
    pub account_id: String,
    #[serde(default)]
    pub endpoint: Option<String>,
    pub auth_mode: AuthMode,
    pub session: String,
    pub local_port: u16,
    pub remote_port: u16,
    pub health_path: Option<String>,
}

impl TunnelSpec {
    pub fn validate(&self) -> Result<()> {
        validate_session_name(&self.session)?;
        if self.remote_port == RESERVED_VM_PORT {
            return Err(Error::new(
                ErrorKind::PortReserved,
                "VM port 8080 is reserved by Colab; use another port such as 8000",
            )
            .with("port", self.remote_port));
        }
        if self.remote_port == 0 || self.local_port < 1024 {
            return Err(Error::invalid(
                "port",
                "local port must be 1024..=65535 and remote port must be 1..=65535",
            ));
        }
        if let Some(p) = &self.health_path {
            if !p.starts_with('/') || p.chars().any(|c| c.is_whitespace() || c.is_control()) {
                return Err(Error::invalid(
                    "health_path",
                    "must start with '/' without whitespace",
                ));
            }
        }
        Ok(())
    }

    pub fn local_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.local_port)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum TunnelState {
    Starting,
    Up,
    Reconnecting { attempt: u32, delay_secs: u64 },
    Failed { error: Error },
    Stopped,
}

impl TunnelState {
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Failed { .. } | Self::Stopped)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TunnelInfo {
    pub id: u64,
    pub spec: TunnelSpec,
    pub state: TunnelState,
    pub healthy: Option<bool>,
    pub local_url: String,
}

/// Build full `ssh` argument vector without ever reading `~/.ssh/config`.
pub fn ssh_args(colab: &str, state_file: &Path, spec: &TunnelSpec) -> Result<Vec<String>> {
    spec.validate()?;
    let opt = |s: &str| ["-o".to_owned(), s.to_owned()];
    let mut args = vec![
        "-N".to_owned(),
        "-T".to_owned(),
        "-F".to_owned(),
        "/dev/null".to_owned(),
    ];
    for o in [
        "ExitOnForwardFailure=yes",
        "ServerAliveInterval=30",
        "ServerAliveCountMax=3",
        "StrictHostKeyChecking=no",
        "UserKnownHostsFile=/dev/null",
        "LogLevel=ERROR",
        "BatchMode=yes",
        "ConnectTimeout=60",
    ] {
        args.extend(opt(o));
    }
    args.extend(opt(&format!(
        "ProxyCommand={}",
        proxy_command(colab, spec.auth_mode, state_file, &spec.session)?
    )));
    args.push("-L".into());
    args.push(format!(
        "127.0.0.1:{}:127.0.0.1:{}",
        spec.local_port, spec.remote_port
    ));
    args.push("root@colab-runtime".into());
    Ok(args)
}

pub fn state_has_session(state_file: &Path, session: &str) -> bool {
    read_session(state_file, session).is_some()
}

/// Classify terminal SSH / ProxyCommand errors (`None` = transient retryable).
pub fn classify_ssh_output(stderr: &str, code: Option<i32>) -> Option<ErrorKind> {
    let has = |s: &str| stderr.contains(s);
    if has("(HTTP 429)") {
        Some(ErrorKind::SshBusy)
    } else if has("Address already in use")
        || has("cannot listen to port")
        || has("Could not request local forwarding")
    {
        Some(ErrorKind::PortInUse)
    } else if has(AUTH_PROMPT_MARKER) || has("Reauthentication is needed") {
        Some(ErrorKind::AuthRequired)
    } else if has("Could not read the OAuth token from the OS keyring") {
        Some(ErrorKind::KeyringUnavailable)
    } else if (has("ession '") && has("not found"))
        || has("appears to be lost")
        || has("(HTTP 404)")
    {
        Some(ErrorKind::SessionGone)
    } else if code == Some(127) {
        Some(ErrorKind::NotInstalled)
    } else {
        None
    }
}

fn local_port_open(port: u16) -> bool {
    if cfg!(test) {
        return false;
    }
    let addr: SocketAddr = ([127, 0, 0, 1], port).into();
    TcpStream::connect_timeout(&addr, Duration::from_millis(250)).is_ok()
}

/// Lightweight local HTTP/1.1 health probe for a successful response.
fn local_http_healthy(port: u16, path: &str, cancel: &CancelToken) -> bool {
    if cfg!(test) || cancel.is_cancelled() {
        return false;
    }
    let addr: SocketAddr = ([127, 0, 0, 1], port).into();
    let Ok(mut stream) = TcpStream::connect_timeout(&addr, Duration::from_secs(2)) else {
        return false;
    };
    if stream
        .set_read_timeout(Some(Duration::from_millis(250)))
        .is_err()
        || stream
            .set_write_timeout(Some(Duration::from_secs(2)))
            .is_err()
    {
        return false;
    }
    let req = format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n");
    if stream.write_all(req.as_bytes()).is_err() {
        return false;
    }
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut line = [0; 128];
    let mut len = 0;
    while len < line.len() && Instant::now() < deadline && !cancel.is_cancelled() {
        match stream.read(&mut line[len..]) {
            Ok(0) => break,
            Ok(n) => {
                let end = len + n;
                if let Some(newline) = line[len..end].iter().position(|&b| b == b'\n') {
                    return healthy_http_status(&line[..len + newline]);
                }
                len = end;
            }
            Err(error)
                if matches!(
                    error.kind(),
                    IoError::TimedOut | IoError::WouldBlock | IoError::Interrupted
                ) => {}
            Err(_) => break,
        }
    }
    false
}

fn healthy_http_status(line: &[u8]) -> bool {
    let status = String::from_utf8_lossy(line);
    let mut fields = status.split_ascii_whitespace();
    matches!(fields.next(), Some("HTTP/1.0" | "HTTP/1.1"))
        && fields
            .next()
            .filter(|code| code.len() == 3)
            .and_then(|code| code.parse::<u16>().ok())
            .is_some_and(|code| (200..300).contains(&code))
}

struct ManagedTunnel {
    mock: bool,
    info: TunnelInfo,
    cancel: CancelToken,
}

/// Manages active local port-forward tunnels across real SSH or mock mode.
pub struct TunnelManager {
    next_id: AtomicU64,
    revision: Arc<AtomicU64>,
    tunnels: Arc<Mutex<BTreeMap<u64, ManagedTunnel>>>,
    host: Host,
}

impl Default for TunnelManager {
    fn default() -> Self {
        Self::new()
    }
}

impl TunnelManager {
    pub fn new() -> Self {
        Self::with_host(Host::new())
    }

    pub fn with_host(host: Host) -> Self {
        Self {
            host,
            next_id: AtomicU64::new(1),
            revision: Arc::new(AtomicU64::new(0)),
            tunnels: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    pub fn list(&self) -> Vec<TunnelInfo> {
        self.tunnels
            .lock()
            .unwrap()
            .values()
            .map(|t| t.info.clone())
            .collect()
    }

    pub(crate) fn revision(&self) -> u64 {
        self.revision.load(Ordering::Relaxed)
    }

    pub(crate) fn list_for(&self, account: &str, mock: bool) -> Vec<TunnelInfo> {
        self.tunnels
            .lock()
            .unwrap()
            .values()
            .filter(|t| t.mock == mock && t.info.spec.account_id == account)
            .map(|t| t.info.clone())
            .collect()
    }

    pub fn active_count(&self) -> usize {
        self.tunnels
            .lock()
            .unwrap()
            .values()
            .filter(|t| !t.info.state.is_terminal())
            .count()
    }

    pub fn open(
        &self,
        spec: TunnelSpec,
        is_mock: bool,
        colab_bin: Option<String>,
        state_file: PathBuf,
    ) -> Result<TunnelInfo> {
        self.open_with_env(spec, is_mock, colab_bin, state_file, None)
    }

    pub(crate) fn open_with_env(
        &self,
        spec: TunnelSpec,
        is_mock: bool,
        colab_bin: Option<String>,
        state_file: PathBuf,
        credentials: Option<VerifiedCredentials>,
    ) -> Result<TunnelInfo> {
        spec.validate()?;
        let transport = if is_mock {
            None
        } else {
            self.host.require_external_io()?;
            let colab = colab_bin
                .ok_or_else(|| Error::new(ErrorKind::NotInstalled, "colab CLI not found"))?;
            let endpoint = spec.endpoint.as_deref().ok_or_else(|| {
                Error::new(ErrorKind::SessionGone, "cannot verify the runtime endpoint")
            })?;
            let pinned = pin_session(&state_file, &spec.session, Some(endpoint))?;
            let mut transport_spec = spec.clone();
            if credentials.is_some() {
                transport_spec.auth_mode = AuthMode::Adc;
            }
            let args = ssh_args(
                &colab,
                &pinned.path().join("sessions.json"),
                &transport_spec,
            )?;
            ensure_ssh_key(&self.host)?;
            if local_port_open(spec.local_port) {
                return Err(Error::new(
                    ErrorKind::PortInUse,
                    format!("local port {} is already in use", spec.local_port),
                ));
            }
            Some((pinned, args))
        };
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let cancel = CancelToken::new();
        let info = TunnelInfo {
            id,
            local_url: spec.local_url(),
            healthy: (is_mock && spec.health_path.is_some()).then_some(true),
            spec: spec.clone(),
            state: if is_mock {
                TunnelState::Up
            } else {
                TunnelState::Starting
            },
        };
        {
            let mut map = self.tunnels.lock().unwrap();
            if map
                .values()
                .any(|t| !t.info.state.is_terminal() && t.info.spec.local_port == spec.local_port)
            {
                return Err(Error::new(
                    ErrorKind::PortInUse,
                    format!("local port {} is already forwarded", spec.local_port),
                ));
            }
            map.insert(
                id,
                ManagedTunnel {
                    mock: is_mock,
                    info: info.clone(),
                    cancel: cancel.clone(),
                },
            );
        }
        self.revision.fetch_add(1, Ordering::Relaxed);
        if let Some((pinned, args)) = transport {
            let map = Arc::clone(&self.tunnels);
            let host = self.host.clone();
            let revision = self.revision.clone();
            if let Err(error) = thread::Builder::new()
                .name(format!("colab-tunnel-{id}"))
                .spawn(move || {
                    let _pinned = pinned;
                    run_real_tunnel(id, spec, args, credentials, cancel, (map, revision), host);
                })
            {
                self.tunnels.lock().unwrap().remove(&id);
                self.revision.fetch_add(1, Ordering::Relaxed);
                return Err(error.into());
            }
        }
        Ok(info)
    }

    pub fn close(&self, id: u64) -> bool {
        let mut guard = self.tunnels.lock().unwrap();
        if let Some(t) = guard.get_mut(&id) {
            t.cancel.cancel();
            t.info.state = TunnelState::Stopped;
            t.info.healthy = None;
            self.revision.fetch_add(1, Ordering::Relaxed);
            true
        } else {
            false
        }
    }

    pub fn dismiss(&self, id: u64) -> bool {
        let mut tunnels = self.tunnels.lock().unwrap();
        if tunnels.get(&id).is_some_and(|t| t.info.state.is_terminal()) {
            tunnels.remove(&id);
            self.revision.fetch_add(1, Ordering::Relaxed);
            true
        } else {
            false
        }
    }

    pub(crate) fn get(&self, id: u64) -> Option<TunnelInfo> {
        self.tunnels
            .lock()
            .unwrap()
            .get(&id)
            .map(|t| t.info.clone())
    }

    pub fn close_for_session(
        &self,
        account_id: &str,
        session: &str,
        endpoint: Option<&str>,
        mock: bool,
    ) -> usize {
        let mut guard = self.tunnels.lock().unwrap();
        let mut closed = 0;
        for t in guard.values_mut() {
            if t.mock == mock
                && endpoint.map_or_else(
                    || t.info.spec.account_id == account_id && t.info.spec.session == session,
                    |ep| t.info.spec.endpoint.as_deref() == Some(ep),
                )
                && !t.info.state.is_terminal()
            {
                t.cancel.cancel();
                t.info.state = TunnelState::Stopped;
                t.info.healthy = None;
                closed += 1;
            }
        }
        if closed != 0 {
            self.revision.fetch_add(1, Ordering::Relaxed);
        }
        closed
    }

    pub fn close_all(&self) -> usize {
        let mut guard = self.tunnels.lock().unwrap();
        let mut closed = 0;
        for t in guard.values_mut() {
            if !t.info.state.is_terminal() {
                t.cancel.cancel();
                t.info.state = TunnelState::Stopped;
                t.info.healthy = None;
                closed += 1;
            }
        }
        if closed != 0 {
            self.revision.fetch_add(1, Ordering::Relaxed);
        }
        closed
    }
}

impl Drop for TunnelManager {
    fn drop(&mut self) {
        self.close_all();
    }
}

type TunnelMap = Arc<Mutex<BTreeMap<u64, ManagedTunnel>>>;

fn update_tunnel(
    (map, rev): &(TunnelMap, Arc<AtomicU64>),
    id: u64,
    state: TunnelState,
    healthy: Option<bool>,
) {
    if let Some(entry) = map.lock().unwrap().get_mut(&id) {
        if !entry.cancel.is_cancelled()
            && (entry.info.state != state || entry.info.healthy != healthy)
        {
            entry.info.state = state;
            entry.info.healthy = healthy;
            rev.fetch_add(1, Ordering::Relaxed);
        }
    }
}

fn run_real_tunnel(
    id: u64,
    spec: TunnelSpec,
    args: Vec<String>,
    credentials: Option<VerifiedCredentials>,
    cancel: CancelToken,
    map: (TunnelMap, Arc<AtomicU64>),
    host: Host,
) {
    if host.require_external_io().is_err() || cancel.is_cancelled() {
        return;
    }
    let mut command = Command::new("ssh");
    isolate_process_group(&mut command);
    let mut child = match command
        .args(args)
        .envs(
            credentials
                .as_ref()
                .into_iter()
                .flat_map(VerifiedCredentials::env),
        )
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(e) => {
            update_tunnel(
                &map,
                id,
                TunnelState::Failed {
                    error: Error::new(ErrorKind::NotInstalled, format!("failed to start ssh: {e}")),
                },
                None,
            );
            return;
        }
    };
    let stderr = Arc::new(Mutex::new(String::new()));
    let writer = Arc::clone(&stderr);
    let mut pipe = child.stderr.take().unwrap();
    thread::spawn(move || {
        let mut bytes = [0; 4096];
        while let Ok(n @ 1..) = pipe.read(&mut bytes) {
            let mut text = writer.lock().unwrap();
            text.push_str(&String::from_utf8_lossy(&bytes[..n]));
            if text.len() > 16_384 {
                *text = tail(&text, 16_380);
            }
        }
    });
    let started = Instant::now();
    let mut up = false;
    let mut last_health = started;
    let mut healthy = None;
    let code = loop {
        if cancel.is_cancelled() || (!up && started.elapsed() >= Duration::from_secs(75)) {
            kill_process_group(&mut child);
            thread::spawn(move || {
                let _ = child.wait();
            });
            break None;
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                kill_process_group(&mut child);
                break status.code();
            }
            Err(_) => {
                kill_process_group(&mut child);
                break None;
            }
            Ok(None) => {}
        }
        if !up && local_port_open(spec.local_port) {
            up = true;
            update_tunnel(&map, id, TunnelState::Up, None);
        }
        if let Some(path) = spec.health_path.as_deref().filter(|_| up) {
            let interval = if healthy == Some(true) { 15 } else { 1 };
            if healthy.is_none() || last_health.elapsed() >= Duration::from_secs(interval) {
                healthy = Some(local_http_healthy(spec.local_port, path, &cancel));
                last_health = Instant::now();
                update_tunnel(&map, id, TunnelState::Up, healthy);
            }
        }
        thread::sleep(Duration::from_millis(150));
    };
    if cancel.is_cancelled() {
        return;
    }
    let text = stderr.lock().unwrap();
    let kind = classify_ssh_output(&text, code).unwrap_or(ErrorKind::SessionGone);
    if kind == ErrorKind::AuthRequired {
        if let Some(credentials) = credentials.as_ref() {
            credentials.invalidate();
        }
    }
    let message = match kind {
        ErrorKind::SshBusy => {
            "Colab refused another SSH connection. Close an existing connection before reopening."
        }
        ErrorKind::PortInUse => "SSH could not bind the local forwarding port.",
        ErrorKind::AuthRequired | ErrorKind::KeyringUnavailable => {
            "Colab rejected the SSH credential context. Verify the account credentials."
        }
        ErrorKind::NotInstalled => "The SSH transport could not execute the configured Colab CLI.",
        _ => "SSH session expired. Verify that the original runtime is still active before reopening.",
    };
    update_tunnel(
        &map,
        id,
        TunnelState::Failed {
            error: Error::new(kind, message),
        },
        None,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tunnel_health_requires_a_successful_http_response() {
        assert!(healthy_http_status(b"HTTP/1.1 204 No Content\r"));
        assert!(!healthy_http_status(b"HTTP/1.1 404 Not Found\r"));
        assert!(!healthy_http_status(b"HTTP/1.1 503 Unavailable\r"));
        assert!(!healthy_http_status(b"junk 200"));
    }

    #[test]
    fn openssh_proxy_paths_escape_tokens_before_shell_quoting() {
        let path = Path::new("/tmp/有 空格/'state%h.json");
        let proxy = proxy_command("/tmp/工具 100%/colab", AuthMode::Adc, path, "session").unwrap();
        assert!(proxy.starts_with("'/tmp/工具 100%%/colab' --auth=adc"));
        assert!(proxy.contains(&crate::util::shell_quote("/tmp/有 空格/'state%%h.json")));
        assert!(proxy.contains("--colab-internal-mode cmd"));
        for bad in [
            "/tmp/colab\nexec",
            "/tmp/colab\r",
            "/tmp/colab\t",
            "/tmp/colab\u{7f}",
        ] {
            assert!(proxy_command(bad, AuthMode::Adc, path, "session").is_err());
        }
    }

    #[test]
    fn tunnel_rejects_port_8080_and_builds_safe_ssh_args() {
        let bad = TunnelSpec {
            account_id: "adc-default".into(),
            endpoint: None,
            auth_mode: AuthMode::Adc,
            session: "s1".into(),
            local_port: 8080,
            remote_port: 8080,
            health_path: None,
        };
        assert_eq!(bad.validate().unwrap_err().kind, ErrorKind::PortReserved);

        let good = TunnelSpec {
            account_id: "adc-default".into(),
            endpoint: None,
            auth_mode: AuthMode::Adc,
            session: "s1".into(),
            local_port: 18000,
            remote_port: 8000,
            health_path: Some("/health".into()),
        };
        let args = ssh_args("/usr/local/bin/colab", Path::new("/tmp/st.json"), &good).unwrap();
        assert!(args.contains(&"/dev/null".to_string()));
        assert!(args
            .iter()
            .any(|a| a.contains("--auth=adc") && a.contains("--proxy-mode")));
    }

    #[test]
    fn mock_tunnel_lifecycle_and_state_gate() {
        let dir = std::env::temp_dir().join(format!("colab-mcp-tun-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let st = dir.join("sessions.json");
        fs::write(&st, r#"{"s1":{"endpoint":"m-s-1"}}"#).unwrap();
        assert!(state_has_session(&st, "s1"));
        assert!(!state_has_session(&st, "gone"));

        let mgr = TunnelManager::new();
        let info = mgr
            .open(
                TunnelSpec {
                    account_id: "adc-default".into(),
                    endpoint: None,
                    auth_mode: AuthMode::Adc,
                    session: "s1".into(),
                    local_port: 18088,
                    remote_port: 8000,
                    health_path: Some("/health".into()),
                },
                true,
                None,
                st,
            )
            .unwrap();
        assert_eq!(info.state, TunnelState::Up);
        assert_eq!(info.healthy, Some(true));
        assert_eq!(mgr.active_count(), 1);
        assert!(!mgr.dismiss(info.id));
        assert!(mgr.close(info.id));
        assert_eq!(mgr.active_count(), 0);
        assert!(mgr.dismiss(info.id));
        assert!(mgr.list().is_empty());

        let failed = mgr.open(info.spec, true, None, PathBuf::new()).unwrap();
        update_tunnel(
            &(mgr.tunnels.clone(), mgr.revision.clone()),
            failed.id,
            TunnelState::Failed {
                error: Error::new(ErrorKind::ConnectionLost, "fixture disconnected"),
            },
            None,
        );
        assert!(mgr.dismiss(failed.id));
        assert!(!mgr.dismiss(failed.id));
        let _ = fs::remove_dir_all(&dir);
    }
}
