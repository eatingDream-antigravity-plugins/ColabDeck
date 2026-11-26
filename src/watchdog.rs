//! [DP5.1] VM-side detached watchdog deployment & hot-reload controller.
//!
//! Embeds `assets/watchdog.py` as base64 and sends numeric timeouts via `--env`
//! so user inputs are never interpolated into Python code. Maps VM clock deadlines
//! back onto local clock time cleanly.

use crate::cli::{ColabBackend, RESULT_MARKER};
use crate::error::{Error, ErrorKind, Result};
use crate::sessions::WatchdogState;
use crate::settings::{AuthMode, GuardPolicy};
use crate::util::{b64_encode, now_secs, CancelToken};
use serde_json::Value;
use std::path::Path;

pub const WATCHDOG_PY: &str = include_str!("../assets/watchdog.py");
pub const VM_BASE: &str = "/root/.colab-mcp";
pub const HEARTBEAT_PATH: &str = "/root/.colab-mcp/heartbeat";
pub const ENV_DEADLINE: &str = "CMCP_WD_DEADLINE_S";
pub const ENV_IDLE: &str = "CMCP_WD_IDLE_S";
pub const ENV_GRACE: &str = "CMCP_WD_GRACE_S";
pub const ENV_INSTANCE: &str = "CMCP_WD_INSTANCE";

fn header() -> String {
    format!(
        r#"import base64, fcntl, json, math, os, re, subprocess, sys, tempfile, time
B = "{VM_BASE}"
os.makedirs(B, mode=0o700, exist_ok=True)
_CONFIG = B + "/watchdog.json"
_SCRIPT = B + "/watchdog.py"
def _cmcp_read():
    try:
        with open(_CONFIG) as fh:
            value = json.load(fh)
        return value if isinstance(value, dict) else {{}}
    except (OSError, ValueError):
        return {{}}
def _cmcp_pid(instance, active=True):
    try:
        legacy = instance is None
        if not legacy and not re.fullmatch(r"[A-Za-z0-9-]+", instance):
            return None
        path = B + f"/watchdog.{{instance}}.pid"
        try:
            with open(path) as fh:
                pid, start, token, ready = json.load(fh)
            if active and not ready:
                return None
        except FileNotFoundError:
            with open(B + "/watchdog.pid") as fh:
                old = json.load(fh)
            if legacy:
                pid, start, token = old, None, None
            else:
                pid, start, token = old
        if token != instance or not isinstance(pid, int) or pid <= 1:
            return None
        with open(f"/proc/{{pid}}/stat") as fh:
            stat = fh.read().rsplit(") ", 1)[1].split()
        with open(f"/proc/{{pid}}/cmdline", "rb") as fh:
            argv = fh.read().split(b"\0")
        suffix = [os.fsencode(_SCRIPT), b"--config", os.fsencode(_CONFIG)]
        if not legacy:
            suffix += [b"--instance", instance.encode()]
        if stat[0] not in ("X", "Z") and (legacy or stat[19] == start) and argv[1:] == suffix + [b""]:
            return pid
    except (OSError, ValueError, IndexError, TypeError):
        pass
    return None
def _cmcp_alive(cfg):
    return bool(cfg) and _cmcp_pid(cfg.get("instance")) is not None
def _cmcp_replace(path, data):
    fd, tmp = tempfile.mkstemp(dir=B)
    try:
        with os.fdopen(fd, "wb") as fh:
            fh.write(data)
        os.replace(tmp, path)
    finally:
        if os.path.exists(tmp):
            os.unlink(tmp)
def _cmcp_write(cfg, path=_CONFIG):
    _cmcp_replace(path, json.dumps(cfg, allow_nan=False).encode())
def _cmcp_num(k):
    v = os.environ.pop(k, "none")
    return None if v in ("", "none") else float(v)
def _cmcp_emit(cfg, alive):
    print("{RESULT_MARKER}" + json.dumps({{
        "now": time.time(),
        "deadline_epoch": cfg.get("deadline_epoch"),
        "idle_timeout_s": cfg.get("idle_timeout_s"),
        "cancelled": bool(cfg.get("cancelled")),
        "runtime_addr": bool(cfg.get("valid_runtime_addr")),
        "instance": cfg.get("instance"),
        "policy_runtime_s": cfg.get("policy_runtime_s"),
        "started": cfg.get("started"),
        "grace_s": cfg.get("grace_s"),
        "alive": alive,
    }}))
"#
    )
}

fn locked_snippet(body: &str) -> String {
    let mut code = header();
    code.push_str("\nwith open(B + \"/watchdog.lock\", \"a\") as lock:\n    fcntl.flock(lock, fcntl.LOCK_EX)\n");
    for line in body.lines() {
        code.push_str("    ");
        code.push_str(line);
        code.push('\n');
    }
    code
}

pub fn start_snippet() -> String {
    let b64 = b64_encode(WATCHDOG_PY.as_bytes());
    locked_snippet(&format!(
        r#"_script = base64.b64decode("{b64}", validate=True)
_ns = {{"__name__": "_colab_watchdog"}}
exec(compile(_script, _SCRIPT, "exec"), _ns)
_dl = _cmcp_num("{ENV_DEADLINE}")
_idle = _cmcp_num("{ENV_IDLE}")
_grace = _cmcp_num("{ENV_GRACE}")
_addr = os.environ.get("TBE_RUNTIME_ADDR", "")
_instance = os.environ.pop("{ENV_INSTANCE}")
_now = time.time()
_cfg = {{
    "deadline_epoch": (_now + _dl) if _dl is not None else None,
    "idle_timeout_s": _idle,
    "cancelled": False,
    "valid_runtime_addr": bool(_ns["runtime_url"](_addr)),
    "instance": _instance,
    "policy_runtime_s": _dl,
    "heartbeat": "{HEARTBEAT_PATH}",
    "log": B + "/watchdog.log",
    "poll_s": 15,
    "grace_s": _grace if _grace is not None else 300,
    "started": _now,
}}
if not _cfg["valid_runtime_addr"] or not _ns["valid_config"](_cfg, _instance):
    raise ValueError("invalid watchdog policy or Colab runtime address")
_previous = _cmcp_read()
_cmcp_replace(_SCRIPT, _script)
def _activate(cfg, rollback):
    instance = cfg["instance"]
    candidate = B + f"/watchdog.{{instance}}.candidate"
    _cmcp_write(cfg, candidate)
    child = None
    selected = False
    try:
        child = subprocess.Popen(
            [sys.executable, _SCRIPT, "--config", _CONFIG, "--instance", instance],
            env={{"PATH": os.defpath + ":/usr/local/bin", "TBE_RUNTIME_ADDR": _addr}},
            stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
            start_new_session=True, close_fds=True,
        )
        def await_child(active):
            for _ in range(30):
                if child.poll() is not None:
                    break
                if _cmcp_pid(instance, active) == child.pid:
                    return
                time.sleep(0.1)
            raise RuntimeError("watchdog did not acknowledge startup")
        await_child(False)
        _cmcp_write(cfg)
        selected = True
        await_child(True)
    except Exception:
        if selected:
            _cmcp_write(rollback)
        if child is not None and child.poll() is None:
            child.terminate()
        raise
    finally:
        try:
            os.unlink(candidate)
        except FileNotFoundError:
            pass

try:
    _activate(_cfg, _previous)
except Exception:
    # Older watchdogs can exit as soon as the candidate policy is selected.
    # Restore the original absolute deadline and idle clock if that happened.
    if (_previous and not _previous.get("cancelled") and _previous.get("valid_runtime_addr")
            and not _cmcp_alive(_previous)):
        restored = {{**_cfg, **_previous, "instance": _instance + "-restore"}}
        try:
            if not _ns["valid_config"](restored, restored["instance"]):
                raise ValueError("invalid previous policy")
            _activate(restored, _previous)
        except Exception:
            raise RuntimeError("watchdog rollback could not re-arm the previous policy") from None
    raise
_old = _cmcp_pid(_previous.get("instance")) if _previous else None
if _old is not None:
    try:
        os.kill(_old, 15)
    except ProcessLookupError:
        pass
_cmcp_emit(_cfg, _cmcp_alive(_cfg))
"#
    ))
}

pub fn extend_snippet() -> String {
    locked_snippet(&format!(
        r#"_cfg = _cmcp_read()
_dl = _cmcp_num("{ENV_DEADLINE}")
if _dl is None or not math.isfinite(_dl) or _dl <= 0:
    raise ValueError("invalid watchdog deadline")
if _cmcp_alive(_cfg) and not _cfg.get("cancelled") and _cfg.get("valid_runtime_addr"):
    _cfg["deadline_epoch"] = time.time() + _dl
    _cmcp_write(_cfg)
_cmcp_emit(_cfg, _cmcp_alive(_cfg))
"#
    ))
}

pub fn cancel_snippet() -> String {
    locked_snippet(
        r#"_cfg = _cmcp_read()
_cfg["cancelled"] = True
_cmcp_write(_cfg)
_cmcp_emit(_cfg, _cmcp_alive(_cfg))
"#,
    )
}

pub fn status_snippet() -> String {
    locked_snippet(
        r#"_cfg = _cmcp_read()
_cmcp_emit(_cfg, _cmcp_alive(_cfg))
"#,
    )
}

pub fn parse_reply(v: &Value, local_now_secs: u64) -> Result<WatchdogState> {
    let vm_now = v
        .get("now")
        .and_then(|x| x.as_f64())
        .ok_or_else(|| Error::new(ErrorKind::Parse, "watchdog reply missing `now`"))?;
    let deadline_secs = v.get("deadline_epoch").and_then(|x| x.as_f64()).map(|dl| {
        let delta = (dl - vm_now).round() as i64;
        if delta >= 0 {
            local_now_secs.saturating_add(delta as u64)
        } else {
            local_now_secs.saturating_sub(delta.unsigned_abs())
        }
    });
    let idle_timeout_secs = v
        .get("idle_timeout_s")
        .and_then(|x| x.as_f64())
        .filter(|&s| s > 0.0)
        .map(|s| s.round() as u64);
    let cancelled = v
        .get("cancelled")
        .and_then(|x| x.as_bool())
        .unwrap_or(false);
    let alive = v.get("alive").and_then(|x| x.as_bool()).unwrap_or(false);
    let runtime_addr = v
        .get("runtime_addr")
        .and_then(|x| x.as_bool())
        .unwrap_or(false);
    Ok(WatchdogState {
        deadline_secs,
        idle_timeout_secs,
        cancelled,
        armed: alive
            && !cancelled
            && runtime_addr
            && (deadline_secs.is_some() || idle_timeout_secs.is_some()),
    })
}

#[allow(clippy::too_many_arguments)]
pub fn start_watchdog(
    backend: &dyn ColabBackend,
    account_id: &str,
    auth_mode: AuthMode,
    state_file: &Path,
    session_name: &str,
    policy: &GuardPolicy,
    grace_secs: u64,
    cancel: &CancelToken,
) -> Result<WatchdogState> {
    policy.validate()?;
    if !policy.needs_watchdog() {
        return Err(Error::invalid(
            "watchdog",
            "a runtime or idle limit is required to arm the watchdog",
        ));
    }
    use std::sync::atomic::{AtomicU64, Ordering};
    static INSTANCE: AtomicU64 = AtomicU64::new(0);
    let instance = format!(
        "wd-{}-{}-{}",
        std::process::id(),
        crate::util::now_millis(),
        INSTANCE.fetch_add(1, Ordering::Relaxed)
    );
    let env = vec![
        (ENV_INSTANCE.into(), instance.clone()),
        (
            ENV_DEADLINE.into(),
            policy
                .max_runtime_secs
                .filter(|&s| s > 0)
                .map(|s| s.to_string())
                .unwrap_or_else(|| "none".into()),
        ),
        (
            ENV_IDLE.into(),
            policy
                .idle_timeout_secs
                .filter(|&s| s > 0)
                .map(|s| s.to_string())
                .unwrap_or_else(|| "none".into()),
        ),
        (ENV_GRACE.into(), grace_secs.to_string()),
    ];
    let out = backend.exec_code(
        account_id,
        auth_mode,
        state_file,
        session_name,
        &start_snippet(),
        &env,
        35,
        cancel,
    )?;
    let val = out
        .result
        .ok_or_else(|| Error::new(ErrorKind::Parse, "watchdog start returned no status JSON"))?;
    let watchdog = parse_reply(&val, now_secs())?;
    let effective_runtime = val["deadline_epoch"]
        .as_f64()
        .zip(val["started"].as_f64())
        .map(|(deadline, started)| (deadline - started).round());
    if !watchdog.armed
        || effective_runtime != policy.max_runtime().map(|d| d.as_secs_f64())
        || val["grace_s"].as_f64() != Some(grace_secs as f64)
        || val["instance"].as_str() != Some(instance.as_str())
        || val["policy_runtime_s"].as_f64() != policy.max_runtime().map(|d| d.as_secs_f64())
        || watchdog.idle_timeout_secs != policy.idle_timeout().map(|d| d.as_secs())
        || watchdog.deadline_secs.is_some() != policy.max_runtime().is_some()
    {
        return Err(Error::new(
            ErrorKind::Other,
            "watchdog did not confirm its instance, runtime address and requested policy",
        ));
    }
    Ok(watchdog)
}

pub fn extend_watchdog(
    backend: &dyn ColabBackend,
    account_id: &str,
    auth_mode: AuthMode,
    state_file: &Path,
    session_name: &str,
    new_remaining_secs: u64,
    cancel: &CancelToken,
) -> Result<WatchdogState> {
    if new_remaining_secs == 0 {
        return Err(Error::invalid(
            "deadline",
            "watchdog runtime must be positive",
        ));
    }
    let env = vec![(ENV_DEADLINE.into(), new_remaining_secs.to_string())];
    let out = backend.exec_code(
        account_id,
        auth_mode,
        state_file,
        session_name,
        &extend_snippet(),
        &env,
        25,
        cancel,
    )?;
    let val = out
        .result
        .ok_or_else(|| Error::new(ErrorKind::Parse, "watchdog extend returned no status JSON"))?;
    parse_reply(&val, now_secs())
}

pub fn cancel_watchdog(
    backend: &dyn ColabBackend,
    account_id: &str,
    auth_mode: AuthMode,
    state_file: &Path,
    session_name: &str,
    cancel: &CancelToken,
) -> Result<WatchdogState> {
    let out = backend.exec_code(
        account_id,
        auth_mode,
        state_file,
        session_name,
        &cancel_snippet(),
        &[],
        25,
        cancel,
    )?;
    let val = out
        .result
        .ok_or_else(|| Error::new(ErrorKind::Parse, "watchdog cancel returned no status JSON"))?;
    let status = parse_reply(&val, now_secs())?;
    if !status.cancelled {
        return Err(Error::new(
            ErrorKind::Other,
            "watchdog did not confirm cancellation",
        ));
    }
    Ok(status)
}

pub fn watchdog_status(
    backend: &dyn ColabBackend,
    account_id: &str,
    auth_mode: AuthMode,
    state_file: &Path,
    session_name: &str,
    cancel: &CancelToken,
) -> Result<WatchdogState> {
    let reply = backend.exec_code(
        account_id,
        auth_mode,
        state_file,
        session_name,
        &status_snippet(),
        &[],
        15,
        cancel,
    )?;
    let value = reply
        .result
        .ok_or_else(|| Error::new(ErrorKind::Parse, "watchdog status returned no JSON"))?;
    parse_reply(&value, now_secs())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::accounts::ADC_ACCOUNT_ID;
    use crate::mock::MockBackend;

    #[test]
    fn watchdog_start_extend_cancel_roundtrip_on_mock() {
        let mock = MockBackend::demo();
        let policy = GuardPolicy::default();
        let st = start_watchdog(
            &mock,
            ADC_ACCOUNT_ID,
            AuthMode::Adc,
            Path::new("/tmp/s.json"),
            "c-g4-train",
            &policy,
            300,
            &CancelToken::new(),
        )
        .unwrap();
        assert!(st.armed);
        assert!(st.deadline_secs.is_some());

        let ext = extend_watchdog(
            &mock,
            ADC_ACCOUNT_ID,
            AuthMode::Adc,
            Path::new("/tmp/s.json"),
            "c-g4-train",
            14_400,
            &CancelToken::new(),
        )
        .unwrap();
        assert!(ext.armed);
        assert!(ext.deadline_secs.unwrap() > st.deadline_secs.unwrap());

        let can = cancel_watchdog(
            &mock,
            ADC_ACCOUNT_ID,
            AuthMode::Adc,
            Path::new("/tmp/s.json"),
            "c-g4-train",
            &CancelToken::new(),
        )
        .unwrap();
        assert!(can.cancelled);
        assert!(!can.armed);
    }
}
