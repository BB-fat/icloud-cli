//! Structured errors with stable codes, hints and exit codes.
use serde_json::{Value, json};

#[derive(Debug, Clone)]
pub struct AppError {
    pub code: &'static str,
    pub message: String,
    pub hint: Option<String>,
    pub retryable: bool,
    pub details: Value,
}

pub type Result<T> = std::result::Result<T, AppError>;

impl AppError {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        let retryable = matches!(
            code,
            "E_NETWORK" | "E_TIMEOUT" | "E_RATE_LIMITED" | "E_UPSTREAM"
        );
        AppError {
            code,
            message: message.into(),
            hint: None,
            retryable,
            details: Value::Null,
        }
    }
    pub fn hint(mut self, h: impl Into<String>) -> Self {
        self.hint = Some(h.into());
        self
    }
    pub fn details(mut self, d: Value) -> Self {
        self.details = d;
        self
    }
    pub fn usage(m: impl Into<String>) -> Self {
        Self::new("E_USAGE", m)
    }
    pub fn internal(m: impl Into<String>) -> Self {
        Self::new("E_INTERNAL", m)
    }
    pub fn not_found(m: impl Into<String>) -> Self {
        Self::new("E_NOT_FOUND", m)
    }
    pub fn exit_code(&self) -> i32 {
        match self.code {
            "E_USAGE" | "E_TIME_PARSE" | "E_CONFIG" => 2,
            "E_AUTH_INVALID" | "E_AUTH_MISSING" => 3,
            "E_NOT_FOUND" => 4,
            "E_CONFLICT" => 5,
            "E_READ_ONLY" | "E_CONFIRM_REQUIRED" | "E_BATCH_LIMIT" => 6,
            "E_NETWORK" | "E_TIMEOUT" => 7,
            "E_RATE_LIMITED" | "E_UPSTREAM" => 8,
            "E_UNSUPPORTED" => 9,
            _ => 1,
        }
    }
    pub fn to_json(&self) -> Value {
        json!({
            "schema": "icloud.error/v1",
            "ok": false,
            "error": {
                "code": self.code,
                "message": self.message,
                "hint": self.hint,
                "retryable": self.retryable,
                "exit_code": self.exit_code(),
                "details": self.details,
            }
        })
    }
}

impl std::fmt::Display for AppError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)?;
        if let Some(h) = &self.hint {
            write!(f, "\nhint: {h}")?;
        }
        Ok(())
    }
}

impl From<reqwest::Error> for AppError {
    fn from(e: reqwest::Error) -> Self {
        if e.is_timeout() {
            AppError::new("E_TIMEOUT", format!("request timed out: {e}"))
                .hint("Retry, or raise --timeout.")
        } else {
            AppError::new("E_NETWORK", format!("network error: {e}"))
                .hint("Check connectivity to iCloud and retry.")
        }
    }
}

/// Map an HTTP status from a DAV server into an error.
pub fn http_error(status: u16, service: &str, url: &str) -> AppError {
    let d = serde_json::json!({ "service": service, "status": status, "url": url });
    match status {
        401 | 403 => AppError::new("E_AUTH_INVALID", format!("{service} rejected the credentials (HTTP {status})"))
            .hint("Use an app-specific password (not the Apple ID password). Generate one at account.apple.com (China: appleid.apple.com.cn), then `icloud auth login --apple-id <id> --password-stdin`.")
            .details(d),
        404 | 410 => AppError::not_found(format!("resource not found (HTTP {status})")).details(d),
        409 | 412 => AppError::new("E_CONFLICT", format!("conflict / precondition failed (HTTP {status})"))
            .hint("The resource changed or already exists. Re-fetch it (`get`) and retry with the new etag.")
            .details(d),
        429 => AppError::new("E_RATE_LIMITED", "rate limited by iCloud (HTTP 429)").hint("Back off and retry later.").details(d),
        500..=599 => AppError::new("E_UPSTREAM", format!("iCloud server error (HTTP {status})")).hint("Retry later.").details(d),
        _ => AppError::new("E_UPSTREAM", format!("unexpected HTTP status {status}")).details(d),
    }
}
