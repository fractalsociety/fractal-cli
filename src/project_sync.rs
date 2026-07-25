//! Explicit, fail-soft project publication hooks.
//!
//! Local `.fractal/project.fractal` persistence never depends on a network.
//! Logging in opts the CLI into private Fractal Society project publication.
//! GitHub mirroring remains explicitly opt-in. `fractal sync --disable` creates
//! a project-local opt-out marker.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::cli::SyncArgs;

#[derive(Debug, Deserialize, Serialize)]
struct SyncPreference {
    schema: String,
    enabled: bool,
    sync_github: bool,
}

#[derive(Debug, Deserialize, Serialize)]
struct SyncState {
    schema: String,
    server: String,
    account: Option<String>,
    last_remote_hash: String,
}

#[derive(Debug, Deserialize)]
struct SyncResponse {
    project_url: String,
    #[serde(default)]
    github_url: Option<String>,
    #[serde(default)]
    github_notice: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RemoteError {
    #[serde(default)]
    error: String,
    #[serde(default)]
    current_hash: Option<String>,
}

enum PutFailure {
    Status(u16, Box<ureq::Response>),
    Transport(anyhow::Error),
}

fn preference_path(workspace: &Path) -> PathBuf {
    workspace.join(".fractal").join("sync.json")
}

fn state_path(workspace: &Path) -> PathBuf {
    workspace.join(".fractal").join("sync-state.json")
}

pub(crate) fn run(args: &SyncArgs) -> Result<()> {
    let workspace = resolve_workspace(args.repo.as_deref())?;
    if args.disable {
        save_preference(
            &workspace,
            &SyncPreference {
                schema: "fractal.project_sync.v1".to_owned(),
                enabled: false,
                sync_github: false,
            },
        )?;
        println!("Automatic web sync disabled for {}.", workspace.display());
        return Ok(());
    }
    if args.enable {
        save_preference(
            &workspace,
            &SyncPreference {
                schema: "fractal.project_sync.v1".to_owned(),
                enabled: true,
                sync_github: args.github,
            },
        )?;
    }
    let response = upload(&workspace, args.github)?;
    println!("Project URL: {}", response.project_url);
    report_github_result(&response, args.github)?;
    if !args.enable {
        println!(
            "One-shot sync complete. Pass --enable to sync future graph updates automatically."
        );
    }
    Ok(())
}

pub(crate) fn maybe_sync(workspace: &Path) {
    let sync_github = match load_preference(workspace) {
        Some(preference) if !preference.enabled => return,
        Some(preference) => preference.sync_github,
        None => {
            // No login means purely local/offline operation, without a warning
            // on every graph build. A valid login enables private web sync.
            if crate::auth::load_session().is_err() {
                return;
            }
            false
        }
    };
    match upload(workspace, sync_github) {
        Ok(response) => {
            println!("  ↗ Project URL: {}", response.project_url);
            if let Err(error) = report_github_result(&response, sync_github) {
                eprintln!("  sync note: {error:#}");
            }
        }
        Err(error) => eprintln!("  sync note: {error:#}"),
    }
}

fn report_github_result(response: &SyncResponse, requested: bool) -> Result<()> {
    if !requested {
        return Ok(());
    }
    if let Some(url) = &response.github_url {
        println!("GitHub graph: {url}");
        return Ok(());
    }
    bail!(
        "{}",
        response.github_notice.as_deref().unwrap_or(
            "GitHub was not mirrored. Open the project URL, connect GitHub, choose a repository, and sync again."
        )
    )
}

fn upload(workspace: &Path, sync_github: bool) -> Result<SyncResponse> {
    let session = crate::auth::load_session()?;
    let document = crate::project_file::load(workspace)?;
    let endpoint = format!(
        "{}/api/cli/projects/{}",
        session.server.trim_end_matches('/'),
        document.project.slug
    );
    let mut body = serde_json::to_value(&document)?;
    body.as_object_mut()
        .context("fractal project must encode as an object")?
        .insert("sync_github".to_owned(), sync_github.into());
    let encoded = serde_json::to_string(&body)?;
    let prior_hash = load_state(workspace)
        .filter(|state| {
            state.server == session.server && state.account == session.account_identity()
        })
        .map(|state| state.last_remote_hash);

    let response = match put_project(
        &endpoint,
        &session.access_token,
        &encoded,
        prior_hash.as_deref(),
    ) {
        Ok(response) => response,
        Err(PutFailure::Status(428, response)) if prior_hash.is_none() => {
            let current_hash =
                fetch_remote_hash(&endpoint, &session.access_token).with_context(|| {
                    format!(
                        "bootstrap remote state after server required an update precondition: {}",
                        remote_error_message(*response)
                    )
                })?;
            match put_project(
                &endpoint,
                &session.access_token,
                &encoded,
                Some(&current_hash),
            ) {
                Ok(response) => response,
                Err(PutFailure::Status(409, response)) => {
                    bail!("project sync conflict: {}", remote_error_message(*response))
                }
                Err(PutFailure::Status(status, response)) => bail!(
                    "retry project graph upload failed with HTTP {status}: {}",
                    remote_error_message(*response)
                ),
                Err(PutFailure::Transport(error)) => {
                    return Err(error).context("retry project graph upload")
                }
            }
        }
        Err(PutFailure::Status(409, response)) => {
            bail!("project sync conflict: {}", remote_error_message(*response))
        }
        Err(PutFailure::Status(428, response)) => {
            bail!(
                "project sync precondition was rejected: {}",
                remote_error_message(*response)
            )
        }
        Err(PutFailure::Status(status, response)) => bail!(
            "project graph upload failed with HTTP {status}: {}",
            remote_error_message(*response)
        ),
        Err(PutFailure::Transport(error)) => return Err(error).context("upload project graph"),
    };
    let remote_hash = response
        .header("ETag")
        .and_then(parse_etag)
        .unwrap_or_else(|| document.graph_hash.clone());
    let result: SyncResponse =
        serde_json::from_reader(response.into_reader()).context("decode project sync response")?;
    if result.project_url.is_empty() {
        bail!("Fractal Society returned an empty project URL");
    }
    let account = session.account_identity();
    save_state(
        workspace,
        &SyncState {
            schema: "fractal.project_sync_state.v1".to_owned(),
            server: session.server,
            account,
            last_remote_hash: remote_hash,
        },
    )?;
    Ok(result)
}

fn put_project(
    endpoint: &str,
    access_token: &str,
    body: &str,
    if_match: Option<&str>,
) -> std::result::Result<ureq::Response, PutFailure> {
    let mut request = ureq::put(endpoint)
        .set("Authorization", &format!("Bearer {access_token}"))
        .set("Content-Type", "application/json");
    if let Some(hash) = if_match {
        request = request.set("If-Match", hash);
    }
    match request.send_string(body) {
        Ok(response) => Ok(response),
        Err(ureq::Error::Status(status, response)) => {
            Err(PutFailure::Status(status, Box::new(response)))
        }
        Err(error) => Err(PutFailure::Transport(error.into())),
    }
}

fn fetch_remote_hash(endpoint: &str, access_token: &str) -> Result<String> {
    let response = ureq::get(endpoint)
        .set("Authorization", &format!("Bearer {access_token}"))
        .call()
        .context("fetch current remote project")?;
    let etag = response
        .header("ETag")
        .and_then(parse_etag)
        .context("remote project response is missing a valid ETag")?;
    Ok(etag)
}

fn parse_etag(raw: &str) -> Option<String> {
    let hash = raw.trim().trim_start_matches("W/").trim_matches('"');
    let digest = hash.strip_prefix("sha256:")?;
    if digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Some(format!("sha256:{}", digest.to_ascii_lowercase()))
    } else {
        None
    }
}

fn remote_error_message(response: ureq::Response) -> String {
    let status = response.status();
    let body: RemoteError =
        serde_json::from_reader(response.into_reader()).unwrap_or(RemoteError {
            error: String::new(),
            current_hash: None,
        });
    let detail = if body.error.is_empty() {
        format!("HTTP {status}")
    } else {
        body.error
    };
    match body.current_hash {
        Some(hash) => format!("{detail} (remote graph is {hash})"),
        None => detail,
    }
}

fn resolve_workspace(repo: Option<&Path>) -> Result<PathBuf> {
    let path = repo
        .map(Path::to_path_buf)
        .unwrap_or(std::env::current_dir()?);
    path.canonicalize()
        .with_context(|| format!("resolve project workspace {}", path.display()))
}

fn load_preference(workspace: &Path) -> Option<SyncPreference> {
    serde_json::from_slice(&fs::read(preference_path(workspace)).ok()?).ok()
}

fn save_preference(workspace: &Path, preference: &SyncPreference) -> Result<()> {
    let path = preference_path(workspace);
    let parent = path.parent().expect("sync preference has parent");
    fs::create_dir_all(parent)?;
    fs::write(&path, serde_json::to_vec_pretty(preference)?)
        .with_context(|| format!("write {}", path.display()))
}

fn load_state(workspace: &Path) -> Option<SyncState> {
    let state: SyncState = serde_json::from_slice(&fs::read(state_path(workspace)).ok()?).ok()?;
    (state.schema == "fractal.project_sync_state.v1"
        && parse_etag(&state.last_remote_hash).is_some())
    .then_some(state)
}

fn save_state(workspace: &Path, state: &SyncState) -> Result<()> {
    let path = state_path(workspace);
    let parent = path.parent().expect("sync state has parent");
    fs::create_dir_all(parent)?;
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    let temporary = path.with_extension(format!("tmp-{}-{nonce}", std::process::id()));
    fs::write(&temporary, serde_json::to_vec_pretty(state)?)
        .with_context(|| format!("write {}", temporary.display()))?;
    fs::rename(&temporary, &path).with_context(|| format!("replace {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_preference_never_requests_sync() -> Result<()> {
        let workspace =
            std::env::temp_dir().join(format!("fractal-sync-test-{}", std::process::id()));
        fs::create_dir_all(&workspace)?;
        save_preference(
            &workspace,
            &SyncPreference {
                schema: "fractal.project_sync.v1".to_owned(),
                enabled: false,
                sync_github: true,
            },
        )?;
        let loaded = load_preference(&workspace).unwrap();
        assert!(!loaded.enabled);
        assert!(loaded.sync_github);
        fs::remove_dir_all(workspace)?;
        Ok(())
    }

    #[test]
    fn requested_github_requires_proof_of_a_real_mirror() {
        let pending = SyncResponse {
            project_url: "https://fractalsociety.com/@builder/app".to_owned(),
            github_url: None,
            github_notice: Some(
                "Connect GitHub and choose a repository on the project page.".to_owned(),
            ),
        };
        let error = report_github_result(&pending, true)
            .expect_err("a notice is not proof that GitHub was mirrored");
        assert!(error.to_string().contains("Connect GitHub"));

        let mirrored = SyncResponse {
            project_url: pending.project_url,
            github_url: Some(
                "https://github.com/builder/app/blob/main/.fractal/project.fractal".to_owned(),
            ),
            github_notice: None,
        };
        assert!(report_github_result(&mirrored, true).is_ok());
    }

    #[test]
    fn sync_state_round_trips_remote_hash_without_enabling_sync() -> Result<()> {
        let workspace = std::env::temp_dir().join(format!(
            "fractal-sync-state-test-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
        ));
        fs::create_dir_all(&workspace)?;
        save_state(
            &workspace,
            &SyncState {
                schema: "fractal.project_sync_state.v1".to_owned(),
                server: "https://fractalsociety.com".to_owned(),
                account: Some("builder".to_owned()),
                last_remote_hash: format!("sha256:{}", "a".repeat(64)),
            },
        )?;
        let loaded = load_state(&workspace).expect("state should load");
        assert_eq!(loaded.account.as_deref(), Some("builder"));
        assert_eq!(
            loaded.last_remote_hash,
            format!("sha256:{}", "a".repeat(64))
        );
        assert!(!preference_path(&workspace).exists());
        fs::remove_dir_all(workspace)?;
        Ok(())
    }

    #[test]
    fn etag_parser_accepts_quoted_hash_and_rejects_malformed_values() {
        let hash = format!("sha256:{}", "A".repeat(64));
        assert_eq!(
            parse_etag(&format!("\"{hash}\"")),
            Some(format!("sha256:{}", "a".repeat(64)))
        );
        assert!(parse_etag("\"not-a-hash\"").is_none());
    }
}
