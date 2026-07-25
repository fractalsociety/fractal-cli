//! Browser-mediated CLI authentication for Fractal Society.
//!
//! The CLI never receives an email address or magic link. It asks the website
//! for a short-lived device code, opens the website, and polls until the user
//! finishes authentication there. The resulting bearer credential is stored
//! owner-only and is never printed.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::cli::LoginArgs;

pub(crate) const DEFAULT_SOCIETY_URL: &str = "https://fractalsociety.com";

#[derive(Clone, Deserialize, Serialize)]
pub(crate) struct StoredSession {
    pub(crate) schema: String,
    pub(crate) server: String,
    pub(crate) access_token: String,
    #[serde(default)]
    pub(crate) username: Option<String>,
    #[serde(default)]
    pub(crate) account_id: Option<String>,
    #[serde(default)]
    pub(crate) expires_at: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DeviceAuthorization {
    device_code: String,
    user_code: String,
    verification_uri: String,
    #[serde(default)]
    verification_uri_complete: Option<String>,
    expires_in: u64,
    #[serde(default = "default_poll_interval")]
    interval: u64,
}

#[derive(Debug, Deserialize)]
struct Account {
    #[serde(default)]
    id: String,
    #[serde(default)]
    username: String,
    #[serde(default)]
    label: String,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    expires_at: Option<String>,
    #[serde(default)]
    account: Option<Account>,
}

fn default_poll_interval() -> u64 {
    5
}

pub(crate) fn server_url(override_url: Option<&str>) -> Result<String> {
    let raw = override_url
        .map(str::to_owned)
        .or_else(|| std::env::var("FRACTAL_SOCIETY_URL").ok())
        .unwrap_or_else(|| DEFAULT_SOCIETY_URL.to_owned());
    let url = raw.trim().trim_end_matches('/').to_owned();
    let local_http = url.starts_with("http://127.0.0.1")
        || url.starts_with("http://localhost")
        || url.starts_with("http://[::1]");
    if !url.starts_with("https://") && !local_http {
        bail!("Fractal Society URL must use HTTPS (HTTP is allowed only for loopback testing)");
    }
    Ok(url)
}

pub(crate) fn run_login(args: &LoginArgs) -> Result<()> {
    let server = server_url(args.server.as_deref())?;
    let authorization: DeviceAuthorization = post_json(
        &format!("{server}/api/cli/auth/device"),
        &serde_json::json!({
            "client_name": "fractal-cli",
            "client_version": env!("CARGO_PKG_VERSION"),
        }),
    )
    .context("start browser authorization")?;
    validate_device_authorization(&authorization)?;

    let browser_url = authorization
        .verification_uri_complete
        .as_deref()
        .unwrap_or(&authorization.verification_uri);
    println!("Authorize Fractal CLI in your browser:");
    println!("  {browser_url}");
    println!("Code: {}", authorization.user_code);
    if !args.no_open {
        let _ = Command::new("open").arg(browser_url).status();
    }

    let deadline = Instant::now()
        + Duration::from_secs(
            args.timeout
                .min(authorization.expires_in)
                .max(authorization.interval),
        );
    let mut interval = authorization.interval.max(1);
    while Instant::now() < deadline {
        thread::sleep(Duration::from_secs(interval));
        let endpoint = format!("{server}/api/cli/auth/device/token");
        let request = serde_json::to_string(&serde_json::json!({
            "device_code": authorization.device_code,
        }))?;
        let response = ureq::post(&endpoint)
            .set("Content-Type", "application/json")
            .send_string(&request);
        match response {
            Ok(response) => {
                let body: serde_json::Value =
                    response_json(response).context("decode browser authorization result")?;
                if response_state(&body) == Some("authorization_pending") {
                    interval = body
                        .get("interval")
                        .and_then(serde_json::Value::as_u64)
                        .unwrap_or(interval)
                        .max(1);
                    continue;
                }
                if response_state(&body) == Some("slow_down") {
                    interval = interval.saturating_add(5);
                    continue;
                }
                let token: TokenResponse =
                    serde_json::from_value(body).context("decode browser authorization result")?;
                let (username, account_id) = match token.account {
                    Some(account) => (
                        account.username(),
                        (!account.id.is_empty()).then_some(account.id),
                    ),
                    None => (None, None),
                };
                let session = StoredSession {
                    schema: "fractal.cli_session.v1".to_owned(),
                    server: server.clone(),
                    access_token: token.access_token,
                    username,
                    account_id,
                    expires_at: token.expires_at,
                };
                save_session(&session)?;
                println!(
                    "Logged in{}.",
                    session
                        .username
                        .as_deref()
                        .map(|name| format!(" as @{name}"))
                        .unwrap_or_default()
                );
                return Ok(());
            }
            Err(ureq::Error::Status(status, response))
                if status == 400 || status == 428 || status == 429 =>
            {
                let body: serde_json::Value = response_json(response).unwrap_or_default();
                let status_name = response_state(&body);
                match status_name {
                    Some("authorization_pending") => {}
                    Some("slow_down") => interval = interval.saturating_add(5),
                    Some("access_denied") => bail!("browser authorization was denied"),
                    Some("expired_token") => {
                        bail!("browser authorization expired; run `fractal login` again")
                    }
                    Some(error) => bail!("browser authorization failed: {error}"),
                    None => bail!("browser authorization failed with HTTP {status}"),
                }
            }
            Err(error) => return Err(error).context("poll browser authorization"),
        }
    }
    bail!("browser authorization timed out; run `fractal login` again")
}

/// Ensure the interactive CLI has a live Fractal Society session.
///
/// A valid owner-only local credential is reused. Missing, malformed, or
/// server-revoked credentials enter the same browser device flow as
/// `fractal login`. `fractal --offline` is the explicit local-only bypass.
pub(crate) fn ensure_login() -> Result<()> {
    let path = session_path()?;
    if path.exists() {
        match load_session_from(&path) {
            Ok(session) => match validate_remote_session(&session) {
                Ok(true) => {
                    println!(
                        "✓ Signed in to Fractal Society{}.",
                        session
                            .username
                            .as_deref()
                            .map(|name| format!(" as @{name}"))
                            .unwrap_or_default()
                    );
                    return Ok(());
                }
                Ok(false) => {
                    println!("Your Fractal Society session expired. Sign in again.");
                }
                Err(error) => {
                    return Err(error).context(
                        "cannot verify Fractal Society login; use `fractal --offline` for local-only work",
                    );
                }
            },
            Err(error) => {
                eprintln!("Credential note: {error:#}");
                println!("Sign in again to repair the local session.");
            }
        }
    } else {
        println!("Welcome to Fractal. Sign in to connect your projects and execution graphs.");
    }

    run_login(&LoginArgs {
        server: None,
        no_open: false,
        timeout: 300,
    })
}

fn validate_remote_session(session: &StoredSession) -> Result<bool> {
    let endpoint = format!(
        "{}/api/fractal/account",
        session.server.trim_end_matches('/')
    );
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(8))
        .timeout_read(Duration::from_secs(12))
        .timeout_write(Duration::from_secs(12))
        .build();
    match agent
        .get(&endpoint)
        .set("Authorization", &format!("Bearer {}", session.access_token))
        .call()
    {
        Ok(response) => Ok(response.status() == 200),
        Err(ureq::Error::Status(401, _)) => Ok(false),
        Err(ureq::Error::Status(status, _)) => {
            bail!("Fractal Society session check failed with HTTP {status}")
        }
        Err(error) => Err(error).context("contact Fractal Society"),
    }
}

fn response_state(body: &serde_json::Value) -> Option<&str> {
    body.get("error")
        .or_else(|| body.get("status"))
        .and_then(serde_json::Value::as_str)
}

pub(crate) fn logout() -> Result<()> {
    let path = session_path()?;
    match fs::remove_file(&path) {
        Ok(()) => println!("Logged out. Removed {}.", path.display()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            println!("Already logged out.")
        }
        Err(error) => return Err(error).with_context(|| format!("remove {}", path.display())),
    }
    Ok(())
}

pub(crate) fn load_session() -> Result<StoredSession> {
    load_session_from(&session_path()?)
}

impl StoredSession {
    pub(crate) fn account_identity(&self) -> Option<String> {
        self.account_id.clone().or_else(|| self.username.clone())
    }
}

impl Account {
    fn username(&self) -> Option<String> {
        if !self.username.is_empty() {
            Some(self.username.clone())
        } else {
            (!self.label.is_empty()).then_some(self.label.clone())
        }
    }
}

fn validate_device_authorization(authorization: &DeviceAuthorization) -> Result<()> {
    if authorization.device_code.is_empty()
        || authorization.user_code.is_empty()
        || authorization.verification_uri.is_empty()
        || authorization.expires_in == 0
    {
        bail!("Fractal Society returned an incomplete device authorization");
    }
    Ok(())
}

fn post_json<T: for<'de> Deserialize<'de>>(url: &str, body: &serde_json::Value) -> Result<T> {
    let body = serde_json::to_string(body)?;
    let response = ureq::post(url)
        .set("Content-Type", "application/json")
        .send_string(&body)
        .context("send request")?;
    response_json(response)
}

fn response_json<T: for<'de> Deserialize<'de>>(response: ureq::Response) -> Result<T> {
    serde_json::from_reader(response.into_reader()).context("decode JSON response")
}

fn fractal_home() -> Result<PathBuf> {
    if let Some(home) = std::env::var_os("FRACTAL_HOME") {
        return Ok(PathBuf::from(home));
    }
    Ok(
        PathBuf::from(std::env::var_os("HOME").context("set FRACTAL_HOME or HOME")?)
            .join(".fractal"),
    )
}

fn session_path() -> Result<PathBuf> {
    Ok(fractal_home()?.join("credentials.json"))
}

fn save_session(session: &StoredSession) -> Result<()> {
    save_session_to(&session_path()?, session)
}

fn save_session_to(path: &Path, session: &StoredSession) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
        set_owner_only_directory(parent)?;
    }
    let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
    let bytes = serde_json::to_vec_pretty(session)?;
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&temporary)
        .with_context(|| format!("create {}", temporary.display()))?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    fs::rename(&temporary, path)?;
    Ok(())
}

fn load_session_from(path: &Path) -> Result<StoredSession> {
    let bytes = fs::read(path).with_context(|| {
        format!(
            "not logged in; run `fractal login` (credential file: {})",
            path.display()
        )
    })?;
    let session: StoredSession = serde_json::from_slice(&bytes)
        .with_context(|| format!("invalid credential file {}", path.display()))?;
    if session.schema != "fractal.cli_session.v1"
        || session.access_token.is_empty()
        || session.server.is_empty()
    {
        bail!("invalid Fractal CLI credential file");
    }
    Ok(session)
}

fn set_owner_only_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
        if fs::metadata(path)?.mode() & 0o077 != 0 {
            bail!("credential directory {} is not owner-only", path.display());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_directory(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "fractal-auth-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[test]
    fn session_round_trip_does_not_change_token() -> Result<()> {
        let directory = temp_directory("round-trip");
        fs::create_dir(&directory)?;
        let path = directory.join("credentials.json");
        let session = StoredSession {
            schema: "fractal.cli_session.v1".to_owned(),
            server: "https://fractalsociety.com".to_owned(),
            access_token: "secret-token".to_owned(),
            username: Some("builder".to_owned()),
            account_id: Some("acct_builder".to_owned()),
            expires_at: None,
        };
        save_session_to(&path, &session)?;
        let loaded = load_session_from(&path)?;
        assert_eq!(loaded.access_token, "secret-token");
        assert_eq!(loaded.username.as_deref(), Some("builder"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(fs::metadata(&path)?.permissions().mode() & 0o777, 0o600);
        }
        let _ = fs::remove_dir_all(directory);
        Ok(())
    }

    #[test]
    fn rejects_insecure_non_loopback_server() {
        assert!(server_url(Some("http://example.com")).is_err());
        assert_eq!(
            server_url(Some("http://127.0.0.1:3000/")).unwrap(),
            "http://127.0.0.1:3000"
        );
    }

    #[test]
    fn recognizes_pending_success_status_without_treating_it_as_a_token() {
        let body = serde_json::json!({
            "error": "authorization_pending",
            "interval": 3
        });
        assert_eq!(response_state(&body), Some("authorization_pending"));
        assert_eq!(body["interval"], 3);
    }

    #[test]
    fn decodes_account_with_distinct_username_and_label_fields() {
        let token: TokenResponse = serde_json::from_value(serde_json::json!({
            "access_token": "test-token",
            "account": {
                "id": "acct_builder",
                "username": "builder",
                "label": "Builder"
            }
        }))
        .unwrap();
        let account = token.account.unwrap();
        assert_eq!(account.username(), Some("builder".to_owned()));
    }
}
