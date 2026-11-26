//! [DP3.2] Hardware-targeted session provisioning loop with VRAM retry and orphan reaping.
//!
//! Pure VM lifecycle primitives ONLY: validates concurrency limits, allocates VMs,
//! probes GPU VRAM via `nvidia-smi`, retries when VRAM is below `min_vram_mib`,
//! and releases only assignments attributable to its own allocation attempt.
//!
//! Strictly lower-triangular: NEVER evaluates cost guards or starts watchdogs
//! directly (DP5 handles that after provisioning returns `ProvisionOutcome`).

use crate::cli::{
    pin_session, valid_endpoint, validate_session_name, ColabBackend, HardwareProbe, ServerSession,
};
use crate::error::{Error, ErrorKind, Result};
use crate::settings::{Accelerator, AuthMode, Defaults};
use crate::util::{now_secs, CancelToken};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::thread;
use std::time::Duration;

/// Hard safety cap on allocation attempts so retries cannot fan out.
pub const MAX_ATTEMPTS_CAP: u32 = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OnExhausted {
    StopAndFail,
    KeepLast,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionLimit {
    pub max_concurrent: u32,
    pub reserved: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProvisionSpec {
    pub name: String,
    pub accelerator: Accelerator,
    pub high_mem: bool,
    pub min_vram_mib: Option<u32>,
    pub max_attempts: u32,
    pub retry_delay_secs: u64,
    pub on_exhausted: OnExhausted,
}

impl ProvisionSpec {
    pub fn from_defaults(name: impl Into<String>, defaults: &Defaults) -> Self {
        Self {
            name: name.into(),
            accelerator: defaults.accelerator,
            high_mem: defaults.high_mem,
            min_vram_mib: defaults.min_vram_mib,
            max_attempts: defaults.max_attempts,
            retry_delay_secs: defaults.retry_delay_secs,
            on_exhausted: OnExhausted::StopAndFail,
        }
    }

    pub fn validate(&self) -> Result<()> {
        validate_session_name(&self.name)?;
        if !(1..=MAX_ATTEMPTS_CAP).contains(&self.max_attempts) {
            return Err(Error::invalid("max_attempts", "expected 1–8 attempts"));
        }
        if self.min_vram_mib.is_some_and(|m| m > 0) && !self.accelerator.is_gpu() {
            return Err(Error::invalid(
                "min_vram_mib",
                "VRAM minimum requires a GPU accelerator",
            ));
        }
        Ok(())
    }

    pub fn attempts(&self) -> u32 {
        self.max_attempts.clamp(1, MAX_ATTEMPTS_CAP)
    }

    fn min_vram(&self) -> Option<u32> {
        self.min_vram_mib.filter(|&m| m > 0)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum ProvisionEvent {
    Allocating {
        attempt: u32,
        max_attempts: u32,
    },
    Allocated {
        attempt: u32,
    },
    EndpointRecorded {
        endpoint: String,
    },
    AllocationFailed {
        attempt: u32,
        message: String,
    },
    Probed {
        attempt: u32,
        gpu: Option<String>,
        vram_mib: Option<u32>,
    },
    Rejected {
        attempt: u32,
        gpu: Option<String>,
        vram_mib: Option<u32>,
        min_vram_mib: u32,
    },
    Releasing {
        attempt: u32,
    },
    Reconciling {
        attempt: u32,
    },
    CleanupPending {
        endpoint: Option<String>,
    },
    Retrying {
        attempt: u32,
        delay_secs: u64,
    },
    Ready {
        name: String,
        gpu: Option<String>,
        vram_mib: Option<u32>,
        below_spec: bool,
    },
    Failed {
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProvisionOutcome {
    pub name: String,
    pub endpoint: Option<String>,
    pub hardware: HardwareProbe,
    pub attempts: u32,
    pub below_spec: bool,
    pub started_at_secs: u64,
}

pub(crate) enum AllocationUpdate<'a> {
    Started {
        started_at_secs: u64,
        receipt: Option<&'a str>,
        prior: &'a [ServerSession],
    },
    Assigned(&'a str),
    Unassigned,
    Released,
    Pending,
}

/// Release an exact assignment and verify its absence in a fresh server listing.
pub fn stop_and_confirm(
    backend: &dyn ColabBackend,
    account_id: &str,
    auth_mode: AuthMode,
    state_file: &Path,
    scratch_state: &Path,
    known_endpoint: Option<&str>,
    cancel: &CancelToken,
) -> Result<bool> {
    let Some(endpoint) = known_endpoint else {
        return Ok(false);
    };
    let issued = backend.reap_endpoint(account_id, auth_mode, scratch_state, endpoint, cancel);
    let live = backend.sessions(account_id, auth_mode, state_file, cancel);
    match live {
        Ok(live) if live.iter().all(|s| s.endpoint != endpoint) => Ok(true),
        Ok(_) => issued.map(|_| false),
        Err(error) => issued.and(Err(error)),
    }
}

#[allow(clippy::too_many_arguments)]
fn release_attempt(
    backend: &dyn ColabBackend,
    account_id: &str,
    auth_mode: AuthMode,
    state_file: &Path,
    scratch_state: &Path,
    endpoint: Option<String>,
    track: &mut dyn FnMut(AllocationUpdate<'_>) -> Result<()>,
    on_event: &mut dyn FnMut(ProvisionEvent),
) -> Result<()> {
    let result = stop_and_confirm(
        backend,
        account_id,
        auth_mode,
        state_file,
        scratch_state,
        endpoint.as_deref(),
        &CancelToken::new(),
    );
    if matches!(result, Ok(true)) {
        track(AllocationUpdate::Released)?;
        return Ok(());
    }
    track(AllocationUpdate::Pending)?;
    on_event(ProvisionEvent::CleanupPending { endpoint });
    Err(result.err().unwrap_or_else(|| {
        Error::new(
            ErrorKind::OrphanSuspected,
            "runtime release is unconfirmed; retrying through the cost guard",
        )
    }))
}

/// Allocate a session matching `spec`, probing VRAM and retrying when below `min_vram_mib`.
#[allow(clippy::too_many_arguments)]
pub fn provision(
    backend: &dyn ColabBackend,
    account_id: &str,
    auth_mode: AuthMode,
    state_file: &Path,
    scratch_state: &Path,
    spec: &ProvisionSpec,
    limit: SessionLimit,
    cancel: &CancelToken,
    on_event: &mut dyn FnMut(ProvisionEvent),
) -> Result<ProvisionOutcome> {
    provision_with_tracking(
        backend,
        account_id,
        auth_mode,
        state_file,
        scratch_state,
        spec,
        limit,
        cancel,
        &mut |_| Ok(()),
        on_event,
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn provision_with_tracking(
    backend: &dyn ColabBackend,
    account_id: &str,
    auth_mode: AuthMode,
    state_file: &Path,
    scratch_state: &Path,
    spec: &ProvisionSpec,
    limit: SessionLimit,
    cancel: &CancelToken,
    track: &mut dyn FnMut(AllocationUpdate<'_>) -> Result<()>,
    on_event: &mut dyn FnMut(ProvisionEvent),
) -> Result<ProvisionOutcome> {
    spec.validate()?;
    cancel.check()?;

    let existing = backend.sessions(account_id, auth_mode, state_file, cancel)?;
    if existing
        .iter()
        .any(|s| s.name.as_deref() == Some(spec.name.as_str()))
    {
        return Err(Error::invalid(
            "name",
            format!(
                "session {:?} already exists; refusing to overwrite",
                spec.name
            ),
        ));
    }
    let occupied = existing.len() as u32 + limit.reserved;
    if occupied >= limit.max_concurrent {
        return Err(Error::new(
            ErrorKind::SessionLimitReached,
            format!(
                "concurrent session limit reached ({occupied}/{})",
                limit.max_concurrent
            ),
        ));
    }

    let max_attempts = spec.attempts();
    let mut last_err: Option<Error> = None;

    for attempt in 1..=max_attempts {
        if cancel.is_cancelled() {
            let err = Error::new(ErrorKind::Cancelled, "provisioning cancelled");
            on_event(ProvisionEvent::Failed {
                message: err.message.clone(),
            });
            return Err(err);
        }

        on_event(ProvisionEvent::Allocating {
            attempt,
            max_attempts,
        });

        let prior = if attempt == 1 {
            existing.clone()
        } else {
            backend.sessions(account_id, auth_mode, state_file, cancel)?
        };
        if prior.len() as u32 + limit.reserved >= limit.max_concurrent
            || prior
                .iter()
                .any(|s| s.name.as_deref() == Some(spec.name.as_str()))
        {
            return Err(Error::new(
                ErrorKind::SessionLimitReached,
                "session name or capacity is no longer available for allocation",
            ));
        }
        let has_owner = |ep: &str| valid_endpoint(ep) && prior.iter().all(|s| s.endpoint != ep);
        let staging = (!backend.is_mock())
            .then(|| crate::util::PrivateDir::new(scratch_state, "allocation"))
            .transpose()?;
        let receipt = staging
            .as_ref()
            .and_then(|dir| dir.path().file_name()?.to_str());
        let operation_file = staging.as_ref().map(|dir| dir.path().join("sessions.json"));
        let started_at_secs = now_secs();
        track(AllocationUpdate::Started {
            started_at_secs,
            receipt,
            prior: &prior,
        })?;
        let create_res = backend.create_session_with_receipt(
            account_id,
            auth_mode,
            state_file,
            operation_file.as_deref().unwrap_or(state_file),
            &spec.name,
            spec.accelerator,
            spec.high_mem,
            cancel,
        );

        let mut register = |endpoint: &str| -> Result<()> {
            if let Err(error) = track(AllocationUpdate::Assigned(endpoint)) {
                let _ = stop_and_confirm(
                    backend,
                    account_id,
                    auth_mode,
                    state_file,
                    scratch_state,
                    Some(endpoint),
                    &CancelToken::new(),
                );
                // The durable intent still refers to the private receipt if the
                // process could not record the endpoint or confirm its release.
                return Err(error);
            }
            on_event(ProvisionEvent::EndpointRecorded {
                endpoint: endpoint.to_owned(),
            });
            Ok(())
        };
        let endpoint = match create_res {
            Ok(endpoint) if has_owner(&endpoint) => {
                if let Err(error) = register(&endpoint) {
                    if let Some(staging) = staging {
                        staging.keep();
                    }
                    return Err(error);
                }
                on_event(ProvisionEvent::Allocated { attempt });
                endpoint
            }
            Ok(_) => {
                let pending = track(AllocationUpdate::Pending);
                if let Some(staging) = staging {
                    staging.keep();
                }
                pending?;
                on_event(ProvisionEvent::CleanupPending { endpoint: None });
                return Err(Error::new(ErrorKind::OrphanSuspected, "allocation reported success without an attributable endpoint; check the Colab account"));
            }
            Err(err) => {
                // Only a new state record written by this CLI invocation proves attribution.
                let attributed = err
                    .context
                    .get("provisioned_endpoint")
                    .and_then(serde_json::Value::as_str)
                    .filter(|ep| has_owner(ep))
                    .map(str::to_owned);
                if let Some(endpoint) = &attributed {
                    if let Err(error) = register(endpoint) {
                        if let Some(staging) = staging {
                            staging.keep();
                        }
                        return Err(error);
                    }
                    on_event(ProvisionEvent::Reconciling { attempt });
                    release_attempt(
                        backend,
                        account_id,
                        auth_mode,
                        state_file,
                        scratch_state,
                        attributed,
                        track,
                        on_event,
                    )?;
                    return Err(err);
                }
                let rejected = err
                    .context
                    .get("allocation_rejected")
                    .and_then(serde_json::Value::as_bool)
                    == Some(true);
                let not_started = err
                    .context
                    .get("allocation_not_started")
                    .and_then(serde_json::Value::as_bool)
                    == Some(true);
                on_event(ProvisionEvent::AllocationFailed {
                    attempt,
                    message: err.message.clone(),
                });
                if !rejected && !not_started {
                    let pending = track(AllocationUpdate::Pending);
                    if let Some(staging) = staging {
                        staging.keep();
                    }
                    pending?;
                    on_event(ProvisionEvent::CleanupPending { endpoint: None });
                    return Err(Error::new(
                        ErrorKind::OrphanSuspected,
                        format!("Allocation endpoint is unconfirmed; inspect the account before retrying. {}", err.message),
                    ).with("pending_stop", true));
                }
                track(AllocationUpdate::Unassigned)?;
                if !rejected || err.kind != ErrorKind::CapacityRefused {
                    on_event(ProvisionEvent::Failed {
                        message: err.message.clone(),
                    });
                    return Err(err);
                }
                last_err = Some(err);
                if attempt < max_attempts {
                    let delay = if backend.is_mock() {
                        0
                    } else {
                        spec.retry_delay_secs
                    };
                    on_event(ProvisionEvent::Retrying {
                        attempt: attempt + 1,
                        delay_secs: delay,
                    });
                    if delay > 0 {
                        sleep_cancellable(Duration::from_secs(delay), cancel)?;
                    }
                }
                continue;
            }
        };

        // The private session record keeps the hardware probe on this assignment.
        let hw = if spec.accelerator.is_gpu() {
            let probe = |file: &Path| {
                backend.probe_hardware(account_id, auth_mode, file, &spec.name, cancel)
            };
            let result = if backend.is_mock() {
                probe(state_file)
            } else {
                match pin_session(state_file, &spec.name, Some(&endpoint)) {
                    Ok(pin) => probe(&pin.path().join("sessions.json")),
                    Err(error) => {
                        release_attempt(
                            backend,
                            account_id,
                            auth_mode,
                            state_file,
                            scratch_state,
                            Some(endpoint),
                            track,
                            on_event,
                        )?;
                        return Err(error);
                    }
                }
            };
            result.unwrap_or_default()
        } else {
            HardwareProbe::default()
        };
        on_event(ProvisionEvent::Probed {
            attempt,
            gpu: hw.gpu.clone(),
            vram_mib: hw.vram_mib,
        });

        if cancel.is_cancelled() {
            on_event(ProvisionEvent::Releasing { attempt });
            release_attempt(
                backend,
                account_id,
                auth_mode,
                state_file,
                scratch_state,
                Some(endpoint),
                track,
                on_event,
            )?;
            return Err(Error::new(
                ErrorKind::Cancelled,
                "provisioning cancelled; VM released",
            ));
        }

        let meets_vram = match spec.min_vram() {
            None => true,
            Some(min_mib) => hw.vram_mib.is_some_and(|v| v >= min_mib),
        };

        if meets_vram {
            on_event(ProvisionEvent::Ready {
                name: spec.name.clone(),
                gpu: hw.gpu.clone(),
                vram_mib: hw.vram_mib,
                below_spec: false,
            });
            return Ok(ProvisionOutcome {
                name: spec.name.clone(),
                endpoint: Some(endpoint),
                hardware: hw,
                attempts: attempt,
                below_spec: false,
                started_at_secs,
            });
        }

        let min_vram = spec.min_vram().unwrap_or(0);
        on_event(ProvisionEvent::Rejected {
            attempt,
            gpu: hw.gpu.clone(),
            vram_mib: hw.vram_mib,
            min_vram_mib: min_vram,
        });

        if attempt == max_attempts && spec.on_exhausted == OnExhausted::KeepLast {
            on_event(ProvisionEvent::Ready {
                name: spec.name.clone(),
                gpu: hw.gpu.clone(),
                vram_mib: hw.vram_mib,
                below_spec: true,
            });
            return Ok(ProvisionOutcome {
                name: spec.name.clone(),
                endpoint: Some(endpoint),
                hardware: hw,
                attempts: attempt,
                below_spec: true,
                started_at_secs,
            });
        }

        // Release under-spec VM before retrying or failing.
        on_event(ProvisionEvent::Releasing { attempt });
        release_attempt(
            backend,
            account_id,
            auth_mode,
            state_file,
            scratch_state,
            Some(endpoint),
            track,
            on_event,
        )?;

        if hw.vram_mib.is_none() {
            return Err(Error::new(
                ErrorKind::Other,
                "minimum VRAM could not be verified; VM released",
            ));
        }
        last_err = None;
        if attempt < max_attempts {
            let delay = if backend.is_mock() {
                0
            } else {
                spec.retry_delay_secs
            };
            on_event(ProvisionEvent::Retrying {
                attempt: attempt + 1,
                delay_secs: delay,
            });
            if delay > 0 {
                sleep_cancellable(Duration::from_secs(delay), cancel)?;
            }
        }
    }

    let err = last_err.unwrap_or_else(|| {
        Error::new(
            ErrorKind::CapacityRefused,
            format!(
                "no VM met minimum VRAM ({} MiB) after {max_attempts} attempt(s)",
                spec.min_vram().unwrap_or(0)
            ),
        )
    });
    on_event(ProvisionEvent::Failed {
        message: err.message.clone(),
    });
    Err(err)
}

fn sleep_cancellable(dur: Duration, cancel: &CancelToken) -> Result<()> {
    let mut left = dur;
    while !left.is_zero() {
        cancel.check()?;
        let step = left.min(Duration::from_millis(100));
        thread::sleep(step);
        left = left.saturating_sub(step);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::accounts::ADC_ACCOUNT_ID;
    use crate::mock::MockBackend;

    #[test]
    fn ambiguous_allocation_does_not_reap_a_shared_name() {
        let mock = MockBackend::empty();
        mock.lose_allocation_reply
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let state = Path::new("fixture-sessions.json");
        let spec = ProvisionSpec::from_defaults("ambiguous", &Defaults::default());
        let mut events = Vec::new();
        let cancel = CancelToken::new();
        let error = provision(
            &mock,
            ADC_ACCOUNT_ID,
            AuthMode::Adc,
            state,
            state,
            &spec,
            SessionLimit {
                max_concurrent: 2,
                reserved: 0,
            },
            &cancel,
            &mut |event| events.push(event),
        )
        .unwrap_err();
        assert_eq!(error.kind, ErrorKind::OrphanSuspected);
        assert!(events.contains(&ProvisionEvent::CleanupPending { endpoint: None }));
        assert!(!stop_and_confirm(
            &mock,
            ADC_ACCOUNT_ID,
            AuthMode::Adc,
            state,
            state,
            None,
            &cancel
        )
        .unwrap());
        let live = mock
            .sessions(ADC_ACCOUNT_ID, AuthMode::Adc, state, &cancel)
            .unwrap();
        assert_eq!(live[0].name.as_deref(), Some("ambiguous"));
    }

    #[test]
    fn provision_retries_a100_40gb_to_get_80gb() {
        let mock = MockBackend::empty();
        let spec = ProvisionSpec {
            name: "a100-big".into(),
            accelerator: Accelerator::A100,
            high_mem: true,
            min_vram_mib: Some(70_000),
            max_attempts: 3,
            retry_delay_secs: 0,
            on_exhausted: OnExhausted::StopAndFail,
        };
        let mut events = Vec::new();
        let out = provision(
            &mock,
            ADC_ACCOUNT_ID,
            AuthMode::Adc,
            Path::new("/tmp/mock-state.json"),
            Path::new("/tmp/mock-scratch.json"),
            &spec,
            SessionLimit {
                max_concurrent: 2,
                reserved: 0,
            },
            &CancelToken::new(),
            &mut |e| events.push(e),
        )
        .expect("second attempt yields 80GB A100");
        assert_eq!(out.attempts, 2);
        assert_eq!(out.hardware.vram_mib, Some(81_920));
        assert!(!out.below_spec);
        assert!(events
            .iter()
            .any(|e| matches!(e, ProvisionEvent::Rejected { attempt: 1, .. })));
    }

    #[test]
    fn provision_enforces_concurrency_cap_and_orphan_reap() {
        let mock = MockBackend::demo(); // already has 2 sessions!
        let spec = ProvisionSpec::from_defaults("third-vm", &Defaults::default());
        let err = provision(
            &mock,
            ADC_ACCOUNT_ID,
            AuthMode::Adc,
            Path::new("/tmp/mock-state.json"),
            Path::new("/tmp/mock-scratch.json"),
            &spec,
            SessionLimit {
                max_concurrent: 2,
                reserved: 0,
            },
            &CancelToken::new(),
            &mut |_| {},
        )
        .unwrap_err();
        assert_eq!(err.kind, ErrorKind::SessionLimitReached);

        // Reap the browser `[?]` orphan in demo mode, freeing a slot.
        let reaped = mock
            .reap_endpoint(
                ADC_ACCOUNT_ID,
                AuthMode::Adc,
                Path::new("/tmp/mock-scratch.json"),
                "m-s-demo-web9",
                &CancelToken::new(),
            )
            .unwrap();
        assert!(reaped);

        let out = provision(
            &mock,
            ADC_ACCOUNT_ID,
            AuthMode::Adc,
            Path::new("/tmp/mock-state.json"),
            Path::new("/tmp/mock-scratch.json"),
            &spec,
            SessionLimit {
                max_concurrent: 2,
                reserved: 0,
            },
            &CancelToken::new(),
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(out.hardware.vram_mib, Some(97_887));
    }
}
