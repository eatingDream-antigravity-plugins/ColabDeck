//! [DP3.1] Session ledger and tracked runtime state.
//!
//! Strictly Layer 3: depends ONLY on DP1 (`settings`) and DP2 (`cli`),
//! never on usage attribution (DP4) or guard/watchdog logic (DP5).

use crate::cli::ServerSession;
use crate::error::{Error, ErrorKind, Result};
use crate::settings::{Accelerator, GuardPolicy};
use crate::util::{fnv1a_64, lock_file, write_atomic_private, PrivateDir};
use serde::{Deserialize, Serialize};
use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, BinaryHeap};
use std::fs::{self, File, TryLockError};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// What the host knows about a session's VM-side watchdog.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatchdogState {
    /// Absolute local Unix timestamp when the VM watchdog will release the runtime.
    pub deadline_secs: Option<u64>,
    pub idle_timeout_secs: Option<u64>,
    pub cancelled: bool,
    /// True only when the VM confirmed the watchdog process is alive and can unassign.
    pub armed: bool,
}

impl WatchdogState {
    pub fn enforced_deadline_secs(&self) -> Option<u64> {
        self.deadline_secs.filter(|_| self.armed && !self.cancelled)
    }

    pub(crate) fn enforced_idle_timeout_secs(&self) -> Option<u64> {
        self.idle_timeout_secs
            .filter(|&secs| secs > 0 && self.armed && !self.cancelled)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StopReason {
    User,
    Guard { limit: String, detail: String },
    Provision,
    Vanished,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StopRecord {
    pub at_secs: u64,
    pub reason: StopReason,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PendingStop {
    pub reason: StopReason,
    pub attempts: u32,
    pub retry_at_epoch: u64,
    pub last_error: Option<String>,
}

impl PendingStop {
    pub fn new(reason: StopReason) -> Self {
        Self {
            reason,
            attempts: 0,
            retry_at_epoch: 0,
            last_error: None,
        }
    }

    pub fn retry(&mut self, now: u64, error: String) {
        self.attempts = self.attempts.saturating_add(1);
        self.retry_at_epoch = now.saturating_add((5_u64 << self.attempts.min(6)).min(300));
        self.last_error = Some(error);
    }
}

/// Spend meter persisted per tracked session (integrated by DP4 `usage.rs`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SpendMeterState {
    pub started_at_secs: u64,
    pub spend_cu: f64,
    pub rate_known: bool,
    pub estimated: bool,
    pub integrated_to_secs: u64,
    pub last_share_cu_per_hour: Option<f64>,
    pub last_observed_secs: Option<u64>,
}

impl SpendMeterState {
    pub fn new(started_at_secs: u64) -> Self {
        Self {
            started_at_secs,
            spend_cu: 0.0,
            rate_known: false,
            estimated: false,
            integrated_to_secs: started_at_secs,
            last_share_cu_per_hour: None,
            last_observed_secs: None,
        }
    }

    pub fn spend(&self) -> Option<f64> {
        self.rate_known.then_some(self.spend_cu)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StartSource {
    Tracked,
    CliHistory,
    FirstSeen,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ObservedUnmanaged {
    pub account_id: String,
    pub endpoint: String,
    #[serde(default)]
    pub external_name: Option<String>,
    #[serde(default)]
    pub hardware: Option<String>,
    pub start_source: StartSource,
    pub spend: SpendMeterState,
    pub last_seen_secs: u64,
}

/// Account rate evidence is shared by polling processes and survives restarts.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RateAttribution {
    #[serde(default)]
    pub(crate) saw_concurrent: bool,
    #[serde(default)]
    pub(crate) concurrent_baseline_unknown: bool,
    pub(crate) combined_rate: Option<f64>,
    pub(crate) sole: Option<SoleRate>,
    pub(crate) observed_at_secs: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct SoleRate {
    pub(crate) endpoint: String,
    pub(crate) cu_per_hour: f64,
    pub(crate) stable_since_secs: u64,
}

/// A session created or managed by `colab-mcp`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrackedSession {
    pub name: String,
    pub account_id: String,
    pub endpoint: Option<String>,
    #[serde(default)]
    pub generation: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allocation: Option<AllocationIntent>,
    #[serde(default)]
    pub is_orphan: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hardware: Option<String>,
    pub accelerator: Accelerator,
    pub high_mem: bool,
    pub gpu: Option<String>,
    pub vram_mib: Option<u32>,
    pub policy: GuardPolicy,
    pub spend: SpendMeterState,
    pub watchdog: Option<WatchdogState>,
    pub stopped: Option<StopRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_stop: Option<PendingStop>,
    pub measured_rate_cu_per_hour: Option<f64>,
    pub below_spec: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AllocationIntent {
    /// Name of a private per-invocation CLI state directory, with no parent path.
    pub(crate) receipt: Option<String>,
    pub(crate) prior_endpoints: BTreeSet<String>,
}

impl TrackedSession {
    pub fn is_active(&self) -> bool {
        self.stopped.is_none()
    }

    pub fn uptime_secs(&self, now: u64) -> u64 {
        let end = self.stopped.as_ref().map(|s| s.at_secs).unwrap_or(now);
        end.saturating_sub(self.spend.started_at_secs)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct LedgerData {
    pub sessions: Vec<TrackedSession>,
    /// Last measured hardware burn rate keyed by `<ACC>:<high_mem>`.
    pub hardware_rates: BTreeMap<String, f64>,
    #[serde(default)]
    pub generation: u64,
    #[serde(default)]
    pub observed_unmanaged: BTreeMap<String, ObservedUnmanaged>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub rate_attribution: BTreeMap<String, RateAttribution>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub deployments: BTreeMap<String, DeployEndpoint>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DeployEndpoint {
    pub(crate) owned: bool,
    pub(crate) runs: BTreeMap<String, DeployRunLease>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct DeployRunLease {
    pub(crate) account_id: String,
    pub(crate) ports: BTreeSet<u16>,
    #[serde(default)]
    pub(crate) generation: u64,
    #[serde(default)]
    pub(crate) registered_at_secs: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) metadata: Option<DeployRunMetadata>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct DeployRunMetadata {
    pub(crate) session: String,
    pub(crate) plugin_id: String,
    pub(crate) created: bool,
    pub(crate) started_at_secs: u64,
}

impl LedgerData {
    pub fn hw_key(acc: Accelerator, high_mem: bool) -> String {
        let hm = high_mem && acc.supports_high_mem();
        format!("{}:{}", acc.as_str(), if hm { "hi" } else { "std" })
    }

    pub fn active_for_account(&self, account_id: &str) -> Vec<&TrackedSession> {
        self.sessions
            .iter()
            .filter(|s| s.account_id == account_id && s.is_active())
            .collect()
    }

    pub(crate) fn observed_for(
        &self,
        account_id: &str,
        endpoint: &str,
    ) -> Option<&ObservedUnmanaged> {
        self.observed_unmanaged
            .get(endpoint)
            .filter(|row| row.account_id == account_id)
    }

    pub(crate) fn released_since(&self, account: &str, endpoint: &str, since: u64) -> bool {
        self.sessions.iter().any(|row| {
            row.account_id == account
                && row.endpoint.as_deref() == Some(endpoint)
                && row.stopped.as_ref().is_some_and(|stop| {
                    stop.at_secs >= since && stop.reason != StopReason::Vanished
                })
        })
    }

    pub fn find_active_mut(&mut self, account_id: &str, name: &str) -> Option<&mut TrackedSession> {
        self.sessions
            .iter_mut()
            .find(|s| s.account_id == account_id && s.name == name && s.is_active())
    }

    pub fn find_live_mut(
        &mut self,
        account_id: &str,
        live: &ServerSession,
    ) -> Option<&mut TrackedSession> {
        let active = |row: &&TrackedSession| row.account_id == account_id && row.is_active();
        let exact = self
            .sessions
            .iter()
            .enumerate()
            .filter(|(_, row)| active(row))
            .find(|(_, row)| row.endpoint.as_ref() == Some(&live.endpoint));
        let index = exact.map(|(i, _)| i).or_else(|| {
            self.sessions
                .iter()
                .enumerate()
                .filter(|(_, row)| active(row))
                .find(|(_, row)| {
                    row.endpoint.is_none()
                        && row.pending_stop.is_none()
                        && row.allocation.is_none()
                        && live.name.as_ref() == Some(&row.name)
                })
                .map(|(i, _)| i)
        })?;
        self.sessions.get_mut(index)
    }

    pub(crate) fn target_mut(
        &mut self,
        account_id: &str,
        name: &str,
        endpoint: Option<&str>,
    ) -> Option<&mut TrackedSession> {
        self.sessions.iter_mut().find(|s| {
            s.account_id == account_id
                && s.is_active()
                && match endpoint {
                    Some(endpoint) => s.endpoint.as_deref() == Some(endpoint),
                    None => s.endpoint.is_none() && s.name == name,
                }
        })
    }

    pub(crate) fn next_generation(&mut self) -> u64 {
        self.generation += 1;
        self.generation
    }

    pub fn record_session(&mut self, mut session: TrackedSession) -> &mut TrackedSession {
        session.generation = self.next_generation();
        self.sessions.push(session);
        while self.sessions.len() > 100 {
            let Some(index) = self.sessions[..self.sessions.len() - 1]
                .iter()
                .position(|s| !s.is_active())
            else {
                break;
            };
            self.sessions.remove(index);
        }
        let index = self.sessions.len() - 1;
        &mut self.sessions[index]
    }

    /// Reconcile live `colab sessions` output against tracked active sessions:
    /// attach discovered endpoints and mark vanished sessions as stopped.
    pub fn reconcile(&mut self, account_id: &str, live: &[ServerSession], now: u64) {
        self.reconcile_sample(account_id, live, now, self.generation);
    }

    pub(crate) fn reconcile_sample(
        &mut self,
        account_id: &str,
        live: &[ServerSession],
        now: u64,
        generation: u64,
    ) {
        let endpoints: BTreeSet<_> = live.iter().map(|row| row.endpoint.as_str()).collect();
        for s in &mut self.sessions {
            if s.account_id != account_id
                || !s.is_active()
                || s.generation > generation
                || s.allocation.is_some()
                || s.endpoint.as_ref().is_some_and(|endpoint| {
                    self.deployments.get(endpoint).is_some_and(|deployment| {
                        deployment
                            .runs
                            .values()
                            .any(|run| run.generation > generation)
                    })
                })
            {
                continue;
            }
            if s.endpoint.is_none() && s.pending_stop.is_some() {
                continue;
            }
            let found = live.iter().find(|l| match &s.endpoint {
                Some(endpoint) => l.endpoint == *endpoint,
                None => l.name.as_deref() == Some(s.name.as_str()),
            });
            if let Some(found) = found {
                if s.endpoint.is_none() {
                    self.generation += 1;
                    s.generation = self.generation;
                    s.endpoint = Some(found.endpoint.clone());
                }
            } else if s.pending_stop.is_none()
                && s.endpoint.is_some()
                && now.saturating_sub(s.spend.started_at_secs) >= 15
            {
                s.stopped = Some(StopRecord {
                    at_secs: now,
                    reason: StopReason::Vanished,
                });
            }
        }
        self.deployments.retain(|endpoint, deployment| {
            if endpoints.contains(endpoint.as_str()) {
                return true;
            }
            deployment.runs.retain(|_, run| {
                run.account_id != account_id
                    || run.generation > generation
                    || now.saturating_sub(run.registered_at_secs) < 15
                    || self.sessions.iter().any(|row| {
                        row.is_active()
                            && row.account_id == account_id
                            && row.endpoint.as_ref() == Some(endpoint)
                    })
            });
            !deployment.runs.is_empty()
        });
        // Older ledgers recorded the endpoint, run ID and ports but omitted
        // the session name. Recover it only from that exact live endpoint.
        for (endpoint, deployment) in &mut self.deployments {
            let Some(name) = live
                .iter()
                .find(|row| &row.endpoint == endpoint)
                .and_then(|row| row.name.as_ref())
            else {
                continue;
            };
            for lease in deployment.runs.values_mut().filter(|run| {
                run.account_id == account_id
                    && run.generation <= generation
                    && run.metadata.is_none()
            }) {
                lease.metadata = Some(DeployRunMetadata {
                    session: name.clone(),
                    plugin_id: "unknown".into(),
                    created: false,
                    started_at_secs: lease.registered_at_secs,
                });
            }
        }
    }

    pub fn mark_stopped(&mut self, account_id: &str, name: &str, reason: StopReason, now: u64) {
        if let Some(s) = self.find_active_mut(account_id, name) {
            s.pending_stop = None;
            s.stopped = Some(StopRecord {
                at_secs: now,
                reason,
            });
        }
    }
}

#[derive(Debug, Clone)]
pub struct SessionLedger {
    path: PathBuf,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct SessionClaim {
    pub account_id: String,
    pub identity: String,
    pub name: String,
}

pub(crate) struct SessionLease {
    _directory: PrivateDir,
    _lock: File,
}

impl SessionLedger {
    pub fn new(data_dir: &Path) -> Self {
        Self {
            path: data_dir.join("ledger.json"),
        }
    }

    pub fn load(&self) -> Result<LedgerData> {
        let recovery = |kind, detail: &dyn std::fmt::Display| {
            Error::new(
                kind,
                format!("cost guard ledger recovery required: cannot read ledger.json ({detail})"),
            )
        };
        match fs::read(&self.path) {
            Ok(raw) => serde_json::from_slice(&raw).map_err(|e| recovery(ErrorKind::Parse, &e)),
            Err(e)
                if e.kind() == io::ErrorKind::NotFound
                    && fs::symlink_metadata(&self.path)
                        .is_err_and(|e| e.kind() == io::ErrorKind::NotFound) =>
            {
                Ok(LedgerData::default())
            }
            Err(e) => Err(recovery(ErrorKind::Io, &e)),
        }
    }

    pub fn save(&self, data: &LedgerData) -> Result<()> {
        self.update(|current| {
            let mut data = data.clone();
            data.generation = data.generation.max(current.generation);
            // Observation changes and pruning use update() against the locked ledger.
            // A full save can also carry a snapshot taken before another process polled.
            for (endpoint, observation) in std::mem::take(&mut current.observed_unmanaged) {
                if data
                    .observed_unmanaged
                    .get(&endpoint)
                    .is_none_or(|row| row.last_seen_secs <= observation.last_seen_secs)
                {
                    data.observed_unmanaged.insert(endpoint, observation);
                }
            }
            for (account, attribution) in std::mem::take(&mut current.rate_attribution) {
                if data
                    .rate_attribution
                    .get(&account)
                    .is_none_or(|row| row.observed_at_secs <= attribution.observed_at_secs)
                {
                    data.rate_attribution.insert(account, attribution);
                }
            }
            *current = data;
            Ok(())
        })
        .map(|_| ())
    }

    pub(crate) fn update<T>(
        &self,
        change: impl FnOnce(&mut LedgerData) -> Result<T>,
    ) -> Result<(T, LedgerData)> {
        let _lock = lock_file(&self.path.with_extension("lock"))?;
        let mut data = self.load()?;
        let previous = data.clone();
        let result = change(&mut data)?;
        if data != previous {
            write_atomic_private(&self.path, &serde_json::to_vec_pretty(&data)?)?;
        }
        Ok((result, data))
    }

    pub(crate) fn lock_endpoint(&self, endpoint: &str) -> Result<File> {
        let key = fnv1a_64(endpoint.as_bytes());
        lock_file(
            &self
                .path
                .with_file_name("endpoints")
                .join(format!("{key:016x}.lock")),
        )
    }

    /// Call under the ledger lock. A crashed provisioner releases its lease automatically.
    pub(crate) fn active_claims(&self) -> Result<Vec<SessionClaim>> {
        let root = self.path.with_file_name("reservations");
        let dirs = match fs::read_dir(root) {
            Ok(dirs) => dirs,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        let mut claims = Vec::new();
        for dir in dirs {
            let dir = dir?;
            if !dir.file_type()?.is_dir()
                || !dir
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".cmcp-provision-")
            {
                continue;
            }
            let lock = match File::open(dir.path().join("claim.lock")) {
                Ok(lock) => lock,
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e.into()),
            };
            match lock.try_lock_shared() {
                Ok(()) => match fs::remove_dir_all(dir.path()) {
                    Ok(()) => {}
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                    Err(e) => return Err(e.into()),
                },
                Err(TryLockError::WouldBlock) => match fs::read(dir.path().join("claim.json")) {
                    Ok(raw) => claims.push(serde_json::from_slice(&raw)?),
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                    Err(e) => return Err(e.into()),
                },
                Err(TryLockError::Error(e)) => return Err(e.into()),
            }
        }
        Ok(claims)
    }

    /// The caller holds the ledger lock from the quota check through lease creation.
    pub(crate) fn claim(&self, claim: &SessionClaim) -> Result<SessionLease> {
        let directory = PrivateDir::new(&self.path.with_file_name("reservations"), "provision")?;
        let lock = lock_file(&directory.path().join("claim.lock"))?;
        write_atomic_private(
            &directory.path().join("claim.json"),
            &serde_json::to_vec(claim)?,
        )?;
        Ok(SessionLease {
            _directory: directory,
            _lock: lock,
        })
    }
}

const CLI_HISTORY_FILES: usize = 64;
const CLI_HISTORY_TAIL_BYTES: u64 = 512 * 1024;
const CLI_HISTORY_RECENT: Duration = Duration::from_secs(48 * 3600);

pub(crate) struct CliHistoryStart {
    pub(crate) started_at_secs: u64,
    pub(crate) external_name: Option<String>,
}

#[derive(Deserialize)]
struct CliHistoryEvent {
    endpoint: Option<String>,
    timestamp: String,
    event_type: CliHistoryEventType,
}

#[derive(Deserialize)]
enum CliHistoryEventType {
    #[serde(rename = "session_created")]
    Created,
    #[serde(rename = "session_terminated", alias = "session_pruned")]
    Terminated,
}

/// Recover only the last lifecycle event for each requested endpoint. The directory,
/// file paths and raw history records stay local to this read-only scan.
pub(crate) fn recover_cli_history<'a>(
    directory: &Path,
    endpoints: &BTreeSet<&'a str>,
    now_secs: u64,
) -> BTreeMap<&'a str, CliHistoryStart> {
    let mut candidates = BinaryHeap::new();
    let now = SystemTime::now();
    if directory.symlink_metadata().is_ok_and(|m| m.is_dir()) {
        if let Ok(entries) = directory.read_dir() {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().is_none_or(|ext| ext != "jsonl") {
                    continue;
                }
                let Ok(metadata) = path.symlink_metadata() else {
                    continue;
                };
                if !metadata.is_file() {
                    continue;
                }
                let modified = metadata.modified().ok();
                let recent = modified
                    .and_then(|at| now.duration_since(at).ok())
                    .is_some_and(|age| age <= CLI_HISTORY_RECENT);
                candidates.push(Reverse((recent, modified, path)));
                if candidates.len() > CLI_HISTORY_FILES {
                    candidates.pop();
                }
            }
        }
    }

    let mut latest = BTreeMap::new();
    let mut tail = Vec::new();
    for Reverse((_, _, path)) in candidates.into_sorted_vec() {
        tail.clear();
        let Some(truncated) = read_cli_history_tail(&path, &mut tail) else {
            continue;
        };
        let mut in_file = BTreeMap::new();
        let mut last_termination = None;
        for line in tail
            .split(|&byte| byte == b'\n')
            .skip(usize::from(truncated))
        {
            let Ok(event) = serde_json::from_slice::<CliHistoryEvent>(line) else {
                continue;
            };
            let Some(timestamp) = parse_history_timestamp(&event.timestamp) else {
                continue;
            };
            if timestamp.0 > now_secs {
                continue;
            }
            let terminated = matches!(event.event_type, CliHistoryEventType::Terminated);
            let key = (timestamp, terminated);
            if event.endpoint.is_none() && terminated {
                last_termination = last_termination.max(Some((timestamp, true)));
            }
            let Some(&endpoint) = event.endpoint.as_deref().and_then(|ep| endpoints.get(ep)) else {
                continue;
            };
            let latest = in_file.entry(endpoint).or_insert(key);
            *latest = (*latest).max(key);
        }
        for (endpoint, mut key) in in_file {
            if let Some(termination) = last_termination {
                key = key.max(termination);
            }
            if latest
                .get(endpoint)
                .is_some_and(|(previous, _)| *previous >= key)
            {
                continue;
            }
            let (timestamp, terminated) = key;
            let start = (!terminated).then(|| {
                let name = path
                    .file_stem()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .chars()
                    .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
                    .take(32)
                    .collect::<String>();
                CliHistoryStart {
                    started_at_secs: timestamp.0,
                    external_name: (!name.is_empty()).then_some(name),
                }
            });
            latest.insert(endpoint, (key, start));
        }
    }
    latest
        .into_iter()
        .filter_map(|(ep, (_, start))| start.map(|s| (ep, s)))
        .collect()
}

fn read_cli_history_tail(path: &Path, tail: &mut Vec<u8>) -> Option<bool> {
    let mut options = File::options();
    options.read(true);
    // Avoid following a symlink or blocking on a FIFO if the file is replaced
    // between directory enumeration and open on the release platforms.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        use std::os::unix::fs::OpenOptionsExt;
        #[cfg(target_os = "linux")]
        const FLAGS: i32 = 0o400000 | 0o4000; // O_NOFOLLOW | O_NONBLOCK
        #[cfg(target_os = "macos")]
        const FLAGS: i32 = 0x0100 | 0x0004;
        options.custom_flags(FLAGS);
    }
    if !path.symlink_metadata().ok()?.is_file() {
        return None;
    }
    let mut file = options.open(path).ok()?;
    let metadata = file.metadata().ok()?;
    if !metadata.is_file() {
        return None;
    }
    let bytes = metadata.len().min(CLI_HISTORY_TAIL_BYTES);
    file.seek(SeekFrom::Start(metadata.len() - bytes)).ok()?;
    file.take(bytes).read_to_end(tail).ok()?;
    Some(bytes < metadata.len())
}

/// Parse a CLI ISO-8601 timestamp. Keep fractional precision for event ordering.
fn parse_history_timestamp(timestamp: &str) -> Option<(u64, u32)> {
    fn digits(bytes: &[u8]) -> Option<i64> {
        bytes.iter().try_fold(0, |n, b| {
            b.is_ascii_digit().then(|| n * 10 + i64::from(b - b'0'))
        })
    }

    let (date, rest) = timestamp.as_bytes().split_first_chunk::<19>()?;
    if date[4] != b'-'
        || date[7] != b'-'
        || !matches!(date[10], b'T' | b't' | b' ')
        || date[13] != b':'
        || date[16] != b':'
    {
        return None;
    }
    let year = digits(&date[..4])?;
    let month = usize::try_from(digits(&date[5..7])? - 1).ok()?;
    let day = digits(&date[8..10])?;
    let hour = digits(&date[11..13])?;
    let minute = digits(&date[14..16])?;
    let second = digits(&date[17..19])?;
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let months = [
        31,
        28 + i64::from(leap),
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    if !(1..=*months.get(month)?).contains(&day) || hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    let days_before_year = |year: i64| {
        let y = year - 1;
        y * 365 + y / 4 - y / 100 + y / 400
    };
    let days =
        days_before_year(year) - days_before_year(1970) + months[..month].iter().sum::<i64>() + day
            - 1;
    let (nanos, zone) = if let Some(fraction) = rest.strip_prefix(b".") {
        let length = fraction.iter().take_while(|b| b.is_ascii_digit()).count();
        if length == 0 {
            return None;
        }
        let precision = length.min(9);
        let nanos = digits(&fraction[..precision])? as u32 * 10_u32.pow((9 - precision) as u32);
        (nanos, &fraction[length..])
    } else {
        (0, rest)
    };
    let offset = match zone {
        [b'Z' | b'z'] => 0,
        [sign @ (b'+' | b'-'), h1, h2, b':', m1, m2] | [sign @ (b'+' | b'-'), h1, h2, m1, m2] => {
            let hours = digits(&[*h1, *h2])?;
            let minutes = digits(&[*m1, *m2])?;
            if hours > 23 || minutes > 59 {
                return None;
            }
            let offset = hours * 3600 + minutes * 60;
            if *sign == b'+' {
                offset
            } else {
                -offset
            }
        }
        _ => return None,
    };
    let secs = days * 86_400 + hour * 3600 + minute * 60 + second - offset;
    Some((u64::try_from(secs).ok()?, nanos))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_reconciliation_keeps_a_newer_registration_and_its_lease() {
        use serde_json::json;

        let root = PrivateDir::new(&std::env::temp_dir(), "stale-session-sample").unwrap();
        let sampler = SessionLedger::new(root.path());
        let generation = sampler.load().unwrap().generation;
        let endpoint = "runtime-late";
        SessionLedger::new(root.path())
            .update(|ledger| {
                let row = serde_json::from_value(json!({
                    "name": "late", "account_id": "fixture", "endpoint": endpoint,
                    "accelerator": "G4", "high_mem": false, "policy": GuardPolicy::unlimited(),
                    "spend": SpendMeterState::new(100), "below_spec": false
                }))?;
                ledger.record_session(row);
                let lease_generation = ledger.next_generation();
                ledger
                    .deployments
                    .entry(endpoint.into())
                    .or_default()
                    .runs
                    .insert(
                        "run-late".into(),
                        DeployRunLease {
                            account_id: "fixture".into(),
                            ports: BTreeSet::from([8000]),
                            generation: lease_generation,
                            registered_at_secs: 100,
                            metadata: None,
                        },
                    );
                Ok(())
            })
            .unwrap();
        let (_, preserved) = sampler
            .update(|ledger| {
                ledger.reconcile_sample("fixture", &[], 200, generation);
                Ok(())
            })
            .unwrap();
        assert!(preserved.sessions[0].is_active());
        assert!(preserved.deployments[endpoint]
            .runs
            .contains_key("run-late"));

        let (_, reconciled) = sampler
            .update(|ledger| {
                ledger.reconcile_sample("fixture", &[], 201, preserved.generation);
                Ok(())
            })
            .unwrap();
        assert_eq!(
            reconciled.sessions[0].stopped.as_ref().unwrap().reason,
            StopReason::Vanished
        );
        assert!(!reconciled.deployments.contains_key(endpoint));
    }

    #[test]
    fn cli_history_endpointless_prune_ends_earlier_creations_in_its_file() {
        let root = PrivateDir::new(&std::env::temp_dir(), "cli-prune").unwrap();
        fs::write(root.path().join("runtime.jsonl"), concat!(
            "{\"timestamp\":\"1970-01-01T01:00:00Z\",\"event_type\":\"session_created\",\"endpoint\":\"runtime-before\"}\n",
            "{\"timestamp\":\"1970-01-01T01:00:10Z\",\"event_type\":\"session_terminated\",\"reason\":\"prune\"}\n",
            "{\"timestamp\":\"1970-01-01T01:00:20Z\",\"event_type\":\"session_created\",\"endpoint\":\"runtime-after\"}\n",
        )).unwrap();
        let history = recover_cli_history(
            root.path(),
            &BTreeSet::from(["runtime-before", "runtime-after"]),
            7200,
        );
        assert!(!history.contains_key("runtime-before"));
        assert_eq!(history["runtime-after"].started_at_secs, 3620);
    }

    #[test]
    fn unreadable_ledger_is_preserved_and_concurrent_updates_merge() {
        let root = PrivateDir::new(&std::env::temp_dir(), "ledger-transaction").unwrap();
        let path = root.path().join("ledger.json");
        let store = SessionLedger::new(root.path());
        assert!(store.load().unwrap().sessions.is_empty());
        fs::write(&path, b"{incomplete").unwrap();
        let result = store.save(&LedgerData::default()).unwrap_err();
        assert_eq!(result.kind, ErrorKind::Parse);
        assert!(result.message.contains("recovery required"));
        assert_eq!(fs::read(&path).unwrap(), b"{incomplete");

        fs::remove_file(&path).unwrap();
        fs::create_dir(&path).unwrap();
        assert_eq!(
            store.save(&LedgerData::default()).unwrap_err().kind,
            ErrorKind::Io
        );
        assert!(path.is_dir());
        fs::remove_dir(&path).unwrap();
        fs::write(&path, br#"{"sessions":[],"hardware_rates":{}}"#).unwrap();
        let stale = store.load().unwrap();
        assert!(stale.observed_unmanaged.is_empty());

        // Independent handles exercise the OS lock used by separate processes.
        std::thread::scope(|scope| {
            for index in 0..12 {
                let writer = SessionLedger::new(root.path());
                scope.spawn(move || {
                    writer
                        .update(|ledger| {
                            ledger
                                .hardware_rates
                                .insert(format!("fixture-{index}"), f64::from(index));
                            let endpoint = format!("runtime-{index}");
                            ledger.observed_unmanaged.insert(
                                endpoint.clone(),
                                ObservedUnmanaged {
                                    account_id: "fixture".into(),
                                    endpoint,
                                    external_name: None,
                                    hardware: None,
                                    start_source: StartSource::FirstSeen,
                                    spend: SpendMeterState::new(1000),
                                    last_seen_secs: 1000,
                                },
                            );
                            Ok(())
                        })
                        .unwrap();
                });
            }
        });
        assert_eq!(store.load().unwrap().hardware_rates.len(), 12);
        store.save(&stale).unwrap();
        assert_eq!(store.load().unwrap().observed_unmanaged.len(), 12);
    }

    #[test]
    fn cli_history_scan_limits_files_and_reads_only_their_tails() {
        use std::io::Write;

        let root = PrivateDir::new(&std::env::temp_dir(), "history-limits").unwrap();
        let write_event = |file: &mut File, endpoint: &str| {
            writeln!(file, r#"{{"timestamp":"1970-01-01T00:00:30Z","event_type":"session_created","endpoint":"{endpoint}"}}"#).unwrap();
        };
        let older = root.path().join("older.jsonl");
        let mut older = File::create(older).unwrap();
        write_event(&mut older, "older-endpoint");
        older
            .set_modified(SystemTime::now() - Duration::from_secs(49 * 3600))
            .unwrap();
        for index in 0..(CLI_HISTORY_FILES - 1) {
            File::create(root.path().join(format!("fixture-{index}.jsonl"))).unwrap();
        }
        let name = "x".repeat(40);
        let mut large = File::create(root.path().join(format!("{name}.jsonl"))).unwrap();
        write_event(&mut large, "head-endpoint");
        large
            .seek(SeekFrom::Current(CLI_HISTORY_TAIL_BYTES as i64))
            .unwrap();
        large.write_all(b"\n").unwrap();
        write_event(&mut large, "tail-endpoint");

        let endpoints = BTreeSet::from(["older-endpoint", "head-endpoint", "tail-endpoint"]);
        let recovered = recover_cli_history(root.path(), &endpoints, 90);
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered["tail-endpoint"].started_at_secs, 30);
        assert_eq!(
            recovered["tail-endpoint"].external_name.as_deref(),
            Some(&name[..32])
        );
    }
}
