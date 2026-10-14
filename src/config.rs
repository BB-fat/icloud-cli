//! Profiles and non-secret configuration (config.toml). Secrets never live here.
use crate::error::{AppError, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current: Option<String>,
    #[serde(default)]
    pub profiles: BTreeMap<String, Profile>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Profile {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub apple_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_only: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_calendar: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tz: Option<String>,
    #[serde(default)]
    pub credential: CredentialCfg,
    #[serde(default)]
    pub endpoints: Endpoints,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_batch: Option<usize>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct CredentialCfg {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password_command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Endpoints {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caldav: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub carddav: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub imap: Option<String>,
}

pub fn config_dir() -> PathBuf {
    if let Ok(d) = std::env::var("ICLOUD_CONFIG_DIR") {
        return PathBuf::from(d);
    }
    if cfg!(windows) {
        return dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("icloud-cli");
    }
    if let Ok(x) = std::env::var("XDG_CONFIG_HOME")
        && !x.is_empty()
    {
        return PathBuf::from(x).join("icloud-cli");
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".config")
        .join("icloud-cli")
}

pub fn config_path() -> PathBuf {
    config_dir().join("config.toml")
}

impl Config {
    pub fn load() -> Result<Config> {
        let p = config_path();
        match std::fs::read_to_string(&p) {
            Ok(s) => toml::from_str(&s).map_err(|e| {
                AppError::new("E_CONFIG", format!("invalid config {}: {e}", p.display()))
                    .hint("Fix or delete the config file.")
            }),
            Err(_) => Ok(Config::default()),
        }
    }
    pub fn save(&self) -> Result<()> {
        let p = config_path();
        std::fs::create_dir_all(p.parent().unwrap())
            .map_err(|e| AppError::internal(format!("cannot create config dir: {e}")))?;
        let s = toml::to_string_pretty(self).map_err(|e| AppError::internal(e.to_string()))?;
        std::fs::write(&p, s)
            .map_err(|e| AppError::internal(format!("cannot write {}: {e}", p.display())))
    }
    pub fn profile_name(&self, flag: Option<&str>) -> String {
        flag.map(|s| s.to_string())
            .or_else(|| self.current.clone())
            .unwrap_or_else(|| "default".into())
    }
}

/// Endpoints per region.
pub fn default_endpoint(region: &str, service: &str) -> &'static str {
    match (region, service) {
        ("cn", "caldav") => "https://caldav.icloud.com.cn/",
        ("cn", "carddav") => "https://contacts.icloud.com.cn/",
        // TODO(v0.1): verify China IMAP host with a real account; Apple docs list the same host.
        ("cn", "imap") => "imaps://imap.mail.me.com:993",
        (_, "caldav") => "https://caldav.icloud.com/",
        (_, "carddav") => "https://contacts.icloud.com/",
        _ => "imaps://imap.mail.me.com:993",
    }
}

/// Set a dotted key on a profile (for `profile set`).
pub fn set_key(p: &mut Profile, key: &str, value: &str) -> Result<()> {
    let opt = |v: &str| {
        if v.is_empty() || v == "null" {
            None
        } else {
            Some(v.to_string())
        }
    };
    let b = |v: &str| -> Result<Option<bool>> {
        match v {
            "true" | "1" | "yes" => Ok(Some(true)),
            "false" | "0" | "no" => Ok(Some(false)),
            "" | "null" => Ok(None),
            _ => Err(AppError::usage(format!("`{key}` expects true/false"))),
        }
    };
    match key {
        "apple_id" => p.apple_id = opt(value),
        "region" => {
            if !["global", "cn", "auto", ""].contains(&value) {
                return Err(AppError::usage("region must be global, cn or auto"));
            }
            p.region = opt(value)
        }
        "read_only" => p.read_only = b(value)?,
        "default_calendar" => p.default_calendar = opt(value),
        "tz" => p.tz = opt(value),
        "max_batch" => p.max_batch = value.parse().ok(),
        "credential.backend" => {
            if !["keychain", "env", "password_command", "file", ""].contains(&value) {
                return Err(AppError::usage("credential.backend must be keychain, env, password_command or file"));
            }
            p.credential.backend = opt(value)
        }
        "credential.password_command" => p.credential.password_command = opt(value),
        "credential.file" => p.credential.file = opt(value),
        "endpoints.caldav" => p.endpoints.caldav = opt(value),
        "endpoints.carddav" => p.endpoints.carddav = opt(value),
        "endpoints.imap" => p.endpoints.imap = opt(value),
        _ => {
            return Err(AppError::usage(format!("unknown key `{key}`")).hint(
                "Keys: apple_id, region, read_only, default_calendar, tz, max_batch, credential.backend, credential.password_command, credential.file, endpoints.caldav, endpoints.carddav, endpoints.imap",
            ))
        }
    }
    Ok(())
}
