//! [DP4] Honest compute-unit usage telemetry, balance history, rate attribution, and spend integration.
//!
//! Strictly Layer 4: depends ONLY on DP1–DP3 (`settings`, `cli`, `sessions`).
//! Unknown or failed usage readings are explicitly marked stale/unknown and NEVER
//! coerced to zero.

use crate::cli::{ServerSession, UsageReading};
use crate::sessions::{
    recover_cli_history, LedgerData, ObservedUnmanaged, RateAttribution, SoleRate, SpendMeterState,
    StartSource, TrackedSession,
};
use crate::settings::Accelerator;
use crate::util::{
    ensure_private_dir, lock_file, read_private_bounded, write_atomic_private, MAX_STATE_BYTES,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};

pub const HISTORY_CAPACITY: usize = 320;
pub const HISTORY_RETENTION_SECS: u64 = 7 * 24 * 3600;
const RAW_WINDOW_SECS: u64 = 10 * 60;
const HOUR_WINDOW_SECS: u64 = 3600;
const DAY_WINDOW_SECS: u64 = 24 * 3600;
const NO_RATE: f64 = 1e-9;
const MEASURE_HOLD: u64 = 120;
const UNMANAGED_RETENTION: u64 = 600;

/// Reference community-measured burn rates (CU/h) used only before live measurement.
pub fn published_rate(accelerator: Accelerator, high_mem: bool) -> f64 {
    let hm = high_mem && accelerator.supports_high_mem();
    match (accelerator, hm) {
        (Accelerator::None, false) => 0.08,
        (Accelerator::None, true) => 0.26,
        (Accelerator::T4, false) => 1.07,
        (Accelerator::T4, true) => 1.41,
        (Accelerator::L4, _) => 1.54,
        (Accelerator::G4, _) => 8.90,
        (Accelerator::A100, false) => 5.30,
        (Accelerator::A100, true) => 6.77,
        (Accelerator::H100, _) => 18.05,
        (Accelerator::TpuV5e1, _) => 2.92,
        (Accelerator::TpuV6e1, _) => 4.08,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct UsageSnapshot {
    pub balance_cu: f64,
    pub rate_cu_per_hour: f64,
    pub assignments: u32,
    pub fetched_at_secs: u64,
    /// True when a subsequent fetch failed; values represent the last known sample.
    pub stale: bool,
}

impl UsageSnapshot {
    pub fn from_reading(r: UsageReading, fetched_at_secs: u64) -> Self {
        Self {
            balance_cu: r.balance_cu,
            rate_cu_per_hour: r.rate_cu_per_hour,
            assignments: r.assignments,
            fetched_at_secs,
            stale: false,
        }
    }

    /// Projected balance extrapolated to `now_secs` at the sampled burn rate.
    pub fn projected_balance(&self, now_secs: u64) -> f64 {
        let age_h = now_secs.saturating_sub(self.fetched_at_secs) as f64 / 3600.0;
        (self.balance_cu - self.rate_cu_per_hour.max(0.0) * age_h).max(0.0)
    }

    /// Estimated remaining seconds before balance reaches 0 (`None` if burn rate == 0).
    pub fn eta_secs(&self, now_secs: u64) -> Option<u64> {
        let bal = self.projected_balance(now_secs);
        if bal <= 0.0 {
            return Some(0);
        }
        if !self.rate_cu_per_hour.is_finite() || self.rate_cu_per_hour <= NO_RATE {
            return None;
        }
        Some(((bal / self.rate_cu_per_hour) * 3600.0).min(1e10) as u64)
    }

    pub fn is_fresh(&self, now_secs: u64, max_age_secs: u64) -> bool {
        !self.stale && now_secs.saturating_sub(self.fetched_at_secs) <= max_age_secs
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct BalancePoint {
    pub at_secs: u64,
    pub balance_cu: f64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct BalanceHistory {
    points: VecDeque<BalancePoint>,
}

impl BalanceHistory {
    pub fn from_points(mut points: Vec<BalancePoint>) -> Self {
        points.retain(|p| p.balance_cu.is_finite());
        points.sort_by_key(|p| p.at_secs);
        let mut history = Self::default();
        if let Some(last_ts) = points.last().map(|p| p.at_secs) {
            history.points = VecDeque::from(points);
            history.compact(last_ts);
        }
        history
    }

    pub fn push(&mut self, at_secs: u64, balance_cu: f64) {
        if !balance_cu.is_finite() {
            return;
        }
        if let Some(last) = self.points.back_mut() {
            if at_secs < last.at_secs {
                return;
            }
            if last.at_secs == at_secs {
                last.balance_cu = balance_cu;
                return;
            }
        }
        self.points.push_back(BalancePoint {
            at_secs,
            balance_cu,
        });
        self.compact(at_secs);
    }

    /// Tiered 7-day downsampling:
    /// - `<= 10m`: raw samples (preserves rapid updates immediately)
    /// - `10m..=1h`: 1-min buckets
    /// - `1h..=24h`: 15-min buckets
    /// - `24h..=7d`: 1-hour buckets
    /// - `> 7d`: pruned
    pub fn compact(&mut self, now_secs: u64) {
        let cutoff = now_secs.saturating_sub(HISTORY_RETENTION_SECS);
        let latest = self.points.back().copied();
        let mut out: Vec<BalancePoint> =
            Vec::with_capacity(self.points.len().min(HISTORY_CAPACITY));
        let mut last_bucket: Option<(u8, u64)> = None;
        for &pt in &self.points {
            if pt.at_secs < cutoff || !pt.balance_cu.is_finite() {
                continue;
            }
            let age = now_secs.saturating_sub(pt.at_secs);
            let bucket = if age <= RAW_WINDOW_SECS {
                None
            } else if age <= HOUR_WINDOW_SECS {
                Some((1, pt.at_secs / 60))
            } else if age <= DAY_WINDOW_SECS {
                Some((2, pt.at_secs / 900))
            } else {
                Some((3, pt.at_secs / 3600))
            };
            if let Some(key) = bucket {
                if last_bucket == Some(key) {
                    if let Some(prev) = out.last_mut() {
                        *prev = pt;
                    }
                    continue;
                }
                last_bucket = Some(key);
            } else {
                last_bucket = None;
            }
            out.push(pt);
        }
        if let Some(tail) = latest {
            if tail.at_secs >= cutoff && out.last().is_none_or(|p| p.at_secs != tail.at_secs) {
                out.push(tail);
            }
        }
        if out.len() > HISTORY_CAPACITY {
            let excess = out.len() - HISTORY_CAPACITY;
            out.drain(..excess);
        }
        self.points = VecDeque::from(out);
    }

    /// Pre-seed a realistic 7-day curve for zero-billing Demo mode ending at `current_balance`.
    pub fn seed_demo(now_secs: u64, current_balance: f64) -> Self {
        let mut pts = Vec::new();
        // Work backwards from now: gentle idle stretches + realistic GPU burn sessions.
        let offsets_and_deltas: &[(u64, f64)] = &[
            (6 * 86400 + 18 * 3600, 74.2),
            (6 * 86400 + 6 * 3600, 68.5),
            (5 * 86400 + 18 * 3600, 68.5),
            (5 * 86400 + 8 * 3600, 58.1),
            (4 * 86400 + 20 * 3600, 52.4),
            (4 * 86400 + 6 * 3600, 52.4),
            (3 * 86400 + 16 * 3600, 41.0),
            (3 * 86400 + 2 * 3600, 38.6),
            (2 * 86400 + 14 * 3600, 31.2),
            (2 * 86400, 29.8),
            (86400 + 14 * 3600, 22.4),
            (86400 + 4 * 3600, 20.1),
            (22 * 3600, 17.8),
            (18 * 3600, 15.2),
            (14 * 3600, 15.2),
            (10 * 3600, 11.6),
            (6 * 3600, 9.4),
            (5 * 3600, 8.1),
            (4 * 3600, 6.9),
            (3 * 3600, 6.1),
            (2 * 3600, 4.8),
            (90 * 60, 3.6),
            (55 * 60, 2.4),
            (40 * 60, 1.8),
            (25 * 60, 1.1),
            (15 * 60, 0.7),
            (5 * 60, 0.25),
            (60, 0.05),
        ];
        for &(ago, extra) in offsets_and_deltas {
            if let Some(ts) = now_secs.checked_sub(ago) {
                pts.push(BalancePoint {
                    at_secs: ts,
                    balance_cu: ((current_balance + extra) * 100.0).round() / 100.0,
                });
            }
        }
        pts.push(BalancePoint {
            at_secs: now_secs,
            balance_cu: current_balance,
        });
        Self::from_points(pts)
    }

    pub fn points(&self) -> Vec<BalancePoint> {
        self.points.iter().copied().collect()
    }
}

/// Atomic 0600 file-backed 7-day balance history store (`${PLUGIN_DATA}/balance_history.json`).
#[derive(Debug, Clone)]
pub struct BalanceHistoryStore {
    path: PathBuf,
    lock_path: PathBuf,
}

impl BalanceHistoryStore {
    pub fn new(data_dir: &Path) -> Self {
        Self {
            path: data_dir.join("balance_history.json"),
            lock_path: data_dir.join("balance_history.lock"),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn load(&self) -> BTreeMap<String, BalanceHistory> {
        let Ok(raw) = read_private_bounded(&self.path, MAX_STATE_BYTES) else {
            return BTreeMap::new();
        };
        let Ok(map) = serde_json::from_slice::<BTreeMap<String, Vec<BalancePoint>>>(&raw) else {
            return BTreeMap::new();
        };
        map.into_iter()
            .map(|(k, pts)| (k, BalanceHistory::from_points(pts)))
            .collect()
    }

    pub fn save_account(&self, account_id: &str, history: &BalanceHistory) {
        if let Some(parent) = self.path.parent() {
            if ensure_private_dir(parent).is_err() {
                return;
            }
        }
        let Ok(_guard) = lock_file(&self.lock_path) else {
            return;
        };
        let mut map: BTreeMap<String, Vec<BalancePoint>> =
            read_private_bounded(&self.path, MAX_STATE_BYTES)
                .ok()
                .and_then(|raw| serde_json::from_slice(&raw).ok())
                .unwrap_or_default();
        map.insert(account_id.to_owned(), history.points());
        if let Ok(bytes) = serde_json::to_vec(&map) {
            let _ = write_atomic_private(&self.path, &bytes);
        }
    }
}

/// Integrate spend over the interval `[meter.integrated_to_secs, now_secs]`.
/// When `rate_cu_per_hour` is `None`: extrapolates with the last known rate (marking
/// `estimated = true`), or leaves `rate_known = false` when no rate was ever observed.
pub fn record_spend(meter: &mut SpendMeterState, now_secs: u64, rate_cu_per_hour: Option<f64>) {
    if now_secs < meter.integrated_to_secs {
        return;
    }
    let dt_h = now_secs.saturating_sub(meter.integrated_to_secs) as f64 / 3600.0;
    match (
        rate_cu_per_hour
            .filter(|r| r.is_finite())
            .map(|r| r.max(0.0)),
        meter.last_share_cu_per_hour,
    ) {
        (Some(r), _) => {
            meter.spend_cu += r * dt_h;
            meter.rate_known = true;
            meter.last_share_cu_per_hour = Some(r);
            meter.last_observed_secs = Some(now_secs);
            meter.integrated_to_secs = now_secs;
        }
        (None, Some(prev)) => {
            meter.spend_cu += prev * dt_h;
            meter.estimated = true;
            meter.integrated_to_secs = now_secs;
        }
        (None, None) => {}
    }
}

impl RateAttribution {
    fn observe_sole(
        &mut self,
        usage: Option<&UsageSnapshot>,
        live: Option<&[ServerSession]>,
        now_secs: u64,
    ) -> Option<(f64, bool)> {
        if now_secs < self.observed_at_secs {
            return None;
        }
        self.observed_at_secs = now_secs;
        let listed_concurrent = live.is_some_and(|live| live.len() > 1);
        let reported_concurrent = usage.is_some_and(|usage| usage.assignments > 1);
        if listed_concurrent || reported_concurrent {
            self.saw_concurrent = true;
            self.sole = None;
            if let Some(usage) =
                usage.filter(|usage| reported_concurrent && usage.rate_cu_per_hour > NO_RATE)
            {
                if listed_concurrent || live.is_none() || self.combined_rate.is_none() {
                    self.combined_rate = Some(usage.rate_cu_per_hour);
                }
                self.concurrent_baseline_unknown = false;
            } else if listed_concurrent || self.combined_rate.is_none() {
                self.concurrent_baseline_unknown = true;
                self.combined_rate = None;
            }
            return None;
        }
        let (Some(usage), Some(live)) = (usage, live) else {
            self.sole = None;
            return None;
        };
        let rate = usage.rate_cu_per_hour;
        if usage.assignments == 0 && live.is_empty() && rate <= NO_RATE {
            self.saw_concurrent = false;
            self.concurrent_baseline_unknown = false;
            self.combined_rate = None;
        }
        let endpoint = match live {
            [only] if usage.assignments == 1 && rate > NO_RATE => &only.endpoint,
            _ => {
                self.sole = None;
                return None;
            }
        };
        // Assignment count can drop before the account rate follows. If usage
        // was unavailable during concurrency, begin with the first sole reading.
        if self.saw_concurrent
            && !self.concurrent_baseline_unknown
            && self
                .combined_rate
                .is_some_and(|combined| rate >= combined - NO_RATE)
        {
            self.sole = None;
            return None;
        }
        if self.sole.as_ref().is_none_or(|previous| {
            previous.endpoint != *endpoint || (previous.cu_per_hour - rate).abs() > NO_RATE
        }) {
            self.sole = Some(SoleRate {
                endpoint: endpoint.clone(),
                cu_per_hour: rate,
                stable_since_secs: now_secs,
            });
        }
        let held = now_secs.saturating_sub(self.sole.as_ref()?.stable_since_secs) >= MEASURE_HOLD;
        if held {
            self.saw_concurrent = false;
            self.concurrent_baseline_unknown = false;
            self.combined_rate = None;
        }
        Some((rate, held))
    }
}

/// Attribute account-total burn rate across live sessions and update their spend meters.
pub fn attribute_rates(
    usage: Option<&UsageSnapshot>,
    live: &[ServerSession],
    ledger: &mut LedgerData,
    account_id: &str,
    now_secs: u64,
) -> BTreeMap<String, (f64, bool)> {
    attribute_rates_with_history(usage, Some(live), ledger, account_id, now_secs, None)
}

pub(crate) fn attribute_rates_with_history(
    usage: Option<&UsageSnapshot>,
    live: Option<&[ServerSession]>,
    ledger: &mut LedgerData,
    account_id: &str,
    now_secs: u64,
    history_dir: Option<&Path>,
) -> BTreeMap<String, (f64, bool)> {
    let mut out = BTreeMap::new();
    let usage = usage.filter(|u| !u.stale && u.rate_cu_per_hour.is_finite());
    let sole_rate = if live.is_some_and(|live| live.len() > 1)
        || usage.is_some_and(|u| u.assignments > 0)
        || ledger.rate_attribution.contains_key(account_id)
    {
        ledger
            .rate_attribution
            .entry(account_id.to_owned())
            .or_default()
            .observe_sole(usage, live, now_secs)
    } else {
        None
    };
    if usage.is_none() || live.is_none() {
        // Usage unknown/stale: charge gap against last known rate (or stay unknown).
        for s in &mut ledger.sessions {
            if s.account_id == account_id && s.is_active() {
                record_spend(&mut s.spend, now_secs, None);
            }
        }
    }
    let Some(live) = live else {
        return out;
    };

    let live_endpoints = live
        .iter()
        .map(|s| s.endpoint.as_str())
        .collect::<BTreeSet<_>>();
    ledger.observed_unmanaged.retain(|endpoint, row| {
        row.account_id != account_id
            || live_endpoints.contains(endpoint.as_str())
            || now_secs.saturating_sub(row.last_seen_secs) <= UNMANAGED_RETENTION
    });

    let weights: Vec<_> = live
        .iter()
        .map(|s| {
            let acc = s.accelerator.unwrap_or(Accelerator::None);
            let hw_k = LedgerData::hw_key(acc, s.high_mem());
            let w = ledger
                .hardware_rates
                .get(&hw_k)
                .copied()
                .unwrap_or_else(|| published_rate(acc, s.high_mem()));
            (s, w.max(0.01))
        })
        .collect();
    let total_w: f64 = weights.iter().map(|(_, w)| *w).sum();
    let total_rate = usage.filter(|sample| {
        live.len() > 1
            && sample.assignments as usize == live.len()
            && sample.rate_cu_per_hour > NO_RATE
    });
    let mut first_observations = Vec::new();
    for (session, weight) in weights {
        let rate = sole_rate.or_else(|| {
            usage.map(|_| {
                let share = total_rate.map_or(weight, |sample| {
                    sample.rate_cu_per_hour * (weight / total_w)
                });
                ((share * 100.0).round() / 100.0, false)
            })
        });
        let measured = rate.and_then(|(value, measured)| measured.then_some(value));
        if let Some(rate) = rate {
            out.insert(session.endpoint.clone(), rate);
        }
        if let (Some(value), Some(acc)) = (measured, session.accelerator) {
            ledger
                .hardware_rates
                .insert(LedgerData::hw_key(acc, session.high_mem()), value);
        }
        let share = rate.map(|(value, _)| value);
        if let Some(tracked) = ledger
            .find_live_mut(account_id, session)
            .filter(|row| row.name != "[?]")
        {
            if let Some(value) = measured {
                tracked.measured_rate_cu_per_hour = Some(value);
            }
            if share.is_some() {
                record_spend(&mut tracked.spend, now_secs, share);
            }
        } else if let Some(observed) = ledger
            .observed_unmanaged
            .get_mut(&session.endpoint)
            .filter(|row| row.account_id == account_id)
        {
            observed.last_seen_secs = observed.last_seen_secs.max(now_secs);
            observed.hardware = Some(session.hardware.clone());
            record_spend(&mut observed.spend, now_secs, share);
        } else {
            first_observations.push((session, share.unwrap_or(weight)));
        }
    }

    let mut recovered = history_dir
        .filter(|_| !first_observations.is_empty())
        .map(|directory| {
            let endpoints = first_observations
                .iter()
                .map(|&(session, _)| session.endpoint.as_str())
                .collect();
            recover_cli_history(directory, &endpoints, now_secs)
        })
        .unwrap_or_default();
    for (session, rate) in first_observations {
        let endpoint = session.endpoint.as_str();
        let history = recovered.remove(endpoint);
        let started_at = history
            .as_ref()
            .map_or(now_secs, |start| start.started_at_secs);
        let mut spend = SpendMeterState::new(started_at);
        spend.estimated = true;
        record_spend(&mut spend, now_secs, Some(rate));
        ledger.observed_unmanaged.insert(
            endpoint.to_owned(),
            ObservedUnmanaged {
                account_id: account_id.to_owned(),
                endpoint: endpoint.to_owned(),
                start_source: if history.is_some() {
                    StartSource::CliHistory
                } else {
                    StartSource::FirstSeen
                },
                external_name: history.and_then(|start| start.external_name),
                hardware: Some(session.hardware.clone()),
                spend,
                last_seen_secs: now_secs,
            },
        );
    }
    out
}

pub fn session_estimated_rate(
    session: &TrackedSession,
    attributed: &BTreeMap<String, (f64, bool)>,
    ledger: &LedgerData,
) -> (f64, bool) {
    if let Some(&(r, measured)) = attributed.get(session.endpoint.as_ref().unwrap_or(&session.name))
    {
        return (r, measured);
    }
    if let Some(r) = session.measured_rate_cu_per_hour {
        return (r, true);
    }
    let hw_k = LedgerData::hw_key(session.accelerator, session.high_mem);
    if let Some(&r) = ledger.hardware_rates.get(&hw_k) {
        return (r, false);
    }
    (published_rate(session.accelerator, session.high_mem), false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sessions::SessionLedger;
    use crate::settings::GuardPolicy;
    use crate::util::PrivateDir;
    use std::fs;

    fn live_session(name: Option<&str>, endpoint: &str, accelerator: Accelerator) -> ServerSession {
        ServerSession {
            name: name.map(str::to_owned),
            endpoint: endpoint.into(),
            hardware: accelerator.as_str().into(),
            accelerator: Some(accelerator),
            shape: "Standard".into(),
            variant: "GPU".into(),
            status: None,
        }
    }

    fn tracked_session(session: &ServerSession, account_id: &str, now: u64) -> TrackedSession {
        TrackedSession {
            name: session.name.clone().unwrap(),
            account_id: account_id.into(),
            endpoint: Some(session.endpoint.clone()),
            generation: 0,
            allocation: None,
            is_orphan: false,
            hardware: None,
            accelerator: session.accelerator.unwrap(),
            high_mem: session.high_mem(),
            gpu: None,
            vram_mib: None,
            policy: GuardPolicy::default(),
            spend: SpendMeterState::new(now),
            watchdog: None,
            stopped: None,
            pending_stop: None,
            measured_rate_cu_per_hour: None,
            below_spec: false,
        }
    }

    fn reading(rate_cu_per_hour: f64, assignments: u32, now: u64) -> UsageSnapshot {
        UsageSnapshot::from_reading(
            UsageReading {
                balance_cu: 90.0,
                rate_cu_per_hour,
                assignments,
            },
            now,
        )
    }

    #[test]
    fn spend_meter_never_treats_unknown_as_zero_and_integrates_gaps() {
        let mut m = SpendMeterState::new(1000);
        record_spend(&mut m, 1900, None);
        assert_eq!(m.spend(), None); // still unknown!
        record_spend(&mut m, 2800, Some(8.0)); // 0.5 h from started_at (1000..2800)
        assert!((m.spend().unwrap() - 4.0).abs() < 1e-6);
        assert!(!m.estimated);
        record_spend(&mut m, 4600, None); // 0.5 h extrapolated
        assert!((m.spend().unwrap() - 8.0).abs() < 1e-6);
        assert!(m.estimated);
    }

    #[test]
    fn sole_session_direct_measurement_and_eta() {
        let snap = reading(9.0, 1, 1000);
        assert_eq!(snap.eta_secs(1000), Some(36_000));
        assert!((snap.projected_balance(2800) - 85.5).abs() < 1e-6);

        let mut ledger = LedgerData::default();
        let mut live = [live_session(Some("s1"), "m-s-1", Accelerator::G4)];
        ledger.record_session(tracked_session(&live[0], "adc-default", 1000));
        let rates = attribute_rates(Some(&snap), &live, &mut ledger, "adc-default", 4480);
        assert_eq!(rates.get("m-s-1"), Some(&(9.0, false)));
        let rates = attribute_rates(Some(&snap), &live, &mut ledger, "adc-default", 4600);
        assert_eq!(rates.get("m-s-1"), Some(&(9.0, true)));
        assert!((ledger.sessions[0].spend.spend().unwrap() - 9.0).abs() < 1e-6);

        // After CLI state is lost the same endpoint is listed as `[?]`.
        live[0].name = None;
        let rates = attribute_rates(Some(&snap), &live, &mut ledger, "adc-default", 6400);
        assert_eq!(rates.get("m-s-1"), Some(&(9.0, true)));
        let spend = &ledger.sessions[0].spend;
        assert_eq!(spend.spend(), Some(13.5));
        let policy = GuardPolicy {
            max_spend_cu: Some(10.0),
            ..GuardPolicy::unlimited()
        };
        assert!(matches!(
            crate::guard::evaluate(&policy, 6400, Some(&snap), spend),
            crate::guard::Decision::Stop(_)
        ));
    }

    #[test]
    fn persisted_measurement_waits_for_rate_to_follow_a_stopped_session_and_settle() {
        let root = PrivateDir::new(&std::env::temp_dir(), "rate-hold").unwrap();
        let t4 = live_session(Some("t4"), "runtime-t4", Accelerator::T4);
        let a100 = live_session(Some("a100"), "runtime-a100", Accelerator::A100);
        let account = "fixture";
        let key = LedgerData::hw_key(Accelerator::T4, false);
        let store = SessionLedger::new(root.path());
        store
            .update(|ledger| {
                ledger.record_session(tracked_session(&t4, account, 1000));
                ledger.record_session(tracked_session(&a100, account, 1000));
                let initial = reading(11.77 + 1.76, 2, 1000);
                attribute_rates(Some(&initial), &[t4.clone(), a100], ledger, account, 1000);
                Ok(())
            })
            .unwrap();

        for (now, reported, expected) in [
            (1030, 13.53, None), // The A100 stopped 30 seconds after the combined sample.
            (1180, 13.53, None), // A held but unchanged combined rate is still lagging.
            (1210, 1.80, None),
            (1300, 1.76, None), // The rate changed again, restarting the hold.
            (1419, 1.76, None),
            (1420, 1.76, Some(1.76)),
        ] {
            let live = std::slice::from_ref(&t4);
            // Independent ledger handles model another polling process taking over.
            let writer = SessionLedger::new(root.path());
            let (rates, _) = writer
                .update(|ledger| {
                    ledger.reconcile(account, live, now);
                    Ok(attribute_rates(
                        Some(&reading(reported, 1, now)),
                        live,
                        ledger,
                        account,
                        now,
                    ))
                })
                .unwrap();
            assert_eq!(rates[&t4.endpoint].1, expected.is_some());
            let persisted = store.load().unwrap();
            assert_eq!(persisted.sessions[0].measured_rate_cu_per_hour, expected);
            assert_eq!(persisted.hardware_rates.get(&key).copied(), expected);
        }
    }

    #[test]
    fn sole_rate_can_settle_after_concurrency_without_a_rate_reading() {
        let account = "fixture";
        let t4 = live_session(Some("t4"), "runtime-t4", Accelerator::T4);
        let a100 = live_session(Some("a100"), "runtime-a100", Accelerator::A100);
        let mut ledger = LedgerData::default();
        ledger.record_session(tracked_session(&t4, account, 1000));
        attribute_rates(None, &[t4.clone(), a100], &mut ledger, account, 1000);
        assert!(ledger.rate_attribution[account].concurrent_baseline_unknown);

        for (now, measured) in [(1030, false), (1149, false), (1150, true)] {
            let rates = attribute_rates(
                Some(&reading(1.76, 1, now)),
                std::slice::from_ref(&t4),
                &mut ledger,
                account,
                now,
            );
            assert_eq!(rates[&t4.endpoint], (1.76, measured));
        }
        assert_eq!(ledger.sessions[0].measured_rate_cu_per_hour, Some(1.76));
    }

    #[test]
    fn unmanaged_history_recovers_start_and_backfills_spend_once() {
        let root = PrivateDir::new(&std::env::temp_dir(), "history-fixtures").unwrap();
        let history = root.path().join("Local !_Model-01.名.jsonl");
        fs::write(&history, concat!(
            "{\"timestamp\":\"1970-01-01T02:00:00.225804+01:00\",\"event_type\":\"session_created\",\"endpoint\":\"runtime-history\"}\n",
            "{\"timestamp\":\"1970-01-01T01:00:00Z\",\"event_type\":\"session_terminated\",\"endpoint\":\"runtime-history\"}\n",
            "{\"timestamp\":\"1970-01-01T01:00:00Z\",\"event_type\":\"session_created\",\"endpoint\":\"runtime-ended\"}\n",
        )).unwrap();
        fs::write(root.path().join("terminated.jsonl"),
            r#"{"timestamp":"1970-01-01T01:00:00.5Z","event_type":"session_terminated","endpoint":"runtime-ended"}"#,
        ).unwrap();
        let linked = root.path().join("ignored.log");
        fs::write(&linked,
            r#"{"timestamp":"1970-01-01T01:00:00Z","event_type":"session_created","endpoint":"runtime-link"}"#,
        ).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(linked, root.path().join("linked.jsonl")).unwrap();

        let live = [
            live_session(None, "runtime-history", Accelerator::G4),
            live_session(None, "runtime-ended", Accelerator::T4),
            live_session(None, "runtime-link", Accelerator::T4),
        ];
        let mut ledger = LedgerData::default();
        let account = "fixture";
        attribute_rates_with_history(
            None,
            Some(&live),
            &mut ledger,
            account,
            7200,
            Some(root.path()),
        );
        let observation = &ledger.observed_unmanaged["runtime-history"];
        assert_eq!(observation.start_source, StartSource::CliHistory);
        assert_eq!(
            observation.external_name.as_deref(),
            Some("Local_Model-01.")
        );
        assert_eq!(observation.spend.started_at_secs, 3600);
        assert_eq!(observation.spend.spend(), Some(8.90));
        assert!(observation.spend.estimated);
        for endpoint in ["runtime-ended", "runtime-link"] {
            let observation = &ledger.observed_unmanaged[endpoint];
            assert_eq!(observation.start_source, StartSource::FirstSeen);
            assert_eq!(observation.external_name, None);
            assert_eq!(observation.spend.started_at_secs, 7200);
        }
        assert!(ledger.sessions.is_empty());

        fs::remove_file(history).unwrap();
        let at = 10_800;
        attribute_rates_with_history(
            Some(&reading(22.08, 3, at)),
            Some(&live),
            &mut ledger,
            account,
            at,
            Some(root.path()),
        );
        let observation = &ledger.observed_unmanaged["runtime-history"];
        assert_eq!(observation.start_source, StartSource::CliHistory);
        assert!((observation.spend.spend().unwrap() - 26.7).abs() < 1e-6);
    }

    #[test]
    fn unmanaged_first_seen_accumulates_and_expires_after_ten_minutes_absent() {
        let account = "fixture";
        let mut ledger = LedgerData::default();
        let live = [live_session(None, "runtime-unmanaged", Accelerator::T4)];
        attribute_rates(
            Some(&reading(1.76, 1, 1000)),
            &live,
            &mut ledger,
            account,
            1000,
        );
        attribute_rates(
            Some(&reading(1.76, 1, 1300)),
            &live,
            &mut ledger,
            account,
            1300,
        );
        let observed = &ledger.observed_unmanaged["runtime-unmanaged"];
        assert_eq!(observed.start_source, StartSource::FirstSeen);
        assert_eq!(observed.spend.started_at_secs, 1000);
        assert_eq!(observed.last_seen_secs, 1300);
        assert!(observed.spend.estimated);
        assert!((observed.spend.spend().unwrap() - 1.76 / 12.0).abs() < 1e-6);

        attribute_rates(None, &[], &mut ledger, account, 1900);
        attribute_rates(None, &[], &mut ledger, "other-account", 1901);
        attribute_rates_with_history(None, None, &mut ledger, account, 1901, None);
        assert_eq!(ledger.observed_unmanaged.len(), 1);
        attribute_rates(None, &[], &mut ledger, account, 1901);
        assert!(ledger.observed_unmanaged.is_empty());
    }

    #[test]
    fn balance_history_seven_day_tiered_compaction_and_store_roundtrip() {
        let now = 20 * 86_400;
        let mut h = BalanceHistory::default();
        // >7d old point should be pruned
        h.push(now - 8 * 86_400, 150.0);
        // 3 days ago: multiple points in same 1h bucket collapse to latest
        h.push(now - 3 * 86_400 + 100, 130.0);
        h.push(now - 3 * 86_400 + 200, 129.0);
        // 6 hours ago: multiple points in same 15m bucket collapse to latest
        h.push(now - 6 * 3600 + 60, 110.0);
        h.push(now - 6 * 3600 + 120, 109.5);
        // 30 minutes ago: multiple points in same 1m bucket collapse to latest
        h.push(now - 1800 + 10, 95.0);
        h.push(now - 1800 + 40, 94.8);
        // Last 10 minutes: kept raw
        h.push(now - 120, 90.2);
        h.push(now - 60, 90.1);
        h.push(now, 90.0);

        let pts = h.points();
        assert!(pts
            .iter()
            .all(|p| p.at_secs >= now - HISTORY_RETENTION_SECS));
        assert_eq!(pts.len(), 6);
        assert_eq!(pts[0].balance_cu, 129.0);
        assert_eq!(pts[1].balance_cu, 109.5);
        assert_eq!(pts[2].balance_cu, 94.8);
        assert_eq!(pts[5].balance_cu, 90.0);

        let root = PrivateDir::new(&std::env::temp_dir(), "bal-hist").unwrap();
        let store = BalanceHistoryStore::new(root.path());
        store.save_account("acc-a", &h);
        let loaded = store.load().remove("acc-a").expect("loaded acc-a");
        assert_eq!(loaded.points().len(), pts.len());
        assert_eq!(loaded.points().last().unwrap().balance_cu, 90.0);

        let demo = BalanceHistory::seed_demo(now, 84.5);
        assert!(demo.points().len() >= 15);
        assert_eq!(demo.points().last().unwrap().balance_cu, 84.5);
        assert!(now - demo.points().first().unwrap().at_secs >= 6 * 86_400);
    }
}
