//! [DP2.4] Deterministic zero-cost MockBackend for safe Demo mode and unit/integration tests.
//!
//! Simulates multiple accounts, A100 40GB -> 80GB VRAM retry, G4 96GB, browser `[?]`
//! orphans, live burn rates, VM watchdog responses, and plugin status/log progression
//! without ever executing `colab` or burning a single Compute Unit.

use crate::accounts::ADC_ACCOUNT_ID;
use crate::cli::{
    ColabBackend, EnvironmentCheck, ExecOutput, HardwareProbe, ServerSession, StopOutcome,
    UsageReading,
};
use crate::error::{Error, ErrorKind, Result};
use crate::settings::{Accelerator, AuthMode};
use crate::util::{now_f64, CancelToken};
use serde_json::json;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

pub(crate) const DEMO_MANAGED_ENDPOINT: &str = "m-s-demo-g401";

#[derive(Debug, Clone)]
struct MockVm {
    name: Option<String>,
    endpoint: String,
    accelerator: Accelerator,
    high_mem: bool,
    gpu_name: Option<String>,
    vram_mib: Option<u32>,
    watchdog_deadline_epoch: Option<f64>,
    watchdog_idle_s: Option<f64>,
    watchdog_cancelled: bool,
    watchdog_alive: bool,
}

#[derive(Debug, Clone)]
struct MockAccountState {
    balance_cu: f64,
    vms: Vec<MockVm>,
    a100_alloc_count: u32,
}

pub struct MockBackend {
    accounts: Mutex<BTreeMap<String, MockAccountState>>,
    next_ep: Mutex<u64>,
    fail_usage: Mutex<bool>,
    #[cfg(test)]
    pub(crate) stop_fault: Mutex<Option<StopOutcome>>,
    #[cfg(test)]
    pub(crate) fail_sessions: std::sync::atomic::AtomicBool,
    #[cfg(test)]
    pub(crate) fail_watchdog_start: std::sync::atomic::AtomicBool,
    #[cfg(test)]
    pub(crate) lose_allocation_reply: std::sync::atomic::AtomicBool,
    #[cfg(test)]
    pub(crate) sessions_gate: Mutex<()>,
    #[cfg(test)]
    pub(crate) probe_gate: Mutex<()>,
}

impl Default for MockBackend {
    fn default() -> Self {
        Self::demo()
    }
}

impl MockBackend {
    pub fn demo() -> Self {
        let mut map = BTreeMap::new();
        let now = now_f64();
        map.insert(
            ADC_ACCOUNT_ID.to_owned(),
            MockAccountState {
                balance_cu: 486.40,
                a100_alloc_count: 0,
                vms: vec![
                    MockVm {
                        name: Some("c-g4-train".into()),
                        endpoint: DEMO_MANAGED_ENDPOINT.into(),
                        accelerator: Accelerator::G4,
                        high_mem: false,
                        gpu_name: Some("NVIDIA RTX PRO 6000 Blackwell".into()),
                        vram_mib: Some(97_887),
                        watchdog_deadline_epoch: Some(now + 5400.0),
                        watchdog_idle_s: Some(1800.0),
                        watchdog_cancelled: false,
                        watchdog_alive: true,
                    },
                    MockVm {
                        name: None, // Unmanaged browser session `[?]`
                        endpoint: "m-s-demo-web9".into(),
                        accelerator: Accelerator::T4,
                        high_mem: false,
                        gpu_name: Some("Tesla T4".into()),
                        vram_mib: Some(15_360),
                        watchdog_deadline_epoch: None,
                        watchdog_idle_s: None,
                        watchdog_cancelled: true,
                        watchdog_alive: false,
                    },
                ],
            },
        );
        Self {
            accounts: Mutex::new(map),
            next_ep: Mutex::new(100),
            fail_usage: Mutex::new(false),
            #[cfg(test)]
            stop_fault: Mutex::new(None),
            #[cfg(test)]
            fail_sessions: std::sync::atomic::AtomicBool::new(false),
            #[cfg(test)]
            fail_watchdog_start: std::sync::atomic::AtomicBool::new(false),
            #[cfg(test)]
            lose_allocation_reply: std::sync::atomic::AtomicBool::new(false),
            #[cfg(test)]
            sessions_gate: Mutex::new(()),
            #[cfg(test)]
            probe_gate: Mutex::new(()),
        }
    }

    pub fn empty() -> Self {
        let mut map = BTreeMap::new();
        map.insert(
            ADC_ACCOUNT_ID.to_owned(),
            MockAccountState {
                balance_cu: 500.0,
                a100_alloc_count: 0,
                vms: Vec::new(),
            },
        );
        Self {
            accounts: Mutex::new(map),
            next_ep: Mutex::new(1),
            fail_usage: Mutex::new(false),
            #[cfg(test)]
            stop_fault: Mutex::new(None),
            #[cfg(test)]
            fail_sessions: std::sync::atomic::AtomicBool::new(false),
            #[cfg(test)]
            fail_watchdog_start: std::sync::atomic::AtomicBool::new(false),
            #[cfg(test)]
            lose_allocation_reply: std::sync::atomic::AtomicBool::new(false),
            #[cfg(test)]
            sessions_gate: Mutex::new(()),
            #[cfg(test)]
            probe_gate: Mutex::new(()),
        }
    }

    pub fn set_fail_usage(&self, fail: bool) {
        *self.fail_usage.lock().unwrap() = fail;
    }

    fn alloc_endpoint(&self) -> String {
        let mut n = self.next_ep.lock().unwrap();
        *n += 1;
        format!("m-s-mock-{:04}", *n)
    }

    fn mock_rate(acc: Accelerator, high_mem: bool) -> f64 {
        match (acc, high_mem && acc.supports_high_mem()) {
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

    fn mock_hw(acc: Accelerator, a100_count: u32) -> (Option<String>, Option<u32>) {
        match acc {
            Accelerator::None | Accelerator::TpuV5e1 | Accelerator::TpuV6e1 => (None, None),
            Accelerator::T4 => (Some("Tesla T4".into()), Some(15_360)),
            Accelerator::L4 => (Some("NVIDIA L4".into()), Some(23_034)),
            Accelerator::G4 => (Some("NVIDIA RTX PRO 6000 Blackwell".into()), Some(97_887)),
            Accelerator::H100 => (Some("NVIDIA H100 80GB HBM3".into()), Some(81_559)),
            // Deterministically hands out 40GB on odd attempts and 80GB on even attempts.
            Accelerator::A100 => {
                if a100_count % 2 == 1 {
                    (Some("NVIDIA A100-SXM4-40GB".into()), Some(40_960))
                } else {
                    (Some("NVIDIA A100-SXM4-80GB".into()), Some(81_920))
                }
            }
        }
    }
}

impl ColabBackend for MockBackend {
    fn is_mock(&self) -> bool {
        true
    }

    fn colab_binary(&self) -> Option<PathBuf> {
        Some(PathBuf::from("/mock/bin/colab"))
    }

    fn check_env(&self, auth_mode: AuthMode) -> EnvironmentCheck {
        EnvironmentCheck {
            colab_found: true,
            colab_path: Some("/mock/bin/colab".into()),
            colab_version: Some("Version: 0.8.0 (demo)".into()),
            ssh_key_ready: true,
            adc_file_present: true,
            auth_mode,
            backend: "mock".into(),
        }
    }

    fn sessions(
        &self,
        account_id: &str,
        _auth_mode: AuthMode,
        _state_file: &Path,
        cancel: &CancelToken,
    ) -> Result<Vec<ServerSession>> {
        #[cfg(test)]
        let _gate = self.sessions_gate.lock().unwrap();
        cancel.check()?;
        #[cfg(test)]
        if self
            .fail_sessions
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            return Err(Error::new(ErrorKind::Parse, "simulated ambiguous sessions"));
        }
        let guard = self.accounts.lock().unwrap();
        let Some(acc) = guard.get(account_id) else {
            return Ok(Vec::new());
        };
        Ok(acc
            .vms
            .iter()
            .map(|vm| ServerSession {
                name: vm.name.clone(),
                endpoint: vm.endpoint.clone(),
                hardware: if vm.accelerator == Accelerator::None {
                    "CPU".into()
                } else {
                    vm.accelerator.as_str().into()
                },
                accelerator: Some(vm.accelerator),
                shape: if vm.high_mem && vm.accelerator.supports_high_mem() {
                    "High-RAM".into()
                } else {
                    "Standard".into()
                },
                variant: if vm.accelerator.is_gpu() {
                    "GPU".into()
                } else if vm.accelerator.is_tpu() {
                    "TPU".into()
                } else {
                    "DEFAULT".into()
                },
                status: Some("Connected".into()),
            })
            .collect())
    }

    fn usage(
        &self,
        account_id: &str,
        _auth_mode: AuthMode,
        _state_file: &Path,
        cancel: &CancelToken,
    ) -> Result<UsageReading> {
        cancel.check()?;
        if *self.fail_usage.lock().unwrap() {
            return Err(Error::new(ErrorKind::ConnectionLost, "mock usage failure"));
        }
        let mut guard = self.accounts.lock().unwrap();
        let acc = guard
            .entry(account_id.to_owned())
            .or_insert(MockAccountState {
                balance_cu: 250.0,
                a100_alloc_count: 0,
                vms: Vec::new(),
            });
        let rate: f64 = acc
            .vms
            .iter()
            .map(|vm| Self::mock_rate(vm.accelerator, vm.high_mem))
            .sum();
        acc.balance_cu = (acc.balance_cu - rate * (1.0 / 3600.0)).max(0.0);
        Ok(UsageReading {
            balance_cu: (acc.balance_cu * 100.0).round() / 100.0,
            rate_cu_per_hour: (rate * 100.0).round() / 100.0,
            assignments: acc.vms.len() as u32,
        })
    }

    fn create_session(
        &self,
        account_id: &str,
        _auth_mode: AuthMode,
        _state_file: &Path,
        name: &str,
        accelerator: Accelerator,
        high_mem: bool,
        cancel: &CancelToken,
    ) -> Result<String> {
        cancel.check()?;
        let ep = self.alloc_endpoint();
        let mut guard = self.accounts.lock().unwrap();
        let acc = guard
            .entry(account_id.to_owned())
            .or_insert(MockAccountState {
                balance_cu: 250.0,
                a100_alloc_count: 0,
                vms: Vec::new(),
            });
        if accelerator == Accelerator::A100 {
            acc.a100_alloc_count += 1;
        }
        let (gpu_name, vram_mib) = Self::mock_hw(accelerator, acc.a100_alloc_count);
        if acc.vms.iter().any(|v| v.name.as_deref() == Some(name)) {
            return Err(Error::invalid("session", "session already exists"));
        }
        acc.vms.push(MockVm {
            name: Some(name.to_owned()),
            endpoint: ep.clone(),
            accelerator,
            high_mem: high_mem && accelerator.supports_high_mem(),
            gpu_name,
            vram_mib,
            watchdog_deadline_epoch: None,
            watchdog_idle_s: None,
            watchdog_cancelled: false,
            watchdog_alive: false,
        });
        #[cfg(test)]
        if self
            .lose_allocation_reply
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            return Err(Error::new(
                ErrorKind::ConnectionLost,
                "simulated allocation without ownership evidence",
            ));
        }
        Ok(ep)
    }

    fn probe_hardware(
        &self,
        account_id: &str,
        _auth_mode: AuthMode,
        _state_file: &Path,
        name: &str,
        cancel: &CancelToken,
    ) -> Result<HardwareProbe> {
        #[cfg(test)]
        let _gate = self.probe_gate.lock().unwrap();
        cancel.check()?;
        let guard = self.accounts.lock().unwrap();
        let vm = guard
            .get(account_id)
            .and_then(|a| a.vms.iter().find(|v| v.name.as_deref() == Some(name)))
            .ok_or_else(|| {
                Error::new(ErrorKind::SessionGone, format!("session {name} not found"))
            })?;
        Ok(HardwareProbe {
            gpu: vm.gpu_name.clone(),
            vram_mib: vm.vram_mib,
        })
    }

    fn stop_session(
        &self,
        account_id: &str,
        _auth_mode: AuthMode,
        _state_file: &Path,
        name: &str,
        cancel: &CancelToken,
    ) -> Result<StopOutcome> {
        cancel.check()?;
        #[cfg(test)]
        if let Some(outcome) = *self.stop_fault.lock().unwrap() {
            if outcome == StopOutcome::NotFound {
                return Err(Error::new(
                    ErrorKind::ConnectionLost,
                    "simulated stop failure",
                ));
            }
            return Ok(outcome);
        }
        let mut guard = self.accounts.lock().unwrap();
        let Some(acc) = guard.get_mut(account_id) else {
            return Ok(StopOutcome::NotFound);
        };
        let before = acc.vms.len();
        acc.vms.retain(|v| v.name.as_deref() != Some(name));
        if acc.vms.len() < before {
            Ok(StopOutcome::Terminated)
        } else {
            Ok(StopOutcome::NotFound)
        }
    }

    fn reap_endpoint(
        &self,
        account_id: &str,
        _auth_mode: AuthMode,
        _scratch_state: &Path,
        endpoint: &str,
        cancel: &CancelToken,
    ) -> Result<bool> {
        cancel.check()?;
        #[cfg(test)]
        if let Some(outcome) = *self.stop_fault.lock().unwrap() {
            return match outcome {
                StopOutcome::NotFound => Err(Error::new(
                    ErrorKind::ConnectionLost,
                    "simulated stop failure",
                )),
                StopOutcome::Terminated => Ok(true),
            };
        }
        let mut guard = self.accounts.lock().unwrap();
        let Some(acc) = guard.get_mut(account_id) else {
            return Ok(false);
        };
        let before = acc.vms.len();
        acc.vms.retain(|v| v.endpoint != endpoint);
        Ok(acc.vms.len() < before)
    }

    fn restart_kernel(
        &self,
        account_id: &str,
        _auth_mode: AuthMode,
        _state_file: &Path,
        name: &str,
        cancel: &CancelToken,
    ) -> Result<()> {
        cancel.check()?;
        let guard = self.accounts.lock().unwrap();
        let exists = guard
            .get(account_id)
            .is_some_and(|a| a.vms.iter().any(|v| v.name.as_deref() == Some(name)));
        if exists {
            Ok(())
        } else {
            Err(Error::new(
                ErrorKind::SessionGone,
                format!("session {name} not found"),
            ))
        }
    }

    fn session_url(
        &self,
        account_id: &str,
        _auth_mode: AuthMode,
        _state_file: &Path,
        name: &str,
        cancel: &CancelToken,
    ) -> Result<String> {
        cancel.check()?;
        let guard = self.accounts.lock().unwrap();
        let vm = guard
            .get(account_id)
            .and_then(|a| a.vms.iter().find(|v| v.name.as_deref() == Some(name)))
            .ok_or_else(|| {
                Error::new(ErrorKind::SessionGone, format!("session {name} not found"))
            })?;
        Ok(format!(
            "https://colab.research.google.com/notebooks/empty.ipynb?dbu=%2Ftun%2Fm%2F{}#datalabBackendUrl=https://colab.research.google.com/tun/m/{}",
            vm.endpoint, vm.endpoint
        ))
    }

    fn exec_code(
        &self,
        account_id: &str,
        _auth_mode: AuthMode,
        _state_file: &Path,
        name: &str,
        code: &str,
        env: &[(String, String)],
        _timeout_secs: u32,
        cancel: &CancelToken,
    ) -> Result<ExecOutput> {
        cancel.check()?;
        let mut guard = self.accounts.lock().unwrap();
        let vm = guard
            .get_mut(account_id)
            .and_then(|a| a.vms.iter_mut().find(|v| v.name.as_deref() == Some(name)))
            .ok_or_else(|| {
                Error::new(ErrorKind::SessionGone, format!("session {name} not found"))
            })?;
        let now = now_f64();
        let env_get = |k: &str| env.iter().find(|(ek, _)| ek == k).map(|(_, v)| v.as_str());

        if code.contains("watchdog.py") && code.contains("base64.b64decode") {
            let dl = env_get("CMCP_WD_DEADLINE_S").and_then(|s| s.parse::<f64>().ok());
            let idle = env_get("CMCP_WD_IDLE_S").and_then(|s| s.parse::<f64>().ok());
            #[cfg(test)]
            if self
                .fail_watchdog_start
                .load(std::sync::atomic::Ordering::Relaxed)
            {
                return Err(Error::new(
                    ErrorKind::Other,
                    "simulated watchdog startup failure",
                ));
            }
            vm.watchdog_deadline_epoch = dl.map(|d| now + d);
            vm.watchdog_idle_s = idle;
            vm.watchdog_cancelled = false;
            vm.watchdog_alive = true;
            return Ok(ExecOutput {
                result: Some(json!({
                    "now": now,
                     "instance": env.iter().find(|(k, _)| k == "CMCP_WD_INSTANCE").map(|(_, v)| v),
                    "policy_runtime_s": dl,
                    "started": now,
                    "grace_s": env_get("CMCP_WD_GRACE_S").and_then(|s| s.parse::<f64>().ok()),
                    "deadline_epoch": vm.watchdog_deadline_epoch,
                    "idle_timeout_s": vm.watchdog_idle_s,
                    "cancelled": false,
                    "runtime_addr": true,
                    "alive": true
                })),
                ..Default::default()
            });
        }
        if code.contains("_cfg[\"cancelled\"] = True") {
            vm.watchdog_cancelled = true;
            vm.watchdog_alive = false;
            return Ok(ExecOutput {
                result: Some(json!({
                    "now": now,
                    "deadline_epoch": vm.watchdog_deadline_epoch,
                    "idle_timeout_s": vm.watchdog_idle_s,
                    "cancelled": true,
                    "runtime_addr": true,
                    "alive": false
                })),
                ..Default::default()
            });
        }
        if code.contains("CMCP_WD_DEADLINE_S") {
            let dl = env_get("CMCP_WD_DEADLINE_S").and_then(|s| s.parse::<f64>().ok());
            if vm.watchdog_alive && !vm.watchdog_cancelled {
                vm.watchdog_deadline_epoch = dl.map(|d| now + d);
            }
            return Ok(ExecOutput {
                result: Some(json!({
                    "now": now,
                    "deadline_epoch": vm.watchdog_deadline_epoch,
                    "idle_timeout_s": vm.watchdog_idle_s,
                    "cancelled": vm.watchdog_cancelled,
                    "runtime_addr": true,
                    "alive": vm.watchdog_alive
                })),
                ..Default::default()
            });
        }
        if code.contains("watchdog.json") {
            return Ok(ExecOutput {
                result: Some(json!({
                    "now": now,
                    "deadline_epoch": vm.watchdog_deadline_epoch,
                    "idle_timeout_s": vm.watchdog_idle_s,
                    "cancelled": vm.watchdog_cancelled,
                    "runtime_addr": true,
                    "alive": vm.watchdog_alive
                })),
                ..Default::default()
            });
        }

        Ok(ExecOutput {
            stdout: "ok\n".into(),
            stderr: String::new(),
            result: Some(json!({"ok": true, "now": now})),
        })
    }
}
