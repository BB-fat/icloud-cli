//! Pluggable credential backends. Never prompts interactively.
use crate::config::{CredentialCfg, Profile};
use crate::error::{AppError, Result};

pub const BACKENDS: &[&str] = &["keychain", "env", "password_command", "file"];
const SERVICE: &str = "icloud-cli";

pub fn env_var_names(profile: &str) -> Vec<String> {
    let p: String = profile
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect();
    vec![
        format!("ICLOUD_{p}_APP_PASSWORD"),
        "ICLOUD_APP_PASSWORD".to_string(),
    ]
}

fn missing_hint(profile: &str) -> String {
    format!(
        "No credential found for profile `{profile}`. Options: (a) personal computer: `printf %s \"$PW\" | icloud auth login --apple-id you@icloud.com --password-stdin`; (b) sandbox/CI: export {} (or ICLOUD_APP_PASSWORD) and ICLOUD_APPLE_ID; (c) secret manager: `icloud profile set credential.backend password_command` + `icloud profile set credential.password_command \"op read op://vault/icloud/password\"`; (d) server: credential.backend file (0600). Use an app-specific password, never the Apple ID password.",
        env_var_names(profile)[0]
    )
}

fn keychain_entry(apple_id: &str) -> Result<keyring::Entry> {
    keyring::Entry::new(SERVICE, apple_id)
        .map_err(|e| AppError::new("E_AUTH_MISSING", format!("keychain unavailable: {e}")))
}

/// Which backend applies for this invocation.
pub fn select_backend(flag: Option<&str>, cfg: &CredentialCfg, profile: &str) -> Option<String> {
    if let Some(f) = flag.filter(|s| !s.is_empty()) {
        return Some(f.to_string());
    }
    if let Some(b) = &cfg.backend {
        return Some(b.clone());
    }
    // auto-detect: an env password is an explicit signal in headless environments
    if env_var_names(profile)
        .iter()
        .any(|n| std::env::var(n).map(|v| !v.is_empty()).unwrap_or(false))
    {
        return Some("env".into());
    }
    None
}

pub async fn read_password(
    backend: Option<&str>,
    prof: &Profile,
    profile: &str,
    apple_id: &str,
) -> Result<(String, String)> {
    let b = match backend {
        Some(b) => b.to_string(),
        None => {
            // auto: try keychain, fail with a structured hint
            match keychain_entry(apple_id).and_then(|e| {
                e.get_password()
                    .map_err(|e| AppError::new("E_AUTH_MISSING", e.to_string()))
            }) {
                Ok(p) => return Ok((p, "keychain".into())),
                Err(_) => {
                    return Err(AppError::new("E_AUTH_MISSING", "no credential configured")
                        .hint(missing_hint(profile)));
                }
            }
        }
    };
    let pw = match b.as_str() {
        "env" => env_var_names(profile)
            .iter()
            .find_map(|n| std::env::var(n).ok().filter(|v| !v.is_empty()))
            .ok_or_else(|| {
                AppError::new(
                    "E_AUTH_MISSING",
                    "credential backend `env` selected but no password variable is set",
                )
                .hint(missing_hint(profile))
            })?,
        "keychain" => keychain_entry(apple_id)?.get_password().map_err(|e| {
            AppError::new(
                "E_AUTH_MISSING",
                format!("keychain has no password for {apple_id}: {e}"),
            )
            .hint(missing_hint(profile))
        })?,
        "password_command" => {
            let cmd = prof.credential.password_command.as_deref().ok_or_else(|| {
                AppError::new("E_CONFIG", "credential.backend is password_command but credential.password_command is not set")
                    .hint("icloud profile set credential.password_command \"<command printing the password>\"")
            })?;
            let out = if cfg!(windows) {
                tokio::process::Command::new("cmd")
                    .args(["/C", cmd])
                    .output()
                    .await
            } else {
                tokio::process::Command::new("sh")
                    .args(["-c", cmd])
                    .output()
                    .await
            }
            .map_err(|e| {
                AppError::new(
                    "E_AUTH_MISSING",
                    format!("password_command failed to start: {e}"),
                )
            })?;
            if !out.status.success() {
                return Err(AppError::new(
                    "E_AUTH_MISSING",
                    format!("password_command exited with {}", out.status),
                )
                .hint("Run the command manually to check it prints the app-specific password."));
            }
            String::from_utf8_lossy(&out.stdout)
                .lines()
                .next()
                .unwrap_or("")
                .trim()
                .to_string()
        }
        "file" => {
            let path = prof.credential.file.as_deref().ok_or_else(|| {
                AppError::new(
                    "E_CONFIG",
                    "credential.backend is file but credential.file is not set",
                )
                .hint("icloud profile set credential.file /path/to/secret")
            })?;
            check_file_perms(path)?;
            std::fs::read_to_string(path)
                .map_err(|e| {
                    AppError::new(
                        "E_AUTH_MISSING",
                        format!("cannot read credential file: {e}"),
                    )
                })?
                .lines()
                .next()
                .unwrap_or("")
                .trim()
                .to_string()
        }
        other => {
            return Err(
                AppError::usage(format!("unknown credential backend `{other}`"))
                    .hint("Use keychain, env, password_command or file."),
            );
        }
    };
    if pw.is_empty() {
        return Err(AppError::new(
            "E_AUTH_MISSING",
            format!("credential backend `{b}` returned an empty password"),
        )
        .hint(missing_hint(profile)));
    }
    Ok((pw, b))
}

#[cfg(unix)]
fn check_file_perms(path: &str) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let md = std::fs::metadata(path)
        .map_err(|e| AppError::new("E_AUTH_MISSING", format!("credential file: {e}")))?;
    if md.permissions().mode() & 0o077 != 0 {
        return Err(AppError::new(
            "E_CONFIG",
            format!("credential file {path} is accessible by group/others"),
        )
        .hint(format!("chmod 600 {path}")));
    }
    Ok(())
}
#[cfg(not(unix))]
fn check_file_perms(_: &str) -> Result<()> {
    Ok(())
}

/// Persist a password for `auth login`. Returns a note for the output.
pub fn store_password(
    backend: &str,
    prof: &Profile,
    apple_id: &str,
    pw: &str,
) -> Result<&'static str> {
    match backend {
        "keychain" => {
            keychain_entry(apple_id)?.set_password(pw).map_err(|e| {
                AppError::new("E_AUTH_MISSING", format!("cannot write to OS keychain: {e}"))
                    .hint("No usable keychain here (sandbox/server?). Use --credential-backend env, password_command or file.")
            })?;
            Ok("stored in OS keychain")
        }
        "file" => {
            let path = prof.credential.file.as_deref().ok_or_else(|| {
                AppError::usage("--credential-file is required with --credential-backend file")
            })?;
            write_secret_file(path, pw)?;
            Ok("stored in credential file (0600)")
        }
        "env" => Ok("not stored; read from environment at each call"),
        "password_command" => Ok("not stored; obtained from password_command at each call"),
        other => Err(AppError::usage(format!(
            "unknown credential backend `{other}`"
        ))),
    }
}

fn write_secret_file(path: &str, pw: &str) -> Result<()> {
    if let Some(parent) = std::path::Path::new(path).parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)
            .map_err(|e| AppError::internal(format!("cannot write credential file: {e}")))?;
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
        f.write_all(pw.as_bytes())
            .map_err(|e| AppError::internal(e.to_string()))?;
    }
    #[cfg(not(unix))]
    std::fs::write(path, pw).map_err(|e| AppError::internal(e.to_string()))?;
    Ok(())
}

pub fn delete_password(backend: &str, prof: &Profile, apple_id: &str) -> Result<&'static str> {
    match backend {
        "keychain" => {
            let _ = keychain_entry(apple_id)?.delete_credential();
            Ok("removed from OS keychain")
        }
        "file" => {
            if let Some(p) = &prof.credential.file {
                let _ = std::fs::remove_file(p);
            }
            Ok("credential file removed")
        }
        _ => Ok("nothing stored for this backend"),
    }
}

pub fn mask(pw: &str) -> String {
    let tail: String = pw
        .chars()
        .rev()
        .take(4)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("****{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn backend_selection_order() {
        let cfg = CredentialCfg {
            backend: Some("file".into()),
            ..Default::default()
        };
        assert_eq!(
            select_backend(Some("env"), &cfg, "x").as_deref(),
            Some("env")
        );
        assert_eq!(select_backend(None, &cfg, "x").as_deref(), Some("file"));
        assert_eq!(env_var_names("my-fam")[0], "ICLOUD_MY_FAM_APP_PASSWORD");
        assert_eq!(mask("abcd-efgh-ijkl-mnop"), "****mnop");
    }
}
