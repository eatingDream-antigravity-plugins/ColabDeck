//! [DP2.2] Account profile store & per-account isolated `--config` paths.
//!
//! Stores ONLY non-secret account metadata in `${PLUGIN_DATA}/accounts.json`.
//! Supports local Application Default Credentials and isolated OAuth profiles,
//! assigning every account its own isolated CLI state file so `colab sessions`
//! on account B can never prune account A's local session records.

use crate::error::{Error, ErrorKind, Result};
use crate::settings::AuthMode;
use crate::util::{now_secs, write_atomic_private};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

pub const ADC_ACCOUNT_ID: &str = "adc-default";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountProfile {
    pub id: String,
    pub label: String,
    pub email: Option<String>,
    pub auth_mode: AuthMode,
    pub added_at: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountRegistry {
    pub active_id: String,
    pub accounts: Vec<AccountProfile>,
}

impl AccountRegistry {
    pub fn default_with_adc() -> Self {
        Self {
            active_id: ADC_ACCOUNT_ID.to_owned(),
            accounts: vec![AccountProfile {
                id: ADC_ACCOUNT_ID.to_owned(),
                label: "Local ADC".to_owned(),
                email: None,
                auth_mode: AuthMode::Adc,
                added_at: now_secs(),
            }],
        }
    }

    pub fn active(&self) -> Option<&AccountProfile> {
        self.accounts.iter().find(|a| a.id == self.active_id)
    }

    pub fn get(&self, id: &str) -> Option<&AccountProfile> {
        self.accounts.iter().find(|a| a.id == id)
    }

    pub(crate) fn upsert(&mut self, profile: AccountProfile, activate: bool) -> Result<()> {
        validate_account_id(&profile.id)?;
        if activate {
            self.active_id.clone_from(&profile.id);
        }
        if let Some(existing) = self.accounts.iter_mut().find(|a| a.id == profile.id) {
            *existing = profile;
        } else {
            self.accounts.push(profile);
        }
        Ok(())
    }

    pub(crate) fn switch(&mut self, account_id: &str) -> Result<()> {
        if self.get(account_id).is_none() {
            return Err(Error::invalid("account_id", "account is not registered"));
        }
        self.active_id = account_id.to_owned();
        Ok(())
    }

    pub(crate) fn remove(&mut self, account_id: &str) -> Result<()> {
        if account_id == ADC_ACCOUNT_ID {
            return Err(Error::invalid(
                "account_id",
                "cannot remove built-in ADC profile",
            ));
        }
        let before = self.accounts.len();
        self.accounts.retain(|a| a.id != account_id);
        if self.accounts.len() == before {
            return Err(Error::invalid("account_id", "account is not registered"));
        }
        if self.active_id == account_id {
            self.active_id = self
                .accounts
                .first()
                .map(|a| a.id.clone())
                .unwrap_or_else(|| ADC_ACCOUNT_ID.to_owned());
        }
        Ok(())
    }
}

pub fn validate_account_id(id: &str) -> Result<()> {
    let ok = !id.is_empty()
        && id.len() <= 32
        && id
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if ok {
        Ok(())
    } else {
        Err(Error::invalid(
            "account_id",
            format!("invalid account id {id:?}"),
        ))
    }
}

#[derive(Debug, Clone)]
pub struct AccountStore {
    data_dir: PathBuf,
    path: PathBuf,
}

impl AccountStore {
    pub fn new(data_dir: &Path) -> Self {
        Self {
            data_dir: data_dir.to_path_buf(),
            path: data_dir.join("accounts.json"),
        }
    }

    /// Isolated CLI `--config` state file for `account_id`.
    pub fn cli_state_file(&self, account_id: &str) -> PathBuf {
        let safe: String = account_id
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
            .collect();
        let name = if safe.is_empty() {
            ADC_ACCOUNT_ID.to_owned()
        } else {
            safe
        };
        self.data_dir
            .join("accounts")
            .join(name)
            .join("sessions.json")
    }

    pub fn load(&self) -> Result<AccountRegistry> {
        let recovery = |kind| {
            Error::new(
                kind,
                "accounts registry degraded; cannot read accounts.json",
            )
        };
        match fs::read(&self.path) {
            Ok(raw) => {
                let registry: AccountRegistry =
                    serde_json::from_slice(&raw).map_err(|_| recovery(ErrorKind::Parse))?;
                if registry.active().is_none()
                    || registry
                        .accounts
                        .iter()
                        .enumerate()
                        .any(|(index, profile)| {
                            validate_account_id(&profile.id).is_err()
                                || registry.accounts[..index]
                                    .iter()
                                    .any(|p| p.id == profile.id)
                        })
                {
                    return Err(recovery(ErrorKind::Parse));
                }
                Ok(registry)
            }
            Err(error)
                if error.kind() == io::ErrorKind::NotFound
                    && fs::symlink_metadata(&self.path)
                        .is_err_and(|error| error.kind() == io::ErrorKind::NotFound) =>
            {
                Ok(AccountRegistry::default_with_adc())
            }
            Err(_) => Err(recovery(ErrorKind::Io)),
        }
    }

    pub(crate) fn exists(&self) -> bool {
        self.path.symlink_metadata().is_ok()
    }

    pub fn save(&self, reg: &AccountRegistry) -> Result<()> {
        self.load()?;
        let bytes = serde_json::to_vec_pretty(reg)?;
        write_atomic_private(&self.path, &bytes)
    }

    pub fn upsert(&self, profile: AccountProfile, activate: bool) -> Result<AccountRegistry> {
        let mut registry = self.load()?;
        registry.upsert(profile, activate)?;
        self.save(&registry)?;
        Ok(registry)
    }

    pub fn switch(&self, account_id: &str) -> Result<AccountRegistry> {
        let mut registry = self.load()?;
        registry.switch(account_id)?;
        self.save(&registry)?;
        Ok(registry)
    }

    pub fn remove(&self, account_id: &str) -> Result<AccountRegistry> {
        let mut registry = self.load()?;
        registry.remove(account_id)?;
        self.save(&registry)?;
        Ok(registry)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_registry_isolation_and_switch() {
        let dir = std::env::temp_dir().join(format!("colab-mcp-acc-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let store = AccountStore::new(&dir);

        let reg = store.load().unwrap();
        assert_eq!(reg.active_id, ADC_ACCOUNT_ID);
        let p_adc = store.cli_state_file(ADC_ACCOUNT_ID);
        let p_work = store.cli_state_file("work-oauth");
        assert_ne!(p_adc, p_work);

        store
            .upsert(
                AccountProfile {
                    id: "work-oauth".into(),
                    label: "Work Pro+".into(),
                    email: Some(["fixture", "@", "example.invalid"].concat()),
                    auth_mode: AuthMode::OAuth2,
                    added_at: 100,
                },
                true,
            )
            .unwrap();
        let reg2 = store.load().unwrap();
        assert_eq!(reg2.active_id, "work-oauth");
        assert_eq!(reg2.accounts.len(), 2);

        let reg3 = store.remove("work-oauth").unwrap();
        assert_eq!(reg3.active_id, ADC_ACCOUNT_ID);
        assert!(store.remove(ADC_ACCOUNT_ID).is_err());

        let _ = fs::remove_dir_all(&dir);
    }
}
