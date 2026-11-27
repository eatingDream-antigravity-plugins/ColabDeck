//! [DP1] Single-source-of-truth configuration, defaults, and atomic persistence.
//!
//! Strictly Layer 1 in the MIT Axiomatic Design matrix: depends ONLY on
//! `error` and `util`, never on CLI, sessions, usage, guards, tunnels, or MCP.

use crate::error::{Error, Result};
use crate::util::{lock_file, write_atomic_private};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Hardware accelerators supported by `colab new`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Accelerator {
    None,
    T4,
    L4,
    #[default]
    G4,
    A100,
    H100,
    #[serde(rename = "V5E1")]
    TpuV5e1,
    #[serde(rename = "V6E1")]
    TpuV6e1,
}

impl Accelerator {
    pub const ALL: [Accelerator; 8] = [
        Accelerator::None,
        Accelerator::T4,
        Accelerator::L4,
        Accelerator::G4,
        Accelerator::A100,
        Accelerator::H100,
        Accelerator::TpuV5e1,
        Accelerator::TpuV6e1,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "NONE",
            Self::T4 => "T4",
            Self::L4 => "L4",
            Self::G4 => "G4",
            Self::A100 => "A100",
            Self::H100 => "H100",
            Self::TpuV5e1 => "V5E1",
            Self::TpuV6e1 => "V6E1",
        }
    }

    pub fn from_label(label: &str) -> Option<Self> {
        let up = label.trim().to_ascii_uppercase();
        if up == "CPU" || up == "NONE" {
            return Some(Self::None);
        }
        Self::ALL.into_iter().find(|a| a.as_str() == up)
    }

    pub fn new_flags(self) -> Vec<String> {
        match self {
            Self::None => vec![],
            Self::TpuV5e1 => vec!["--tpu".into(), "v5e1".into()],
            Self::TpuV6e1 => vec!["--tpu".into(), "v6e1".into()],
            gpu => vec!["--gpu".into(), gpu.as_str().into()],
        }
    }

    pub fn is_gpu(self) -> bool {
        matches!(
            self,
            Self::T4 | Self::L4 | Self::G4 | Self::A100 | Self::H100
        )
    }

    pub fn is_tpu(self) -> bool {
        matches!(self, Self::TpuV5e1 | Self::TpuV6e1)
    }

    /// Accelerators that only offer a single machine shape ignore `--high-mem`.
    pub fn supports_high_mem(self) -> bool {
        !matches!(self, Self::L4 | Self::TpuV5e1 | Self::TpuV6e1)
    }
}

/// Authentication strategy for `colab` CLI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AuthMode {
    /// Local Google Application Default Credentials (`colab --auth=adc`).
    #[default]
    Adc,
    /// Multi-account OAuth2 keyring mode (`colab --auth=oauth2`).
    OAuth2,
}

impl AuthMode {
    pub fn cli_flag(self) -> &'static str {
        match self {
            Self::Adc => "adc",
            Self::OAuth2 => "oauth2",
        }
    }
}

/// Execution backend selection: real local `colab` CLI vs deterministic zero-cost demo.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum BackendMode {
    #[default]
    Real,
    Mock,
}

/// Guard policy applied per session (values in seconds / compute units).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GuardPolicy {
    pub max_runtime_secs: Option<u64>,
    pub max_spend_cu: Option<f64>,
    pub min_balance_cu: Option<f64>,
    pub idle_timeout_secs: Option<u64>,
}

impl Default for GuardPolicy {
    fn default() -> Self {
        Self {
            max_runtime_secs: Some(2 * 3600),
            max_spend_cu: Some(25.0),
            min_balance_cu: Some(5.0),
            idle_timeout_secs: Some(30 * 60),
        }
    }
}

impl GuardPolicy {
    pub fn unlimited() -> Self {
        Self {
            max_runtime_secs: None,
            max_spend_cu: None,
            min_balance_cu: None,
            idle_timeout_secs: None,
        }
    }

    pub fn max_runtime(&self) -> Option<Duration> {
        self.max_runtime_secs
            .filter(|&s| s > 0)
            .map(Duration::from_secs)
    }

    pub fn idle_timeout(&self) -> Option<Duration> {
        self.idle_timeout_secs
            .filter(|&s| s > 0)
            .map(Duration::from_secs)
    }

    pub fn needs_watchdog(&self) -> bool {
        self.max_runtime().is_some() || self.idle_timeout().is_some()
    }

    pub fn validate(&self) -> Result<()> {
        if self.max_runtime_secs == Some(0) {
            return Err(Error::invalid("max_runtime_secs", "must be positive"));
        }
        if self.idle_timeout_secs == Some(0) {
            return Err(Error::invalid("idle_timeout_secs", "must be positive"));
        }
        if let Some(s) = self.max_spend_cu {
            if !s.is_finite() || s <= 0.0 {
                return Err(Error::invalid("max_spend_cu", "must be > 0"));
            }
        }
        if let Some(b) = self.min_balance_cu {
            if !b.is_finite() || b < 0.0 {
                return Err(Error::invalid("min_balance_cu", "must be >= 0"));
            }
        }
        Ok(())
    }
}

/// All user-editable defaults live in one place and can be overridden per action.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Defaults {
    pub accelerator: Accelerator,
    pub high_mem: bool,
    pub min_vram_mib: Option<u32>,
    pub max_attempts: u32,
    pub retry_delay_secs: u64,
    pub max_concurrent_sessions: u32,
    pub guard: GuardPolicy,
    pub watchdog_grace_secs: u64,
    pub poll_active_secs: u64,
    pub poll_idle_secs: u64,
}

impl Default for Defaults {
    fn default() -> Self {
        Self {
            accelerator: Accelerator::G4,
            high_mem: false,
            min_vram_mib: None,
            max_attempts: 3,
            retry_delay_secs: 8,
            max_concurrent_sessions: 2,
            guard: GuardPolicy::default(),
            watchdog_grace_secs: 300,
            poll_active_secs: 30,
            poll_idle_secs: 300,
        }
    }
}

impl Defaults {
    pub fn usage_poll_interval(&self, has_sessions: bool) -> Duration {
        let secs = if has_sessions {
            self.poll_active_secs.clamp(10, 600)
        } else {
            self.poll_idle_secs.clamp(30, 3600)
        };
        Duration::from_secs(secs)
    }

    pub fn validate(&self) -> Result<()> {
        if self.max_attempts == 0 || self.max_attempts > 8 {
            return Err(Error::invalid("max_attempts", "must be 1..=8"));
        }
        if self.max_concurrent_sessions == 0 || self.max_concurrent_sessions > 8 {
            return Err(Error::invalid("max_concurrent_sessions", "must be 1..=8"));
        }
        if self.min_vram_mib.is_some_and(|m| m > 0) && !self.accelerator.is_gpu() {
            return Err(Error::invalid("min_vram_mib", "requires a GPU accelerator"));
        }
        self.guard.validate()?;
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub auth_mode: AuthMode,
    pub backend_mode: BackendMode,
    #[serde(alias = "lang")]
    pub locale: String,
    pub defaults: Defaults,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            auth_mode: AuthMode::Adc,
            backend_mode: BackendMode::Real,
            locale: "zh-CN".to_owned(),
            defaults: Defaults::default(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SettingsPatch {
    pub auth_mode: Option<AuthMode>,
    pub backend_mode: Option<BackendMode>,
    #[serde(alias = "lang")]
    pub locale: Option<String>,
    pub accelerator: Option<Accelerator>,
    pub high_mem: Option<bool>,
    #[serde(default, deserialize_with = "present_nullable")]
    pub min_vram_mib: Option<Option<u32>>,
    pub max_attempts: Option<u32>,
    pub retry_delay_secs: Option<u64>,
    pub max_concurrent_sessions: Option<u32>,
    #[serde(default, deserialize_with = "present_nullable")]
    pub max_runtime_secs: Option<Option<u64>>,
    #[serde(default, deserialize_with = "present_nullable")]
    pub max_spend_cu: Option<Option<f64>>,
    #[serde(default, deserialize_with = "present_nullable")]
    pub min_balance_cu: Option<Option<f64>>,
    #[serde(default, deserialize_with = "present_nullable")]
    pub idle_timeout_secs: Option<Option<u64>>,
    pub poll_active_secs: Option<u64>,
    pub poll_idle_secs: Option<u64>,
}

pub(crate) fn present_nullable<'de, T: Deserialize<'de>, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Option<Option<T>>, D::Error> {
    Option::<T>::deserialize(deserializer).map(Some)
}

/// File identity fingerprint for non-destructive live hot-reload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SettingsStamp {
    pub mtime_ns: i128,
    pub size: u64,
    pub ino: u64,
}

impl SettingsStamp {
    fn from_meta(meta: &fs::Metadata) -> Self {
        let mtime_ns = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_nanos() as i128);
        #[cfg(unix)]
        let ino = {
            use std::os::unix::fs::MetadataExt;
            meta.ino()
        };
        #[cfg(not(unix))]
        let ino = 0;
        Self {
            mtime_ns,
            size: meta.len(),
            ino,
        }
    }
}

/// Atomic file-backed Settings store (`${PLUGIN_DATA}/settings.json`).
#[derive(Debug, Clone)]
pub struct SettingsStore {
    path: PathBuf,
    lock_path: PathBuf,
}

impl SettingsStore {
    pub fn new(data_dir: &Path) -> Self {
        Self {
            path: data_dir.join("settings.json"),
            lock_path: data_dir.join("settings.lock"),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn stamp(&self) -> Option<SettingsStamp> {
        fs::symlink_metadata(&self.path)
            .ok()
            .filter(|meta| meta.file_type().is_file())
            .map(|meta| SettingsStamp::from_meta(&meta))
    }

    /// Non-destructive live hot-reload check. Tracks `(mtime_ns, size, ino)` so
    /// external edits to `settings.json` take effect on the next tick without
    /// ever renaming/quarantining a syntactically invalid file.
    pub fn reload_if_changed(
        &self,
        last_stamp: &mut Option<SettingsStamp>,
    ) -> Result<Option<Settings>> {
        let current = match fs::symlink_metadata(&self.path) {
            Ok(meta) if meta.file_type().is_file() => Some(SettingsStamp::from_meta(&meta)),
            Ok(_) => {
                return Err(Error::invalid(
                    "settings",
                    "settings.json must be a regular file",
                ))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(_) => return Err(Error::invalid("settings", "cannot stat settings.json")),
        };
        if current == *last_stamp {
            return Ok(None);
        }
        *last_stamp = current;
        if current.is_none() {
            return Ok(Some(Settings::default()));
        }
        self.load_current().map(Some)
    }

    /// Load settings; if corrupt JSON is encountered, move aside to `.corrupt` and fall back cleanly.
    pub fn load(&self) -> (Settings, bool) {
        let Ok(raw) = fs::read_to_string(&self.path) else {
            return (Settings::default(), false);
        };
        match serde_json::from_str::<Settings>(&raw) {
            Ok(mut s) => {
                if s.defaults.validate().is_err() {
                    s.defaults = Defaults::default();
                }
                (s, false)
            }
            Err(_) => {
                let corrupt = self.path.with_extension("json.corrupt");
                let _ = fs::rename(&self.path, corrupt);
                (Settings::default(), true)
            }
        }
    }

    pub fn save(&self, settings: &Settings) -> Result<()> {
        let _guard = lock_file(&self.lock_path)?;
        self.save_locked(settings)
    }

    pub fn apply_patch(&self, patch: SettingsPatch) -> Result<Settings> {
        let _guard = lock_file(&self.lock_path)?;
        let cur = self.load_current()?.patched(patch)?;
        self.save_locked(&cur)?;
        Ok(cur)
    }

    fn save_locked(&self, settings: &Settings) -> Result<()> {
        settings.defaults.validate()?;
        let payload = serde_json::to_vec_pretty(settings)?;
        write_atomic_private(&self.path, &payload)
    }

    pub(crate) fn load_current(&self) -> Result<Settings> {
        let settings = match fs::read(&self.path) {
            Ok(raw) => serde_json::from_slice::<Settings>(&raw)
                .map_err(|_| Error::invalid("settings", "cannot parse settings.json"))?,
            Err(error)
                if error.kind() == std::io::ErrorKind::NotFound
                    && self
                        .path
                        .symlink_metadata()
                        .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
            {
                Settings::default()
            }
            Err(_) => return Err(Error::invalid("settings", "cannot read settings.json")),
        };
        settings.defaults.validate()?;
        Ok(settings)
    }
}

impl Settings {
    pub(crate) fn patched(mut self, patch: SettingsPatch) -> Result<Self> {
        let cur = &mut self;
        if let Some(v) = patch.auth_mode {
            cur.auth_mode = v;
        }
        if let Some(v) = patch.backend_mode {
            cur.backend_mode = v;
        }
        if let Some(v) = patch.locale {
            if matches!(v.as_str(), "en" | "zh-CN") {
                cur.locale = v;
            }
        }
        if let Some(v) = patch.accelerator {
            cur.defaults.accelerator = v;
            if !v.is_gpu() {
                cur.defaults.min_vram_mib = None;
            }
        }
        if let Some(v) = patch.high_mem {
            cur.defaults.high_mem = v;
        }
        if let Some(v) = patch.min_vram_mib {
            cur.defaults.min_vram_mib = v.filter(|&m| m > 0);
        }
        if let Some(v) = patch.max_attempts {
            cur.defaults.max_attempts = v.clamp(1, 8);
        }
        if let Some(v) = patch.retry_delay_secs {
            cur.defaults.retry_delay_secs = v.clamp(1, 120);
        }
        if let Some(v) = patch.max_concurrent_sessions {
            cur.defaults.max_concurrent_sessions = v.clamp(1, 8);
        }
        if let Some(v) = patch.max_runtime_secs {
            cur.defaults.guard.max_runtime_secs = v.filter(|&secs| secs != 0);
        }
        if let Some(v) = patch.max_spend_cu {
            cur.defaults.guard.max_spend_cu = v.filter(|&cu| cu != 0.0);
        }
        if let Some(v) = patch.min_balance_cu {
            cur.defaults.guard.min_balance_cu = v;
        }
        if let Some(v) = patch.idle_timeout_secs {
            cur.defaults.guard.idle_timeout_secs = v.filter(|&secs| secs != 0);
        }
        if let Some(v) = patch.poll_active_secs {
            cur.defaults.poll_active_secs = v.clamp(10, 600);
        }
        if let Some(v) = patch.poll_idle_secs {
            cur.defaults.poll_idle_secs = v.clamp(30, 3600);
        }
        cur.defaults.validate()?;
        Ok(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_roundtrip_and_corrupt_quarantine() {
        let dir = std::env::temp_dir().join(format!("colab-mcp-s-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let store = SettingsStore::new(&dir);

        let (def, corrupt) = store.load();
        assert!(!corrupt);
        assert_eq!(def.auth_mode, AuthMode::Adc);
        assert_eq!(def.defaults.accelerator, Accelerator::G4);

        let patched = store
            .apply_patch(SettingsPatch {
                accelerator: Some(Accelerator::A100),
                min_vram_mib: Some(Some(70_000)),
                locale: Some("en".into()),
                ..Default::default()
            })
            .expect("patch ok");
        assert_eq!(patched.defaults.accelerator, Accelerator::A100);
        assert_eq!(patched.defaults.min_vram_mib, Some(70_000));
        assert_eq!(patched.locale, "en");

        fs::write(store.path(), "{not-json").unwrap();
        let (fallback, was_corrupt) = store.load();
        assert!(was_corrupt);
        assert_eq!(fallback.defaults.accelerator, Accelerator::G4);
        assert!(store.path().with_extension("json.corrupt").exists());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn accelerator_flags_and_high_mem_rules() {
        assert_eq!(Accelerator::G4.new_flags(), vec!["--gpu", "G4"]);
        for accelerator in [Accelerator::L4, Accelerator::TpuV5e1, Accelerator::TpuV6e1] {
            assert!(!accelerator.supports_high_mem());
        }
        assert!(Accelerator::G4.supports_high_mem());
        assert!(Accelerator::A100.supports_high_mem());
        assert_eq!(Accelerator::TpuV6e1.new_flags(), vec!["--tpu", "v6e1"]);
        assert_eq!(Accelerator::from_label("CPU"), Some(Accelerator::None));
    }

    #[test]
    fn reload_if_changed_hot_reloads_and_fails_closed_without_quarantine() {
        let dir = std::env::temp_dir().join(format!("colab-mcp-reload-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let store = SettingsStore::new(&dir);
        let initial = store
            .apply_patch(SettingsPatch {
                locale: Some("zh-CN".into()),
                ..Default::default()
            })
            .expect("seed settings");
        assert_eq!(initial.locale, "zh-CN");

        let mut stamp = store.stamp();
        assert_eq!(store.reload_if_changed(&mut stamp).unwrap(), None);

        // External edit using `lang` alias + polling cadence updates
        fs::write(
            store.path(),
            r#"{"lang":"en","backend_mode":"mock","defaults":{"poll_active_secs":15,"poll_idle_secs":120}}"#,
        )
        .unwrap();
        let reloaded = store
            .reload_if_changed(&mut stamp)
            .expect("valid reload")
            .expect("should detect changed file");
        assert_eq!(reloaded.locale, "en");
        assert_eq!(reloaded.backend_mode, BackendMode::Mock);
        assert_eq!(reloaded.defaults.poll_active_secs, 15);
        assert_eq!(reloaded.defaults.poll_idle_secs, 120);

        // Second check with same file identity is a no-op
        assert_eq!(store.reload_if_changed(&mut stamp).unwrap(), None);

        // Invalid syntax fails closed without quarantining or deleting settings.json
        fs::write(store.path(), "{broken json").unwrap();
        assert!(store.reload_if_changed(&mut stamp).is_err());
        assert!(store.path().exists());
        assert!(!store.path().with_extension("json.corrupt").exists());

        let _ = fs::remove_dir_all(&dir);
    }
}
