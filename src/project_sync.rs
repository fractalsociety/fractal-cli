//! Explicit, fail-soft project publication hooks.
//!
//! Local `.fractal/project.fractal` persistence never depends on a network.
//! Logging in opts the CLI into private Fractal Society project publication.
//! GitHub access remains entirely local: Fractal uses the repository's Git
//! remote and the user's existing `git`/`gh` authentication, then sends only
//! sanitized public repository links to Fractal Society.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Condvar, Mutex, OnceLock};
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
    browser_url: String,
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

#[derive(Debug, Deserialize)]
struct ControlEnvelope {
    command: Option<ControlCommand>,
}

#[derive(Debug, Deserialize)]
struct ControlCommand {
    command_id: String,
    action: String,
    #[serde(default)]
    task_ref: String,
    #[serde(default)]
    wave: Option<u32>,
    #[serde(default)]
    instruction: String,
}

pub(crate) enum HostedControl {
    Pause(String),
    AmendmentQueued,
}

enum PutFailure {
    Status(u16, Box<ureq::Response>),
    Transport(anyhow::Error),
}

#[derive(Default)]
struct UploadSchedule {
    active: bool,
    waiting_priority: usize,
}

#[derive(Default)]
struct UploadScheduler {
    state: Mutex<UploadSchedule>,
    ready: Condvar,
}

struct UploadPermit<'a> {
    scheduler: &'a UploadScheduler,
}

impl UploadScheduler {
    fn acquire(&self, priority: bool) -> UploadPermit<'_> {
        let mut state = self.state.lock().expect("project upload scheduler");
        if priority {
            state.waiting_priority += 1;
        }
        while state.active || (!priority && state.waiting_priority > 0) {
            state = self.ready.wait(state).expect("project upload scheduler");
        }
        if priority {
            state.waiting_priority -= 1;
        }
        state.active = true;
        UploadPermit { scheduler: self }
    }
}

impl Drop for UploadPermit<'_> {
    fn drop(&mut self) {
        let mut state = self
            .scheduler
            .state
            .lock()
            .expect("project upload scheduler");
        state.active = false;
        self.scheduler.ready.notify_all();
    }
}

#[derive(Default)]
struct RuntimeSync {
    dirty: bool,
}

static UPLOAD_SCHEDULER: OnceLock<UploadScheduler> = OnceLock::new();
static RUNTIME_SYNCS: OnceLock<Mutex<BTreeMap<PathBuf, RuntimeSync>>> = OnceLock::new();
pub(crate) const PROJECT_NAME_TAKEN_MARKER: &str = "FRACTAL_PROJECT_NAME_TAKEN";
const MAX_PROJECT_UPLOAD_BYTES: usize = 10 * 1024 * 1024;

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
    let response = upload(&workspace, repository.as_ref(), false, false)?;
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

/// Publish the current project when cloud sync is enabled and return the
/// authenticated Fractal Society page that should be shown to the user.
pub(crate) fn maybe_sync(workspace: &Path) -> Option<String> {
    maybe_sync_with_options(workspace, true)
}

/// Publish an early planning graph without waiting for GitHub initialization.
/// The full graph performs the normal GitHub sync after planning completes.
pub(crate) fn maybe_sync_planning(workspace: &Path) -> Option<String> {
    maybe_sync_with_options(workspace, false)
}

/// Refuse to start a new managed project when its profile URL already
/// belongs to another project. A missing Fractal Society login means there is
/// no profile publication to collide with, so local-only builds remain valid.
pub(crate) fn ensure_new_project_name_available(name: &str) -> Result<()> {
    if std::env::var_os("FRACTAL_OFFLINE").is_some() {
        return Ok(());
    }
    let session = match crate::auth::load_session() {
        Ok(session) => session,
        Err(_) => return Ok(()),
    };
    let slug = crate::project_file::slug_from(name);
    let endpoint = format!(
        "{}/api/cli/projects/{slug}",
        session.server.trim_end_matches('/')
    );
    match ureq::get(&endpoint)
        .set("Authorization", &format!("Bearer {}", session.access_token))
        .timeout(std::time::Duration::from_secs(5))
        .call()
    {
        Ok(_) => bail!("{PROJECT_NAME_TAKEN_MARKER}:{slug}"),
        Err(ureq::Error::Status(404, _)) => Ok(()),
        Err(ureq::Error::Status(status, response)) => bail!(
            "check project-name availability failed with HTTP {status}: {}",
            remote_error_message(response)
        ),
        Err(error) => {
            Err(anyhow::anyhow!(error)).context("check Fractal Society project-name availability")
        }
    }
}

/// Coalesce node transitions into at most one active and one follow-up upload
/// per workspace. Every pass reads the newest project document after receiving
/// its upload permit, so intermediate transition states collapse naturally.
pub(crate) fn maybe_sync_runtime(workspace: &Path) {
    if std::env::var_os("FRACTAL_OFFLINE").is_some() {
        return;
    }
    let workspace = workspace
        .canonicalize()
        .unwrap_or_else(|_| workspace.to_path_buf());
    let syncs = RUNTIME_SYNCS.get_or_init(|| Mutex::new(BTreeMap::new()));
    {
        let mut syncs = syncs.lock().expect("runtime sync registry");
        if let Some(sync) = syncs.get_mut(&workspace) {
            sync.dirty = true;
            return;
        }
        syncs.insert(workspace.clone(), RuntimeSync::default());
    }
    std::thread::spawn(move || loop {
        if let Err(error) = sync_runtime_now(&workspace) {
            eprintln!("  live graph sync note: {error:#}");
        }
        let mut syncs = RUNTIME_SYNCS
            .get()
            .expect("runtime sync registry")
            .lock()
            .expect("runtime sync registry");
        if syncs.get(&workspace).is_some_and(|sync| sync.dirty) {
            syncs.insert(workspace.clone(), RuntimeSync::default());
            continue;
        }
        syncs.remove(&workspace);
        break;
    });
}

pub(crate) fn sync_runtime_now(workspace: &Path) -> Result<()> {
    if std::env::var_os("FRACTAL_OFFLINE").is_some() {
        return Ok(());
    }
    match load_preference(workspace) {
        Some(preference) if !preference.enabled => return Ok(()),
        Some(_) => {}
        None if crate::auth::load_session().is_err() => return Ok(()),
        None => {}
    }
    upload(workspace, None, false, false).map(|_| ())
}

/// Publish a halted graph ahead of queued routine transition uploads.
pub(crate) fn sync_runtime_halt_now(workspace: &Path) -> Result<()> {
    if std::env::var_os("FRACTAL_OFFLINE").is_some() {
        return Ok(());
    }
    match load_preference(workspace) {
        Some(preference) if !preference.enabled => return Ok(()),
        Some(_) => {}
        None if crate::auth::load_session().is_err() => return Ok(()),
        None => {}
    }
    upload(workspace, None, true, false).map(|_| ())
}

/// Poll the authenticated Fractal Society project for an owner-requested pause.
/// The command is acknowledged before local cancellation, preventing a page
/// refresh from repeatedly stopping a later resumed run.
pub(crate) fn poll_control_command(workspace: &Path) -> Result<Option<HostedControl>> {
    if std::env::var_os("FRACTAL_OFFLINE").is_some() {
        return Ok(None);
    }
    let session = match crate::auth::load_session() {
        Ok(session) => session,
        Err(_) => return Ok(None),
    };
    let document = match crate::project_file::load(workspace) {
        Ok(document) => document,
        Err(_) => return Ok(None),
    };
    let endpoint = format!(
        "{}/api/cli/projects/{}/control",
        session.server.trim_end_matches('/'),
        document.project.slug
    );
    let response = match ureq::get(&endpoint)
        .set("Authorization", &format!("Bearer {}", session.access_token))
        .timeout(std::time::Duration::from_secs(3))
        .call()
    {
        Ok(response) => response,
        Err(ureq::Error::Status(404, _)) => return Ok(None),
        Err(error) => return Err(anyhow::anyhow!(error)).context("poll hosted graph control"),
    };
    let envelope: ControlEnvelope =
        serde_json::from_reader(response.into_reader()).context("decode hosted graph control")?;
    let Some(command) = envelope.command else {
        return Ok(None);
    };
    if matches!(command.action.as_str(), "add_branch" | "add_wave_task") {
        crate::amendments::queue(
            workspace,
            &command.command_id,
            &command.action,
            &command.task_ref,
            command.wave,
            &command.instruction,
            "fractal-society",
        )?;
        update_hosted_command(workspace, &command.command_id, "accepted", None)?;
        if command.action == "add_wave_task" {
            println!(
                "  ✓ accepted hosted task request for wave {}; lead planner will apply it at the next safe boundary",
                command.wave.unwrap_or_default()
            );
        } else {
            println!(
                "  ✓ accepted hosted branch request for task {}; lead planner will apply it between waves",
                command.task_ref
            );
        }
        return Ok(Some(HostedControl::AmendmentQueued));
    }
    if command.action != "pause" {
        return Ok(None);
    }
    let body = serde_json::json!({
        "command_id": command.command_id,
        "status": "accepted",
    })
    .to_string();
    match ureq::post(&endpoint)
        .set("Authorization", &format!("Bearer {}", session.access_token))
        .set("Content-Type", "application/json")
        .timeout(std::time::Duration::from_secs(3))
        .send_string(&body)
    {
        Ok(_) => Ok(Some(HostedControl::Pause(command.command_id))),
        Err(error) => Err(anyhow::anyhow!(error)).context("acknowledge hosted graph control"),
    }
}

pub(crate) fn mark_amendment_result(
    workspace: &Path,
    command_id: &str,
    applied: bool,
    error: Option<&str>,
) -> Result<()> {
    update_hosted_command(
        workspace,
        command_id,
        if applied { "applied" } else { "failed" },
        error,
    )
}

fn update_hosted_command(
    workspace: &Path,
    command_id: &str,
    status: &str,
    error: Option<&str>,
) -> Result<()> {
    let session = crate::auth::load_session()?;
    let document = crate::project_file::load(workspace)?;
    let endpoint = format!(
        "{}/api/cli/projects/{}/control",
        session.server.trim_end_matches('/'),
        document.project.slug
    );
    let body = serde_json::json!({
        "command_id": command_id,
        "status": status,
        "error": error,
    })
    .to_string();
    ureq::post(&endpoint)
        .set("Authorization", &format!("Bearer {}", session.access_token))
        .set("Content-Type", "application/json")
        .timeout(std::time::Duration::from_secs(3))
        .send_string(&body)
        .map(|_| ())
        .map_err(anyhow::Error::new)
        .context("update hosted graph command")
}

/// Report that the coordinator and worker process groups have been terminated.
/// The final `synchronized` state is set by the server only after it receives
/// the halted project graph.
pub(crate) fn mark_pause_agents_stopped(workspace: &Path, command_id: &str) -> Result<()> {
    let session = crate::auth::load_session()?;
    let document = crate::project_file::load(workspace)?;
    let endpoint = format!(
        "{}/api/cli/projects/{}/control",
        session.server.trim_end_matches('/'),
        document.project.slug
    );
    let body = serde_json::json!({
        "command_id": command_id,
        "status": "agents_stopped",
    })
    .to_string();
    ureq::post(&endpoint)
        .set("Authorization", &format!("Bearer {}", session.access_token))
        .set("Content-Type", "application/json")
        .timeout(std::time::Duration::from_secs(3))
        .send_string(&body)
        .map(|_| ())
        .map_err(anyhow::Error::new)
        .context("report stopped agents to hosted graph")
}

fn maybe_sync_with_options(workspace: &Path, publish_github: bool) -> Option<String> {
    if std::env::var_os("FRACTAL_OFFLINE").is_some() {
        return None;
    }
    match load_preference(workspace) {
        Some(preference) if !preference.enabled => return None,
        Some(_) => {}
        None => {
            // No login means purely local/offline operation, without a warning
            // on every graph build. A valid login enables private web sync.
            if crate::auth::load_session().is_err() {
                return None;
            }
        }
    }
    let repository = if publish_github {
        match publish_local_github(workspace) {
            Ok(repository) => repository,
            Err(error) => {
                eprintln!("  GitHub sync note: {error:#}");
                None
            }
        }
    } else {
        None
    };
    match upload(workspace, repository.as_ref(), false, false) {
        Ok(response) => {
            println!("  ↗ Project URL: {}", response.project_url);
            if let Some(repository) = repository {
                println!("  ↗ GitHub graph: {}", repository.github_graph_url);
            }
            Some(if response.browser_url.is_empty() {
                response.project_url
            } else {
                response.browser_url
            })
        }
        Err(error) => {
            eprintln!("  sync note: {error:#}");
            None
        }
    }
}

fn upload(
    workspace: &Path,
    repository: Option<&RepositoryLink>,
    priority: bool,
    authoritative_local_visibility: bool,
) -> Result<SyncResponse> {
    let _permit = UPLOAD_SCHEDULER
        .get_or_init(UploadScheduler::default)
        .acquire(priority);
    crate::project_file::backfill_execution(workspace).ok();
    if !crate::run_control::workspace_is_running(workspace) {
        crate::project_file::release_stale_assignments(workspace).ok();
    }
    let session = crate::auth::load_session()?;
    let mut document = crate::project_file::load(workspace)?;
    let endpoint = format!(
        "{}/api/cli/projects/{}",
        session.server.trim_end_matches('/'),
        document.project.slug
    );
    let mut hosted_hash = None;
    if !authoritative_local_visibility {
        if let Ok(response) = ureq::get(&endpoint)
            .set("Authorization", &format!("Bearer {}", session.access_token))
            .timeout(std::time::Duration::from_secs(5))
            .call()
        {
            let response_hash = response.header("ETag").and_then(parse_etag);
            if let Ok(hosted) = serde_json::from_reader::<_, crate::project_file::FractalProject>(
                response.into_reader(),
            ) {
                if hosted.graph_hash == document.graph_hash {
                    hosted_hash = response_hash;
                    if hosted.project.visibility != document.project.visibility {
                        crate::project_file::set_visibility(workspace, &hosted.project.visibility)?;
                        document = crate::project_file::load(workspace)?;
                    }
                }
            }
        }
    }
    let encoded = encode_upload_body_from_document(&document, repository)?;
    let prior_hash = hosted_hash.or_else(|| {
        load_state(workspace)
            .filter(|state| {
                state.server == session.server && state.account == session.account_identity()
            })
            .map(|state| state.last_remote_hash)
    });

    let response = match put_project(
        &endpoint,
        &session.access_token,
        &encoded,
        prior_hash.as_deref(),
    ) {
        Ok(response) => response,
        Err(PutFailure::Status(409 | 412 | 428, response)) if prior_hash.is_none() => bail!(
            "{PROJECT_NAME_TAKEN_MARKER}:{} ({})",
            document.project.slug,
            remote_error_message(*response)
        ),
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
    let mut result: SyncResponse =
        serde_json::from_reader(response.into_reader()).context("decode project sync response")?;
    if result.project_url.starts_with('/') {
        result.project_url = format!(
            "{}{}",
            session.server.trim_end_matches('/'),
            result.project_url
        );
    }
    if result.browser_url.starts_with('/') {
        result.browser_url = format!(
            "{}{}",
            session.server.trim_end_matches('/'),
            result.browser_url
        );
    }
    if !is_safe_project_url(&result.project_url) {
        bail!(
            "Fractal Society returned an invalid project URL: {}",
            result.project_url
        );
    }
    if !result.browser_url.is_empty() && !is_safe_project_url(&result.browser_url) {
        bail!(
            "Fractal Society returned an invalid browser handoff URL: {}",
            result.browser_url
        );
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

#[cfg(test)]
fn encode_upload_body(workspace: &Path, repository: Option<&RepositoryLink>) -> Result<String> {
    let document = crate::project_file::load(workspace)?;
    encode_upload_body_from_document(&document, repository)
}

fn encode_upload_body_from_document(
    document: &crate::project_file::FractalProject,
    repository: Option<&RepositoryLink>,
) -> Result<String> {
    let mut body = serde_json::to_value(document)?;
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
    if encoded.len() > MAX_PROJECT_UPLOAD_BYTES {
        bail!(
            "project graph upload payload is too large ({} bytes > {} bytes)",
            encoded.len(),
            MAX_PROJECT_UPLOAD_BYTES
        );
    }
    Ok(encoded)
}

pub(crate) fn publish_visibility(workspace: &Path) -> Result<()> {
    let repository = publish_local_github(workspace)?
        .context("a GitHub origin is required to synchronize repository visibility")?;
    let response = upload(workspace, Some(&repository), true, true)?;
    println!("Project URL: {}", response.project_url);
    println!("GitHub graph: {}", repository.github_graph_url);
    Ok(())
}

fn is_safe_project_url(project_url: &str) -> bool {
    project_url.starts_with("https://")
        || has_loopback_http_authority(project_url, "127.0.0.1")
        || has_loopback_http_authority(project_url, "localhost")
        || has_loopback_http_authority(project_url, "[::1]")
}

fn has_loopback_http_authority(project_url: &str, authority: &str) -> bool {
    project_url
        .strip_prefix("http://")
        .and_then(|rest| rest.strip_prefix(authority))
        .is_some_and(|rest| {
            rest.is_empty()
                || rest.starts_with(':')
                || rest.starts_with('/')
                || rest.starts_with('?')
                || rest.starts_with('#')
        })
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
    } else {
        // Creation is create-only. Standards-compliant servers reject this PUT
        // if another project claimed the slug after the preflight check.
        request = request.set("If-None-Match", "*");
    }
    match request.send_string(body) {
        Ok(response) => Ok(response),
        Err(ureq::Error::Status(status, response)) => {
            Err(PutFailure::Status(status, Box::new(response)))
        }
        Err(error) => Err(PutFailure::Transport(error.into())),
    }
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
    use serde_json::{json, Value};

    fn temp_workspace(prefix: &str) -> Result<PathBuf> {
        let workspace = std::env::temp_dir().join(format!(
            "{prefix}-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
        ));
        fs::create_dir_all(&workspace)?;
        Ok(workspace)
    }

    fn valid_graph() -> Value {
        let mut graph = json!({
            "schema": "fractal.execution_graph.v1",
            "graph_id": "fg_sync_boundary",
            "nodes": [
                {"id": "build", "capability": "code.generate", "instruction": "Build", "future_node_field": {"keep": true}},
                {"id": "verify", "capability": "project.tests.execute", "instruction": "Verify"}
            ],
            "edges": [{"from": "build", "to": "verify"}],
            "future_graph_field": {"preserve": ["complete", "graph"]}
        });
        graph["graph_hash"] = Value::String(fractal_contracts::canonical_sha256(&graph).unwrap());
        graph
    }

    #[test]
    fn mocked_upload_payload_preserves_enriched_current_project_document() -> Result<()> {
        let workspace = temp_workspace("fractal-sync-enriched-payload")?;
        let graph = valid_graph();
        crate::project_file::persist(&workspace, &graph, "Sync enriched graph")?;
        crate::project_file::mark_node_ready(&workspace, "build")?;
        crate::project_file::checkout_start_node(&workspace, "build", "agent-1", "Agent One")?;
        crate::project_file::record_artifact_produced(&workspace, "build", "artifact:log")?;
        crate::project_file::record_artifact_consumed(&workspace, "build", "artifact:spec")?;
        crate::project_file::record_verification_result(
            &workspace,
            "build",
            true,
            vec!["artifact:log".to_owned()],
        )?;
        crate::project_file::finish_node(
            &workspace,
            "build",
            "agent-1",
            crate::learning_data::NodeOutcome::VerifiedSuccess,
        )?;
        crate::project_file::record_graph_edit(
            &workspace,
            graph["graph_hash"].as_str().unwrap(),
            "add_branch",
            Some("build"),
            vec!["verify".to_owned()],
            "operator requested verifier",
            "lead",
        )?;
        let outcome =
            crate::learning_data::aggregate(&crate::project_file::load(&workspace)?.learning);
        crate::project_file::store_graph_outcome(&workspace, outcome)?;

        let payload: Value = serde_json::from_str(&encode_upload_body(&workspace, None)?)?;

        assert_eq!(payload["graph"], graph);
        assert_eq!(
            payload["graph"]["future_graph_field"],
            json!({"preserve": ["complete", "graph"]})
        );
        assert_eq!(
            payload["learning"]["nodes"]["build"]["outcome"],
            json!("verified_success")
        );
        assert_eq!(
            payload["learning"]["nodes"]["build"]["artifacts_produced"],
            json!(["artifact:log"])
        );
        assert_eq!(
            payload["learning"]["nodes"]["build"]["consumed_by"],
            json!(["artifact:spec"])
        );
        assert_eq!(
            payload["learning"]["nodes"]["build"]["verification"]["passed"],
            json!(true)
        );
        assert_eq!(
            payload["learning"]["graph_edits"][0]["action"]["type"],
            json!("add_branch")
        );
        assert!(payload["learning"]["outcome"].is_object());
        fs::remove_dir_all(workspace)?;
        Ok(())
    }

    #[test]
    fn mocked_upload_payload_normalizes_legacy_project_without_stripping_graph() -> Result<()> {
        let workspace = temp_workspace("fractal-sync-legacy-payload")?;
        fs::create_dir_all(workspace.join(".fractal"))?;
        let graph = valid_graph();
        let legacy = json!({
            "schema": "fractal.project.v1",
            "project": {"slug": "legacy-sync", "title": "Legacy Sync", "visibility": "private"},
            "graph_hash": graph["graph_hash"],
            "graph": graph,
            "updated_at": "2024-01-01T00:00:00Z",
            "future_project_field": {"kept": true}
        });
        fs::write(
            crate::project_file::path(&workspace),
            serde_json::to_vec_pretty(&legacy)?,
        )?;

        let payload: Value = serde_json::from_str(&encode_upload_body(&workspace, None)?)?;

        assert_eq!(
            payload["graph"]["future_graph_field"],
            json!({"preserve": ["complete", "graph"]})
        );
        assert_eq!(payload["future_project_field"], json!({"kept": true}));
        assert_eq!(payload["learning"]["schema"], json!("fractal.learning.v1"));
        assert_eq!(
            payload["learning"]["nodes"]["verify"]["depends_on"],
            json!(["build"])
        );
        fs::remove_dir_all(workspace)?;
        Ok(())
    }

    #[test]
    fn invalid_or_oversized_project_data_is_refused_before_upload_payload_exists() -> Result<()> {
        let workspace = temp_workspace("fractal-sync-invalid-payload")?;
        let graph = valid_graph();
        crate::project_file::persist(&workspace, &graph, "Invalid upload")?;
        let mut raw: Value =
            serde_json::from_slice(&fs::read(crate::project_file::path(&workspace))?)?;
        raw["learning"]["nodes"]["build"]["notes"] = json!("x".repeat(1001));
        fs::write(
            crate::project_file::path(&workspace),
            serde_json::to_vec_pretty(&raw)?,
        )?;
        assert!(encode_upload_body(&workspace, None)
            .expect_err("oversized learning data must not be encoded for upload")
            .to_string()
            .contains("notes exceed"));
        fs::remove_dir_all(workspace)?;
        Ok(())
    }

    #[test]
    fn cross_boundary_sync_payload_preserves_ac1_through_ac8_fields() -> Result<()> {
        std::env::set_var("FRACTAL_OFFLINE", "1");
        let workspace = temp_workspace("fractal-sync-cross-boundary")?;
        fs::create_dir_all(workspace.join(".fractal"))?;
        fs::write(
            workspace.join(".fractal").join("lead-prd.json"),
            serde_json::to_vec(&json!({
                "schema": "fractal.prd.v1",
                "acceptance_criteria": [{"id": "AC-1"}, {"id": "AC-2"}]
            }))?,
        )?;
        let graph = valid_graph();
        let before_hash = graph["graph_hash"].as_str().unwrap().to_owned();
        crate::project_file::persist(&workspace, &graph, "Sync cross-boundary")?;
        crate::project_file::mark_node_ready(&workspace, "build")?;
        crate::project_file::checkout_start_node(&workspace, "build", "agent-1", "Agent One")?;
        crate::project_file::record_artifact_produced(&workspace, "build", "artifact:build")?;
        crate::project_file::record_artifact_consumed(&workspace, "build", "artifact:spec")?;
        crate::project_file::set_node_costs(&workspace, "build", Some(1.0), Some(1.25))?;
        crate::project_file::record_human_intervention(
            &workspace,
            "build",
            Some("operator nudged"),
        )?;
        crate::project_file::record_verification_result(
            &workspace,
            "build",
            true,
            vec!["evidence:ac-1".to_owned()],
        )?;
        crate::project_file::finish_node(
            &workspace,
            "build",
            "agent-1",
            crate::learning_data::NodeOutcome::VerifiedSuccess,
        )?;
        crate::project_file::record_graph_edit(
            &workspace,
            &before_hash,
            "add_branch",
            Some("build"),
            vec!["verify".to_owned()],
            "operator requested verifier",
            "lead",
        )?;
        let document = crate::project_file::load(&workspace)?;
        let outcome =
            crate::learning_data::aggregate_for_graph(&document.learning, &document.graph);
        crate::project_file::store_graph_outcome(&workspace, outcome)?;

        let payload: Value = serde_json::from_str(&encode_upload_body(&workspace, None)?)?;
        assert_eq!(payload["graph"], graph);
        assert_eq!(payload["graph_hash"], json!(before_hash));
        let build = &payload["learning"]["nodes"]["build"];
        assert_eq!(build["node_id"], json!("build"));
        assert_eq!(build["node_type"], json!("implementation"));
        assert!(!build["objective"].as_str().unwrap_or("").is_empty());
        assert_eq!(build["depends_on"], json!([]));
        assert!(build["created_at"].as_str().is_some());
        assert!(build["ready_at"].as_str().is_some());
        assert!(build["started_at"].as_str().is_some());
        assert!(build["finished_at"].as_str().is_some());
        assert_eq!(build["executor"]["agent"], json!("Agent One"));
        assert_eq!(build["attempt_count"], json!(1));
        assert_eq!(build["outcome"], json!("verified_success"));
        assert!(build.get("failure_code").is_none());
        assert_eq!(build["verification"]["passed"], json!(true));
        assert_eq!(
            build["verification"]["evidence_refs"],
            json!(["evidence:ac-1"])
        );
        assert_eq!(build["artifacts_produced"], json!(["artifact:build"]));
        assert_eq!(build["consumed_by"], json!(["artifact:spec"]));
        assert_eq!(build["human_intervention"], json!(true));
        assert_eq!(build["estimated_cost"], json!(1.0));
        assert_eq!(build["actual_cost"], json!(1.25));
        assert_eq!(
            payload["learning"]["graph_edits"][0]["action"]["type"],
            json!("add_branch")
        );
        assert_eq!(
            payload["learning"]["graph_edits"][0]["graph_before_hash"],
            json!(before_hash)
        );
        let outcome = &payload["learning"]["outcome"];
        assert!(outcome.is_object());
        assert!(outcome["acceptance_criteria"].is_array());
        assert!(outcome["maximum_parallelism"].as_u64().is_some());
        assert!(outcome["retry_count"].as_u64().is_some());
        assert!(outcome["reopened_node_count"].as_u64().is_some());
        assert!(outcome["dead_or_unused_node_count"].as_u64().is_some());
        assert!(outcome["human_intervention_count"].as_u64().is_some());
        assert!(outcome["verification_coverage"].as_f64().is_some());
        assert!(outcome["verification_coverage_denominator"]
            .as_u64()
            .is_some());
        for optional in [
            "final_verified_success",
            "total_duration_seconds",
            "critical_path_duration_seconds",
            "total_agent_time_seconds",
            "total_cost",
            "stopped_too_early",
            "expanded_unnecessarily",
        ] {
            if let Some(value) = outcome.get(optional) {
                assert!(
                    value.is_null() || value.is_boolean() || value.is_number(),
                    "{optional} must be null, bool, or number when present"
                );
            }
        }
        let typed = crate::project_file::load(&workspace)?
            .learning
            .outcome
            .unwrap();
        assert!(typed.human_intervention_count >= 1);
        assert!(typed.verification_coverage_denominator >= 1);
        let encoded = serde_json::to_string(&payload)?;
        assert!(!encoded.contains("chain_of_thought"));
        assert!(!encoded.contains("api_key"));
        fs::remove_dir_all(workspace)?;
        Ok(())
    }

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

    #[test]
    fn browser_project_urls_require_https_or_a_loopback_server() {
        assert!(is_safe_project_url(
            "https://fractalsociety.com/builder/project"
        ));
        assert!(is_safe_project_url("http://127.0.0.1:3000/builder/project"));
        assert!(is_safe_project_url("http://localhost:3000/builder/project"));
        assert!(!is_safe_project_url("http://fractalsociety.com/project"));
        assert!(!is_safe_project_url("http://localhost.evil.test/project"));
        assert!(!is_safe_project_url("file:///tmp/project"));
        assert!(!is_safe_project_url(""));
    }
}
