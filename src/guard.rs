//! [DP5.2] Host-side pure cost-guard evaluator.
//!
//! Second layer running while the MCP server is alive: checks runtime cap,
//! integrated session spend cap, and projected account balance floor with
//! 80% early warnings and honest `UsageUnknown` reporting when usage is
//! missing or stale (>5 min).

use crate::sessions::SpendMeterState;
use crate::settings::GuardPolicy;
use crate::usage::UsageSnapshot;
use serde::{Deserialize, Serialize};

pub const WARN_FRACTION: f64 = 0.8;
pub const USAGE_STALE_AFTER_SECS: u64 = 5 * 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LimitKind {
    Runtime,
    Spend,
    Balance,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Concern {
    Limit {
        limit: LimitKind,
        current: f64,
        threshold: f64,
        estimated: bool,
    },
    UsageUnknown {
        limit: LimitKind,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "decision", content = "concerns", rename_all = "snake_case")]
pub enum Decision {
    Ok,
    Warn(Vec<Concern>),
    Stop(Concern),
}

/// Pure guard decision function: depends ONLY on its arguments.
pub fn evaluate(
    policy: &GuardPolicy,
    now_secs: u64,
    usage: Option<&UsageSnapshot>,
    meter: &SpendMeterState,
) -> Decision {
    let mut warns = Vec::new();

    if let Some(max_s) = policy.max_runtime_secs.filter(|&s| s > 0) {
        let elapsed = now_secs.saturating_sub(meter.started_at_secs) as f64;
        let max_f = max_s as f64;
        let c = Concern::Limit {
            limit: LimitKind::Runtime,
            current: elapsed,
            threshold: max_f,
            estimated: false,
        };
        if elapsed >= max_f {
            return Decision::Stop(c);
        }
        if elapsed >= max_f * WARN_FRACTION {
            warns.push(c);
        }
    }

    if let Some(max_cu) = policy.max_spend_cu.filter(|&x| x.is_finite() && x > 0.0) {
        match meter.spend() {
            Some(spend) => {
                let c = Concern::Limit {
                    limit: LimitKind::Spend,
                    current: spend,
                    threshold: max_cu,
                    estimated: meter.estimated,
                };
                if spend >= max_cu {
                    return Decision::Stop(c);
                }
                if spend >= max_cu * WARN_FRACTION {
                    warns.push(c);
                }
            }
            None => warns.push(Concern::UsageUnknown {
                limit: LimitKind::Spend,
            }),
        }
    }

    if let Some(floor) = policy.min_balance_cu.filter(|&x| x.is_finite() && x >= 0.0) {
        match usage {
            Some(sample) if sample.is_fresh(now_secs, USAGE_STALE_AFTER_SECS) => {
                let balance = sample.projected_balance(now_secs);
                let c = Concern::Limit {
                    limit: LimitKind::Balance,
                    current: balance,
                    threshold: floor,
                    estimated: false,
                };
                if balance <= floor {
                    return Decision::Stop(c);
                }
                if floor > 0.0 && floor >= balance * WARN_FRACTION {
                    warns.push(c);
                }
            }
            _ => warns.push(Concern::UsageUnknown {
                limit: LimitKind::Balance,
            }),
        }
    }

    if warns.is_empty() {
        Decision::Ok
    } else {
        Decision::Warn(warns)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::UsageReading;
    use crate::usage::record_spend;

    #[test]
    fn guard_evaluates_ok_warn_stop_and_unknown_honestly() {
        let policy = GuardPolicy {
            max_runtime_secs: Some(3600),
            max_spend_cu: Some(10.0),
            min_balance_cu: Some(20.0),
            idle_timeout_secs: None,
        };
        let mut meter = SpendMeterState::new(1000);
        // No usage yet -> warns UsageUnknown for Spend and Balance.
        assert!(matches!(evaluate(&policy, 1100, None, &meter), Decision::Warn(w) if w.len() == 2));

        let snap = UsageSnapshot::from_reading(
            UsageReading {
                balance_cu: 100.0,
                rate_cu_per_hour: 10.0,
                assignments: 1,
            },
            1500,
        );
        record_spend(&mut meter, 1500, Some(10.0)); // ~1.39 CU
        assert_eq!(evaluate(&policy, 1500, Some(&snap), &meter), Decision::Ok);

        // 85% runtime -> Warn
        assert!(matches!(
            evaluate(
                &policy,
                4100,
                Some(&UsageSnapshot {
                    fetched_at_secs: 4050,
                    ..snap
                }),
                &meter
            ),
            Decision::Warn(_)
        ));

        // Balance drops to floor -> Stop
        let low = UsageSnapshot::from_reading(
            UsageReading {
                balance_cu: 19.5,
                rate_cu_per_hour: 10.0,
                assignments: 1,
            },
            1600,
        );
        assert!(matches!(
            evaluate(&policy, 1600, Some(&low), &meter),
            Decision::Stop(Concern::Limit {
                limit: LimitKind::Balance,
                ..
            })
        ));
    }
}
