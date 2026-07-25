//! Explicit, fail-soft project publication hooks.
//!
//! Local `.fractal/project.fractal` persistence never depends on a network.
//! Logging in opts the CLI into private Fractal Society project publication.
//! GitHub access remains entirely local: Fractal uses the repository's Git
//! remote and the user's existing `git`/`gh` authentication, then sends only
//! sanitized public repository links to Fractal Society.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
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
}

#[derive(Debug, Serialize)]
struct RepositoryLink {
    repository_url: String,
    github_graph_url: String,
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
                sync_github: true,
            },
        )?;
    }
    let repository = match publish_local_github(&workspace) {
        Ok(repository) => repository,
        Err(error) if args.github => return Err(error),
        Err(error) => {
            eprintln!("GitHub sync note: {error:#}");
            None
        }
    };
    let response = upload(&workspace, repository.as_ref())?;
    println!("Project URL: {}", response.project_url);
    if let Some(repository) = repository {
        println!("GitHub graph: {}", repository.github_graph_url);
    }
    if !args.enable {
        println!(
            "One-shot sync complete. Pass --enable to sync future graph updates automatically."
        );
    }
    Ok(())
}

pub(crate) fn maybe_sync(workspace: &Path) {
    match load_preference(workspace) {
        Some(preference) if !preference.enabled => return,
        Some(_) => {}
        None => {
            // No login means purely local/offline operation, without a warning
            // on every graph build. A valid login enables private web sync.
            if crate::auth::load_session().is_err() {
                return;
            }
        }
    }
    let repository = match publish_local_github(workspace) {
        Ok(repository) => repository,
        Err(error) => {
            eprintln!("  GitHub sync note: {error:#}");
            None
        }
    };
    match upload(workspace, repository.as_ref()) {
        Ok(response) => {
            println!("  ↗ Project URL: {}", response.project_url);
            if let Some(repository) = repository {
                println!("  ↗ GitHub graph: {}", repository.github_graph_url);
            }
        }
        Err(error) => eprintln!("  sync note: {error:#}"),
    }
}

fn upload(workspace: &Path, repository: Option<&RepositoryLink>) -> Result<SyncResponse> {
    let session = crate::auth::load_session()?;
    let document = crate::project_file::load(workspace)?;
    let endpoint = format!(
        "{}/api/cli/projects/{}",
        session.server.trim_end_matches('/'),
        document.project.slug
    );
    let mut body = serde_json::to_value(&document)?;
    if let Some(repository) = repository {
        let object = body
            .as_object_mut()
            .context("fractal project must encode as an object")?;
        object.insert(
            "repository_url".to_owned(),
            repository.repository_url.clone().into(),
        );
        object.insert(
            "github_graph_url".to_owned(),
            repository.github_graph_url.clone().into(),
        );
    }
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

fn publish_local_github(workspace: &Path) -> Result<Option<RepositoryLink>> {
    let root = match git_output(workspace, &["rev-parse", "--show-toplevel"]) {
        Ok(root) => PathBuf::from(root),
        Err(_) => {
            run_command(
                Command::new("git").arg("init").arg(workspace),
                "initialize local Git repository",
            )?;
            workspace.to_path_buf()
        }
    };
    let graph = crate::project_file::path(workspace);
    let relative_graph = graph.strip_prefix(&root).with_context(|| {
        format!(
            "project graph {} is outside Git repository {}",
            graph.display(),
            root.display()
        )
    })?;

    let remote = match git_output(&root, &["remote", "get-url", "origin"]) {
        Ok(remote) => remote,
        Err(_) => {
            let slug = crate::project_file::load(workspace)?.project.slug;
            run_command(
                Command::new("gh")
                    .current_dir(&root)
                    .args(["repo", "create", &slug, "--private", "--source", "."])
                    .args(["--remote", "origin"]),
                "create a private GitHub repository with local `gh` authentication",
            )
            .context("run `gh auth login` first, or add a GitHub `origin` remote")?;
            git_output(&root, &["remote", "get-url", "origin"])?
        }
    };
    let repository_url = canonical_github_repository(&remote)
        .context("origin must point to github.com before Fractal can publish it")?;

    let relative = relative_graph
        .to_str()
        .context("project graph path must be valid UTF-8")?;
    let mut artifacts = vec![relative.to_owned()];
    for name in ["lead-prd.json", "closeout.json"] {
        let path = workspace.join(".fractal").join(name);
        if path.is_file() {
            validate_publishable_artifact(&path)?;
            artifacts.push(
                path.strip_prefix(&root)?
                    .to_str()
                    .context("Fractal artifact path must be valid UTF-8")?
                    .to_owned(),
            );
        }
    }
    let mut add = Command::new("git");
    add.current_dir(&root).args(["add", "-f", "--"]);
    add.args(&artifacts);
    run_command(&mut add, "stage Fractal project artifacts")?;
    let mut diff = Command::new("git");
    diff.current_dir(&root)
        .args(["diff", "--cached", "--quiet", "--"])
        .args(&artifacts);
    let changed = !command_success(&mut diff)?;
    if changed {
        let mut commit = Command::new("git");
        commit
            .current_dir(&root)
            .args([
                "commit",
                "--only",
                "-m",
                "Update Fractal project artifacts",
                "--",
            ])
            .args(&artifacts);
        run_command(&mut commit, "commit Fractal project artifacts")?;
    }
    let commit = git_output(&root, &["rev-parse", "HEAD"])
        .context("Git repository needs a commit before it can be published")?;
    run_command(
        Command::new("git")
            .current_dir(&root)
            .args(["push", "--set-upstream", "origin", "HEAD"]),
        "push the Fractal graph with local GitHub credentials",
    )?;
    let path = relative
        .split('/')
        .map(url_path_segment)
        .collect::<Vec<_>>()
        .join("/");
    Ok(Some(RepositoryLink {
        github_graph_url: format!("{repository_url}/blob/{commit}/{path}"),
        repository_url,
    }))
}

fn validate_publishable_artifact(path: &Path) -> Result<()> {
    fn walk(value: &serde_json::Value) -> Result<()> {
        match value {
            serde_json::Value::Object(object) => {
                for (key, child) in object {
                    let normalized = key.to_ascii_lowercase().replace('-', "_");
                    if matches!(
                        normalized.as_str(),
                        "access_token"
                            | "api_key"
                            | "authorization"
                            | "cookie"
                            | "credentials"
                            | "password"
                            | "private_key"
                            | "refresh_token"
                            | "secret"
                            | "token"
                    ) {
                        bail!("refuse to publish credential-shaped field `{key}`");
                    }
                    walk(child)?;
                }
            }
            serde_json::Value::Array(values) => {
                for value in values {
                    walk(value)?;
                }
            }
            _ => {}
        }
        Ok(())
    }
    let value: serde_json::Value = serde_json::from_slice(&fs::read(path)?)
        .with_context(|| format!("decode publishable artifact {}", path.display()))?;
    walk(&value).with_context(|| format!("inspect {}", path.display()))
}

fn git_output(root: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .current_dir(root)
        .args(args)
        .output()
        .with_context(|| format!("run git {}", args.join(" ")))?;
    if !output.status.success() {
        bail!("{}", String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

fn run_command(command: &mut Command, action: &str) -> Result<()> {
    let status = command.status().with_context(|| action.to_owned())?;
    if !status.success() {
        bail!("{action} failed with {status}");
    }
    Ok(())
}

fn command_success(command: &mut Command) -> Result<bool> {
    Ok(command.status()?.success())
}

fn canonical_github_repository(remote: &str) -> Option<String> {
    let path = if let Some(path) = remote.strip_prefix("git@github.com:") {
        path
    } else if let Some(path) = remote.strip_prefix("ssh://git@github.com/") {
        path
    } else {
        remote
            .strip_prefix("https://github.com/")
            .or_else(|| remote.strip_prefix("http://github.com/"))?
    };
    let path = path.trim_end_matches('/').trim_end_matches(".git");
    let mut parts = path.split('/');
    let owner = parts.next()?;
    let repository = parts.next()?;
    if parts.next().is_some() || !safe_github_segment(owner) || !safe_github_segment(repository) {
        return None;
    }
    Some(format!("https://github.com/{owner}/{repository}"))
}

fn safe_github_segment(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 100
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn url_path_segment(value: &str) -> String {
    value
        .bytes()
        .flat_map(|byte| {
            if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.') {
                vec![char::from(byte)]
            } else {
                format!("%{byte:02X}").chars().collect()
            }
        })
        .collect()
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
    fn normalizes_only_safe_github_repository_remotes() {
        assert_eq!(
            canonical_github_repository("git@github.com:builder/app.git").as_deref(),
            Some("https://github.com/builder/app")
        );
        assert_eq!(
            canonical_github_repository("https://github.com/builder/app/").as_deref(),
            Some("https://github.com/builder/app")
        );
        assert!(canonical_github_repository("https://token@github.com/builder/app.git").is_none());
        assert!(canonical_github_repository("https://gitlab.com/builder/app.git").is_none());
    }

    #[test]
    fn refuses_credential_fields_in_lead_artifacts() -> Result<()> {
        let directory = std::env::temp_dir().join(format!(
            "fractal-lead-artifact-{}",
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
        ));
        fs::create_dir_all(&directory)?;
        let safe = directory.join("safe.json");
        fs::write(&safe, r#"{"schema":"fractal.prd.v1","summary":"safe"}"#)?;
        assert!(validate_publishable_artifact(&safe).is_ok());
        let unsafe_path = directory.join("unsafe.json");
        fs::write(
            &unsafe_path,
            r#"{"architecture":{"api_key":"do-not-publish"}}"#,
        )?;
        assert!(validate_publishable_artifact(&unsafe_path).is_err());
        fs::remove_dir_all(directory)?;
        Ok(())
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
