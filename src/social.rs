use std::process::Command;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::Deserialize;

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
    post_url: Option<String>,
    #[serde(default)]
    connect_url: Option<String>,
    #[serde(default)]
    error: Option<String>,
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
        bail!("X post not published; after the user explicitly confirms this preview, repeat with `--yes`");
    }
    let (status, response) = request(
        "POST",
        &endpoint,
        &session.access_token,
        &serde_json::json!({
            "handle": args.handle,
            "help_requested": help,
            "confirm": true,
        }),
    )?;
    let result: XResponse = serde_json::from_str(&response).context("decode X post result")?;
    if status == 409 {
        if let Some(connect_url) = result.connect_url {
            let absolute = if connect_url.starts_with('/') {
                format!("{server}{connect_url}")
            } else {
                connect_url
            };
            let _ = Command::new("open").arg(&absolute).status();
            bail!(
                "connect your X account in the opened browser, then repeat the confirmed command"
            );
        }
    }
    if !(200..300).contains(&status) {
        bail!(
            "{}",
            result
                .error
                .unwrap_or_else(|| format!("X post failed with HTTP {status}"))
        );
    }
    println!(
        "Posted to X successfully: {}",
        result.post_url.context("X returned no post URL")?
    );
    Ok(())
}

pub(crate) fn connect_x(args: &ConnectXArgs) -> Result<()> {
    let (session, server) = session_and_server(args.server.as_deref())?;
    let username = session
        .username
        .as_deref()
        .context("Fractal Society username missing; run `fractal login` again")?;
    let next = args
        .project
        .as_deref()
        .map(|project| format!("/@{}/{}", segment(username), segment(project)))
        .unwrap_or_else(|| "/account/projects".to_owned());
    let redirect_path = format!("/api/providers/x/authorize?next={}", segment(&next));
    let endpoint = format!(
        "{}/api/fractal/auth/browser-handoff",
        server.trim_end_matches('/')
    );
    let (status, response) = request(
        "POST",
        &endpoint,
        &session.access_token,
        &serde_json::json!({ "redirect_path": redirect_path }),
    )?;
    let result: BrowserHandoffResponse =
        serde_json::from_str(&response).context("decode browser handoff response")?;
    if !(200..300).contains(&status) {
        bail!(
            "{}",
            result
                .error
                .unwrap_or_else(|| format!("browser handoff failed with HTTP {status}"))
        );
    }
    let url = result
        .browser_url
        .context("Fractal Society returned no browser connection URL")?;
    println!("Authorize X for Fractal Society:\n  {url}");
    if !args.no_open {
        Command::new("open")
            .arg(&url)
            .status()
            .context("open X authorization in browser")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_segments_encode_untrusted_values() {
        assert_eq!(segment("hello world/@x"), "hello%20world%2F%40x");
    }
}
