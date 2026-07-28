use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::cli::{ConnectXArgs, InviteArgs, ShareXArgs};

#[derive(Deserialize)]
struct InviteResponse {
    #[serde(default)]
    invite_url: Option<String>,
    #[serde(default)]
    email_sent: bool,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Deserialize)]
struct XResponse {
    #[serde(default)]
    preview: Option<String>,
    #[serde(default)]
    intent_url: Option<String>,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Serialize)]
struct ExternalXShareHandoff<'a> {
    schema: &'static str,
    intent_url: &'a str,
    preview: &'a str,
    created_at_ms: u128,
}

#[derive(Deserialize)]
struct BrowserHandoffResponse {
    #[serde(default)]
    browser_url: Option<String>,
    #[serde(default)]
    error: Option<String>,
}

fn open_share_preview(
    session: &crate::auth::StoredSession,
    server: &str,
    body: serde_json::Value,
) -> Result<()> {
    let endpoint = format!(
        "{}/api/fractal/share-previews",
        server.trim_end_matches('/')
    );
    let (status, response) = request("POST", &endpoint, &session.access_token, &body)?;
    let result: BrowserHandoffResponse =
        serde_json::from_str(&response).context("decode share preview handoff")?;
    if !(200..300).contains(&status) {
        bail!(
            "{}",
            result
                .error
                .unwrap_or_else(|| format!("share preview failed with HTTP {status}"))
        );
    }
    let url = result
        .browser_url
        .context("Fractal Society returned no share preview URL")?;
    Command::new("open")
        .arg(&url)
        .status()
        .context("open share preview in browser")?;
    println!("Opened the secure share preview on Fractal Society.");
    Ok(())
}

fn segment(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(*byte as char)
            }
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    encoded
}

fn session_and_server(override_url: Option<&str>) -> Result<(crate::auth::StoredSession, String)> {
    let session = crate::auth::load_session()
        .context("Fractal Society login required; run `fractal login` first")?;
    let server = override_url
        .map(|url| crate::auth::server_url(Some(url)))
        .transpose()?
        .unwrap_or_else(|| session.server.trim_end_matches('/').to_owned());
    Ok((session, server))
}

fn endpoint(server: &str, username: &str, project: &str, suffix: &str) -> String {
    format!(
        "{server}/api/fractal/projects/{}/{suffix}?owner={}",
        segment(project),
        segment(username)
    )
}

fn request(
    method: &str,
    endpoint: &str,
    access_token: &str,
    body: &serde_json::Value,
) -> Result<(u16, String)> {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(10))
        .timeout_read(Duration::from_secs(30))
        .timeout_write(Duration::from_secs(20))
        .build();
    let payload = serde_json::to_string(body)?;
    let call = match method {
        "POST" => agent
            .post(endpoint)
            .set("Authorization", &format!("Bearer {access_token}"))
            .set("Content-Type", "application/json")
            .send_string(&payload),
        _ => bail!("unsupported social request method"),
    };
    match call {
        Ok(response) => {
            let status = response.status();
            Ok((status, response.into_string().unwrap_or_default()))
        }
        Err(ureq::Error::Status(status, response)) => {
            Ok((status, response.into_string().unwrap_or_default()))
        }
        Err(error) => Err(error).context("contact Fractal Society"),
    }
}

pub(crate) fn invite(args: &InviteArgs) -> Result<()> {
    let email = args.email.trim().to_lowercase();
    if !email.contains('@') || email.len() > 254 {
        bail!("a valid recipient email is required");
    }
    let help = args
        .message
        .as_deref()
        .unwrap_or("help finishing project tasks and, if available, spare agent compute")
        .trim();
    println!("Email invitation preview:");
    println!("  Project: {}", args.project);
    println!("  Recipient: {email}");
    println!("  Permission: {}", args.role.as_str());
    println!("  Help requested: {help}");
    let (session, server) = session_and_server(args.server.as_deref())?;
    let username = session
        .username
        .as_deref()
        .context("Fractal Society username missing; run `fractal login` again")?;
    if !args.yes {
        open_share_preview(
            &session,
            &server,
            serde_json::json!({
                "kind": "email",
                "owner": username,
                "project": args.project,
                "email": email,
                "role": args.role.as_str(),
                "help_requested": help,
            }),
        )?;
        bail!("email not sent; after the user explicitly confirms, repeat with `--yes`");
    }
    let endpoint = endpoint(&server, username, &args.project, "invitations");
    let (status, response) = request(
        "POST",
        &endpoint,
        &session.access_token,
        &serde_json::json!({
            "email": email,
            "role": args.role.as_str(),
            "help_requested": help,
        }),
    )?;
    let result: InviteResponse =
        serde_json::from_str(&response).context("decode invitation response")?;
    if !(200..300).contains(&status) || !result.email_sent {
        bail!(
            "{}",
            result
                .error
                .unwrap_or_else(|| format!("invitation failed with HTTP {status}"))
        );
    }
    println!("Invitation email sent successfully.");
    if let Some(url) = result.invite_url {
        println!("Invitation: {url}");
    }
    Ok(())
}

pub(crate) fn share_x(args: &ShareXArgs) -> Result<()> {
    let (session, server) = session_and_server(args.server.as_deref())?;
    let username = session
        .username
        .as_deref()
        .context("Fractal Society username missing; run `fractal login` again")?;
    let endpoint = endpoint(&server, username, &args.project, "share/x");
    let help = args.message.as_deref().unwrap_or("").trim();
    let preview_body = serde_json::json!({
        "handle": args.handle,
        "help_requested": help,
        "confirm": false,
    });
    let (status, response) = request("POST", &endpoint, &session.access_token, &preview_body)?;
    let preview: XResponse = serde_json::from_str(&response).context("decode X post preview")?;
    if !(200..300).contains(&status) {
        bail!(
            "{}",
            preview
                .error
                .unwrap_or_else(|| format!("X post preview failed with HTTP {status}"))
        );
    }
    let text = preview
        .preview
        .context("Fractal Society returned no X post preview")?;
    println!("X post preview:\n\n{text}\n");
    if !args.yes {
        open_share_preview(
            &session,
            &server,
            serde_json::json!({
                "kind": "x",
                "owner": username,
                "project": args.project,
                "handle": args.handle,
                "help_requested": help,
            }),
        )?;
        bail!("X composer not opened; after the user explicitly confirms this exact preview, repeat with `--yes`");
    }
    let intent_url = preview
        .intent_url
        .context("Fractal Society returned no X composer URL")?;
    validate_x_intent_url(&intent_url)?;
    let handoff = queue_x_share(&intent_url, &text)?;
    let launched = launch_fractal_voice(&handoff);
    println!(
        "{} X composer request to Fractal Voice. Review the prefilled post in X, then choose Post.",
        if launched { "Sent" } else { "Queued" }
    );
    Ok(())
}

fn validate_x_intent_url(value: &str) -> Result<()> {
    const PREFIX: &str = "https://x.com/intent/tweet?";
    let query = value
        .strip_prefix(PREFIX)
        .context("Fractal Society returned an untrusted X composer URL")?;
    if value.len() > 4_096
        || value.chars().any(char::is_control)
        || !query.starts_with("text=")
        || query.contains('&')
        || query.contains('#')
        || query.len() <= "text=".len()
    {
        bail!("Fractal Society returned an invalid X composer URL");
    }
    Ok(())
}

fn queue_x_share(intent_url: &str, preview: &str) -> Result<PathBuf> {
    let created_at_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before Unix epoch")?
        .as_millis();
    let envelope = ExternalXShareHandoff {
        schema: "fractal.external_x_share.v1",
        intent_url,
        preview,
        created_at_ms,
    };
    let bytes = serde_json::to_vec(&envelope).context("encode X share handoff")?;
    let mut seed = Sha256::new();
    seed.update(&bytes);
    seed.update(std::process::id().to_le_bytes());
    let nonce: String = seed
        .finalize()
        .iter()
        .take(12)
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let path = PathBuf::from("/tmp").join(format!(
        "fractal-x-share-{}-{nonce}.fractalxshare",
        std::process::id()
    ));
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(&path)
        .with_context(|| format!("create secure X share handoff {}", path.display()))?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    Ok(path)
}

fn launch_fractal_voice(path: &Path) -> bool {
    const APP_PATH: &str = "/Applications/Fractal Voice.app";
    const BUNDLE_ID: &str = "com.fractalsociety.voice";
    if Path::new(APP_PATH).is_dir() && run_open(&["-a", APP_PATH], path) {
        return true;
    }
    run_open(&["-b", BUNDLE_ID], path)
}

fn run_open(arguments: &[&str], path: &Path) -> bool {
    Command::new("/usr/bin/open")
        .args(arguments)
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

pub(crate) fn connect_x(_args: &ConnectXArgs) -> Result<()> {
    println!("X OAuth is disabled and is not required.");
    println!(
        "Use `fractal share-x --project PROJECT --handle @PERSON --message TEXT`; \
         Fractal Voice will open X's free prefilled composer after confirmation."
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn url_segments_encode_untrusted_values() {
        assert_eq!(segment("hello world/@x"), "hello%20world%2F%40x");
    }

    #[test]
    fn x_share_handoff_is_private_and_accepts_only_x_composer_urls() {
        let intent = "https://x.com/intent/tweet?text=Hello%20%40buildfractal";
        validate_x_intent_url(intent).unwrap();
        assert!(validate_x_intent_url("https://evil.example/intent/tweet?text=no").is_err());
        assert!(validate_x_intent_url("https://x.com/intent/tweet?text=ok&url=bad").is_err());

        let path = queue_x_share(intent, "Hello @buildfractal").unwrap();
        let metadata = fs::metadata(&path).unwrap();
        let value: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
        assert_eq!(value["schema"], "fractal.external_x_share.v1");
        assert_eq!(value["intent_url"], intent);
        assert_eq!(value["preview"], "Hello @buildfractal");
        fs::remove_file(path).unwrap();
    }
}
