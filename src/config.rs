//! Optional config file with named profiles (`~/.config/lexware/config.json`).
//!
//! The binary works without it: `LXW_API_KEY` alone is enough. Secrets live
//! in the OS credential store (see `secrets`) unless a profile opted into the
//! file store. The file is written with mode 0600 and all read-modify-write
//! cycles hold an exclusive lock, so parallel processes cannot lose rotated
//! OAuth refresh tokens.

use crate::error::{CliError, Kind};
use crate::secrets::{self, Secrets};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

pub const DEFAULT_BASE_URL: &str = "https://api.lexware.io";
pub const DEFAULT_AUTH_URL: &str = "https://app.lexware.de";
pub const SANDBOX_BASE_URL: &str = "https://api.lexware-sandbox.io";
pub const SANDBOX_AUTH_URL: &str = "https://app.lexware-sandbox.de";
pub const DEFAULT_PROFILE: &str = "default";

#[derive(Debug, Default, Serialize, Deserialize, Clone)]
pub struct Config {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_profile: Option<String>,
    #[serde(default)]
    pub profiles: BTreeMap<String, Profile>,
}

#[derive(Debug, Default, Serialize, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AuthKind {
    #[default]
    ApiKey,
    Oauth,
}

/// Where a profile keeps its secrets.
#[derive(Debug, Default, Serialize, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SecretStore {
    /// The OS credential store (macOS Keychain, Secret Service).
    #[default]
    Os,
    /// Plaintext in this file, chosen explicitly with `auth login --store file`.
    File,
}

#[derive(Debug, Default, Serialize, Deserialize, Clone)]
pub struct Profile {
    #[serde(default)]
    pub auth: AuthKind,
    #[serde(default)]
    pub secret_store: SecretStore,
    /// Kept in the file only for `SecretStore::File` profiles.
    #[serde(flatten)]
    pub secrets: Secrets,
    /// The API host these credentials were verified against; stored
    /// credentials are never sent anywhere else.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requests_per_second: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub burst: Option<u32>,
    // OAuth2 (Partner API) fields.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redirect_uri: Option<String>,
    /// Unix seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
}

impl Profile {
    /// `LXW_CREDENTIAL_STORE=file` keeps every profile out of the OS store
    /// (headless machines, tests).
    pub fn uses_os_store(&self) -> bool {
        self.secret_store == SecretStore::Os && !secrets::disabled_by_env()
    }

    /// The API host stored credentials may be sent to.
    pub fn bound_base_url(&self) -> &str {
        self.base_url.as_deref().unwrap_or(DEFAULT_BASE_URL)
    }

    pub fn load_secrets(&self, name: &str) -> Result<Secrets, CliError> {
        if self.uses_os_store() {
            Ok(secrets::read(name)?.unwrap_or_default())
        } else {
            Ok(self.secrets.clone())
        }
    }

    /// Saves `new` where this profile keeps secrets: the OS store right away,
    /// or this struct (persisted with the config file).
    pub fn store_secrets(&mut self, name: &str, new: Secrets) -> Result<(), CliError> {
        if self.uses_os_store() {
            secrets::write(name, &new)?;
            self.secrets = Secrets::default();
        } else {
            self.secrets = new;
        }
        Ok(())
    }
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

fn env_dir(var: &str) -> Option<PathBuf> {
    std::env::var_os(var).filter(|v| !v.is_empty()).map(PathBuf::from)
}

pub fn config_dir() -> PathBuf {
    env_dir("LXW_CONFIG_DIR")
        .or_else(|| env_dir("XDG_CONFIG_HOME").map(|d| d.join("lxw")))
        .or_else(|| home().map(|h| h.join(".config").join("lxw")))
        .unwrap_or_else(|| PathBuf::from(".lxw"))
}

pub fn cache_dir() -> PathBuf {
    env_dir("LXW_CACHE_DIR")
        .or_else(|| env_dir("XDG_CACHE_HOME").map(|d| d.join("lxw")))
        .or_else(|| home().map(|h| h.join(".cache").join("lxw")))
        .unwrap_or_else(|| std::env::temp_dir().join("lxw"))
}

pub fn config_path() -> PathBuf {
    config_dir().join("config.json")
}

impl Config {
    pub fn load() -> Result<Config, CliError> {
        load_from(&config_path())
    }

    /// Profile name from flag, `LXW_PROFILE`, the config default, or "default".
    pub fn profile_name(&self, flag: Option<&str>) -> String {
        flag.map(str::to_string)
            .or_else(|| std::env::var("LXW_PROFILE").ok().filter(|s| !s.is_empty()))
            .or_else(|| self.default_profile.clone())
            .unwrap_or_else(|| DEFAULT_PROFILE.to_string())
    }
}

fn load_from(path: &Path) -> Result<Config, CliError> {
    match fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text)
            .map_err(|e| CliError::new(Kind::Usage, format!("invalid config file {}: {e}", path.display()))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
        Err(e) => Err(e.into()),
    }
}

/// Exclusive lock around a config read-modify-write; released on drop.
pub struct ConfigLock {
    _file: File,
}

pub fn lock() -> Result<ConfigLock, CliError> {
    Ok(ConfigLock {
        _file: open_locked(&config_dir().join("config.lock"))?,
    })
}

/// Opens (creating it and its directory if needed) and exclusively locks a file;
/// the lock is released when the file is dropped. Shared by config and rate limiter.
pub fn open_locked(path: &Path) -> std::io::Result<File> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    file.lock()?;
    Ok(file)
}

/// Loads, modifies and atomically saves the config under the lock.
pub fn update<T>(f: impl FnOnce(&mut Config) -> Result<T, CliError>) -> Result<T, CliError> {
    let _lock = lock()?;
    let mut cfg = Config::load()?;
    let out = f(&mut cfg)?;
    save(&cfg)?;
    Ok(out)
}

fn save(cfg: &Config) -> Result<(), CliError> {
    let path = config_path();
    let dir = path.parent().expect("config path has a parent");
    fs::create_dir_all(dir)?;
    // Never let secrets of OS-store profiles reach the file.
    let mut cfg = cfg.clone();
    for profile in cfg.profiles.values_mut().filter(|p| p.uses_os_store()) {
        profile.secrets = Secrets::default();
    }
    let text = serde_json::to_string_pretty(&cfg).map_err(|e| CliError::internal(e.to_string()))?;
    let tmp = dir.join(format!("config.json.{}.tmp", std::process::id()));
    {
        let mut opts = OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut file = opts.open(&tmp)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
    }
    fs::rename(&tmp, &path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_round_trips_through_json() {
        let mut cfg = Config::default();
        cfg.profiles.insert(
            "default".into(),
            Profile {
                secret_store: SecretStore::File,
                secrets: Secrets {
                    api_key: Some("k".into()),
                    ..Default::default()
                },
                requests_per_second: Some(1.5),
                ..Default::default()
            },
        );
        cfg.profiles.insert(
            "partner".into(),
            Profile {
                auth: AuthKind::Oauth,
                client_id: Some("c".into()),
                expires_at: Some(1),
                ..Default::default()
            },
        );
        let text = serde_json::to_string_pretty(&cfg).unwrap();
        let back: Config = serde_json::from_str(&text).unwrap();
        assert_eq!(back.profiles["default"].secrets.api_key.as_deref(), Some("k"));
        assert_eq!(back.profiles["default"].secret_store, SecretStore::File);
        assert_eq!(back.profiles["partner"].auth, AuthKind::Oauth);
        assert!(text.contains("\"auth\": \"oauth\""), "{text}");
        assert!(
            text.contains("\"api_key\": \"k\""),
            "secrets stay flat in the file: {text}"
        );
    }

    #[test]
    fn profiles_default_to_the_os_store() {
        let cfg: Config = serde_json::from_str(r#"{"profiles":{"default":{"auth":"api_key"}}}"#).unwrap();
        let p = &cfg.profiles["default"];
        assert!(p.uses_os_store());
        assert_eq!(p.bound_base_url(), DEFAULT_BASE_URL);
    }

    #[test]
    fn missing_file_is_empty_config() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = load_from(&dir.path().join("nope.json")).unwrap();
        assert!(cfg.profiles.is_empty());
    }
}
