//! Coordinates account-scoped operations and publishes immutable panel snapshots.

use crate::accounts::{AccountProfile, AccountRegistry, AccountStore, ADC_ACCOUNT_ID};
use crate::cli::{
    merge_pinned_session, pin_session, read_session, valid_endpoint, validate_session_name,
    ColabBackend, EnvironmentCheck, HardwareProbe, RealCliBackend, ServerSession,
};
use crate::error::{Error, ErrorKind, Result};
use crate::guard::{evaluate as eval_guard, Concern, Decision};
use crate::hf::{list_gguf_files, search_gguf_models, HfModel};
use crate::host::Host;
use crate::mock::{MockBackend, DEMO_MANAGED_ENDPOINT};
use crate::plugins::{
    cancel_vm_run, launch_detached, poll_vm_run, validate_plugin_id, vm_run_alive, InstalledPlugin,
    PluginRunInfo, PluginStore, RunEndpointInfo,
};
use crate::provision::{
    provision_with_tracking, stop_and_confirm, AllocationUpdate, OnExhausted, ProvisionEvent,
    ProvisionSpec, SessionLimit,
};
use crate::quant::{preview_for_accelerator, FitChoice};
use crate::sessions::{
    AllocationIntent, DeployRunLease, DeployRunMetadata, LedgerData, PendingStop, SessionClaim,
    SessionLease, SessionLedger, SpendMeterState, StartSource, StopReason, StopRecord,
    TrackedSession, WatchdogState,
};
use crate::settings::{
    present_nullable, Accelerator, AuthMode, BackendMode, Defaults, GuardPolicy, Settings,
    SettingsPatch, SettingsStore,
};
use crate::tunnel::{TunnelInfo, TunnelManager, TunnelSpec, TunnelState};
use crate::usage::{
    attribute_rates_with_history, published_rate, session_estimated_rate, BalanceHistory,
    UsageSnapshot,
};
use crate::util::{
    display_path, generate_session_name, now_millis, now_secs, redact_json, redact_secrets,
    CancelToken, PrivateDir,
};
use crate::watchdog::{cancel_watchdog, extend_watchdog, start_watchdog, watchdog_status};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex, RwLock};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProvisionJobView {
    pub job_id: String,
    pub name: String,
    pub accelerator: Accelerator,
    pub done: bool,
    pub last_status: String,
    pub error: Option<String>,
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
struct AccountKey {
    mode: BackendMode,
    id: String,
}

#[derive(Clone)]
pub(crate) struct AccountContext {
    key: AccountKey,
    backend: Arc<dyn ColabBackend>,
    auth: AuthMode,
    state_file: PathBuf,
    endpoint_hint: Option<(String, String)>,
    pending_stop: Option<PendingTarget>,
    explicit_endpoint: bool,
}

#[derive(Clone)]
struct PendingTarget {
    name: String,
    endpoint: Option<String>,
    generation: u64,
}

impl From<&TrackedSession> for PendingTarget {
    fn from(row: &TrackedSession) -> Self {
        Self {
            name: row.name.clone(),
            endpoint: row.endpoint.clone(),
            generation: row.generation,
        }
    }
}

pub(crate) struct StopResult {
    pub(crate) confirmed: bool,
    pub(crate) ledger_warning: Option<String>,
}

enum StopRequest<'a> {
    Start(StopReason, Option<&'a str>),
    Retry(&'a PendingTarget, bool),
}

impl StopRequest<'_> {
    fn manual_retry(&self) -> bool {
        matches!(self, Self::Retry(_, true))
    }

    fn is_manual(&self) -> bool {
        self.manual_retry() || matches!(self, Self::Start(StopReason::User, _))
    }
}

impl AccountContext {
    fn hinted_endpoint(&self, name: &str) -> Option<&str> {
        self.endpoint_hint
            .as_ref()
            .filter(|(session, _)| session == name)
            .map(|(_, endpoint)| endpoint.as_str())
    }
}

#[derive(Clone)]
struct ManagedJob {
    account: AccountKey,
    view: ProvisionJobView,
    cancel: CancelToken,
}

#[derive(Clone)]
struct ManagedRun {
    context: AccountContext,
    info: PluginRunInfo,
    cancel: CancelToken,
    pinned: Option<Arc<PrivateDir>>,
    worker_active: bool,
    needs_recovery_check: bool,
}

impl ManagedRun {
    fn active(&self) -> bool {
        matches!(
            self.info.state.as_str(),
            "running" | "ready" | "cancelling" | "cancel_failed"
        )
    }
}

#[derive(Clone, Default)]
struct AccountSample {
    usage: Option<UsageSnapshot>,
    history: BalanceHistory,
    live: Vec<ServerSession>,
    rates: BTreeMap<String, (f64, bool)>,
    decisions: BTreeMap<String, Decision>,
    env: Option<EnvironmentCheck>,
    error: Option<String>,
    attempted: Option<Instant>,
    sampled_at: Option<u64>,
    guard_paused: bool,
}

type RefreshKey = (AccountKey, u64);

#[derive(Default)]
struct Flight {
    done: Mutex<Option<Result<()>>>,
    signal: Condvar,
}

impl Flight {
    fn finish(&self, result: Result<()>) {
        *self.done.lock().unwrap() = Some(result);
        self.signal.notify_all();
    }

    fn wait(&self) -> Result<()> {
        self.signal
            .wait_while(self.done.lock().unwrap(), |done| done.is_none())
            .unwrap()
            .as_ref()
            .unwrap()
            .clone()
    }
}

struct CoordinatorState {
    settings: Settings,
    real_settings: Settings,
    real_accounts: AccountRegistry,
    real_accounts_error: Option<Error>,
    mock_accounts: AccountRegistry,
    epoch: u64,
    real_ledger: LedgerData,
    mock_ledger: LedgerData,
    real_guard_enabled: bool,
    real_ledger_error: Option<String>,
    samples: BTreeMap<AccountKey, AccountSample>,
    flights: BTreeMap<RefreshKey, Arc<Flight>>,
    // Mock endpoints exist only inside this process.
    mock_endpoint_gates: BTreeMap<String, Arc<Mutex<()>>>,
    reservations: BTreeMap<AccountKey, BTreeSet<String>>,
    jobs: Vec<ManagedJob>,
    runs: Vec<ManagedRun>,
    hf_token: Option<String>,
    plugins: Arc<Vec<InstalledPlugin>>,
}

impl CoordinatorState {
    fn settings_for(&self, mode: BackendMode) -> &Settings {
        if mode == BackendMode::Real {
            &self.real_settings
        } else {
            &self.settings
        }
    }

    fn active_key(&self) -> AccountKey {
        AccountKey {
            mode: self.settings.backend_mode,
            id: self.accounts(self.settings.backend_mode).active_id.clone(),
        }
    }

    fn accounts(&self, mode: BackendMode) -> &AccountRegistry {
        match mode {
            BackendMode::Mock => &self.mock_accounts,
            BackendMode::Real => &self.real_accounts,
        }
    }

    fn ledger(&self, mode: BackendMode) -> &LedgerData {
        match mode {
            BackendMode::Mock => &self.mock_ledger,
            BackendMode::Real => &self.real_ledger,
        }
    }

    fn config_changed(&mut self) {
        self.epoch += 1;
        for sample in self.samples.values_mut() {
            sample.attempted = None;
            sample.env = None;
        }
    }

    fn refresh_interval(&self, key: &AccountKey) -> Duration {
        let active = self.samples.get(key).is_some_and(|s| !s.live.is_empty())
            || !self.ledger(key.mode).active_for_account(&key.id).is_empty()
            || self.reservations.get(key).is_some_and(|r| !r.is_empty());
        self.settings_for(key.mode)
            .defaults
            .usage_poll_interval(active)
            .max(Duration::from_secs(if active { 15 } else { 30 }))
    }
}

pub struct PanelSnapshot {
    pub rev: u64,
    payload: Value,
    public_payload: Value,
    pub summary: String,
}

impl PanelSnapshot {
    pub fn json(&self, since_rev: Option<u64>, entered: Instant) -> Value {
        Self::snapshot_json(&self.payload, self.rev, since_rev, entered)
    }

    pub fn public_json(&self, entered: Instant) -> Value {
        Self::snapshot_json(
            &self.public_payload,
            self.rev.saturating_sub(1),
            None,
            entered,
        )
    }

    fn snapshot_json(payload: &Value, rev: u64, since_rev: Option<u64>, entered: Instant) -> Value {
        if since_rev == Some(rev) {
            return json!({ "unchanged": true, "rev": rev, "server_ms": entered.elapsed().as_millis() as u64 });
        }
        let mut value = payload.clone();
        value["rev"] = json!(rev);
        value["server_ms"] = json!(entered.elapsed().as_millis() as u64);
        value["unchanged"] = json!(false);
        value
    }
}

pub struct ColabCoordinator {
    data_dir: PathBuf,
    history_dir: Option<PathBuf>,
    settings_store: SettingsStore,
    account_store: AccountStore,
    ledger_store: SessionLedger,
    plugin_store: PluginStore,
    host: Host,
    real_backend: Arc<RealCliBackend>,
    mock_backend: Arc<MockBackend>,
    tunnels: TunnelManager,
    force_mock: Option<bool>,
    next_seq: AtomicU64,
    state: Mutex<CoordinatorState>,
    config_write: Mutex<()>,
    ledger_write: Mutex<()>,
    deploy_registration: Mutex<()>,
    snapshot_write: Mutex<()>,
    snapshot: RwLock<Arc<PanelSnapshot>>,
    refresh_signal: mpsc::SyncSender<()>,
    force_refresh: AtomicBool,
    published_tunnels: AtomicU64,
    shutdown: CancelToken,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProvisionRequest {
    pub name: Option<String>,
    pub accelerator: Option<Accelerator>,
    pub high_mem: Option<bool>,
    #[serde(default, deserialize_with = "present_nullable")]
    pub min_vram_mib: Option<Option<u32>>,
    pub max_attempts: Option<u32>,
    #[serde(default, deserialize_with = "present_nullable")]
    pub max_runtime_secs: Option<Option<u64>>,
    #[serde(default, deserialize_with = "present_nullable")]
    pub idle_timeout_secs: Option<Option<u64>>,
    #[serde(default, deserialize_with = "present_nullable")]
    pub max_spend_cu: Option<Option<f64>>,
    #[serde(default, deserialize_with = "present_nullable")]
    pub min_balance_cu: Option<Option<f64>>,
}

struct Reservation {
    coordinator: Arc<ColabCoordinator>,
    account: AccountKey,
    name: String,
    _lease: Option<SessionLease>,
}

impl Drop for Reservation {
    fn drop(&mut self) {
        if let Some(names) = self
            .coordinator
            .state
            .lock()
            .unwrap()
            .reservations
            .get_mut(&self.account)
        {
            names.remove(&self.name);
        }
    }
}

impl ColabCoordinator {
    pub fn new(data_dir: PathBuf, force_mock: Option<bool>) -> Arc<Self> {
        Self::with_host(data_dir, force_mock, Host::new())
    }

    pub fn with_host(data_dir: PathBuf, force_mock: Option<bool>, host: Host) -> Arc<Self> {
        Self::with_history_dir(data_dir, force_mock, host, None)
    }

    pub fn with_history_dir(
        data_dir: PathBuf,
        force_mock: Option<bool>,
        host: Host,
        history_dir: Option<PathBuf>,
    ) -> Arc<Self> {
        let settings_store = SettingsStore::new(&data_dir);
        let account_store = AccountStore::new(&data_dir);
        let ledger_store = SessionLedger::new(&data_dir);
        let plugin_store = PluginStore::new(&data_dir);
        let real_settings = if force_mock == Some(true) {
            settings_store.load_current().unwrap_or_default()
        } else {
            settings_store.load().0
        };
        let mut settings = real_settings.clone();
        if let Some(mock) = force_mock {
            settings.backend_mode = if mock {
                BackendMode::Mock
            } else {
                BackendMode::Real
            };
        }
        let (accounts, real_accounts_error) = match account_store.load() {
            Ok(accounts) => (accounts, None),
            Err(error) => (AccountRegistry::default(), Some(error)),
        };
        let (real_ledger, real_ledger_error) = if force_mock != Some(true) {
            match ledger_store.load() {
                Ok(ledger) => (ledger, None),
                Err(e) => (LedgerData::default(), Some(e.message)),
            }
        } else {
            (LedgerData::default(), None)
        };
        let real_guard_enabled = settings.backend_mode == BackendMode::Real
            || real_ledger_error.is_some()
            || real_ledger.sessions.iter().any(TrackedSession::is_active)
            || !real_ledger.deployments.is_empty();
        let mock_accounts = if accounts.active().is_some() {
            accounts.clone()
        } else {
            AccountRegistry::default_with_adc()
        };
        let plugins = Arc::new(plugin_store.list());
        let (refresh_signal, refresh_requests) = mpsc::sync_channel(1);
        let coord = Arc::new(Self {
            real_backend: Arc::new(RealCliBackend::new(host.clone(), &data_dir)),
            mock_backend: Arc::new(MockBackend::demo()),
            tunnels: TunnelManager::with_host(host.clone()),
            data_dir,
            history_dir,
            settings_store,
            account_store,
            ledger_store,
            plugin_store,
            host,
            force_mock,
            next_seq: AtomicU64::new(1),
            state: Mutex::new(CoordinatorState {
                settings,
                real_settings,
                mock_accounts,
                real_accounts: accounts,
                real_accounts_error,
                epoch: 0,
                real_ledger,
                mock_ledger: LedgerData::default(),
                real_guard_enabled,
                real_ledger_error,
                samples: BTreeMap::new(),
                flights: BTreeMap::new(),
                mock_endpoint_gates: BTreeMap::new(),
                reservations: BTreeMap::new(),
                jobs: Vec::new(),
                runs: Vec::new(),
                hf_token: None,
                plugins,
            }),
            config_write: Mutex::new(()),
            ledger_write: Mutex::new(()),
            deploy_registration: Mutex::new(()),
            snapshot_write: Mutex::new(()),
            refresh_signal,
            force_refresh: AtomicBool::new(false),
            published_tunnels: AtomicU64::new(0),
            snapshot: RwLock::new(Arc::new(PanelSnapshot {
                rev: 0,
                payload: Value::Null,
                public_payload: Value::Null,
                summary: String::new(),
            })),
            shutdown: CancelToken::new(),
        });
        coord.restore_deploys();
        coord.publish();
        coord.schedule_active(false);
        coord.guard_tick();
        let weak = Arc::downgrade(&coord);
        thread::spawn(move || {
            while refresh_requests.recv().is_ok() {
                let Some(coord) = weak.upgrade() else { break };
                let force = coord.force_refresh.swap(false, Ordering::AcqRel);
                coord.schedule_active(force);
            }
        });
        coord
    }

    pub fn shutdown(&self) {
        self.shutdown.cancel();
        self.tunnels.close_all();
        let state = self.state.lock().unwrap();
        for job in &state.jobs {
            job.cancel.cancel();
        }
        for run in &state.runs {
            run.cancel.cancel();
        }
    }

    fn context(&self, state: &CoordinatorState, key: AccountKey) -> Option<AccountContext> {
        if key.mode == BackendMode::Real && state.real_accounts_error.is_some() {
            return None;
        }
        let profile = state.accounts(key.mode).get(&key.id)?;
        let auth = if profile.id == ADC_ACCOUNT_ID {
            state.settings_for(key.mode).auth_mode
        } else {
            profile.auth_mode
        };
        let backend: Arc<dyn ColabBackend> = match key.mode {
            BackendMode::Mock => self.mock_backend.clone(),
            BackendMode::Real => self.real_backend.clone(),
        };
        Some(AccountContext {
            state_file: self.account_store.cli_state_file(&key.id),
            endpoint_hint: None,
            pending_stop: None,
            explicit_endpoint: false,
            key,
            backend,
            auth,
        })
    }

    fn active_context(&self) -> Result<AccountContext> {
        self.pin_context(None)
    }

    fn pin_context(&self, session: Option<&str>) -> Result<AccountContext> {
        self.pin_action_context(session, None, None, None, false)
    }

    pub(crate) fn pin_action_context(
        &self,
        session: Option<&str>,
        endpoint: Option<&str>,
        account_id: Option<&str>,
        expected_mode: Option<BackendMode>,
        stopping: bool,
    ) -> Result<AccountContext> {
        let state = self.state.lock().unwrap();
        let key = state.active_key();
        verify_backend_mode(expected_mode, key.mode)?;
        if key.mode == BackendMode::Real {
            if let Some(error) = &state.real_accounts_error {
                return Err(error.clone());
            }
        }
        if account_id.is_some_and(|account| account != key.id) {
            return Err(Error::invalid(
                "account_id",
                "account mismatch; refresh panel",
            ));
        }
        if endpoint.is_some_and(|endpoint| !valid_endpoint(endpoint)) {
            return Err(Error::invalid("endpoint", "invalid endpoint"));
        }
        let endpoint_hint = session.and_then(|name| {
            endpoint
                .map(str::to_owned)
                .or_else(|| {
                    state
                        .samples
                        .get(&key)
                        .into_iter()
                        .flat_map(|sample| &sample.live)
                        .find(|s| s.name.as_deref() == Some(name))
                        .map(|s| s.endpoint.clone())
                })
                .or_else(|| {
                    state
                        .ledger(key.mode)
                        .active_for_account(&key.id)
                        .into_iter()
                        .find(|s| s.name == name && s.endpoint.is_some())
                        .and_then(|row| row.endpoint.clone())
                })
                .map(|endpoint| (name.to_owned(), endpoint))
        });
        let pending_stop = if stopping {
            find_pending_stop(state.ledger(key.mode), &key.id, session, endpoint)?
        } else {
            None
        };
        let mut context = self.context(&state, key).ok_or_else(account_mismatch)?;
        context.endpoint_hint = endpoint_hint;
        context.pending_stop = pending_stop;
        context.explicit_endpoint = endpoint.is_some();
        Ok(context)
    }

    pub(crate) fn verify_action_context(
        &self,
        ctx: &AccountContext,
        expected: Option<BackendMode>,
        account: Option<&str>,
    ) -> Result<()> {
        let state = self.state.lock().unwrap();
        verify_backend_mode(expected, state.settings.backend_mode)?;
        verify_backend_mode(expected, ctx.key.mode)?;
        if account.is_some_and(|id| id != ctx.key.id) {
            return Err(account_mismatch());
        }
        if ctx.key.mode == BackendMode::Real {
            if let Some(error) = &state.real_accounts_error {
                return Err(error.clone());
            }
        }
        Ok(())
    }

    fn update_ledger<T>(
        &self,
        mode: BackendMode,
        change: impl FnOnce(&mut LedgerData) -> Result<T>,
    ) -> Result<T> {
        if mode == BackendMode::Mock {
            return change(&mut self.state.lock().unwrap().mock_ledger);
        }
        let _writer = self.ledger_write.lock().unwrap();
        let (result, data) = self.ledger_store.update(change)?;
        let mut state = self.state.lock().unwrap();
        state.real_ledger = data;
        state.real_ledger_error = None;
        Ok(result)
    }

    fn reload_real_ledger(&self) -> bool {
        let _writer = self.ledger_write.lock().unwrap();
        let loaded = self.ledger_store.load();
        let accounts = self.account_store.load();
        let mut state = self.state.lock().unwrap();
        let previous_error = state.real_ledger_error.clone();
        let previous_accounts_error = state.real_accounts_error.clone();
        let mut accounts_changed = false;
        match accounts {
            Ok(registry) => {
                if state.real_accounts != registry
                    && (self.account_store.exists()
                        || state.real_accounts.get(ADC_ACCOUNT_ID).is_none()
                        || state.real_accounts.accounts.len() != 1)
                {
                    state.real_accounts = registry;
                    accounts_changed = true;
                }
                state.real_accounts_error = None;
            }
            Err(error) => state.real_accounts_error = Some(error),
        }
        if accounts_changed || previous_accounts_error != state.real_accounts_error {
            state.config_changed();
        }
        match loaded {
            Ok(ledger) => {
                state.real_ledger = ledger;
                state.real_ledger_error = None;
            }
            Err(error) => state.real_ledger_error = Some(error.message),
        }
        accounts_changed
            || previous_accounts_error != state.real_accounts_error
            || previous_error != state.real_ledger_error
    }

    fn with_endpoint<T>(
        &self,
        key: &AccountKey,
        endpoint: &str,
        op: impl FnOnce() -> Result<T>,
    ) -> Result<T> {
        if key.mode == BackendMode::Real {
            let _lock = self.ledger_store.lock_endpoint(endpoint)?;
            op()
        } else {
            let lock = self
                .state
                .lock()
                .unwrap()
                .mock_endpoint_gates
                .entry(endpoint.to_owned())
                .or_default()
                .clone();
            let _guard = lock.lock().unwrap();
            op()
        }
    }

    fn schedule_active(self: &Arc<Self>, force: bool) -> Option<Arc<Flight>> {
        let state = if force {
            self.state.lock().ok()?
        } else {
            self.state.try_lock().ok()?
        };
        let key = state.active_key();
        self.schedule_locked(state, key, force)
    }

    fn schedule_locked(
        self: &Arc<Self>,
        mut state: std::sync::MutexGuard<'_, CoordinatorState>,
        key: AccountKey,
        force: bool,
    ) -> Option<Arc<Flight>> {
        if self.shutdown.is_cancelled() {
            return None;
        }
        let id = (key.clone(), state.epoch);
        if let Some(existing) = state.flights.get(&id) {
            return Some(existing.clone());
        }
        let attempted = state.samples.get(&key).and_then(|sample| sample.attempted);
        if !force && attempted.is_some_and(|at| at.elapsed() < state.refresh_interval(&key)) {
            return None;
        }
        let context = self.context(&state, key)?;
        let flight = Arc::new(Flight::default());
        state.flights.insert(id.clone(), flight.clone());
        drop(state);
        let coord = self.clone();
        let done = flight.clone();
        let task_id = id.clone();
        let spawn = thread::Builder::new()
            .name("colab-refresh".into())
            .spawn(move || {
                let result = coord.refresh_account(&context, task_id.1);
                let mut state = coord.state.lock().unwrap();
                state.flights.remove(&task_id);
                done.finish(result);
            });
        if let Err(error) = spawn {
            flight.finish(Err(error.into()));
            self.state.lock().unwrap().flights.remove(&id);
        }
        Some(flight)
    }

    pub fn refresh(self: &Arc<Self>, force: bool) -> Result<()> {
        if force {
            self.active_context()?;
        }
        if let Some(flight) = self.schedule_active(force).filter(|_| force) {
            return flight.wait();
        }
        self.shutdown.check()
    }

    pub fn guard_tick(self: &Arc<Self>) {
        let real_guard_enabled = self.state.lock().unwrap().real_guard_enabled;
        if real_guard_enabled && self.reload_real_ledger() {
            self.publish();
        }
        let keys = {
            let state = self.state.lock().unwrap();
            let current = state.active_key();
            let now = now_secs();
            let mut keys = BTreeMap::from([(current, false)]);
            for mode in [BackendMode::Real, BackendMode::Mock] {
                if mode == BackendMode::Real && !state.real_guard_enabled {
                    continue;
                }
                for session in state.ledger(mode).sessions.iter().filter(|s| s.is_active()) {
                    let key = AccountKey {
                        mode,
                        id: session.account_id.clone(),
                    };
                    let due = session
                        .pending_stop
                        .as_ref()
                        .is_some_and(|p| session.endpoint.is_some() && p.retry_at_epoch <= now);
                    keys.entry(key)
                        .and_modify(|value| *value |= due)
                        .or_insert(due);
                }
                for deployment in state.ledger(mode).deployments.values() {
                    for run in deployment.runs.values() {
                        keys.entry(AccountKey {
                            mode,
                            id: run.account_id.clone(),
                        })
                        .or_default();
                    }
                }
            }
            keys
        };
        for (key, due) in keys {
            self.schedule_locked(self.state.lock().unwrap(), key, due);
        }
        if self.published_tunnels.load(Ordering::Relaxed) != self.tunnels.revision() {
            self.publish();
        }
    }

    fn refresh_account(&self, context: &AccountContext, epoch: u64) -> Result<()> {
        let key = &context.key;
        let superseded = || Error::new(ErrorKind::Cancelled, "account refresh was superseded");
        let (need_env, known, cached_generation) = {
            let state = self.state.lock().unwrap();
            if state.epoch != epoch || self.shutdown.is_cancelled() {
                return Err(superseded());
            }
            let need_env = state.samples.get(key).is_none_or(|s| s.env.is_none());
            let known = state
                .ledger(key.mode)
                .active_for_account(&key.id)
                .iter()
                .filter_map(|s| s.endpoint.clone())
                .collect::<BTreeSet<_>>();
            (need_env, known, state.ledger(key.mode).generation)
        };
        // Capture the committed generation before sampling, including writes by
        // another coordinator. If the ledger is unreadable, skip reconciliation.
        let generation = if key.mode == BackendMode::Real {
            self.ledger_store
                .load()
                .ok()
                .map(|ledger| ledger.generation)
        } else {
            Some(cached_generation)
        };
        let requested_at = now_secs();
        let (sessions, reading, env) = thread::scope(|scope| {
            let usage = scope.spawn(|| {
                context
                    .backend
                    .usage(&key.id, context.auth, &context.state_file, &self.shutdown)
            });
            let env = need_env.then(|| scope.spawn(|| context.backend.check_env(context.auth)));
            let sessions = context.backend.sessions(
                &key.id,
                context.auth,
                &context.state_file,
                &self.shutdown,
            );
            (
                sessions,
                usage.join().unwrap(),
                env.map(|worker| worker.join().unwrap()),
            )
        });
        let now = now_secs();
        let mut demo = Vec::new();
        if context.backend.is_mock() {
            if let Ok(live) = &sessions {
                for s in live {
                    if let Some(name) = s.name.as_ref().filter(|_| {
                        s.endpoint == DEMO_MANAGED_ENDPOINT && !known.contains(&s.endpoint)
                    }) {
                        let hw = context
                            .backend
                            .probe_hardware(
                                &key.id,
                                context.auth,
                                &context.state_file,
                                name,
                                &self.shutdown,
                            )
                            .unwrap_or_default();
                        demo.push((s.clone(), hw));
                    }
                }
            }
        }
        let (mut sample, policy) = {
            let state = self.state.lock().unwrap();
            if state.epoch != epoch || self.shutdown.is_cancelled() {
                return Err(superseded());
            }
            (
                state.samples.get(key).cloned().unwrap_or_default(),
                state.settings_for(key.mode).defaults.guard.clone(),
            )
        };
        sample.attempted = Some(Instant::now());
        sample.error = None;
        if env.is_some() {
            sample.env = env;
        }
        let refresh_error = sessions
            .as_ref()
            .err()
            .or_else(|| reading.as_ref().err())
            .cloned();
        let sessions_ok = sessions.is_ok();
        let emergency_live = sessions.as_ref().ok().cloned();
        let usage_ok = reading.is_ok();
        match reading {
            Ok(reading) => {
                sample.history.push(now, reading.balance_cu);
                sample.usage = Some(UsageSnapshot::from_reading(reading, now));
            }
            Err(_) => {
                if let Some(old) = &mut sample.usage {
                    old.stale = true;
                }
            }
        }
        if let Some(error) = &refresh_error {
            sample.error = Some(error.message.clone());
        }
        let default_history = if self.history_dir.is_none()
            && key.mode == BackendMode::Real
            && self.host.permits_external_io()
        {
            std::env::var_os("HOME")
                .filter(|home| !home.is_empty())
                .map(|home| PathBuf::from(home).join(".config/colab-cli/history"))
        } else {
            None
        };
        let history_dir = self.history_dir.as_deref().or(default_history.as_deref());
        let updates = self.update_ledger(key.mode, |ledger| {
            let mut receipts = Vec::new();
            if let Ok(mut live) = sessions {
                if let Some(generation) = generation {
                    ledger.reconcile_sample(&key.id, &live, requested_at, generation);
                    if key.mode == BackendMode::Real {
                        receipts = self.reconcile_allocations(key, ledger, &live, generation)?;
                    }
                }
                for (s, hw) in demo {
                    let name = s.name.unwrap();
                    if ledger.sessions.iter().any(|row| {
                        row.account_id == key.id && row.endpoint.as_ref() == Some(&s.endpoint)
                    }) {
                        continue;
                    }
                    let spec = ProvisionSpec {
                        name,
                        accelerator: s.accelerator.unwrap_or(Accelerator::G4),
                        high_mem: s.shape.eq_ignore_ascii_case("high-ram"),
                        min_vram_mib: None,
                        max_attempts: 1,
                        retry_delay_secs: 0,
                        on_exhausted: OnExhausted::StopAndFail,
                    };
                    let mut row = tracked_session(
                        context,
                        &spec,
                        policy.clone(),
                        Some(s.endpoint),
                        hw,
                        now.saturating_sub(1800),
                    );
                    row.watchdog = Some(WatchdogState {
                        deadline_secs: Some(now + 5400),
                        idle_timeout_secs: Some(1800),
                        cancelled: false,
                        armed: true,
                    });
                    ledger.record_session(row);
                }
                live.retain(|s| !ledger.released_since(&key.id, &s.endpoint, requested_at));
                sample.live = live;
                sample.sampled_at = Some(now);
            }
            sample.rates = attribute_rates_with_history(
                sample.usage.as_ref(),
                sessions_ok.then_some(sample.live.as_slice()),
                ledger,
                &key.id,
                now,
                history_dir,
            );
            sample.decisions.clear();
            sample.guard_paused = !sessions_ok || !usage_ok;
            let mut stops = Vec::new();
            for row in ledger
                .sessions
                .iter_mut()
                .filter(|s| s.account_id == key.id && s.is_active() && s.endpoint.is_some())
            {
                let decision = eval_guard(&row.policy, now, sample.usage.as_ref(), &row.spend);
                if let Decision::Stop(concern) = &decision {
                    row.pending_stop
                        .get_or_insert_with(|| PendingStop::new(guard_reason(concern)));
                }
                sample.decisions.insert(
                    row.endpoint.clone().unwrap_or_else(|| row.name.clone()),
                    decision,
                );
                if row
                    .pending_stop
                    .as_ref()
                    .is_some_and(|p| p.retry_at_epoch <= now)
                {
                    stops.push(PendingTarget::from(&*row));
                }
            }
            Ok((stops, receipts))
        });
        let (stops, receipts) = match updates {
            Ok(updates) => updates,
            Err(error) => {
                sample.guard_paused = true;
                sample.error = Some(error.message.clone());
                if let Some(live) = emergency_live {
                    sample.live = live;
                    sample.sampled_at = Some(now);
                }
                let mut state = self.state.lock().unwrap();
                if state.epoch == epoch {
                    state.samples.insert(key.clone(), sample);
                }
                drop(state);
                self.publish();
                return Err(error);
            }
        };
        for receipt in receipts {
            let _ = std::fs::remove_dir_all(receipt);
        }
        {
            let mut state = self.state.lock().unwrap();
            if state.epoch != epoch || self.shutdown.is_cancelled() {
                return Err(superseded());
            }
            // A stop may finish between sampling and publishing.
            sample.live.retain(|s| {
                !state
                    .ledger(key.mode)
                    .released_since(&key.id, &s.endpoint, requested_at)
            });
            state.samples.insert(key.clone(), sample);
        }
        self.restore_deploys();
        self.sync_completed_leases();
        self.publish();
        for target in stops {
            if self.shutdown.is_cancelled() {
                break;
            }
            let result = self.retry_pending(context, &target, false);
            if let Err(error) = result {
                self.record_sample_error(context, &error);
            }
        }
        if sessions_ok && key.mode == BackendMode::Real {
            self.check_recovered_deploys(context, now);
        }
        refresh_error.map_or(Ok(()), Err)
    }

    fn reconcile_allocations(
        &self,
        key: &AccountKey,
        ledger: &mut LedgerData,
        live: &[ServerSession],
        generation: u64,
    ) -> Result<Vec<PathBuf>> {
        let stale = |row: &TrackedSession| {
            row.account_id == key.id
                && row.is_active()
                && row.generation <= generation
                && row.allocation.is_some()
        };
        if !ledger.sessions.iter().any(stale) {
            return Ok(Vec::new());
        }
        let claims = self.ledger_store.active_claims()?;
        let mut consumed = Vec::new();
        for row in ledger.sessions.iter_mut().filter(|row| stale(row)) {
            if claims
                .iter()
                .any(|claim| claim.account_id == key.id && claim.name == row.name)
            {
                continue;
            }
            let Some(intent) = &row.allocation else {
                continue;
            };
            let receipt = intent
                .receipt
                .as_deref()
                .filter(|name| {
                    name.strip_prefix(".cmcp-allocation-")
                        .is_some_and(|suffix| {
                            !suffix.is_empty()
                                && suffix.bytes().all(|c| c.is_ascii_digit() || c == b'-')
                        })
                })
                .map(|name| self.data_dir.join(name));
            let recovered = row.endpoint.clone().or_else(|| {
                let directory = receipt.as_ref()?;
                if !directory.symlink_metadata().is_ok_and(|meta| meta.is_dir()) {
                    return None;
                }
                let file = directory.join("sessions.json");
                if !file.symlink_metadata().is_ok_and(|meta| meta.is_file()) {
                    return None;
                }
                let state = read_session(&file, &row.name)?;
                let endpoint = state["endpoint"].as_str()?;
                (valid_endpoint(endpoint) && !intent.prior_endpoints.contains(endpoint))
                    .then(|| endpoint.to_owned())
            });
            let pending = row
                .pending_stop
                .get_or_insert_with(|| PendingStop::new(StopReason::Provision));
            if let Some(endpoint) = recovered {
                if row.endpoint.is_none() {
                    ledger.generation += 1;
                    row.generation = ledger.generation;
                }
                row.endpoint = Some(endpoint);
                row.allocation = None;
                pending.retry_at_epoch = 0;
                if let Some(receipt) = receipt {
                    consumed.push(receipt);
                }
            } else {
                let unverified_live = live
                    .iter()
                    .any(|s| !intent.prior_endpoints.contains(&s.endpoint));
                pending.last_error = Some(if unverified_live {
                    "allocation may still be live; inspect the account and verify the original endpoint"
                } else {
                    "allocation endpoint is unconfirmed; inspect the account before retrying"
                }.into());
            }
        }
        Ok(consumed)
    }

    pub fn refresh_env(&self) -> Result<EnvironmentCheck> {
        let (context, epoch) = {
            let state = self.state.lock().unwrap();
            let key = state.active_key();
            let context = self.context(&state, key).ok_or_else(|| {
                state
                    .real_accounts_error
                    .clone()
                    .unwrap_or_else(account_mismatch)
            })?;
            (context, state.epoch)
        };
        let env = context.backend.check_env(context.auth);
        let mut state = self.state.lock().unwrap();
        if state.epoch == epoch {
            state.samples.entry(context.key).or_default().env = Some(env.clone());
        }
        drop(state);
        self.publish();
        Ok(env)
    }

    pub(crate) fn redact_result(&self, result: &mut Value, public: bool) {
        let mut secrets = Vec::new();
        {
            let state = self.state.lock().unwrap();
            secrets.extend(state.hf_token.clone());
            if public {
                secrets.extend(
                    state
                        .real_accounts
                        .accounts
                        .iter()
                        .chain(&state.mock_accounts.accounts)
                        .filter_map(|account| account.email.clone()),
                );
            }
        }
        redact_json(result, &secrets);
    }

    pub fn panel_snapshot(self: &Arc<Self>) -> Arc<PanelSnapshot> {
        self.request_refresh(false);
        self.cached_snapshot()
    }

    pub(crate) fn request_refresh(&self, force: bool) {
        if force {
            self.force_refresh.store(true, Ordering::Release);
        }
        let _ = self.refresh_signal.try_send(());
    }

    pub fn cached_snapshot(&self) -> Arc<PanelSnapshot> {
        self.snapshot.read().unwrap().clone()
    }

    pub fn snapshot_json(&self, since_rev: Option<u64>) -> Value {
        let entered = Instant::now();
        self.cached_snapshot().json(since_rev, entered)
    }

    fn publish(&self) {
        let _writer = self.snapshot_write.lock().unwrap();
        let (
            key,
            settings,
            accounts,
            ledger,
            sample,
            jobs,
            runs,
            plugins,
            hf_token,
            ledger_error,
            accounts_error,
        ) = {
            let state = self.state.lock().unwrap();
            let key = state.active_key();
            (
                key.clone(),
                state.settings.clone(),
                state.accounts(key.mode).clone(),
                state.ledger(key.mode).clone(),
                state.samples.get(&key).cloned().unwrap_or_default(),
                state
                    .jobs
                    .iter()
                    .filter(|j| j.account == key)
                    .map(|j| j.view.clone())
                    .collect::<Vec<_>>(),
                state
                    .runs
                    .iter()
                    .filter(|r| r.context.key == key)
                    .map(|r| r.info.clone())
                    .collect::<Vec<_>>(),
                state.plugins.clone(),
                state.hf_token.clone(),
                (key.mode == BackendMode::Real)
                    .then(|| state.real_ledger_error.clone())
                    .flatten(),
                state
                    .real_accounts_error
                    .as_ref()
                    .map(|error| error.message.clone()),
            )
        };
        let tunnel_revision = self.tunnels.revision();
        let tunnels = self
            .tunnels
            .list_for(&key.id, key.mode == BackendMode::Mock);
        self.published_tunnels
            .store(tunnel_revision, Ordering::Relaxed);
        let now = now_secs();
        let sampled_at = sample.sampled_at.unwrap_or(now);
        let usage = sample.usage.map(|u| {
            json!({
                "balance_cu": u.balance_cu, "projected_balance_cu": u.projected_balance(now),
                "rate_cu_per_hour": u.rate_cu_per_hour, "assignments": u.assignments,
                "eta_secs": u.eta_secs(now), "fetched_at_secs": u.fetched_at_secs,
                "sampled_at_epoch": u.fetched_at_secs, "stale": u.stale,
            })
        });
        let active = ledger.active_for_account(&key.id);
        let sessions = sample.live.iter().map(|s| {
            let row = active.iter().copied()
                .find(|t| t.endpoint.as_deref() == Some(s.endpoint.as_str()));
            let tracked = row.filter(|t| t.name != "[?]");
            let observed = ledger.observed_for(&key.id, &s.endpoint).filter(|_| tracked.is_none());
            let display = s.name.as_deref().or_else(|| tracked.map(|t| t.name.as_str()));
            let rate = tracked.map(|t| session_estimated_rate(t, &sample.rates, &ledger))
                .or_else(|| sample.rates.get(&s.endpoint).copied())
                .or_else(|| observed.and_then(|o| o.spend.last_share_cu_per_hour).map(|r| (r, false)));
            let spend = tracked.map(|t| &t.spend).or_else(|| observed.map(|o| &o.spend));
            let started_at = spend.map_or(sampled_at, |meter| meter.started_at_secs);
            let start_source = if tracked.is_some() {
                StartSource::Tracked
            } else {
                observed.map_or(StartSource::FirstSeen, |o| o.start_source)
            };
            let watchdog = tracked.and_then(|t| t.watchdog.as_ref()).map(|w| json!({
                "armed": w.armed, "cancelled": w.cancelled,
                "deadline_secs": w.deadline_secs, "deadline_epoch": w.deadline_secs,
                "remaining_secs": w.enforced_deadline_secs().map(|d| d.saturating_sub(sampled_at)),
                "idle_timeout_secs": w.idle_timeout_secs,
            }));
            json!({
                "display_name": display.unwrap_or("[?]"), "is_orphan": display.is_none_or(|name| name == "[?]"), "endpoint": s.endpoint,
                "hardware": s.hardware, "shape": s.shape, "gpu": tracked.and_then(|t| t.gpu.as_deref()),
                "vram_mib": tracked.and_then(|t| t.vram_mib), "started_at_epoch": started_at,
                "uptime_secs": sampled_at.saturating_sub(started_at), "start_source": start_source,
                "external_name": observed.and_then(|o| o.external_name.as_deref()),
                "rate_cu_per_hour": rate.map(|r| r.0), "rate_measured": rate.is_some_and(|r| r.1),
                "spend_cu": spend.and_then(SpendMeterState::spend), "spend_estimated": spend.is_none_or(|meter| meter.estimated),
                "watchdog": watchdog, "guard_decision": sample.decisions.get(&s.endpoint),
                "pending_stop": row.and_then(|t| t.pending_stop.as_ref()),
            })
        }).collect::<Vec<_>>();
        let pending_stops = active
            .into_iter()
            .filter_map(|s| {
                s.pending_stop.as_ref().map(
                    |stop| json!({ "session": s.name, "endpoint": s.endpoint, "status": stop, "is_orphan": s.is_orphan || s.name == "[?]" }),
                )
            })
            .collect::<Vec<_>>();
        let hardware_rates: BTreeMap<_, _> = Accelerator::ALL.into_iter().map(|acc| {
            let measured = ledger.hardware_rates.get(&LedgerData::hw_key(acc, false))
                .or_else(|| ledger.hardware_rates.get(&LedgerData::hw_key(acc, true)))
                .copied();
            (acc.as_str(), json!({ "cu_per_hour": measured.unwrap_or_else(|| published_rate(acc, false)), "measured": measured.is_some() }))
        }).collect();
        let mut ended: Vec<_> = ledger
            .sessions
            .iter()
            .rev()
            .filter(|row| row.account_id == key.id)
            .filter_map(|row| row.stopped.as_ref().map(|stop| (row, stop)))
            .collect();
        ended.sort_by_key(|(_, stop)| Reverse(stop.at_secs));
        let ended_sessions: Vec<_> = ended.into_iter().take(10).map(|(row, stop)| {
            let reason = match stop.reason {
                StopReason::User => "user",
                StopReason::Guard { .. } => "guard",
                StopReason::Provision => "provision",
                StopReason::Vanished => "vanished",
            };
            json!({
                "name": row.name, "hardware": row.hardware.as_deref().unwrap_or(row.accelerator.as_str()),
                "started_at_epoch": row.spend.started_at_secs, "ended_at_epoch": stop.at_secs,
                "duration_secs": row.uptime_secs(stop.at_secs), "spend_cu": row.spend.spend(), "stop_reason": reason,
            })
        }).collect();
        let private_emails = accounts
            .accounts
            .iter()
            .filter_map(|account| account.email.clone())
            .collect::<Vec<_>>();
        let mut payload = json!({
            "backend_mode": key.mode, "active_account": accounts.active(), "accounts": accounts.accounts,
            "usage": usage, "balance_history": sample.history.points(), "sessions": sessions,
            "hardware_rates": hardware_rates, "ended_sessions": ended_sessions,
            "sampled_at_epoch": sample.sampled_at, "guard_paused": sample.guard_paused || sample.sampled_at.is_none() || ledger_error.is_some() || (key.mode == BackendMode::Real && accounts_error.is_some()),
            "pending_stops": pending_stops, "provision_jobs": jobs, "tunnels": tunnels,
            "plugins": plugins.as_ref(), "plugin_runs": runs, "hf_token_set": hf_token.is_some(),
            "settings": settings, "settings_path": display_path(self.settings_store.path()), "env": sample.env,
            "ledger_error": ledger_error,
            "accounts_error": accounts_error,
            "last_error": if key.mode == BackendMode::Real { accounts_error.or(ledger_error).or(sample.error) } else { sample.error },
        });
        let secrets = hf_token.into_iter().collect::<Vec<_>>();
        redact_json(&mut payload, &secrets);
        let previous = self.cached_snapshot();
        if previous.payload == payload {
            return;
        }
        // A public tool result must not satisfy an app poll's revision check.
        let rev = previous.rev + 2;
        let mut public_payload = payload.clone();
        if let Some(active) = public_payload.get_mut("active_account") {
            make_account_public(active, &private_emails);
        }
        if let Some(accounts) = public_payload["accounts"].as_array_mut() {
            for account in accounts {
                make_account_public(account, &private_emails);
            }
        }
        redact_json(&mut public_payload, &private_emails);
        let summary = concise_summary(&public_payload);
        *self.snapshot.write().unwrap() = Arc::new(PanelSnapshot {
            rev,
            payload,
            public_payload,
            summary,
        });
    }

    fn record_sample_error(&self, ctx: &AccountContext, error: &Error) {
        self.state
            .lock()
            .unwrap()
            .samples
            .entry(ctx.key.clone())
            .or_default()
            .error = Some(error.message.clone());
        self.publish();
    }

    fn reserve(
        self: &Arc<Self>,
        context: &AccountContext,
        name: &str,
        max: u32,
    ) -> Result<Reservation> {
        let key = &context.key;
        let cancel = &self.shutdown;
        let lease = if key.mode == BackendMode::Real {
            // Check both the server's assignments and local claims. The network
            // requests finish before taking the ledger lock.
            self.ledger_store.load()?;
            let identity = self
                .real_backend
                .verified_env(&key.id, context.auth, cancel)?
                .identity;
            let sessions =
                context
                    .backend
                    .sessions(&key.id, context.auth, &context.state_file, cancel)?;
            Some(self.claim_real(context, name, identity, &sessions)?)
        } else {
            let live =
                context
                    .backend
                    .sessions(&key.id, context.auth, &context.state_file, cancel)?;
            let mut state = self.state.lock().unwrap();
            if state.mock_accounts.get(&key.id).is_none() {
                return Err(Error::invalid(
                    "account_id",
                    "account mismatch; refresh panel",
                ));
            }
            if state
                .mock_ledger
                .active_for_account(&key.id)
                .iter()
                .any(|row| row.name == name)
                || live.iter().any(|s| s.name.as_deref() == Some(name))
            {
                return Err(Error::invalid(
                    "name",
                    "session is already active or tracked",
                ));
            }
            let mut endpoints: BTreeSet<_> = live.iter().map(|row| row.endpoint.as_str()).collect();
            let mut occupied_names: BTreeSet<_> =
                live.iter().filter_map(|s| s.name.as_deref()).collect();
            let mut unknown = 0;
            for row in state.mock_ledger.active_for_account(&key.id) {
                occupied_names.insert(&row.name);
                if let Some(endpoint) = &row.endpoint {
                    endpoints.insert(endpoint);
                } else {
                    unknown += 1;
                }
            }
            let mut occupied = endpoints.len() + unknown;
            if let Some(names) = state.reservations.get(key) {
                occupied += names
                    .iter()
                    .filter(|name| !occupied_names.contains(name.as_str()))
                    .count();
            }
            if occupied >= max as usize {
                return Err(session_quota());
            }
            let names = state.reservations.entry(key.clone()).or_default();
            if !names.insert(name.to_owned()) {
                return Err(Error::invalid("name", "session name is already reserved"));
            }
            None
        };
        if lease.is_some() {
            self.state
                .lock()
                .unwrap()
                .reservations
                .entry(key.clone())
                .or_default()
                .insert(name.to_owned());
        }
        Ok(Reservation {
            coordinator: self.clone(),
            account: key.clone(),
            name: name.to_owned(),
            _lease: lease,
        })
    }

    fn claim_real(
        &self,
        context: &AccountContext,
        name: &str,
        identity: String,
        sessions: &[ServerSession],
    ) -> Result<SessionLease> {
        let key = &context.key;
        let mut endpoints: BTreeSet<_> = sessions.iter().map(|s| s.endpoint.clone()).collect();
        if sessions.iter().any(|s| s.name.as_deref() == Some(name)) {
            return Err(Error::invalid("name", "session is already active"));
        }
        self.update_ledger(key.mode, |ledger| {
            let persisted = self.settings_store.load_current()?;
            if self.force_mock != Some(false) {
                verify_backend_mode(Some(key.mode), persisted.backend_mode)?;
            }
            let max = persisted.defaults.max_concurrent_sessions;
            let registry = self.account_store.load()?;
            if registry.get(&key.id).is_none()
                || (context.auth == AuthMode::OAuth2 && registry.active_id != key.id)
                || (key.id == ADC_ACCOUNT_ID && context.auth != persisted.auth_mode)
            {
                return Err(Error::invalid(
                    "account_id",
                    "account mismatch; refresh panel",
                ));
            }
            let mut accounted = BTreeSet::new();
            let mut unknown = 0;
            for row in ledger.sessions.iter().filter(|row| row.is_active()) {
                if row.account_id != key.id
                    && self
                        .real_backend
                        .binding_identity(&row.account_id)?
                        .as_deref()
                        != Some(&identity)
                {
                    continue;
                }
                if row.name == name {
                    return Err(Error::invalid("name", "session is already tracked"));
                }
                accounted.insert((row.account_id.as_str(), row.name.as_str()));
                if let Some(endpoint) = &row.endpoint {
                    endpoints.insert(endpoint.clone());
                } else {
                    unknown += 1;
                }
            }
            let claims = self.ledger_store.active_claims()?;
            let mut outstanding = 0;
            for claim in claims.iter().filter(|claim| claim.identity == identity) {
                if claim.name == name {
                    return Err(Error::invalid("name", "session name is already reserved"));
                }
                if !accounted.contains(&(claim.account_id.as_str(), claim.name.as_str())) {
                    outstanding += 1;
                }
            }
            if endpoints.len() + unknown + outstanding >= max as usize {
                return Err(session_quota());
            }
            self.ledger_store.claim(&SessionClaim {
                account_id: key.id.clone(),
                identity,
                name: name.to_owned(),
            })
        })
    }

    fn session_name(&self, context: &AccountContext, accelerator: Accelerator) -> String {
        let state = self.state.lock().unwrap();
        let mut used = state
            .samples
            .get(&context.key)
            .into_iter()
            .flat_map(|s| &s.live)
            .filter_map(|s| s.name.clone())
            .collect::<Vec<_>>();
        used.extend(
            state
                .reservations
                .get(&context.key)
                .into_iter()
                .flatten()
                .cloned(),
        );
        generate_session_name(accelerator.as_str(), &used)
    }

    fn allocate(
        &self,
        context: &AccountContext,
        spec: &ProvisionSpec,
        policy: GuardPolicy,
        defaults: &Defaults,
        cancel: &CancelToken,
        on_event: &mut dyn FnMut(ProvisionEvent),
    ) -> Result<String> {
        let key = &context.key;
        let mut registration = None;
        let outcome = provision_with_tracking(
            context.backend.as_ref(),
            &key.id,
            context.auth,
            &context.state_file,
            &self.data_dir,
            spec,
            SessionLimit {
                max_concurrent: defaults.max_concurrent_sessions,
                reserved: 0,
            },
            cancel,
            &mut |update| {
                registration = self.update_ledger(key.mode, |ledger| {
                    if let AllocationUpdate::Started { started_at_secs, receipt, prior } = update {
                        if key.mode == BackendMode::Real {
                            let current = self.settings_store.load_current()?;
                            if self.force_mock != Some(false) {
                                verify_backend_mode(Some(key.mode), current.backend_mode)?;
                            }
                            if prior.len() >= current.defaults.max_concurrent_sessions as usize {
                                return Err(session_quota());
                            }
                        }
                        let mut row = tracked_session(
                            context, spec, policy.clone(), None, HardwareProbe::default(), started_at_secs,
                        );
                        row.allocation = Some(AllocationIntent {
                            receipt: receipt.map(str::to_owned),
                            prior_endpoints: prior.iter().map(|session| session.endpoint.clone()).collect(),
                        });
                        return Ok(Some(ledger.record_session(row).generation));
                    }

                    let Some(index) = ledger.sessions.iter().position(|row| {
                        row.account_id == key.id && Some(row.generation) == registration
                    }) else {
                        return Err(allocation_stopped());
                    };
                    match update {
                        AllocationUpdate::Assigned(endpoint) => {
                            if !ledger.sessions[index].is_active()
                                || ledger.sessions.iter().any(|row| {
                                    row.account_id == key.id && row.endpoint.as_deref() == Some(endpoint)
                                })
                            {
                                return Err(allocation_stopped());
                            }
                            let generation = ledger.next_generation();
                            let row = &mut ledger.sessions[index];
                            row.endpoint = Some(endpoint.to_owned());
                            row.generation = generation;
                            Ok(Some(generation))
                        }
                        AllocationUpdate::Unassigned => {
                            ledger.sessions.remove(index);
                            Ok(None)
                        }
                        AllocationUpdate::Released => {
                            let row = &mut ledger.sessions[index];
                            row.allocation = None;
                            row.pending_stop = None;
                            row.stopped.get_or_insert(StopRecord { at_secs: now_secs(), reason: StopReason::Provision });
                            Ok(None)
                        }
                        AllocationUpdate::Pending => {
                            let row = &mut ledger.sessions[index];
                            if row.is_active() {
                                let pending = row.pending_stop.get_or_insert_with(|| PendingStop::new(StopReason::Provision));
                                if row.endpoint.is_none() {
                                    pending.last_error = Some("allocation endpoint is unconfirmed; inspect the account before retrying".into());
                                } else {
                                    row.allocation = None;
                                }
                            }
                            Ok(registration)
                        }
                        AllocationUpdate::Started { .. } => unreachable!(),
                    }
                })?;
                Ok(())
            },
            &mut |event| {
                if !matches!(event, ProvisionEvent::Ready { .. }) {
                    on_event(event);
                }
            },
        )?;
        let endpoint = outcome.endpoint.ok_or_else(|| {
            Error::new(
                ErrorKind::OrphanSuspected,
                "no attributable runtime endpoint",
            )
        })?;
        self.with_endpoint(key, &endpoint, || {
            self.update_ledger(key.mode, |ledger| {
                let row = ledger
                    .target_mut(&key.id, &spec.name, Some(&endpoint))
                    .filter(|row| {
                        Some(row.generation) == registration && row.pending_stop.is_none()
                    })
                    .ok_or_else(allocation_stopped)?;
                row.gpu = outcome.hardware.gpu;
                row.vram_mib = outcome.hardware.vram_mib;
                row.below_spec = outcome.below_spec;
                Ok(())
            })?;
            {
                let mut state = self.state.lock().unwrap();
                let live = &mut state.samples.entry(key.clone()).or_default().live;
                if !live.iter().any(|s| s.endpoint == endpoint) {
                    live.push(ServerSession {
                        name: Some(spec.name.clone()),
                        endpoint: endpoint.clone(),
                        hardware: spec.accelerator.as_str().into(),
                        accelerator: Some(spec.accelerator),
                        shape: if spec.high_mem {
                            "HIGH_RAM"
                        } else {
                            "STANDARD"
                        }
                        .into(),
                        variant: String::new(),
                        status: None,
                    });
                }
            }
            let armed = (|| -> Result<()> {
                cancel.check()?;
                let watchdog = if policy.needs_watchdog() {
                    let mut remaining = policy;
                    let now = now_secs();
                    remaining.max_runtime_secs = remaining.max_runtime_secs.map(|limit| {
                        limit.saturating_sub(now.saturating_sub(outcome.started_at_secs))
                    });
                    let pin = self.pin_named_state(context, &spec.name, &endpoint)?;
                    let file = pinned_file(context, pin.as_ref());
                    Some(start_watchdog(
                        context.backend.as_ref(),
                        &key.id,
                        context.auth,
                        &file,
                        &spec.name,
                        &remaining,
                        defaults.watchdog_grace_secs,
                        cancel,
                    )?)
                } else {
                    None
                };
                cancel.check()?;
                self.update_ledger(key.mode, |ledger| {
                    let row = ledger
                        .target_mut(&key.id, &spec.name, Some(&endpoint))
                        .filter(|row| {
                            Some(row.generation) == registration && row.pending_stop.is_none()
                        })
                        .ok_or_else(allocation_stopped)?;
                    if let Some(watchdog) = &watchdog {
                        sync_watchdog_policy(row, watchdog);
                    }
                    row.allocation = None;
                    Ok(())
                })
            })();
            if let Err(error) = armed {
                let _ = self.attempt_stop(context, &spec.name, &endpoint, StopReason::Provision);
                return Err(error);
            }
            Ok(())
        })?;
        Ok(endpoint)
    }

    pub fn start_provision(self: &Arc<Self>, req: ProvisionRequest) -> Result<ProvisionJobView> {
        self.start_provision_in(self.active_context()?, req)
    }

    pub(crate) fn start_provision_in(
        self: &Arc<Self>,
        context: AccountContext,
        req: ProvisionRequest,
    ) -> Result<ProvisionJobView> {
        let defaults = self
            .state
            .lock()
            .unwrap()
            .settings_for(context.key.mode)
            .defaults
            .clone();
        let accelerator = req.accelerator.unwrap_or(defaults.accelerator);
        let name = req
            .name
            .unwrap_or_else(|| self.session_name(&context, accelerator));
        let spec = ProvisionSpec {
            name,
            accelerator,
            high_mem: req.high_mem.unwrap_or(defaults.high_mem),
            min_vram_mib: req
                .min_vram_mib
                .unwrap_or(defaults.min_vram_mib)
                .filter(|&m| m > 0 && accelerator.is_gpu()),
            max_attempts: req.max_attempts.unwrap_or(defaults.max_attempts),
            retry_delay_secs: defaults.retry_delay_secs,
            on_exhausted: OnExhausted::StopAndFail,
        };
        spec.validate()?;
        let policy = GuardPolicy {
            max_runtime_secs: req
                .max_runtime_secs
                .unwrap_or(defaults.guard.max_runtime_secs)
                .filter(|&secs| secs != 0),
            idle_timeout_secs: req
                .idle_timeout_secs
                .unwrap_or(defaults.guard.idle_timeout_secs)
                .filter(|&secs| secs != 0),
            max_spend_cu: req
                .max_spend_cu
                .unwrap_or(defaults.guard.max_spend_cu)
                .filter(|&cu| cu != 0.0),
            min_balance_cu: req.min_balance_cu.unwrap_or(defaults.guard.min_balance_cu),
        };
        policy.validate()?;
        let reserve = self.reserve(&context, &spec.name, defaults.max_concurrent_sessions)?;
        let view = ProvisionJobView {
            job_id: format!("prov-{}", self.next_seq.fetch_add(1, Ordering::Relaxed)),
            name: spec.name.clone(),
            accelerator,
            done: false,
            last_status: "Starting allocation…".into(),
            error: None,
        };
        let cancel = CancelToken::new();
        {
            let mut state = self.state.lock().unwrap();
            state
                .jobs
                .retain(|j| !j.view.done || j.account != context.key);
            state.jobs.push(ManagedJob {
                account: context.key.clone(),
                view: view.clone(),
                cancel: cancel.clone(),
            });
        }
        self.publish();
        let coord = self.clone();
        let id = view.job_id.clone();
        let spawn = thread::Builder::new()
            .name(format!("colab-{id}"))
            .spawn(move || {
                let _reservation = reserve;
                let result =
                    coord.allocate(&context, &spec, policy, &defaults, &cancel, &mut |event| {
                        coord.job_update(&id, format_prov_event(&event), None, false);
                    });
                match result {
                    Ok(_) => coord.job_update(&id, format!("Ready ({})", spec.name), None, true),
                    Err(e) => coord.job_update(
                        &id,
                        format!("Failed: {}", e.message),
                        Some(e.message),
                        true,
                    ),
                }
                coord.schedule_locked(coord.state.lock().unwrap(), context.key, true);
            });
        if let Err(e) = spawn {
            self.job_update(
                &view.job_id,
                "Allocation worker could not start".into(),
                Some(e.to_string()),
                true,
            );
            return Err(e.into());
        }
        Ok(view)
    }

    fn job_update(&self, id: &str, status: String, error: Option<String>, done: bool) {
        if let Some(job) = self
            .state
            .lock()
            .unwrap()
            .jobs
            .iter_mut()
            .find(|j| j.view.job_id == id)
        {
            job.view.last_status = status;
            job.view.error = error;
            job.view.done = done;
        }
        self.publish();
    }

    pub fn cancel_provision(&self, job_id: &str) -> bool {
        self.cancel_provision_checked(job_id, None, None)
            .unwrap_or(false)
    }

    pub(crate) fn cancel_provision_checked(
        &self,
        job_id: &str,
        mode: Option<BackendMode>,
        account: Option<&str>,
    ) -> Result<bool> {
        let state = self.state.lock().unwrap();
        verify_backend_mode(mode, state.settings.backend_mode)?;
        if let Some(job) = state.jobs.iter().find(|job| job.view.job_id == job_id) {
            verify_target_account(mode, account, &job.account)?;
            if !job.view.done {
                job.cancel.cancel();
                return Ok(true);
            }
        }
        Ok(false)
    }

    // The caller holds the endpoint lock across the stop and its verification.
    fn stop_endpoint(
        &self,
        context: &AccountContext,
        name: &str,
        endpoint: &str,
        request: StopRequest<'_>,
        live: Option<&ServerSession>,
    ) -> Result<StopResult> {
        let key = &context.key;
        let registered = self.update_ledger(key.mode, |ledger| match &request {
            StopRequest::Retry(target, manual) => Ok(ledger
                .sessions
                .iter()
                .find(|row| {
                    row.account_id == key.id
                        && row.is_active()
                        && row.generation == target.generation
                        && row.name == target.name
                        && row.endpoint.as_deref() == Some(endpoint)
                        && row
                            .pending_stop
                            .as_ref()
                            .is_some_and(|p| *manual || p.retry_at_epoch <= now_secs())
                })
                .map(|row| row.generation)),
            StopRequest::Start(reason, _) => {
                let observed = ledger.observed_for(&key.id, endpoint).cloned();
                let row = if let Some(row) = ledger.target_mut(&key.id, name, Some(endpoint)) {
                    row
                } else {
                    let spec = ProvisionSpec::from_defaults(name, &Defaults::default());
                    let mut row = tracked_session(
                        context,
                        &spec,
                        GuardPolicy::unlimited(),
                        Some(endpoint.to_owned()),
                        HardwareProbe::default(),
                        now_secs(),
                    );
                    if let Some(live) = live {
                        row.hardware = Some(live.hardware.clone());
                        row.accelerator = live.accelerator.unwrap_or(Accelerator::None);
                        row.high_mem = live.high_mem();
                    }
                    if let Some(observed) = observed {
                        row.hardware = observed.hardware.or(row.hardware);
                        row.spend = observed.spend;
                    }
                    ledger.record_session(row)
                };
                let user_requested = *reason == StopReason::User;
                let pending = row
                    .pending_stop
                    .get_or_insert_with(|| PendingStop::new(reason.clone()));
                if user_requested {
                    pending.reason = StopReason::User;
                }
                Ok(Some(row.generation))
            }
        });
        let (generation, mut warning) = match registered {
            Ok(Some(generation)) => (Some(generation), None),
            Ok(None) if request.manual_retry() => {
                return Err(Error::new(
                    ErrorKind::SessionGone,
                    "the original pending stop is no longer active; refresh panel",
                ));
            }
            Ok(None) => {
                return Ok(StopResult {
                    confirmed: false,
                    ledger_warning: None,
                })
            }
            Err(error) if request.is_manual() && live.is_some() => {
                (None, Some(self.note_ledger_warning(context, error)))
            }
            Err(error) => return Err(error),
        };

        self.tunnels
            .close_for_session(&key.id, name, Some(endpoint), context.backend.is_mock());
        let result = stop_and_confirm(
            context.backend.as_ref(),
            &key.id,
            context.auth,
            &context.state_file,
            &self.data_dir,
            Some(endpoint),
            &CancelToken::new(),
        );
        let confirmed = matches!(result, Ok(true));
        if let Err(error) = self.update_ledger(key.mode, |ledger| {
            let now = now_secs();
            let observed = confirmed
                .then(|| ledger.observed_for(&key.id, endpoint).cloned())
                .flatten();
            if let Some(row) = ledger.sessions.iter_mut().find(|row| {
                row.account_id == key.id
                    && row.is_active()
                    && generation == Some(row.generation)
                    && row.endpoint.as_deref() == Some(endpoint)
            }) {
                if let Some(mut pending) = row.pending_stop.take() {
                    if confirmed {
                        if let Some(observed) =
                            observed.filter(|_| row.is_orphan || row.name == "[?]")
                        {
                            row.hardware = observed.hardware.or(row.hardware.take());
                            row.spend = observed.spend;
                        }
                        row.stopped = Some(StopRecord {
                            at_secs: now,
                            reason: pending.reason,
                        });
                        row.allocation = None;
                    } else {
                        let message = result
                            .as_ref()
                            .err()
                            .map_or("runtime still listed after stop", |error| {
                                error.message.as_str()
                            });
                        pending.retry(now, redact_secrets(message, &[]));
                        row.pending_stop = Some(pending);
                    }
                }
            }
            if confirmed {
                ledger.deployments.remove(endpoint);
            }
            Ok(())
        }) {
            warning = Some(self.note_ledger_warning(context, error));
        }
        if confirmed {
            let mut state = self.state.lock().unwrap();
            for run in &mut state.runs {
                if run.context.key.mode == key.mode
                    && run.info.endpoint.as_deref() == Some(endpoint)
                    && run.active()
                    && !matches!(request, StopRequest::Start(_, Some(id)) if id == run.info.run_id)
                {
                    run.cancel.cancel();
                    run.info.state = "stopped".into();
                    run.info.message = "Runtime released".into();
                    run.pinned = None;
                }
            }
            for (sample_key, sample) in &mut state.samples {
                if sample_key.mode == key.mode {
                    sample.live.retain(|session| session.endpoint != endpoint);
                }
            }
        }
        self.publish();
        let ledger_warning = warning;
        match result {
            Ok(_) => Ok(StopResult {
                confirmed,
                ledger_warning,
            }),
            Err(error) => match ledger_warning {
                Some(warning) => Err(error.with("ledger_warning", warning)),
                None => Err(error.with("pending_stop", true)),
            },
        }
    }

    fn note_ledger_warning(&self, context: &AccountContext, error: Error) -> String {
        let message = redact_secrets(&error.message, &[]);
        if context.key.mode == BackendMode::Real {
            self.state.lock().unwrap().real_ledger_error = Some(message.clone());
        }
        message
    }

    fn attempt_stop(
        &self,
        ctx: &AccountContext,
        name: &str,
        endpoint: &str,
        reason: StopReason,
    ) -> Result<bool> {
        self.stop_endpoint(ctx, name, endpoint, StopRequest::Start(reason, None), None)
            .map(|stop| stop.confirmed)
    }

    fn retry_pending(
        &self,
        ctx: &AccountContext,
        target: &PendingTarget,
        manual: bool,
    ) -> Result<StopResult> {
        let endpoint = target.endpoint.as_deref().ok_or_else(|| {
            Error::new(ErrorKind::OrphanSuspected,
                "the original pending stop has no verified endpoint; inspect the account before retrying")
        })?;
        self.with_endpoint(&ctx.key, endpoint, || {
            let live = if manual && ctx.key.mode == BackendMode::Real {
                ctx.backend
                    .sessions(&ctx.key.id, ctx.auth, &ctx.state_file, &self.shutdown)
                    .ok()
                    .and_then(|sessions| sessions.into_iter().find(|row| row.endpoint == endpoint))
            } else {
                None
            };
            self.stop_endpoint(
                ctx,
                &target.name,
                endpoint,
                StopRequest::Retry(target, manual),
                live.as_ref(),
            )
        })
    }

    fn pending_target(
        &self,
        ctx: &AccountContext,
        session: Option<&str>,
        endpoint: Option<&str>,
    ) -> Result<Option<PendingTarget>> {
        if let Some(target) = &ctx.pending_stop {
            return Ok(Some(target.clone()));
        }
        if ctx.key.mode == BackendMode::Real {
            // If the ledger is unreadable, a fresh live match can still authorize
            // emergency stop of an explicit endpoint.
            if let Ok(ledger) = self.ledger_store.load() {
                return find_pending_stop(&ledger, &ctx.key.id, session, endpoint);
            }
        }
        let state = self.state.lock().unwrap();
        find_pending_stop(state.ledger(ctx.key.mode), &ctx.key.id, session, endpoint)
    }

    pub fn stop_session(self: &Arc<Self>, name: &str) -> Result<bool> {
        let context = self.pin_action_context(Some(name), None, None, None, true)?;
        self.stop_session_in(context, name)
            .map(|result| result.confirmed)
    }

    pub(crate) fn stop_session_in(
        self: &Arc<Self>,
        context: AccountContext,
        name: &str,
    ) -> Result<StopResult> {
        let key = &context.key;
        let hinted = context.hinted_endpoint(name);
        let explicit = hinted.filter(|_| context.explicit_endpoint);
        let result = if let Some(target) = self.pending_target(&context, Some(name), explicit)? {
            self.retry_pending(&context, &target, true)
        } else if name == "[?]" {
            let endpoint = explicit
                .ok_or_else(|| {
                    Error::invalid("endpoint", "an orphan stop requires a verified endpoint")
                })?
                .to_owned();
            return self.reap_orphan_in(context, Some(name), &endpoint);
        } else {
            validate_session_name(name)?;
            let live = context.backend.sessions(
                &key.id,
                context.auth,
                &context.state_file,
                &self.shutdown,
            );
            let known = |endpoint| self.known_endpoint(&context, name, endpoint);
            let (endpoint, verified) = match live {
                Ok(live) => {
                    let named = live.iter().find(|s| s.name.as_deref() == Some(name));
                    let endpoint = match (hinted, named) {
                        (Some(expected), Some(named)) => {
                            require_same_endpoint(expected, &named.endpoint)?;
                            expected
                        }
                        (None, Some(named)) => &named.endpoint,
                        (Some(expected), None)
                            if known(expected)
                                && live
                                    .iter()
                                    .all(|row| row.endpoint != expected || row.name.is_none()) =>
                        {
                            expected
                        }
                        _ => return Err(session_not_found()),
                    };
                    let verified = live.iter().find(|row| row.endpoint == endpoint).cloned();
                    (endpoint.to_owned(), verified)
                }
                Err(error) => match hinted {
                    Some(endpoint) if known(endpoint) => (endpoint.to_owned(), None),
                    _ => return Err(error),
                },
            };
            self.with_endpoint(key, &endpoint, || {
                if !context.explicit_endpoint && context.pending_stop.is_none() {
                    if let Some(target) = self.pending_target(&context, Some(name), None)? {
                        if target.endpoint.as_deref() != Some(&endpoint) {
                            return Err(Error::new(ErrorKind::SessionGone, "a pending stop refers to a different or unknown endpoint; refresh panel"));
                        }
                        return self.stop_endpoint(&context, &target.name, &endpoint, StopRequest::Retry(&target, true), verified.as_ref());
                    }
                }
                self.stop_endpoint(&context, name, &endpoint, StopRequest::Start(StopReason::User, None), verified.as_ref())
            })
        };
        self.schedule_locked(self.state.lock().unwrap(), key.clone(), true);
        result
    }

    pub fn reap_orphan(self: &Arc<Self>, endpoint: &str) -> Result<bool> {
        self.reap_orphan_in(self.active_context()?, None, endpoint)
            .map(|result| result.confirmed)
    }

    pub(crate) fn reap_orphan_in(
        self: &Arc<Self>,
        context: AccountContext,
        name: Option<&str>,
        endpoint: &str,
    ) -> Result<StopResult> {
        if !valid_endpoint(endpoint) {
            return Err(Error::invalid("endpoint", "invalid endpoint"));
        }
        let key = &context.key;
        let result = if let Some(target) = self.pending_target(&context, name, Some(endpoint))? {
            self.retry_pending(&context, &target, true)
        } else {
            self.with_endpoint(key, endpoint, || {
                let live = context.backend.sessions(
                    &key.id,
                    context.auth,
                    &context.state_file,
                    &self.shutdown,
                )?;
                if let Some(named) =
                    name.and_then(|name| live.iter().find(|s| s.name.as_deref() == Some(name)))
                {
                    require_same_endpoint(endpoint, &named.endpoint)?;
                }
                let orphan = live
                    .iter()
                    .find(|s| s.name.is_none() && s.endpoint == endpoint)
                    .ok_or_else(|| {
                        Error::invalid(
                            "endpoint",
                            "endpoint is not an unmanaged runtime for this account",
                        )
                    })?;
                self.stop_endpoint(
                    &context,
                    "[?]",
                    endpoint,
                    StopRequest::Start(StopReason::User, None),
                    Some(orphan),
                )
            })
        };
        self.schedule_locked(self.state.lock().unwrap(), key.clone(), true);
        result
    }

    fn known_endpoint(&self, ctx: &AccountContext, name: &str, endpoint: &str) -> bool {
        let key = &ctx.key;
        let state = self.state.lock().unwrap();
        state.samples.get(key).is_some_and(|sample| {
            sample
                .live
                .iter()
                .any(|s| s.endpoint == endpoint && s.name.as_deref() == Some(name))
        }) || state.ledger(key.mode).sessions.iter().any(|row| {
            row.account_id == key.id
                && row.name == name
                && row.endpoint.as_deref() == Some(endpoint)
        }) || (!ctx.backend.is_mock()
            && read_session(&ctx.state_file, name)
                .is_some_and(|row| row["endpoint"].as_str() == Some(endpoint)))
    }

    fn pin_named_state(
        &self,
        ctx: &AccountContext,
        name: &str,
        endpoint: &str,
    ) -> Result<Option<PrivateDir>> {
        if ctx.backend.is_mock() {
            let current = self.live_endpoint(ctx, name, &self.shutdown)?;
            require_same_endpoint(endpoint, &current)?;
            Ok(None)
        } else {
            pin_session(&ctx.state_file, name, Some(endpoint)).map(Some)
        }
    }

    pub fn restart_kernel(&self, name: &str) -> Result<()> {
        self.restart_kernel_in(self.pin_context(Some(name))?, name)
    }

    pub(crate) fn restart_kernel_in(&self, ctx: AccountContext, name: &str) -> Result<()> {
        let cancel = &self.shutdown;
        let endpoint = self.live_endpoint(&ctx, name, cancel)?;
        self.with_endpoint(&ctx.key, &endpoint, || {
            let pin = self.pin_named_state(&ctx, name, &endpoint)?;
            let file = pinned_file(&ctx, pin.as_ref());
            ctx.backend
                .restart_kernel(&ctx.key.id, ctx.auth, &file, name, cancel)?;
            if pin.is_some() {
                merge_pinned_session(&ctx.state_file, &file, name, &endpoint)?;
            }
            Ok(())
        })
    }

    pub fn session_url(&self, name: &str) -> Result<String> {
        self.session_url_in(self.pin_context(Some(name))?, name)
    }

    pub(crate) fn session_url_in(&self, ctx: AccountContext, name: &str) -> Result<String> {
        let cancel = &self.shutdown;
        let endpoint = self.live_endpoint(&ctx, name, cancel)?;
        self.with_endpoint(&ctx.key, &endpoint, || {
            let pin = self.pin_named_state(&ctx, name, &endpoint)?;
            ctx.backend.session_url(
                &ctx.key.id,
                ctx.auth,
                &pinned_file(&ctx, pin.as_ref()),
                name,
                cancel,
            )
        })
    }

    pub fn watchdog_action(
        &self,
        name: &str,
        action: &str,
        hours: Option<f64>,
    ) -> Result<WatchdogState> {
        self.watchdog_action_in(self.pin_context(Some(name))?, name, action, hours)
    }

    pub(crate) fn watchdog_action_in(
        &self,
        ctx: AccountContext,
        name: &str,
        action: &str,
        hours: Option<f64>,
    ) -> Result<WatchdogState> {
        if !matches!(action, "extend" | "cancel" | "arm") {
            return Err(Error::invalid("action", "unknown watchdog action"));
        }
        if hours.is_some_and(|h| !h.is_finite() || !(0.25..=24.0).contains(&h)) {
            return Err(Error::invalid("hours", "expected 0.25–24 hours"));
        }
        let cancel = &self.shutdown;
        let endpoint = self.live_endpoint(&ctx, name, cancel)?;
        let defaults = self
            .state
            .lock()
            .unwrap()
            .settings_for(ctx.key.mode)
            .defaults
            .clone();
        self.with_endpoint(&ctx.key, &endpoint, || {
            let pin = self.pin_named_state(&ctx, name, &endpoint)?;
            let file = &pinned_file(&ctx, pin.as_ref());
            let backend = ctx.backend.as_ref();
            let account = &ctx.key.id;
            let (previous, idle_timeout_secs) = self.update_ledger(ctx.key.mode, |ledger| {
                let row = ledger.target_mut(account, name, Some(&endpoint));
                if let Some(row) = row {
                    let previous = row.watchdog.clone();
                    if let Some(watchdog) = &mut row.watchdog {
                        // A process exit during a remote update must not leave a stale armed flag.
                        watchdog.armed = false;
                    }
                    Ok((previous, row.policy.idle_timeout_secs))
                } else {
                    Ok((None, None))
                }
            })?;
            let start = |policy: &GuardPolicy| {
                start_watchdog(
                    backend,
                    account,
                    ctx.auth,
                    file,
                    name,
                    policy,
                    defaults.watchdog_grace_secs,
                    cancel,
                )
            };
            let result = match action {
                "extend" => {
                    let remaining = previous
                        .as_ref()
                        .and_then(WatchdogState::enforced_deadline_secs)
                        .map_or(0, |deadline| deadline.saturating_sub(now_secs()))
                        .saturating_add((hours.unwrap_or(2.0) * 3600.0) as u64);
                    extend_watchdog(backend, account, ctx.auth, file, name, remaining, cancel)
                        .and_then(|watchdog| {
                            if watchdog.armed {
                                Ok(watchdog)
                            } else {
                                start(&GuardPolicy {
                                    max_runtime_secs: Some(remaining),
                                    idle_timeout_secs,
                                    ..GuardPolicy::unlimited()
                                })
                            }
                        })
                }
                "cancel" => cancel_watchdog(backend, account, ctx.auth, file, name, cancel),
                "arm" => {
                    let mut policy = defaults.guard;
                    if let Some(hours) = hours {
                        policy.max_runtime_secs = Some((hours * 3600.0) as u64);
                    }
                    start(&policy)
                }
                _ => unreachable!(),
            }
            .and_then(|watchdog| {
                if action != "cancel" && !watchdog.armed {
                    return Err(Error::new(
                        ErrorKind::Other,
                        "watchdog did not confirm that it is armed",
                    ));
                }
                Ok(watchdog)
            });
            let observed = match &result {
                Ok(watchdog) => Some(watchdog.clone()),
                Err(_) => watchdog_status(backend, account, ctx.auth, file, name, cancel)
                    .ok()
                    .or_else(|| {
                        previous.map(|mut watchdog| {
                            watchdog.armed = false;
                            watchdog
                        })
                    }),
            };
            self.update_ledger(ctx.key.mode, |ledger| {
                let row = match ledger.target_mut(account, name, Some(&endpoint)) {
                    Some(row) => row,
                    None if result.is_ok() => {
                        let spec = ProvisionSpec::from_defaults(name, &Defaults::default());
                        ledger.record_session(tracked_session(
                            &ctx,
                            &spec,
                            GuardPolicy::unlimited(),
                            Some(endpoint.clone()),
                            HardwareProbe::default(),
                            now_secs(),
                        ));
                        ledger.sessions.last_mut().unwrap()
                    }
                    None => return Ok(()),
                };
                if let Some(watchdog) = &observed {
                    if result.is_ok() || watchdog.armed || watchdog.cancelled {
                        sync_watchdog_policy(row, watchdog);
                    } else {
                        row.watchdog = Some(watchdog.clone());
                    }
                }
                Ok(())
            })?;
            self.publish();
            result
        })
    }

    fn live_endpoint(
        &self,
        ctx: &AccountContext,
        name: &str,
        cancel: &CancelToken,
    ) -> Result<String> {
        self.live_named_session(ctx, name, cancel)
            .map(|session| session.endpoint)
    }

    fn live_named_session(
        &self,
        ctx: &AccountContext,
        name: &str,
        cancel: &CancelToken,
    ) -> Result<ServerSession> {
        validate_session_name(name)?;
        let session = ctx
            .backend
            .sessions(&ctx.key.id, ctx.auth, &ctx.state_file, cancel)?
            .into_iter()
            .find(|s| s.name.as_deref() == Some(name))
            .ok_or_else(session_not_found)?;
        if let Some(expected) = ctx.hinted_endpoint(name) {
            require_same_endpoint(expected, &session.endpoint)?;
        }
        Ok(session)
    }

    fn open_pinned_tunnel(&self, ctx: &AccountContext, spec: TunnelSpec) -> Result<TunnelInfo> {
        let bin = ctx
            .backend
            .colab_binary()
            .map(|p| p.to_string_lossy().into_owned());
        let credentials = if ctx.backend.is_mock() {
            None
        } else {
            Some(
                self.real_backend
                    .verified_env(&ctx.key.id, ctx.auth, &self.shutdown)?,
            )
        };
        let result = self.tunnels.open_with_env(
            spec,
            ctx.backend.is_mock(),
            bin,
            ctx.state_file.clone(),
            credentials,
        );
        self.publish();
        result
    }

    pub fn open_tunnel(
        &self,
        session: &str,
        local_port: u16,
        remote_port: u16,
        health_path: Option<String>,
    ) -> Result<TunnelInfo> {
        self.open_tunnel_in(
            self.pin_context(Some(session))?,
            session,
            local_port,
            remote_port,
            health_path,
        )
    }

    pub(crate) fn open_tunnel_in(
        &self,
        ctx: AccountContext,
        session: &str,
        local_port: u16,
        remote_port: u16,
        health_path: Option<String>,
    ) -> Result<TunnelInfo> {
        let mut spec = TunnelSpec {
            account_id: ctx.key.id.clone(),
            auth_mode: ctx.auth,
            session: session.to_owned(),
            endpoint: None,
            local_port,
            remote_port,
            health_path,
        };
        spec.validate()?;
        let endpoint = self.live_endpoint(&ctx, session, &self.shutdown)?;
        spec.endpoint = Some(endpoint.clone());
        self.with_endpoint(&ctx.key, &endpoint, || self.open_pinned_tunnel(&ctx, spec))
    }

    pub fn close_tunnel(&self, id: u64) -> bool {
        let closed = self.tunnels.close(id);
        self.publish();
        closed
    }

    pub fn dismiss_tunnel(&self, id: u64) -> bool {
        let dismissed = self.tunnels.dismiss(id);
        if dismissed {
            self.publish();
        }
        dismissed
    }

    pub fn list_tunnels(&self) -> Vec<TunnelInfo> {
        let key = self.state.lock().unwrap().active_key();
        self.tunnels
            .list_for(&key.id, key.mode == BackendMode::Mock)
    }

    pub fn update_settings(self: &Arc<Self>, mut patch: SettingsPatch) -> Result<Settings> {
        let _write = self.config_write.lock().unwrap();
        let previous = self.state.lock().unwrap().settings.clone();
        if let Some(mock) = self.force_mock {
            let forced = if mock {
                BackendMode::Mock
            } else {
                BackendMode::Real
            };
            if patch.backend_mode.is_some_and(|mode| mode != forced) {
                return Err(Error::invalid(
                    "backend_mode",
                    "backend mode was fixed at startup",
                ));
            }
            patch.backend_mode = None;
        }
        let mode = patch.backend_mode.unwrap_or(previous.backend_mode);
        let (updated, real_settings, accounts) = if previous.backend_mode == BackendMode::Mock
            && mode == BackendMode::Mock
        {
            (previous.clone().patched(patch)?, None, None)
        } else {
            let (updated, real, accounts) = self.update_ledger(BackendMode::Real, |ledger| {
                let persisted = self.settings_store.load_current()?;
                if mode == BackendMode::Mock {
                    if ledger.sessions.iter().any(TrackedSession::is_active)
                        || !ledger.deployments.is_empty() || !self.ledger_store.active_claims()?.is_empty()
                    {
                        return Err(Error::invalid("backend_mode", "stop real runtimes and finish pending allocations before saving mock mode"));
                    }
                    let mock = persisted.clone().patched(patch)?;
                    let durable = Settings { backend_mode: BackendMode::Mock, ..persisted };
                    self.settings_store.save(&durable)?;
                    Ok((mock, durable, None))
                } else {
                    let next = if previous.backend_mode == BackendMode::Mock {
                        // A switch out of mock restores the real defaults from disk.
                        Settings { backend_mode: BackendMode::Real, ..persisted.clone() }
                    } else {
                        persisted.clone().patched(patch)?
                    };
                    if next.auth_mode != persisted.auth_mode
                        && (!ledger.active_for_account(ADC_ACCOUNT_ID).is_empty()
                            || self.ledger_store.active_claims()?.iter().any(|claim| claim.account_id == ADC_ACCOUNT_ID))
                    {
                        return Err(Error::invalid("auth_mode", "stop tracked ADC-profile runtimes before changing their authentication mode"));
                    }
                    let accounts = if previous.backend_mode == BackendMode::Mock {
                        Some(self.account_store.load()?)
                    } else {
                        None
                    };
                    self.settings_store.save(&next)?;
                    let visible = Settings { backend_mode: mode, ..next.clone() };
                    Ok((visible, next, accounts))
                }
            })?;
            (updated, Some(real), accounts)
        };
        if updated.backend_mode != previous.backend_mode {
            self.tunnels.close_all();
        }
        {
            let mut state = self.state.lock().unwrap();
            if let Some(real) = real_settings {
                if real.auth_mode != state.real_settings.auth_mode {
                    self.real_backend
                        .invalidate_credentials(Some(ADC_ACCOUNT_ID));
                }
                state.real_settings = real;
            }
            if let Some(accounts) = accounts {
                state.real_accounts = accounts;
                state.real_accounts_error = None;
                state.real_guard_enabled = true;
            }
            state.settings = updated.clone();
            state.config_changed();
        }
        self.restore_deploys();
        self.publish();
        self.schedule_active(false);
        Ok(updated)
    }

    pub fn preview_account_switch(&self) -> Value {
        let state = self.state.lock().unwrap();
        let key = state.active_key();
        let live = state
            .samples
            .get(&key)
            .map_or(&[][..], |sample| sample.live.as_slice());
        let ledger = state.ledger(key.mode);
        let managed = ledger.active_for_account(&key.id);
        let now = now_secs();
        let mut watched = Vec::new();
        let mut unwatched = Vec::new();
        for row in managed.iter().filter(|row| row.name != "[?]") {
            let mut entry = json!({
                "name": row.name, "hardware": row.accelerator.as_str(), "endpoint": row.endpoint,
            });
            let deadline = row
                .watchdog
                .as_ref()
                .and_then(WatchdogState::enforced_deadline_secs);
            let idle = row
                .watchdog
                .as_ref()
                .and_then(WatchdogState::enforced_idle_timeout_secs);
            if deadline.is_some() || idle.is_some() {
                entry["remaining_secs"] = json!(deadline.map(|at| at.saturating_sub(now)));
                watched.push(entry);
            } else {
                unwatched.push(entry);
            }
        }
        let orphans: Vec<_> = live.iter().filter(|session| {
            !managed.iter().any(|row| {
                row.name != "[?]" && row.endpoint.as_deref() == Some(session.endpoint.as_str())
            })
        }).map(|session| {
            json!({
                "endpoint": session.endpoint, "hardware": session.hardware,
                "external_name": session.name.as_deref().or_else(|| ledger.observed_for(&key.id, &session.endpoint).and_then(|o| o.external_name.as_deref())),
            })
        }).collect();
        let closing: Vec<_> = self
            .tunnels
            .list()
            .into_iter()
            .filter(|tunnel| !tunnel.state.is_terminal())
            .map(|tunnel| {
                json!({
                    "id": tunnel.id, "local_url": tunnel.local_url,
                    "session": tunnel.spec.session, "remote_port": tunnel.spec.remote_port,
                })
            })
            .collect();
        json!({
            "open_tunnels": closing.len(), "active_sessions": live.len(), "closing_tunnels": closing,
            "watched_sessions": watched, "unwatched_sessions": unwatched, "orphan_sessions": orphans,
        })
    }

    pub fn switch_account(self: &Arc<Self>, account_id: &str) -> Result<AccountRegistry> {
        let _config = self.config_write.lock().unwrap();
        let (mode, default_auth) = {
            let state = self.state.lock().unwrap();
            (state.settings.backend_mode, state.settings.auth_mode)
        };
        let registry = if mode == BackendMode::Mock {
            let mut state = self.state.lock().unwrap();
            state.mock_accounts.switch(account_id)?;
            state.mock_accounts.clone()
        } else {
            let registry = self.account_store.load()?;
            let auth_of = |profile: &AccountProfile| {
                if profile.id == ADC_ACCOUNT_ID {
                    default_auth
                } else {
                    profile.auth_mode
                }
            };
            let current = registry.active().ok_or_else(account_mismatch)?;
            let target = registry
                .get(account_id)
                .ok_or_else(|| Error::invalid("account_id", "account is not registered"))?;
            let leaving_oauth = current.id != account_id && auth_of(current) == AuthMode::OAuth2;
            if leaving_oauth {
                self.check_live_oauth_departure(&current.id)?;
            }
            if auth_of(target) == AuthMode::OAuth2 {
                self.real_backend
                    .verified_env(account_id, AuthMode::OAuth2, &self.shutdown)?;
            }
            let saved = self.update_ledger(mode, |ledger| {
                let mut latest = self.account_store.load()?;
                if latest.active_id != registry.active_id
                    || latest.get(account_id).is_none_or(|profile| {
                        profile.auth_mode != target.auth_mode || profile.email != target.email
                    })
                    || latest.active().is_none_or(|profile| {
                        profile.auth_mode != current.auth_mode || profile.email != current.email
                    })
                {
                    return Err(Error::invalid(
                        "account_id",
                        "account registry changed; refresh panel",
                    ));
                }
                if leaving_oauth {
                    self.check_oauth_departure_claims(ledger, &current.id)?;
                }
                latest.switch(account_id)?;
                self.account_store.save(&latest)?;
                Ok(latest)
            })?;
            self.real_backend.invalidate_credentials(None);
            saved
        };
        self.tunnels.close_all();
        self.set_accounts(mode, registry.clone());
        Ok(registry)
    }

    fn check_live_oauth_departure(&self, account_id: &str) -> Result<()> {
        if !self
            .ledger_store
            .load()?
            .active_for_account(account_id)
            .is_empty()
        {
            return Err(oauth_in_use());
        }
        let file = self.account_store.cli_state_file(account_id);
        if !self
            .real_backend
            .sessions(account_id, AuthMode::OAuth2, &file, &self.shutdown)?
            .is_empty()
        {
            return Err(oauth_in_use());
        }
        Ok(())
    }

    fn check_oauth_departure_claims(&self, ledger: &LedgerData, account_id: &str) -> Result<()> {
        if !ledger.active_for_account(account_id).is_empty()
            || self
                .ledger_store
                .active_claims()?
                .iter()
                .any(|claim| claim.account_id == account_id)
        {
            return Err(oauth_in_use());
        }
        Ok(())
    }

    pub fn add_account(self: &Arc<Self>, profile: AccountProfile) -> Result<AccountRegistry> {
        let _config = self.config_write.lock().unwrap();
        let mode = self.state.lock().unwrap().settings.backend_mode;
        let add = |registry: &mut AccountRegistry| {
            if let Some(existing) = registry.get(&profile.id) {
                if existing.auth_mode != profile.auth_mode || existing.email != profile.email {
                    return Err(Error::invalid(
                        "account_id",
                        "credential bindings are immutable for existing account IDs",
                    ));
                }
            }
            registry.upsert(profile, false)
        };
        let registry = if mode == BackendMode::Mock {
            let mut state = self.state.lock().unwrap();
            add(&mut state.mock_accounts)?;
            state.mock_accounts.clone()
        } else {
            self.update_ledger(mode, |_| {
                let mut registry = self.account_store.load()?;
                add(&mut registry)?;
                self.account_store.save(&registry)?;
                Ok(registry)
            })?
        };
        self.set_accounts(mode, registry.clone());
        Ok(registry)
    }

    pub fn remove_account(self: &Arc<Self>, account_id: &str) -> Result<AccountRegistry> {
        let _config = self.config_write.lock().unwrap();
        let in_use = || {
            Error::invalid(
                "account_id",
                "stop tracked runtimes and plugin runs before removing their account",
            )
        };
        let (mode, default_auth) =
            {
                let state = self.state.lock().unwrap();
                let mode = state.settings.backend_mode;
                if state.reservations.iter().any(|(key, names)| {
                    key.mode == mode && key.id == account_id && !names.is_empty()
                }) || state.runs.iter().any(|r| {
                    r.context.key.mode == mode && r.context.key.id == account_id && r.active()
                }) {
                    return Err(in_use());
                }
                (mode, state.real_settings.auth_mode)
            };
        let registry = if mode == BackendMode::Mock {
            let mut state = self.state.lock().unwrap();
            if !state.mock_ledger.active_for_account(account_id).is_empty() {
                return Err(in_use());
            }
            state.mock_accounts.remove(account_id)?;
            state.mock_accounts.clone()
        } else {
            let previous = self.account_store.load()?;
            let target = previous
                .get(account_id)
                .ok_or_else(|| Error::invalid("account_id", "account is not registered"))?;
            let mut next = previous.clone();
            next.remove(account_id)?;
            let auth = if account_id == ADC_ACCOUNT_ID {
                default_auth
            } else {
                target.auth_mode
            };
            let leaving_oauth = previous.active_id == account_id && auth == AuthMode::OAuth2;
            if leaving_oauth {
                self.check_live_oauth_departure(account_id)?;
            }
            let registry = self.update_ledger(mode, |ledger| {
                let mut registry = self.account_store.load()?;
                if registry.active_id != previous.active_id
                    || registry.get(account_id) != Some(target)
                {
                    return Err(Error::invalid(
                        "account_id",
                        "account registry changed; refresh panel",
                    ));
                }
                if leaving_oauth {
                    self.check_oauth_departure_claims(ledger, account_id)?;
                }
                if !ledger.active_for_account(account_id).is_empty()
                    || ledger.deployments.values().any(|deployment| {
                        deployment
                            .runs
                            .values()
                            .any(|run| run.account_id == account_id)
                    })
                    || self
                        .ledger_store
                        .active_claims()?
                        .iter()
                        .any(|claim| claim.account_id == account_id)
                {
                    return Err(in_use());
                }
                registry.remove(account_id)?;
                self.account_store.save(&registry)?;
                Ok(registry)
            })?;
            if registry.active_id != previous.active_id {
                self.tunnels.close_all();
            }
            self.real_backend.invalidate_credentials(Some(account_id));
            registry
        };
        self.set_accounts(mode, registry.clone());
        Ok(registry)
    }

    fn set_accounts(self: &Arc<Self>, mode: BackendMode, registry: AccountRegistry) {
        {
            let mut state = self.state.lock().unwrap();
            match mode {
                BackendMode::Mock => state.mock_accounts = registry,
                BackendMode::Real => {
                    state.real_accounts = registry;
                    state.real_accounts_error = None;
                }
            }
            state.config_changed();
        }
        self.publish();
        self.schedule_active(false);
    }

    pub fn set_hf_token(&self, token: String) {
        let token = token.trim();
        self.state.lock().unwrap().hf_token = (!token.is_empty()).then(|| token.to_owned());
        self.publish();
    }

    pub fn search_hf(&self, query: &str) -> Result<Vec<HfModel>> {
        let (mock, token) = self.hf_context();
        search_gguf_models(&self.host, query, 12, token.as_deref(), mock)
    }

    pub fn inspect_hf(
        &self,
        repo: &str,
        accelerator: Accelerator,
    ) -> Result<(Vec<FitChoice>, Value)> {
        let (mock, token) = self.hf_context();
        let listing = list_gguf_files(&self.host, repo, token.as_deref(), mock)?;
        let fits = preview_for_accelerator(&listing, accelerator, "auto", 0);
        Ok((fits, serde_json::to_value(listing)?))
    }

    fn hf_context(&self) -> (bool, Option<String>) {
        let state = self.state.lock().unwrap();
        (
            state.settings.backend_mode == BackendMode::Mock,
            state.hf_token.clone(),
        )
    }

    pub fn list_plugins(&self) -> Arc<Vec<InstalledPlugin>> {
        self.state.lock().unwrap().plugins.clone()
    }

    fn reload_plugins(&self) {
        let plugins = Arc::new(self.plugin_store.list());
        self.state.lock().unwrap().plugins = plugins;
        self.publish();
    }

    pub fn install_plugin_dir(&self, path: &Path) -> Result<InstalledPlugin> {
        let _write = self.config_write.lock().unwrap();
        let installed = self.plugin_store.install_from_dir(path)?;
        self.reload_plugins();
        Ok(installed)
    }

    pub fn install_plugin_git(&self, url: &str) -> Result<InstalledPlugin> {
        let _write = self.config_write.lock().unwrap();
        if self.state.lock().unwrap().settings.backend_mode == BackendMode::Mock {
            return Err(Error::invalid(
                "action",
                "Git installation requires real backend mode",
            ));
        }
        let installed = self.plugin_store.install_from_git(&self.host, url)?;
        self.reload_plugins();
        Ok(installed)
    }

    pub fn remove_plugin(&self, id: &str) -> Result<bool> {
        let _write = self.config_write.lock().unwrap();
        let removed = self.plugin_store.remove(id)?;
        if removed {
            self.reload_plugins();
        }
        Ok(removed)
    }

    fn restore_deploys(&self) {
        let _registration = self.deploy_registration.lock().unwrap();
        let mut state = self.state.lock().unwrap();
        let restored = state
            .real_ledger
            .deployments
            .iter()
            .flat_map(|(endpoint, deployment)| {
                deployment
                    .runs
                    .iter()
                    .filter_map(|(id, lease)| {
                        let meta = lease.metadata.as_ref()?;
                        if !valid_endpoint(endpoint)
                            || validate_plugin_id(id).is_err()
                            || validate_session_name(&meta.session).is_err()
                            || state.runs.iter().any(|run| run.info.run_id == *id)
                        {
                            return None;
                        }
                        let key = AccountKey {
                            mode: BackendMode::Real,
                            id: lease.account_id.clone(),
                        };
                        let context = self.context(&state, key)?;
                        Some(ManagedRun {
                            context,
                            info: PluginRunInfo {
                                run_id: id.clone(),
                                plugin_id: meta.plugin_id.clone(),
                                session: meta.session.clone(),
                                created: meta.created,
                                endpoint: Some(endpoint.clone()),
                                state: "running".into(),
                                stage: None,
                                progress: None,
                                message: "Recovered deployment; local tunnels need to be reopened"
                                    .into(),
                                outputs: BTreeMap::new(),
                                log_tail: Vec::new(),
                                endpoints: Vec::new(),
                                started_at_secs: meta.started_at_secs,
                            },
                            cancel: CancelToken::new(),
                            pinned: None,
                            worker_active: false,
                            needs_recovery_check: true,
                        })
                    })
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
        state.runs.extend(restored);
    }

    fn sync_completed_leases(&self) {
        let mut state = self.state.lock().unwrap();
        let CoordinatorState {
            real_ledger,
            mock_ledger,
            runs,
            ..
        } = &mut *state;
        for run in runs
            .iter_mut()
            .filter(|run| run.active() && !run.worker_active)
        {
            let Some(endpoint) = &run.info.endpoint else {
                continue;
            };
            let ledger = match run.context.key.mode {
                BackendMode::Mock => &*mock_ledger,
                BackendMode::Real => &*real_ledger,
            };
            if ledger
                .deployments
                .get(endpoint)
                .is_none_or(|deployment| !deployment.runs.contains_key(&run.info.run_id))
            {
                run.info.state = "stopped".into();
                run.info.message = "Deployment runtime is no longer active".into();
                run.cancel.cancel();
                run.pinned = None;
            }
        }
    }

    fn check_recovered_deploys(&self, ctx: &AccountContext, now: u64) {
        let runs = {
            let state = self.state.lock().unwrap();
            let Some(sample) = state.samples.get(&ctx.key) else {
                return;
            };
            state
                .runs
                .iter()
                .filter(|run| {
                    run.context.key == ctx.key
                        && run.needs_recovery_check
                        && run.info.state == "running"
                        && run.info.endpoint.as_ref().is_some_and(|ep| {
                            sample.live.iter().any(|row| &row.endpoint == ep)
                                && state
                                    .real_ledger
                                    .deployments
                                    .get(ep)
                                    .and_then(|deployment| deployment.runs.get(&run.info.run_id))
                                    .is_some_and(|lease| {
                                        now.saturating_sub(lease.registered_at_secs) >= 15
                                    })
                        })
                })
                .cloned()
                .collect::<Vec<_>>()
        };
        for run in runs {
            let Some(endpoint) = &run.info.endpoint else {
                continue;
            };
            let info = &run.info;
            let result = self.with_endpoint(&ctx.key, endpoint, || {
                if run.cancel.is_cancelled() {
                    return Ok(());
                }
                self.require_deploy_lease(ctx.key.mode, endpoint, &info.run_id)?;
                let pin = self.pin_named_state(ctx, &info.session, endpoint)?;
                let file = pinned_file(ctx, pin.as_ref());
                let backend = ctx.backend.as_ref();
                if vm_run_alive(
                    backend,
                    &ctx.key.id,
                    ctx.auth,
                    &file,
                    &info.session,
                    &info.run_id,
                    &self.shutdown,
                )? {
                    self.update_run(&info.run_id, |entry| entry.needs_recovery_check = false);
                    return Ok(());
                }
                // The launcher may have exited while a marked subprocess is
                // still alive. Confirm group cleanup before releasing its ports.
                cancel_vm_run(
                    backend,
                    &ctx.key.id,
                    ctx.auth,
                    &file,
                    &info.session,
                    &info.run_id,
                )?;
                self.release_deploy_lease(ctx.key.mode, endpoint, &info.run_id)?;
                self.update_run(&info.run_id, |entry| {
                    if entry.info.state == "running" {
                        entry.info.state = "failed".into();
                        entry.info.message =
                            "Recovered deployment worker is no longer active".into();
                    }
                    entry.needs_recovery_check = false;
                });
                Ok(())
            });
            if let Err(error) = result {
                self.record_sample_error(ctx, &error);
            }
        }
    }

    fn release_deploy_lease(&self, mode: BackendMode, endpoint: &str, run_id: &str) -> Result<()> {
        self.update_ledger(mode, |ledger| {
            if let Some(deployment) = ledger.deployments.get_mut(endpoint) {
                deployment.runs.remove(run_id);
                if deployment.runs.is_empty() {
                    ledger.deployments.remove(endpoint);
                }
            }
            Ok(())
        })
    }

    fn require_deploy_lease(&self, mode: BackendMode, endpoint: &str, run_id: &str) -> Result<()> {
        let has_lease = |ledger: &LedgerData| {
            ledger
                .deployments
                .get(endpoint)
                .is_some_and(|deployment| deployment.runs.contains_key(run_id))
        };
        let active = match mode {
            BackendMode::Real => has_lease(&self.ledger_store.load()?),
            BackendMode::Mock => has_lease(&self.state.lock().unwrap().mock_ledger),
        };
        active.then_some(()).ok_or_else(allocation_stopped)
    }

    pub fn deploy_plugin(
        self: &Arc<Self>,
        plugin_id: &str,
        session: Option<String>,
        accelerator: Option<Accelerator>,
        inputs: BTreeMap<String, Value>,
    ) -> Result<PluginRunInfo> {
        self.deploy_plugin_in(
            self.pin_context(session.as_deref())?,
            plugin_id,
            session,
            accelerator,
            inputs,
            false,
        )
    }

    pub(crate) fn deploy_plugin_in(
        self: &Arc<Self>,
        ctx: AccountContext,
        plugin_id: &str,
        session: Option<String>,
        accelerator: Option<Accelerator>,
        inputs: BTreeMap<String, Value>,
        stop_on_failure: bool,
    ) -> Result<PluginRunInfo> {
        validate_plugin_id(plugin_id)?;
        let plugin = self
            .list_plugins()
            .iter()
            .find(|p| p.manifest.id == plugin_id)
            .cloned()
            .ok_or_else(|| Error::invalid("plugin_id", "plugin is not installed"))?;
        let (settings, token) = {
            let state = self.state.lock().unwrap();
            (
                state.settings_for(ctx.key.mode).clone(),
                state.hf_token.clone(),
            )
        };
        let (env, secrets) = plugin.manifest.resolve_inputs(&inputs, token.as_deref())?;
        let endpoints = plugin
            .manifest
            .endpoints
            .iter()
            .map(|ep| {
                let port = plugin
                    .manifest
                    .resolve_endpoint_port(ep, &inputs)
                    .ok_or_else(|| {
                        Error::invalid("port", "plugin endpoint requires a valid non-reserved port")
                    })?;
                let spec = TunnelSpec {
                    account_id: ctx.key.id.clone(),
                    auth_mode: ctx.auth,
                    endpoint: None,
                    session: session.clone().unwrap_or_else(|| "provision".into()),
                    local_port: port,
                    remote_port: port,
                    health_path: ep.health.clone(),
                };
                spec.validate()?;
                Ok(RunEndpointInfo {
                    id: ep.id.clone(),
                    label: ep.label.get(&settings.locale),
                    kind: ep.kind.clone(),
                    remote_port: port,
                    path: ep.path.clone(),
                    health: ep.health.clone(),
                    tunnel_id: None,
                    local_url: None,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let accelerator = accelerator
            .or(plugin.manifest.provision.accelerator)
            .unwrap_or(Accelerator::G4);
        if endpoints
            .iter()
            .map(|ep| ep.remote_port)
            .collect::<BTreeSet<_>>()
            .len()
            != endpoints.len()
        {
            return Err(Error::invalid(
                "port",
                "plugin endpoints resolve to duplicate VM ports",
            ));
        }
        let needs_gpu = plugin.bundled && plugin_id == "llama-cpp";
        if needs_gpu && !accelerator.is_gpu() {
            return Err(Error::invalid(
                "accelerator",
                "llama-cpp requires a GPU accelerator",
            ));
        }
        let created = session.is_none();
        let name = session.unwrap_or_else(|| self.session_name(&ctx, accelerator));
        validate_session_name(&name)?;
        let spec = ProvisionSpec {
            name: name.clone(),
            accelerator,
            high_mem: plugin.manifest.provision.high_mem,
            min_vram_mib: (plugin.manifest.provision.min_vram_mib > 0)
                .then_some(plugin.manifest.provision.min_vram_mib),
            max_attempts: settings.defaults.max_attempts,
            retry_delay_secs: settings.defaults.retry_delay_secs,
            on_exhausted: OnExhausted::StopAndFail,
        };
        let reserve = if created {
            spec.validate()?;
            Some(self.reserve(&ctx, &name, settings.defaults.max_concurrent_sessions)?)
        } else {
            None
        };
        let seq = self.next_seq.fetch_add(1, Ordering::Relaxed);
        let run_id = format!("run-{}-{}-{seq}", std::process::id(), now_millis());
        let mut info = PluginRunInfo {
            run_id: run_id.clone(),
            plugin_id: plugin_id.into(),
            session: name,
            created,
            endpoint: None,
            state: "running".into(),
            stage: Some("detect".into()),
            progress: Some(0.0),
            message: "Preparing deployment…".into(),
            outputs: BTreeMap::new(),
            log_tail: Vec::new(),
            endpoints,
            started_at_secs: now_secs(),
        };
        let cancel = CancelToken::new();
        let target = if created {
            None
        } else {
            let live = self.live_named_session(&ctx, &info.session, &self.shutdown)?;
            if needs_gpu
                && live
                    .accelerator
                    .is_some_and(|accelerator| !accelerator.is_gpu())
            {
                return Err(Error::invalid(
                    "accelerator",
                    "llama-cpp requires a GPU runtime",
                ));
            }
            Some(live.endpoint)
        };
        let registration = self.deploy_registration.lock().unwrap();
        let pinned = if let Some(endpoint) = target {
            info.endpoint = Some(endpoint.clone());
            self.with_endpoint(&ctx.key, &endpoint, || {
                let pin = self.pin_named_state(&ctx, &info.session, &endpoint)?;
                self.register_deploy(&ctx, &info, &endpoint, false)?;
                Ok(pin.map(Arc::new))
            })?
        } else {
            None
        };
        let run = ManagedRun {
            context: ctx,
            info: info.clone(),
            cancel,
            pinned,
            worker_active: true,
            needs_recovery_check: false,
        };
        {
            let mut state = self.state.lock().unwrap();
            state
                .runs
                .retain(|r| r.worker_active || r.active() || r.context.key != run.context.key);
            state.runs.push(run.clone());
        }
        drop(registration);
        self.publish();
        let coord = self.clone();
        let mut worker_run = run.clone();
        let spawn = thread::Builder::new()
            .name(format!("colab-{run_id}"))
            .spawn(move || {
                let run = &mut worker_run;
                let result = coord.run_deploy(
                    run,
                    &plugin,
                    spec,
                    settings.defaults,
                    reserve,
                    env,
                    &secrets,
                );
                if let Err(error) = result {
                    let mut cancelled = run.cancel.is_cancelled();
                    coord.update_run(&run.info.run_id, |entry| {
                        cancelled = entry.cancel.is_cancelled();
                        if entry.info.state == "running" {
                            entry.info.state = "cancelling".into();
                            entry.info.message = "Cleaning up failed deployment".into();
                        }
                    });
                    let cleanup = coord.cleanup_run(run, run.info.created && stop_on_failure);
                    coord.update_run(&run.info.run_id, |entry| {
                        entry.worker_active = false;
                        if entry.info.state == "stopped" {
                            return;
                        }
                        entry.info.state = if cleanup.is_err() {
                            "cancel_failed"
                        } else if cancelled {
                            "cancelled"
                        } else {
                            "failed"
                        }
                        .into();
                        entry.info.message = redact_secrets(
                            &cleanup.err().map_or(error.message.clone(), |e| {
                                format!("{}. Cleanup: {}", error.message, e.message)
                            }),
                            &secrets,
                        );
                        if entry.info.state != "cancel_failed" {
                            entry.pinned = None;
                        }
                    });
                }
            });
        if let Err(e) = spawn {
            let cleanup = self.cleanup_run(&run, false);
            self.update_run(&run_id, |run| {
                run.worker_active = false;
                if run.info.state == "stopped" {
                    return;
                }
                run.info.state = if cleanup.is_ok() {
                    "failed"
                } else {
                    "cancel_failed"
                }
                .into();
                run.info.message = cleanup.err().map_or_else(
                    || "Deployment worker could not start".into(),
                    |error| {
                        format!(
                            "Deployment worker could not start. Cleanup: {}",
                            error.message
                        )
                    },
                );
            });
            return Err(e.into());
        }
        Ok(info)
    }

    // Register before launching anything on the endpoint. The lease survives a
    // coordinator crash because the detached VM worker may still be active.
    fn register_deploy(
        &self,
        ctx: &AccountContext,
        run: &PluginRunInfo,
        endpoint: &str,
        created: bool,
    ) -> Result<()> {
        let key = &ctx.key;
        if key.mode == BackendMode::Mock
            && !created
            && self
                .state
                .lock()
                .unwrap()
                .reservations
                .get(key)
                .is_some_and(|names| names.contains(&run.session))
        {
            return Err(Error::invalid("session", "runtime is still provisioning"));
        }
        self.update_ledger(key.mode, |ledger| {
            let tracked = ledger
                .sessions
                .iter()
                .filter(|row| row.endpoint.as_deref() == Some(endpoint))
                .collect::<Vec<_>>();
            if tracked
                .iter()
                .any(|row| row.stopped.is_some() || row.pending_stop.is_some())
            {
                return Err(Error::new(
                    ErrorKind::SessionGone,
                    "runtime is stopped or pending stop",
                ));
            }
            if key.mode == BackendMode::Real {
                if self.account_store.load()?.get(&key.id).is_none() {
                    return Err(Error::invalid(
                        "account_id",
                        "account mismatch; refresh panel",
                    ));
                }
                if !created
                    && self.ledger_store.active_claims()?.iter().any(|claim| {
                        (claim.account_id == key.id && claim.name == run.session)
                            || tracked.iter().any(|row| {
                                row.account_id == claim.account_id && row.name == claim.name
                            })
                    })
                {
                    return Err(Error::invalid("session", "runtime is still provisioning"));
                }
            }
            let ports = run
                .endpoints
                .iter()
                .map(|endpoint| endpoint.remote_port)
                .collect::<BTreeSet<_>>();
            if ports.len() != run.endpoints.len()
                || ledger.deployments.get(endpoint).is_some_and(|deployment| {
                    deployment
                        .runs
                        .values()
                        .any(|existing| !existing.ports.is_disjoint(&ports))
                })
            {
                return Err(Error::invalid(
                    "port",
                    "a deployment already uses this VM port",
                ));
            }
            let generation = ledger.next_generation();
            let deployment = ledger.deployments.entry(endpoint.to_owned()).or_default();
            deployment.owned |= created;
            deployment.runs.insert(
                run.run_id.clone(),
                DeployRunLease {
                    account_id: key.id.clone(),
                    ports,
                    generation,
                    registered_at_secs: now_secs(),
                    metadata: Some(DeployRunMetadata {
                        session: run.session.clone(),
                        plugin_id: run.plugin_id.clone(),
                        created,
                        started_at_secs: run.started_at_secs,
                    }),
                },
            );
            Ok(())
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn run_deploy(
        self: &Arc<Self>,
        run: &mut ManagedRun,
        plugin: &InstalledPlugin,
        spec: ProvisionSpec,
        defaults: Defaults,
        reservation: Option<Reservation>,
        env: Vec<(String, String)>,
        secrets: &[String],
    ) -> Result<()> {
        let ctx = &run.context;
        let cancel = &run.cancel;
        let name = &run.info.session;
        let rid = &run.info.run_id;
        let backend = ctx.backend.as_ref();
        let account = &ctx.key.id;
        if run.info.created {
            let endpoint = self.allocate(
                ctx,
                &spec,
                defaults.guard.clone(),
                &defaults,
                cancel,
                &mut |event| {
                    self.update_run(rid, |r| {
                        if r.info.state == "stopped" {
                            return;
                        }
                        if let ProvisionEvent::EndpointRecorded { endpoint } = &event {
                            r.info.endpoint = Some(endpoint.clone());
                        }
                        if r.info.state == "running" {
                            r.info.message = format_prov_event(&event);
                        }
                    })
                },
            )?;
            run.info.endpoint = Some(endpoint.clone());
            self.with_endpoint(&ctx.key, &endpoint, || {
                cancel.check()?;
                let pin = self.pin_named_state(ctx, name, &endpoint)?.map(Arc::new);
                self.register_deploy(ctx, &run.info, &endpoint, true)?;
                run.pinned = pin.clone();
                self.update_run(rid, |r| r.pinned = pin);
                Ok(())
            })?;
        }
        drop(reservation);
        let endpoint = run
            .info
            .endpoint
            .as_deref()
            .ok_or_else(allocation_stopped)?;
        cancel.check()?;
        if backend.is_mock() {
            for (stage, progress) in [
                ("detect", 0.2),
                ("plan", 0.4),
                ("download", 0.65),
                ("server", 0.85),
                ("warmup", 1.0),
            ] {
                cancel.check()?;
                self.update_running_run(rid, |r| {
                    r.info.stage = Some(stage.into());
                    r.info.progress = Some(progress);
                    r.info.message = format!("Demo deployment: {stage}");
                });
                thread::sleep(Duration::from_millis(120));
            }
        } else {
            let pinned = &run
                .pinned
                .as_ref()
                .ok_or_else(allocation_stopped)?
                .path()
                .join("sessions.json");
            self.with_endpoint(&ctx.key, endpoint, || {
                cancel.check()?;
                self.require_deploy_lease(ctx.key.mode, endpoint, rid)?;
                launch_detached(
                    backend, account, ctx.auth, pinned, name, plugin, rid, env, secrets, cancel,
                )
            })?;
            let mut failures = 0;
            let deadline = Instant::now() + Duration::from_secs(3600);
            loop {
                wait_for(cancel, Duration::from_secs(3))?;
                if Instant::now() >= deadline {
                    return Err(Error::new(
                        ErrorKind::ExecTimeout,
                        "plugin deployment timed out",
                    ));
                }
                cancel.check()?;
                let reply = poll_vm_run(
                    backend, account, ctx.auth, pinned, name, rid, secrets, cancel,
                );
                let status = match reply {
                    Ok(reply) => {
                        failures = 0;
                        reply
                    }
                    Err(_) if failures < 2 => {
                        failures += 1;
                        continue;
                    }
                    Err(e) => return Err(e),
                };
                self.update_running_run(rid, |r| {
                    r.info.stage = status.stage;
                    r.info.progress = status.progress;
                    r.info.message = status.message.clone();
                    r.info.outputs = status.outputs;
                    r.info.log_tail = status.log_tail;
                });
                match status.state.as_str() {
                    "ready" => break,
                    "failed" => return Err(Error::new(ErrorKind::Other, status.message)),
                    "running" => {}
                    _ => {
                        return Err(Error::new(
                            ErrorKind::Parse,
                            "unrecognized plugin run state",
                        ))
                    }
                }
            }
        }
        cancel.check()?;
        if !run.info.endpoints.is_empty() {
            self.update_running_run(rid, |r| {
                r.info.stage = Some("tunnel".into());
                r.info.message = "Waiting for SSH port forwarding".into();
            });
        }
        self.with_endpoint(&ctx.key, endpoint, || {
            self.require_deploy_lease(ctx.key.mode, endpoint, rid)?;
            for ep in &mut run.info.endpoints {
                cancel.check()?;
                let tunnel = self
                    .open_pinned_tunnel(
                        ctx,
                        TunnelSpec {
                            account_id: account.clone(),
                            auth_mode: ctx.auth,
                            session: name.clone(),
                            endpoint: Some(endpoint.to_owned()),
                            local_port: ep.remote_port,
                            remote_port: ep.remote_port,
                            health_path: ep.health.clone(),
                        },
                    )
                    .map_err(|mut error| {
                        error.message = format!(
                            "Cannot open required tunnel for VM port {}. {}",
                            ep.remote_port, error.message
                        );
                        error
                    })?;
                ep.tunnel_id = Some(tunnel.id);
                ep.local_url = Some(tunnel.local_url);
            }
            Ok(())
        })?;
        let deadline = Instant::now() + Duration::from_secs(80);
        loop {
            cancel.check()?;
            let mut ready = true;
            for ep in &run.info.endpoints {
                match ep.tunnel_id.and_then(|id| self.tunnels.get(id)) {
                    Some(tunnel)
                        if tunnel.state == TunnelState::Up
                            && (ep.health.is_none() || tunnel.healthy == Some(true)) => {}
                    Some(TunnelInfo {
                        state: TunnelState::Failed { error },
                        ..
                    }) => return Err(error),
                    Some(TunnelInfo {
                        state: TunnelState::Stopped,
                        ..
                    })
                    | None => {
                        return Err(Error::new(
                            ErrorKind::ConnectionLost,
                            "required deploy tunnel was closed",
                        ));
                    }
                    _ => ready = false,
                }
            }
            if ready {
                break;
            }
            if Instant::now() >= deadline {
                return Err(Error::new(
                    ErrorKind::ExecTimeout,
                    "required deploy tunnel or service did not become healthy",
                ));
            }
            wait_for(cancel, Duration::from_millis(150))?;
        }
        let accepted = self.with_endpoint(&ctx.key, endpoint, || {
            cancel.check()?;
            self.require_deploy_lease(ctx.key.mode, endpoint, rid)?;
            let mut state = self.state.lock().unwrap();
            Ok(state
                .runs
                .iter_mut()
                .find(|entry| entry.info.run_id == *rid)
                .filter(|entry| !cancel.is_cancelled() && entry.info.state == "running")
                .is_some_and(|entry| {
                    entry.info.endpoints = run.info.endpoints.clone();
                    entry.info.state = "ready".into();
                    entry.info.message = "Deployment ready".into();
                    entry.info.progress = Some(1.0);
                    entry.worker_active = false;
                    true
                }))
        })?;
        self.publish();
        if accepted {
            Ok(())
        } else {
            Err(Error::new(ErrorKind::Cancelled, "deployment cancelled"))
        }
    }

    fn update_run(&self, run_id: &str, update: impl FnOnce(&mut ManagedRun)) {
        if let Some(run) = self
            .state
            .lock()
            .unwrap()
            .runs
            .iter_mut()
            .find(|r| r.info.run_id == run_id)
        {
            update(run);
        }
        self.publish();
    }

    fn update_running_run(&self, run_id: &str, update: impl FnOnce(&mut ManagedRun)) {
        self.update_run(run_id, |run| {
            if run.info.state == "running" {
                update(run);
            }
        });
    }

    fn cleanup_run(&self, run: &ManagedRun, release_on_failure: bool) -> Result<()> {
        for id in run.info.endpoints.iter().filter_map(|ep| ep.tunnel_id) {
            self.tunnels.close(id);
        }
        let ctx = &run.context;
        let Some(endpoint) = run.info.endpoint.as_deref() else {
            return Ok(());
        };
        self.with_endpoint(&ctx.key, endpoint, || {
            let release_runtime = release_on_failure || run.cancel.is_cancelled();
            let (already_released, release, leased) =
                self.update_ledger(ctx.key.mode, |ledger| {
                    if ledger.released_since(&ctx.key.id, endpoint, 0) {
                        return Ok((true, false, false));
                    }
                    let Some(deployment) = ledger.deployments.get(endpoint) else {
                        return Ok((false, release_runtime && run.info.created, false));
                    };
                    let only_user = deployment.runs.keys().all(|id| id == &run.info.run_id);
                    let release =
                        release_runtime && (deployment.owned || run.info.created) && only_user;
                    Ok((
                        false,
                        release,
                        deployment.runs.contains_key(&run.info.run_id),
                    ))
                })?;
            if already_released {
                return Ok(());
            }
            if release {
                if self
                    .stop_endpoint(
                        ctx,
                        &run.info.session,
                        endpoint,
                        StopRequest::Start(StopReason::Provision, Some(&run.info.run_id)),
                        None,
                    )?
                    .confirmed
                {
                    return Ok(());
                }
                return Err(Error::new(
                    ErrorKind::OrphanSuspected,
                    "runtime cleanup is pending confirmation",
                ));
            }

            if leased && !ctx.backend.is_mock() {
                let remote = (|| {
                    let pin = match &run.pinned {
                        Some(pin) => pin.clone(),
                        None => Arc::new(
                            self.pin_named_state(ctx, &run.info.session, endpoint)?
                                .ok_or_else(allocation_stopped)?,
                        ),
                    };
                    cancel_vm_run(
                        ctx.backend.as_ref(),
                        &ctx.key.id,
                        ctx.auth,
                        &pin.path().join("sessions.json"),
                        &run.info.session,
                        &run.info.run_id,
                    )
                })();
                if let Err(error) = remote {
                    // A changed name or CLI state is not proof that the original
                    // endpoint has stopped. Keep the lease if it is still live.
                    let gone = ctx
                        .backend
                        .sessions(&ctx.key.id, ctx.auth, &ctx.state_file, &self.shutdown)
                        .is_ok_and(|live| live.iter().all(|s| s.endpoint != endpoint));
                    if !gone {
                        return Err(error);
                    }
                }
            }
            self.release_deploy_lease(ctx.key.mode, endpoint, &run.info.run_id)
        })
    }

    pub fn cancel_run(self: &Arc<Self>, id: &str) -> bool {
        self.cancel_run_snapshot(id, None, None)
            .is_ok_and(|(cancelled, _)| cancelled)
    }

    pub(crate) fn cancel_run_snapshot(
        self: &Arc<Self>,
        id: &str,
        mode: Option<BackendMode>,
        account: Option<&str>,
    ) -> Result<(bool, Arc<PanelSnapshot>)> {
        let cleanup = {
            let mut state = self.state.lock().unwrap();
            verify_backend_mode(mode, state.settings.backend_mode)?;
            let Some(run) = state.runs.iter_mut().find(|run| run.info.run_id == id) else {
                return Ok((false, self.cached_snapshot()));
            };
            verify_target_account(mode, account, &run.context.key)?;
            if !matches!(
                run.info.state.as_str(),
                "running" | "ready" | "cancel_failed"
            ) {
                return Ok((false, self.cached_snapshot()));
            }
            run.info.state = "cancelling".into();
            run.info.message = "Stopping deployment…".into();
            run.cancel.cancel();
            let start_cleanup = !run.worker_active;
            run.worker_active = true;
            start_cleanup.then(|| run.clone())
        };
        self.publish();
        let snapshot = self.cached_snapshot();
        if let Some(run) = cleanup {
            let coord = self.clone();
            if let Err(error) = thread::Builder::new()
                .name("colab-cancel-run".into())
                .spawn(move || {
                    let result = coord.cleanup_run(&run, true);
                    coord.update_run(&run.info.run_id, |entry| {
                        entry.worker_active = false;
                        if entry.info.state == "stopped" {
                            return;
                        }
                        match result {
                            Ok(()) => {
                                entry.info.state = "cancelled".into();
                                entry.info.message = "Deployment cancelled".into();
                                entry.pinned = None;
                            }
                            Err(error) => {
                                entry.info.state = "cancel_failed".into();
                                entry.info.message = error.message;
                            }
                        }
                    });
                })
            {
                self.update_run(id, |entry| {
                    entry.worker_active = false;
                    if entry.info.state != "stopped" {
                        entry.info.state = "cancel_failed".into();
                        entry.info.message = format!("Cleanup worker could not start. {error}");
                    }
                });
                return Ok((true, self.cached_snapshot()));
            }
        }
        Ok((true, snapshot))
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }
}

fn make_account_public(value: &mut Value, emails: &[String]) {
    if let Some(account) = value.as_object_mut() {
        account.remove("email");
        let label = account["label"].as_str().unwrap_or_default();
        if emails.iter().any(|email| {
            !email.is_empty()
                && label
                    .to_ascii_lowercase()
                    .contains(&email.to_ascii_lowercase())
        }) || label.split_whitespace().any(|word| {
            word.split_once('@')
                .is_some_and(|(local, domain)| !local.is_empty() && domain.contains('.'))
        }) {
            account.insert("label".into(), account["id"].clone());
        }
    }
}

fn pinned_file(context: &AccountContext, pin: Option<&PrivateDir>) -> PathBuf {
    pin.map_or_else(
        || context.state_file.clone(),
        |dir| dir.path().join("sessions.json"),
    )
}

fn require_same_endpoint(expected: &str, actual: &str) -> Result<()> {
    if expected != actual {
        return Err(Error::new(
            ErrorKind::SessionGone,
            "the selected session was replaced; refresh panel",
        ));
    }
    Ok(())
}

fn verify_backend_mode(expected: Option<BackendMode>, actual: BackendMode) -> Result<()> {
    if expected.is_some_and(|mode| mode != actual) {
        return Err(Error::invalid(
            "expected_backend_mode",
            "backend mode changed; refresh panel",
        ));
    }
    Ok(())
}

fn account_mismatch() -> Error {
    Error::invalid("account_id", "account mismatch; refresh panel")
}

fn oauth_in_use() -> Error {
    Error::invalid(
        "account_id",
        "stop active OAuth2 runtimes before leaving this account",
    )
}

fn verify_target_account(
    mode: Option<BackendMode>,
    account: Option<&str>,
    actual: &AccountKey,
) -> Result<()> {
    verify_backend_mode(mode, actual.mode)?;
    if account.is_some_and(|account| account != actual.id) {
        return Err(account_mismatch());
    }
    Ok(())
}

fn find_pending_stop(
    ledger: &LedgerData,
    account: &str,
    session: Option<&str>,
    endpoint: Option<&str>,
) -> Result<Option<PendingTarget>> {
    let mut pending = ledger.sessions.iter().filter(|row| {
        row.account_id == account
            && row.is_active()
            && row.pending_stop.is_some()
            && match endpoint {
                Some(endpoint) => {
                    row.endpoint.as_deref() == Some(endpoint)
                        && session
                            .is_none_or(|name| name == row.name || name == "[?]" || row.is_orphan)
                }
                None => session == Some(row.name.as_str()),
            }
    });
    let target = pending.next().map(PendingTarget::from);
    if pending.next().is_some() {
        return Err(Error::invalid(
            "endpoint",
            "multiple pending stops match; select the original endpoint",
        ));
    }
    Ok(target)
}

fn session_not_found() -> Error {
    Error::new(
        ErrorKind::SessionGone,
        "session not found on the selected account",
    )
}

fn allocation_stopped() -> Error {
    Error::new(
        ErrorKind::SessionGone,
        "runtime stopped or pending stop during allocation",
    )
}

fn sync_watchdog_policy(row: &mut TrackedSession, watchdog: &WatchdogState) {
    row.watchdog = Some(watchdog.clone());
    row.policy.max_runtime_secs = watchdog
        .enforced_deadline_secs()
        .map(|deadline| deadline.saturating_sub(row.spend.started_at_secs).max(1));
    row.policy.idle_timeout_secs = watchdog.enforced_idle_timeout_secs();
    if watchdog.enforced_deadline_secs().is_none_or(|deadline| deadline > now_secs())
        && row.pending_stop.as_ref().is_some_and(|stop| {
            matches!(&stop.reason, StopReason::Guard { limit, .. } if limit == "Runtime" || limit == "Idle")
        })
    {
        row.pending_stop = None;
    }
}

fn session_quota() -> Error {
    Error::new(
        ErrorKind::SessionLimitReached,
        "concurrent session quota is reserved or in use",
    )
}

fn wait_for(cancel: &CancelToken, duration: Duration) -> Result<()> {
    let until = Instant::now() + duration;
    loop {
        cancel.check()?;
        let left = until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Ok(());
        }
        thread::sleep(left.min(Duration::from_millis(100)));
    }
}

fn tracked_session(
    ctx: &AccountContext,
    spec: &ProvisionSpec,
    policy: GuardPolicy,
    endpoint: Option<String>,
    hw: HardwareProbe,
    started_at: u64,
) -> TrackedSession {
    TrackedSession {
        name: spec.name.clone(),
        account_id: ctx.key.id.clone(),
        endpoint,
        generation: 0,
        allocation: None,
        is_orphan: spec.name == "[?]",
        hardware: None,
        accelerator: spec.accelerator,
        high_mem: spec.high_mem,
        gpu: hw.gpu,
        vram_mib: hw.vram_mib,
        policy,
        spend: SpendMeterState::new(started_at),
        watchdog: None,
        stopped: None,
        pending_stop: None,
        measured_rate_cu_per_hour: None,
        below_spec: false,
    }
}

fn guard_reason(concern: &Concern) -> StopReason {
    let (limit, detail) = match concern {
        Concern::Limit {
            limit,
            current,
            threshold,
            ..
        } => (
            format!("{limit:?}"),
            format!("{current:.2} / {threshold:.2}"),
        ),
        Concern::UsageUnknown { limit } => (format!("{limit:?}"), "usage unknown".into()),
    };
    StopReason::Guard { limit, detail }
}

fn concise_summary(snapshot: &Value) -> String {
    let mode = snapshot["backend_mode"].as_str().unwrap_or("real");
    let account = snapshot["active_account"]["label"]
        .as_str()
        .unwrap_or("Local ADC");
    let balance = snapshot["usage"]["balance_cu"]
        .as_f64()
        .map_or("unknown".into(), |b| format!("{b:.2} CU"));
    let rate = snapshot["usage"]["rate_cu_per_hour"]
        .as_f64()
        .map_or("unknown".into(), |r| format!("{r:.2} CU/h"));
    let sessions = snapshot["sessions"].as_array().map_or(0, Vec::len);
    let tunnels = snapshot["tunnels"].as_array().map_or(0, |all| {
        all.iter()
            .filter(|t| !matches!(t["state"]["state"].as_str(), Some("stopped" | "failed")))
            .count()
    });
    format!("Colab Deck ({mode}, {account}): balance {balance}, burn {rate}, {sessions} active runtime(s), {tunnels} active tunnel(s).")
}

fn format_prov_event(ev: &ProvisionEvent) -> String {
    match ev {
        ProvisionEvent::EndpointRecorded { .. } => "Allocation recorded".into(),
        ProvisionEvent::Allocating {
            attempt,
            max_attempts,
        } => {
            format!("Allocating VM (attempt {attempt}/{max_attempts})…")
        }
        ProvisionEvent::Allocated { attempt } => {
            format!("Allocated VM on attempt {attempt}; probing GPU…")
        }
        ProvisionEvent::AllocationFailed { attempt, message } => {
            format!("Attempt {attempt} failed: {message}")
        }
        ProvisionEvent::Probed { gpu, vram_mib, .. } => format!(
            "Probed {} ({} MiB)",
            gpu.as_deref().unwrap_or("CPU/TPU"),
            vram_mib.unwrap_or(0)
        ),
        ProvisionEvent::Rejected {
            vram_mib,
            min_vram_mib,
            ..
        } => {
            format!(
                "Rejected {} MiB < {min_vram_mib} MiB; releasing…",
                vram_mib.unwrap_or(0)
            )
        }
        ProvisionEvent::Releasing { attempt } => format!("Releasing VM from attempt {attempt}…"),
        ProvisionEvent::Reconciling { .. } => "Checking for orphan runtimes…".into(),
        ProvisionEvent::CleanupPending { .. } => "Runtime release pending confirmation".into(),
        ProvisionEvent::Retrying {
            attempt,
            delay_secs,
        } => {
            format!("Retrying attempt {attempt} in {delay_secs}s…")
        }
        ProvisionEvent::Ready { name, .. } => format!("Ready: {name}"),
        ProvisionEvent::Failed { message } => format!("Failed: {message}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::StopOutcome;
    use crate::mcp::tools::call_tool_json;

    #[test]
    fn failed_stop_stays_pending_until_an_inactive_account_confirms_release() {
        let tmp = PrivateDir::new(&std::env::temp_dir(), "guard-test").unwrap();
        let coord = ColabCoordinator::with_host(tmp.path().to_owned(), Some(true), Host::denied());
        coord.refresh(true).unwrap();
        let name = "c-g4-train";
        for fault in [StopOutcome::Terminated, StopOutcome::NotFound] {
            *coord.mock_backend.stop_fault.lock().unwrap() = Some(fault);
            let result = coord.stop_session(name);
            if fault == StopOutcome::Terminated {
                assert!(!result.unwrap());
            } else {
                assert_eq!(result.unwrap_err().kind, ErrorKind::ConnectionLost);
            }
            let row = coord.snapshot_json(None);
            assert_eq!(row["pending_stops"][0]["session"], name);
            assert!(
                row["pending_stops"][0]["status"]["retry_at_epoch"]
                    .as_u64()
                    .unwrap()
                    > now_secs()
            );
        }

        coord
            .add_account(AccountProfile {
                id: "other".into(),
                label: "Other demo account".into(),
                email: None,
                auth_mode: AuthMode::Adc,
                added_at: now_secs(),
            })
            .unwrap();
        coord.switch_account("other").unwrap();
        *coord.mock_backend.stop_fault.lock().unwrap() = None;
        coord
            .mock_backend
            .fail_sessions
            .store(true, Ordering::Relaxed);
        let context = {
            let mut state = coord.state.lock().unwrap();
            let row = state
                .mock_ledger
                .find_active_mut(ADC_ACCOUNT_ID, name)
                .unwrap();
            row.pending_stop.as_mut().unwrap().retry_at_epoch = 0;
            coord
                .context(
                    &state,
                    AccountKey {
                        mode: BackendMode::Mock,
                        id: ADC_ACCOUNT_ID.into(),
                    },
                )
                .unwrap()
        };
        let await_guard = || {
            coord.guard_tick();
            let flight = {
                let state = coord.state.lock().unwrap();
                state
                    .flights
                    .get(&(context.key.clone(), state.epoch))
                    .cloned()
            };
            if let Some(flight) = flight {
                let _ = flight.wait();
            }
        };
        await_guard();
        {
            let mut state = coord.state.lock().unwrap();
            let row = state
                .mock_ledger
                .find_active_mut(ADC_ACCOUNT_ID, name)
                .unwrap();
            assert!(row.pending_stop.as_ref().unwrap().attempts >= 3);
            row.pending_stop.as_mut().unwrap().retry_at_epoch = 0;
        }
        coord
            .mock_backend
            .fail_sessions
            .store(false, Ordering::Relaxed);
        await_guard();
        let state = coord.state.lock().unwrap();
        let row = state
            .mock_ledger
            .sessions
            .iter()
            .find(|s| s.name == name)
            .unwrap();
        assert_eq!(row.stopped.as_ref().unwrap().reason, StopReason::User);
        assert!(row.pending_stop.is_none());
        drop(state);
        coord.shutdown();
    }

    #[test]
    fn watchdog_updates_the_host_deadline_and_only_rearms_with_confirmation() {
        let tmp = PrivateDir::new(&std::env::temp_dir(), "watchdog-policy").unwrap();
        let coord = ColabCoordinator::with_host(tmp.path().to_owned(), Some(true), Host::denied());
        coord.refresh(true).unwrap();
        let row = || {
            coord
                .state
                .lock()
                .unwrap()
                .mock_ledger
                .active_for_account(ADC_ACCOUNT_ID)
                .into_iter()
                .find(|row| row.name == "c-g4-train")
                .unwrap()
                .clone()
        };
        let old = row();
        let extended = coord
            .watchdog_action("c-g4-train", "extend", Some(0.25))
            .unwrap();
        let current = row();
        assert_eq!(
            current.spend.started_at_secs + current.policy.max_runtime_secs.unwrap(),
            extended.deadline_secs.unwrap()
        );
        assert!(current.policy.max_runtime_secs > old.policy.max_runtime_secs);
        assert!(!matches!(
            eval_guard(
                &current.policy,
                old.spend.started_at_secs + old.policy.max_runtime_secs.unwrap(),
                None,
                &current.spend
            ),
            Decision::Stop(_)
        ));

        coord
            .mock_backend
            .fail_watchdog_start
            .store(true, Ordering::Relaxed);
        assert!(coord
            .watchdog_action("c-g4-train", "arm", Some(0.25))
            .is_err());
        assert!(row().watchdog.unwrap().armed);
        assert_eq!(
            row().policy.max_runtime_secs,
            current.policy.max_runtime_secs
        );

        let cancelled = coord.watchdog_action("c-g4-train", "cancel", None).unwrap();
        assert!(cancelled.cancelled && !cancelled.armed);
        assert!(cancelled.deadline_secs.is_some());
        let impact = coord.preview_account_switch();
        assert_eq!(impact["watched_sessions"], json!([]));
        assert_eq!(impact["unwatched_sessions"][0]["name"], "c-g4-train");
        let cleared = row();
        assert_eq!(cleared.policy.max_runtime_secs, None);
        assert_eq!(cleared.policy.idle_timeout_secs, None);
        assert_eq!(cleared.policy.max_spend_cu, current.policy.max_spend_cu);
        assert!(coord
            .watchdog_action("c-g4-train", "extend", Some(0.25))
            .is_err());
        assert!(!row().watchdog.unwrap().armed);

        coord
            .mock_backend
            .fail_watchdog_start
            .store(false, Ordering::Relaxed);
        let rearmed = coord
            .watchdog_action("c-g4-train", "extend", Some(0.25))
            .unwrap();
        assert!(rearmed.armed && !rearmed.cancelled);
        assert_eq!(row().policy.idle_timeout_secs, None);
        coord.shutdown();
    }

    #[test]
    fn failed_deploy_stays_pending_when_the_new_runtime_is_still_listed() {
        let tmp = PrivateDir::new(&std::env::temp_dir(), "deploy-release-pending").unwrap();
        let coord = ColabCoordinator::with_host(tmp.path().to_owned(), Some(true), Host::denied());
        coord.refresh(true).unwrap();
        assert!(coord.reap_orphan("m-s-demo-web9").unwrap());
        // The forward fails, then the stop reports success while leaving the runtime live.
        coord.open_tunnel("c-g4-train", 8209, 8209, None).unwrap();
        *coord.mock_backend.stop_fault.lock().unwrap() = Some(StopOutcome::Terminated);
        let run = coord
            .deploy_plugin_in(
                coord.active_context().unwrap(),
                "llama-cpp",
                None,
                None,
                BTreeMap::from([
                    ("repo".into(), json!("fixture/repo")),
                    ("port".into(), json!(8209)),
                ]),
                true,
            )
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(4);
        loop {
            let snapshot = coord.snapshot_json(None);
            if snapshot["plugin_runs"][0]["state"] == "cancel_failed" {
                let pending = &snapshot["pending_stops"][0];
                assert_eq!(pending["session"], run.session);
                assert_eq!(pending["status"]["reason"]["kind"], "provision");
                assert_eq!(pending["status"]["attempts"], 1);
                assert!(snapshot["sessions"].as_array().unwrap().iter().any(|row| {
                    row["endpoint"] == pending["endpoint"] && row["display_name"] == run.session
                }));
                break;
            }
            assert!(
                Instant::now() < deadline,
                "deployment cleanup did not finish"
            );
            thread::sleep(Duration::from_millis(20));
        }
        coord.shutdown();
    }

    #[test]
    fn duplicate_resolved_deploy_ports_are_rejected_before_reservation() {
        let root = PrivateDir::new(&std::env::temp_dir(), "duplicate-deploy-ports").unwrap();
        let coord = ColabCoordinator::with_host(root.path().to_owned(), Some(true), Host::denied());
        coord.refresh(true).unwrap();
        let mut plugin = coord
            .list_plugins()
            .iter()
            .find(|plugin| plugin.bundled)
            .unwrap()
            .clone();
        plugin.bundled = false;
        plugin.manifest.id = "fixture-plugin".into();
        let mut duplicate = plugin.manifest.endpoints[0].clone();
        duplicate.id = "duplicate".into();
        duplicate.port = Some(8209);
        duplicate.port_input = None;
        plugin.manifest.endpoints.push(duplicate);
        coord.state.lock().unwrap().plugins = Arc::new(vec![plugin]);
        let error = coord
            .deploy_plugin(
                "fixture-plugin",
                None,
                None,
                BTreeMap::from([
                    ("repo".into(), json!("fixture/repo")),
                    ("port".into(), json!(8209)),
                ]),
            )
            .unwrap_err();
        assert!(error.message.contains("duplicate VM ports"));
        assert!(coord.snapshot_json(None)["plugin_runs"]
            .as_array()
            .unwrap()
            .is_empty());
        coord.shutdown();
    }

    #[test]
    fn orphan_reap_keeps_pending_row_and_unknown_stop_is_rejected() {
        let tmp = PrivateDir::new(&std::env::temp_dir(), "orphan-confirm").unwrap();
        let coord = ColabCoordinator::with_host(tmp.path().to_owned(), Some(true), Host::denied());
        coord.refresh(true).unwrap();
        *coord.mock_backend.stop_fault.lock().unwrap() = Some(StopOutcome::Terminated);
        let orphan = "m-s-demo-web9";
        let reply = call_tool_json(
            "colab_session_action",
            &json!({"action":"reap_orphan","endpoint":orphan}),
            &coord,
            Instant::now(),
            None,
        );
        assert_eq!(reply["structuredContent"]["released"], false);
        assert!(reply["content"][0]["text"]
            .as_str()
            .unwrap()
            .starts_with("Stop pending confirmation"));
        let snapshot = &reply["structuredContent"]["snapshot"];
        assert!(snapshot["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["endpoint"] == orphan && row["is_orphan"] == true));
        assert_eq!(snapshot["pending_stops"][0]["endpoint"], orphan);
        assert_eq!(snapshot["pending_stops"][0]["is_orphan"], true);

        let unknown = call_tool_json(
            "colab_session_action",
            &json!({"action":"stop","session":"unknown"}),
            &coord,
            Instant::now(),
            None,
        );
        assert_eq!(unknown["isError"], true);
        assert!(unknown["structuredContent"]["error"]
            .as_str()
            .unwrap()
            .contains("not found"));
        assert!(!coord.snapshot_json(None)["pending_stops"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["session"] == "unknown"));
        *coord.mock_backend.stop_fault.lock().unwrap() = None;
        let retry = call_tool_json(
            "colab_session_action",
            &json!({"action":"stop","session":"[?]","endpoint":orphan}),
            &coord,
            Instant::now(),
            None,
        );
        assert_eq!(retry["structuredContent"]["stopped"], true);
        let observation = snapshot["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["endpoint"] == orphan)
            .unwrap();
        let ended = &coord.snapshot_json(None)["ended_sessions"][0];
        assert_eq!(ended["hardware"], "T4");
        assert_eq!(ended["started_at_epoch"], observation["started_at_epoch"]);
        assert!(ended["spend_cu"].is_number());
        assert!(coord.snapshot_json(None)["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .all(|row| row["endpoint"] != orphan));
        coord.shutdown();
    }

    #[test]
    fn pending_stop_retries_never_follow_a_reused_name() {
        let tmp = PrivateDir::new(&std::env::temp_dir(), "retry-target").unwrap();
        let coord = ColabCoordinator::with_host(tmp.path().to_owned(), Some(true), Host::denied());
        coord.refresh(true).unwrap();
        let ctx = coord.active_context().unwrap();
        let name = "c-g4-train";
        *coord.mock_backend.stop_fault.lock().unwrap() = Some(StopOutcome::Terminated);
        assert!(!coord.stop_session(name).unwrap());
        *coord.mock_backend.stop_fault.lock().unwrap() = None;
        let backend = ctx.backend.as_ref();
        assert!(backend
            .reap_endpoint(
                &ctx.key.id,
                ctx.auth,
                tmp.path(),
                DEMO_MANAGED_ENDPOINT,
                &CancelToken::new()
            )
            .unwrap());
        let replacement = backend
            .create_session(
                &ctx.key.id,
                ctx.auth,
                &ctx.state_file,
                name,
                Accelerator::L4,
                false,
                &CancelToken::new(),
            )
            .unwrap();

        assert!(coord.stop_session(name).unwrap());
        coord.refresh(true).unwrap();
        assert!(coord.preview_account_switch()["orphan_sessions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["endpoint"] == replacement && row["external_name"] == name));

        coord
            .update_ledger(ctx.key.mode, |ledger| {
                let spec = ProvisionSpec::from_defaults(name, &Defaults::default());
                let mut pending = tracked_session(
                    &ctx,
                    &spec,
                    GuardPolicy::unlimited(),
                    None,
                    HardwareProbe::default(),
                    now_secs(),
                );
                pending.pending_stop = Some(PendingStop::new(StopReason::Provision));
                ledger.record_session(pending);
                Ok(())
            })
            .unwrap();
        assert_eq!(
            coord.stop_session(name).unwrap_err().kind,
            ErrorKind::OrphanSuspected
        );
        assert!(backend
            .sessions(&ctx.key.id, ctx.auth, &ctx.state_file, &CancelToken::new())
            .unwrap()
            .iter()
            .any(|row| row.endpoint == replacement));
        coord.shutdown();
    }

    #[test]
    fn emergency_stop_and_reap_work_with_an_unreadable_ledger() {
        let tmp = PrivateDir::new(&std::env::temp_dir(), "damaged-ledger-stop").unwrap();
        let path = tmp.path().join("ledger.json");
        std::fs::write(&path, b"{incomplete").unwrap();
        let coord = ColabCoordinator::with_host(tmp.path().to_owned(), Some(false), Host::denied());
        let mut ctx = coord.active_context().unwrap();
        ctx.backend = coord.mock_backend.clone();
        let call = |args| {
            call_tool_json(
                "colab_session_action",
                &args,
                &coord,
                Instant::now(),
                Some(ctx.clone()),
            )
        };

        *coord.mock_backend.stop_fault.lock().unwrap() = Some(StopOutcome::Terminated);
        let args = json!({"action":"stop","session":"c-g4-train","expected_backend_mode":"real","account_id":ADC_ACCOUNT_ID});
        let unconfirmed = call(args.clone());
        assert_eq!(unconfirmed["structuredContent"]["stopped"], false);
        assert!(unconfirmed["structuredContent"]["ledger_warning"].is_string());

        *coord.mock_backend.stop_fault.lock().unwrap() = None;
        let stopped = call(args);
        assert_eq!(stopped["structuredContent"]["stopped"], true);
        assert!(stopped["structuredContent"]["ledger_warning"].is_string());
        let reaped = call(json!({"action":"reap_orphan","endpoint":"m-s-demo-web9"}));
        assert_eq!(reaped["structuredContent"]["released"], true);
        assert!(reaped["structuredContent"]["ledger_warning"].is_string());
        assert_eq!(std::fs::read(path).unwrap(), b"{incomplete");
        coord.shutdown();
    }

    #[test]
    fn an_allocation_waiting_for_hardware_does_not_block_emergency_stop() {
        let tmp = PrivateDir::new(&std::env::temp_dir(), "slow-probe").unwrap();
        let coord = ColabCoordinator::with_host(tmp.path().to_owned(), Some(true), Host::denied());
        coord.refresh(true).unwrap();
        coord
            .update_settings(SettingsPatch {
                max_concurrent_sessions: Some(3),
                ..Default::default()
            })
            .unwrap();
        let probe = coord.mock_backend.probe_gate.lock().unwrap();
        let req = serde_json::from_value(json!({"name":"slow-probe", "accelerator":"G4"})).unwrap();
        coord.start_provision(req).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while !coord.snapshot_json(None)["provision_jobs"][0]["last_status"]
            .as_str()
            .is_some_and(|status| status.contains("probing GPU"))
        {
            assert!(
                Instant::now() < deadline,
                "allocation did not reach its hardware probe"
            );
            thread::sleep(Duration::from_millis(10));
        }
        {
            let state = coord.state.lock().unwrap();
            let allocated = state
                .mock_ledger
                .active_for_account(ADC_ACCOUNT_ID)
                .into_iter()
                .find(|row| row.name == "slow-probe")
                .unwrap();
            assert!(allocated.allocation.is_some());
            assert!(allocated.endpoint.is_some());
            assert!(allocated.vram_mib.is_none());
        }
        let (stopped, stop) = mpsc::channel();
        let worker = coord.clone();
        let thread = thread::spawn(move || {
            let _ = stopped.send(worker.stop_session("c-g4-train"));
        });
        let result = stop.recv_timeout(Duration::from_secs(2));
        drop(probe);
        thread.join().unwrap();
        assert!(result
            .expect("emergency stop waited for the unrelated hardware probe")
            .unwrap());
        coord.shutdown();
    }

    #[test]
    fn billing_identity_quota_uses_the_same_lease_across_coordinators() {
        let tmp = PrivateDir::new(&std::env::temp_dir(), "billing-lease").unwrap();
        AccountStore::new(tmp.path())
            .upsert(
                AccountProfile {
                    id: "alias".into(),
                    label: "Alias".into(),
                    email: None,
                    auth_mode: AuthMode::Adc,
                    added_at: 0,
                },
                false,
            )
            .unwrap();
        let first = ColabCoordinator::with_host(tmp.path().to_owned(), Some(false), Host::denied());
        let second =
            ColabCoordinator::with_host(tmp.path().to_owned(), Some(false), Host::denied());
        let first_ctx = first.active_context().unwrap();
        let key = AccountKey {
            mode: BackendMode::Real,
            id: "alias".into(),
        };
        let second_ctx = second.context(&second.state.lock().unwrap(), key).unwrap();
        let identity = "same-fixture-identity";
        let lease = first
            .claim_real(&first_ctx, "first", identity.into(), &[])
            .unwrap();
        first
            .update_settings(SettingsPatch {
                max_concurrent_sessions: Some(1),
                ..SettingsPatch::default()
            })
            .unwrap();
        let denied = second
            .claim_real(&second_ctx, "second", identity.into(), &[])
            .err()
            .unwrap();
        assert_eq!(denied.kind, ErrorKind::SessionLimitReached);
        drop(lease);
        assert!(second
            .claim_real(&second_ctx, "second", identity.into(), &[])
            .is_ok());
        first.shutdown();
        second.shutdown();
    }

    #[test]
    fn mock_guard_settings_stay_in_memory_and_real_pending_stops_protect_startup_mode() {
        let root = PrivateDir::new(&std::env::temp_dir(), "mock-settings").unwrap();
        let coord = ColabCoordinator::with_host(root.path().to_owned(), None, Host::denied());
        let patch = |value| serde_json::from_value::<SettingsPatch>(value).unwrap();
        coord
            .update_settings(patch(json!({"max_spend_cu":7})))
            .unwrap();
        let mock = coord
            .update_settings(patch(json!({"backend_mode":"mock", "max_spend_cu":null})))
            .unwrap();
        assert_eq!(mock.defaults.guard.max_spend_cu, None);
        let real_defaults = coord.settings_store.load_current().unwrap().defaults;
        assert_eq!(real_defaults.guard.max_spend_cu, Some(7.0));
        let saved = std::fs::read(coord.settings_store.path()).unwrap();
        coord
            .update_settings(patch(json!({"max_runtime_secs":null})))
            .unwrap();
        assert_eq!(std::fs::read(coord.settings_store.path()).unwrap(), saved);

        let restored = coord
            .update_settings(patch(json!({"backend_mode":"real"})))
            .unwrap();
        assert_eq!(restored.defaults, real_defaults);
        let ctx = coord.active_context().unwrap();
        coord
            .update_ledger(BackendMode::Real, |ledger| {
                let spec = ProvisionSpec::from_defaults("pending", &real_defaults);
                let mut row = tracked_session(
                    &ctx,
                    &spec,
                    GuardPolicy::unlimited(),
                    None,
                    HardwareProbe::default(),
                    now_secs(),
                );
                row.pending_stop = Some(PendingStop::new(StopReason::Provision));
                ledger.record_session(row);
                Ok(())
            })
            .unwrap();
        assert!(coord
            .update_settings(patch(json!({"backend_mode":"mock"})))
            .is_err());
        assert_eq!(
            coord.settings_store.load_current().unwrap().backend_mode,
            BackendMode::Real
        );
        coord.shutdown();
    }

    #[test]
    fn degraded_accounts_registry_is_visible_and_preserved() {
        let root = PrivateDir::new(&std::env::temp_dir(), "accounts-recovery").unwrap();
        let path = root.path().join("accounts.json");
        std::fs::write(&path, b"{incomplete").unwrap();
        let coord =
            ColabCoordinator::with_host(root.path().to_owned(), Some(false), Host::denied());
        assert_eq!(
            coord.snapshot_json(None)["accounts_error"],
            "accounts registry degraded; cannot read accounts.json"
        );
        assert_eq!(
            coord
                .add_account(AccountProfile {
                    id: "fixture".into(),
                    label: "Fixture".into(),
                    email: None,
                    auth_mode: AuthMode::Adc,
                    added_at: 0,
                })
                .unwrap_err()
                .kind,
            ErrorKind::Parse
        );
        let store = AccountStore::new(root.path());
        assert!(store.save(&AccountRegistry::default_with_adc()).is_err());
        assert_eq!(std::fs::read(path).unwrap(), b"{incomplete");
        coord.shutdown();
    }

    #[test]
    fn allocation_receipt_recovers_the_original_endpoint_after_a_crash() {
        let root = PrivateDir::new(&std::env::temp_dir(), "recover-allocation").unwrap();
        let receipt = PrivateDir::new(root.path(), "allocation").unwrap();
        let coord =
            ColabCoordinator::with_host(root.path().to_owned(), Some(false), Host::denied());
        let _ = coord.refresh(true);
        let mut ctx = coord.active_context().unwrap();
        ctx.backend = Arc::new(MockBackend::empty());
        let backend = ctx.backend.as_ref();
        let spec = ProvisionSpec::from_defaults("reserved", &Defaults::default());
        let create = |name| {
            backend
                .create_session(
                    &ctx.key.id,
                    ctx.auth,
                    &ctx.state_file,
                    name,
                    Accelerator::T4,
                    false,
                    &CancelToken::new(),
                )
                .unwrap()
        };
        let replacement = create(&spec.name);
        let allocated = create("original");
        std::fs::write(
            receipt.path().join("sessions.json"),
            json!({"reserved":{"endpoint":allocated}}).to_string(),
        )
        .unwrap();
        coord
            .update_ledger(BackendMode::Real, |ledger| {
                let mut row = tracked_session(
                    &ctx,
                    &spec,
                    GuardPolicy::unlimited(),
                    None,
                    HardwareProbe::default(),
                    now_secs(),
                );
                row.allocation = Some(AllocationIntent {
                    receipt: Some(
                        receipt
                            .path()
                            .file_name()
                            .unwrap()
                            .to_string_lossy()
                            .into_owned(),
                    ),
                    prior_endpoints: BTreeSet::new(),
                });
                ledger.record_session(row);
                Ok(())
            })
            .unwrap();
        let epoch = coord.state.lock().unwrap().epoch;
        coord.refresh_account(&ctx, epoch).unwrap();
        let ledger = coord.ledger_store.load().unwrap();
        assert_eq!(
            ledger.sessions[0].endpoint.as_deref(),
            Some(allocated.as_str())
        );
        assert!(ledger.sessions[0].stopped.is_some());
        assert!(!receipt.path().exists());
        let live = backend
            .sessions(&ctx.key.id, ctx.auth, &ctx.state_file, &CancelToken::new())
            .unwrap();
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].endpoint, replacement);
        coord.shutdown();
    }

    #[test]
    fn restarted_coordinator_can_cancel_a_persisted_deployment() {
        let root = PrivateDir::new(&std::env::temp_dir(), "recover-deployment").unwrap();
        let first =
            ColabCoordinator::with_host(root.path().to_owned(), Some(false), Host::denied());
        let ctx = first.active_context().unwrap();
        let run: PluginRunInfo = serde_json::from_value(json!({
            "run_id":"run-recovery","plugin_id":"llama-cpp","session":"c-g4-train","created":true,
            "endpoint":DEMO_MANAGED_ENDPOINT,"state":"running","message":"fixture",
            "outputs":{}, "log_tail":[], "started_at_secs":now_secs(),
            "endpoints":[{"id":"api","label":"API","kind":"api","remote_port":8209,"path":"/"}],
        }))
        .unwrap();
        first
            .register_deploy(&ctx, &run, DEMO_MANAGED_ENDPOINT, true)
            .unwrap();
        first.shutdown();
        let second =
            ColabCoordinator::with_host(root.path().to_owned(), Some(false), Host::denied());
        let restored = &second.snapshot_json(None)["plugin_runs"][0];
        assert_eq!(restored["plugin_id"], run.plugin_id);
        assert_eq!(restored["created"], true);
        assert_eq!(restored["endpoint"], DEMO_MANAGED_ENDPOINT);
        assert_eq!(
            second.ledger_store.load().unwrap().deployments[DEMO_MANAGED_ENDPOINT].runs
                [&run.run_id]
                .ports,
            BTreeSet::from([8209])
        );
        second.state.lock().unwrap().runs[0].context.backend = second.mock_backend.clone();
        assert!(second.cancel_run(&run.run_id));
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if second.snapshot_json(None)["plugin_runs"][0]["state"] == "cancelled" {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "recovered deployment did not stop"
            );
            thread::sleep(Duration::from_millis(10));
        }
        assert!(second.ledger_store.load().unwrap().deployments.is_empty());
        assert!(second
            .mock_backend
            .sessions(
                ADC_ACCOUNT_ID,
                AuthMode::Adc,
                &ctx.state_file,
                &CancelToken::new()
            )
            .unwrap()
            .iter()
            .all(|row| row.endpoint != DEMO_MANAGED_ENDPOINT));
        second.shutdown();
    }

    #[test]
    fn cached_reads_do_not_wait_for_refresh_and_coalesce_by_epoch() {
        let tmp = PrivateDir::new(&std::env::temp_dir(), "single-flight").unwrap();
        let coord = ColabCoordinator::with_host(tmp.path().to_owned(), Some(true), Host::denied());
        coord.refresh(true).unwrap();
        let lock = coord.mock_backend.sessions_gate.lock().unwrap();
        let before = coord.cached_snapshot();
        let panel = crate::mcp::tools::call_tool_json(
            "colab_panel",
            &json!({"refresh": true}),
            &coord,
            Instant::now(),
            None,
        );
        assert_eq!(panel["structuredContent"]["rev"], before.rev - 1);
        let deadline = Instant::now() + Duration::from_secs(2);
        let flight = loop {
            let running = coord.state.lock().unwrap().flights.values().next().cloned();
            if let Some(flight) = running {
                break flight;
            }
            assert!(
                Instant::now() < deadline,
                "forced panel refresh did not start"
            );
            thread::sleep(Duration::from_millis(5));
        };
        assert!(Arc::ptr_eq(&flight, &coord.schedule_active(true).unwrap()));
        for _ in 0..32 {
            assert!(Arc::ptr_eq(&before, &coord.panel_snapshot()));
        }
        assert!(before.json(Some(before.rev), Instant::now())["unchanged"] == true);
        coord
            .update_settings(SettingsPatch {
                locale: Some("en".into()),
                ..Default::default()
            })
            .unwrap();
        let new_flight = coord.schedule_active(true).unwrap();
        assert!(!Arc::ptr_eq(&flight, &new_flight));
        drop(lock);
        assert!(flight.wait().is_err());
        new_flight.wait().unwrap();
        assert_eq!(coord.snapshot_json(None)["settings"]["locale"], "en");
        coord.shutdown();
    }

    #[test]
    fn published_balance_includes_elapsed_burn() {
        let tmp = PrivateDir::new(&std::env::temp_dir(), "projected-balance").unwrap();
        let coord = ColabCoordinator::with_host(tmp.path().to_owned(), Some(true), Host::denied());
        coord.refresh(true).unwrap();
        {
            let mut state = coord.state.lock().unwrap();
            let key = state.active_key();
            let sample = state.samples.get_mut(&key).unwrap();
            let usage = sample.usage.as_mut().unwrap();
            usage.balance_cu = 4.0;
            usage.rate_cu_per_hour = 8.0;
            usage.fetched_at_secs = now_secs() - 3600;
        }
        coord.publish();
        let snapshot = coord.snapshot_json(None);
        assert_eq!(snapshot["usage"]["balance_cu"], 4.0);
        assert_eq!(snapshot["usage"]["projected_balance_cu"], 0.0);
        coord.shutdown();
    }
}
