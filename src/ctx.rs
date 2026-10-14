//! Per-invocation context: profile, time zone, credentials, endpoints, safety policy.
use crate::config::{Config, Profile, default_endpoint};
use crate::creds;
use crate::dav::Dav;
use crate::error::{AppError, Result};
use crate::output::Format;
use jiff::tz::TimeZone;
use reqwest::Url;
use std::time::{Duration, Instant};

#[allow(dead_code)]
pub struct Ctx {
    pub cfg: Config,
    pub profile_name: String,
    pub profile: Profile,
    pub tz: TimeZone,
    pub tz_name: String,
    pub format: Format,
    pub fields: Option<Vec<String>>,
    pub limit: usize,
    pub cursor: Option<String>,
    pub dry_run: bool,
    pub read_only_flag: bool,
    pub credential_backend: Option<String>,
    pub timeout: Duration,
    pub quiet: bool,
    pub started: Instant,
    pub apple_id_override: Option<String>,
    pub password_override: Option<String>,
}

fn env_bool(k: &str) -> bool {
    std::env::var(k)
        .map(|v| matches!(v.as_str(), "1" | "true" | "yes"))
        .unwrap_or(false)
}

impl Ctx {
    pub fn apple_id(&self) -> Result<String> {
        self.apple_id_override
            .clone()
            .or_else(|| std::env::var("ICLOUD_APPLE_ID").ok().filter(|s| !s.is_empty()))
            .or_else(|| self.profile.apple_id.clone())
            .ok_or_else(|| {
                AppError::new("E_AUTH_MISSING", format!("no Apple ID configured for profile `{}`", self.profile_name))
                    .hint("Run `icloud auth login --apple-id you@icloud.com --password-stdin`, or set ICLOUD_APPLE_ID + ICLOUD_APP_PASSWORD.")
            })
    }
    pub fn region(&self) -> String {
        std::env::var("ICLOUD_REGION")
            .ok()
            .filter(|s| !s.is_empty())
            .or_else(|| self.profile.region.clone())
            .filter(|r| r != "auto")
            .unwrap_or_else(|| "global".into())
    }
    pub fn endpoint(&self, svc: &str) -> Result<Url> {
        let env = std::env::var(format!("ICLOUD_{}_URL", svc.to_ascii_uppercase()))
            .ok()
            .filter(|s| !s.is_empty());
        let prof = match svc {
            "caldav" => self.profile.endpoints.caldav.clone(),
            "carddav" => self.profile.endpoints.carddav.clone(),
            _ => self.profile.endpoints.imap.clone(),
        };
        let s = env
            .or(prof)
            .unwrap_or_else(|| default_endpoint(&self.region(), svc).to_string());
        Url::parse(&s)
            .map_err(|e| AppError::new("E_CONFIG", format!("invalid {svc} endpoint `{s}`: {e}")))
    }
    pub fn backend(&self) -> Option<String> {
        creds::select_backend(
            self.credential_backend.as_deref(),
            &self.profile.credential,
            &self.profile_name,
        )
    }
    pub async fn password(&self) -> Result<String> {
        if let Some(p) = &self.password_override {
            return Ok(p.clone());
        }
        let id = self.apple_id()?;
        Ok(creds::read_password(
            self.backend().as_deref(),
            &self.profile,
            &self.profile_name,
            &id,
        )
        .await?
        .0)
    }
    pub async fn dav(&self, svc: &'static str) -> Result<(Dav, Url)> {
        let id = self.apple_id()?;
        let pw = self.password().await?;
        Ok((Dav::new(&id, &pw, self.timeout, svc)?, self.endpoint(svc)?))
    }
    pub fn read_only(&self) -> bool {
        self.read_only_flag
            || env_bool("ICLOUD_READ_ONLY")
            || self.profile.read_only.unwrap_or(false)
    }
    /// Policy gate for every write.
    pub fn guard_write(&self, what: &str) -> Result<()> {
        if self.read_only() {
            return Err(AppError::new("E_READ_ONLY", format!("refusing `{what}`: read-only mode is on"))
                .hint("Read-only is enabled via --read-only, ICLOUD_READ_ONLY or the profile's read_only setting. Ask the user before changing it."));
        }
        Ok(())
    }
    pub fn guard_destructive(
        &self,
        what: &str,
        yes: bool,
        affected: serde_json::Value,
    ) -> Result<()> {
        self.guard_write(what)?;
        if !yes && !self.dry_run {
            return Err(AppError::new("E_CONFIRM_REQUIRED", format!("`{what}` is destructive and needs --yes"))
                .hint("Show the affected items to the user; only re-run with --yes after they explicitly approve. Use --dry-run to preview.")
                .details(serde_json::json!({ "affected": affected })));
        }
        Ok(())
    }
    #[allow(dead_code)]
    pub fn max_batch(&self) -> usize {
        self.profile.max_batch.unwrap_or(20)
    }
}
