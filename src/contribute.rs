//! Secure Fractal Society task handoff.
//!
//! The website issues a short-lived token bound to the signed-in account. This
//! command exchanges it with the CLI's existing bearer session, clones only the
//! graph's declared GitHub repository, creates the server-selected review
//! branch, and runs the requested task through the normal Fractal executor.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use serde::Deserialize;

use crate::cli::{ContributeArgs, DEFAULT_GRAPH_PORT};

#[derive(Debug, Deserialize)]
struct Handoff {
    action: String,
    owner: String,
    project: Project,
    task: Option<Task>,
}

#[derive(Debug, Deserialize)]
struct Project {
    slug: String,
    title: String,
    repository_url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Task {
    id: String,
    task_ref: String,
    title: String,
    description: String,
    branch_name: Option<String>,
}

pub(crate) fn run(
    args: &ContributeArgs,
    fractalwork_override: Option<&Path>,
    coordinate: bool,
) -> Result<()> {
    let session = crate::auth::load_session()
        .context("log in with `fractal login` before accepting a website task")?;
    let server = crate::auth::server_url(args.server.as_deref())?;
    if session.server.trim_end_matches('/') != server {
        bail!("task handoff server does not match the Fractal CLI login");
    }
    let endpoint = format!(
        "{server}/api/fractal/task-handoffs/{}",
        percent_encode(&args.token)
    );
    let response = ureq::post(&endpoint)
        .set("Authorization", &format!("Bearer {}", session.access_token))
        .set("Content-Type", "application/json")
        .timeout(std::time::Duration::from_secs(8))
        .send_string("{}")
        .map_err(anyhow::Error::new)
        .context("exchange task handoff")?;
    let handoff: Handoff =
        serde_json::from_reader(response.into_reader()).context("decode task handoff")?;

    if handoff.action == "resume" {
        let project = crate::projects::list()
            .into_iter()
            .find(|candidate| {
                Path::new(&candidate.workspace)
                    .file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with(&handoff.project.slug))
            })
            .context("this project's local checkpoint was not found on this Mac")?;
        crate::interactive::resume_project(
            project.number,
            fractalwork_override,
            DEFAULT_GRAPH_PORT,
            coordinate,
        )?;
        return Ok(());
    }

    let task = handoff
        .task
        .context("task handoff omitted its graph task")?;
    let repository = handoff
        .project
        .repository_url
        .as_deref()
        .context("this graph is not linked to a GitHub repository")?;
    validate_github_repository(repository)?;
    let branch = task
        .branch_name
        .as_deref()
        .context("task handoff omitted its review branch")?;
    validate_branch(branch)?;
    let workspace = contribution_workspace(&handoff.project.slug, &task.task_ref)?;
    clone_repository(repository, &workspace)?;
    run_git(&workspace, &["checkout", "-b", branch])?;
    crate::interactive::trust_managed_workspace(&workspace)?;
    update_claim(
        &server,
        &session.access_token,
        &handoff.owner,
        &handoff.project.slug,
        &task.id,
        "working",
        None,
    )?;

    println!(
        "Claimed task {} — {} from {}/{} ({})",
        task.task_ref, task.title, handoff.owner, handoff.project.slug, handoff.project.title,
    );
    println!("Review branch: {branch}");
    let request = format!(
        "Complete only Fractal graph task {}: {}.\n\n{}\n\n\
         Work inside the existing repository. Preserve unrelated behavior, run the relevant tests, \
         and leave the branch ready for the project owner to review.",
        task.task_ref, task.title, task.description
    );
    crate::interactive::execute_ingested(
        &request,
        Some(&workspace),
        fractalwork_override,
        coordinate,
        DEFAULT_GRAPH_PORT,
        None,
    )?;

    if git_has_changes(&workspace)? {
        run_git(&workspace, &["add", "-A"])?;
        run_git(
            &workspace,
            &[
                "commit",
                "-m",
                &format!("Complete task {}: {}", task.task_ref, task.title),
            ],
        )?;
    }
    let review_url =
        push_and_open_review(&workspace, repository, branch, &task.task_ref, &task.title)?;
    update_claim(
        &server,
        &session.access_token,
        &handoff.owner,
        &handoff.project.slug,
        &task.id,
        "review",
        Some(&review_url),
    )?;
    println!(
        "✓ Review branch pushed for the owner: {branch}\n  Review and open the pull request: {review_url}"
    );
    Ok(())
}

fn push_and_open_review(
    workspace: &Path,
    repository: &str,
    branch: &str,
    task_ref: &str,
    title: &str,
) -> Result<String> {
    let direct_push = run_git(workspace, &["push", "-u", "origin", branch]).is_ok();
    let mut head = branch.to_owned();
    if !direct_push {
        let fork = Command::new("gh")
            .current_dir(workspace)
            .args([
                "repo",
                "fork",
                repository,
                "--remote",
                "--remote-name",
                "contributor",
                "--clone=false",
            ])
            .status()
            .context("launch GitHub CLI fork for public contribution")?;
        if !fork.success() {
            bail!(
                "the upstream repository rejected the branch and GitHub CLI could not create your fork"
            );
        }
        run_git(workspace, &["push", "-u", "contributor", branch])?;
        let login = command_text(
            Command::new("gh").args(["api", "user", "--jq", ".login"]),
            "read GitHub username",
        )?;
        head = format!("{login}:{branch}");
    }

    let pull_request = command_text(
        Command::new("gh").current_dir(workspace).args([
            "pr",
            "create",
            "--repo",
            repository,
            "--head",
            &head,
            "--title",
            &format!("Complete task {task_ref}: {title}"),
            "--body",
            &format!(
                "Fractal Society contribution for graph task {task_ref}. \
                 The project owner should review verification evidence before merging."
            ),
        ]),
        "create GitHub pull request",
    );
    match pull_request {
        Ok(url) if url.starts_with("https://github.com/") => Ok(url),
        _ => Ok(format!(
            "{}/compare/{}?expand=1",
            repository.trim_end_matches(".git"),
            percent_encode(&head),
        )),
    }
}

fn command_text(command: &mut Command, description: &str) -> Result<String> {
    let output = command
        .output()
        .with_context(|| format!("launch command to {description}"))?;
    if !output.status.success() {
        bail!("{description} failed with {}", output.status);
    }
    let value = String::from_utf8(output.stdout)
        .with_context(|| format!("decode output while trying to {description}"))?;
    let value = value.trim().to_owned();
    if value.is_empty() {
        bail!("{description} returned no result");
    }
    Ok(value)
}

fn update_claim(
    server: &str,
    access_token: &str,
    owner: &str,
    project: &str,
    task_id: &str,
    status: &str,
    review_url: Option<&str>,
) -> Result<()> {
    let endpoint = format!(
        "{server}/api/fractal/projects/{}/tasks/{}/claim?owner={}",
        percent_encode(project),
        percent_encode(task_id),
        percent_encode(owner),
    );
    let body = serde_json::json!({
        "status": status,
        "review_url": review_url,
    })
    .to_string();
    ureq::patch(&endpoint)
        .set("Authorization", &format!("Bearer {access_token}"))
        .set("Content-Type", "application/json")
        .timeout(std::time::Duration::from_secs(8))
        .send_string(&body)
        .map(|_| ())
        .map_err(anyhow::Error::new)
        .context("update hosted task checkout")
}

fn contribution_workspace(slug: &str, task_ref: &str) -> Result<PathBuf> {
    let root = std::env::var_os("FRACTAL_PROJECTS_DIR")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| PathBuf::from(home).join("fractal-projects"))
        })
        .context("HOME is unavailable for contribution projects")?
        .join("contributions");
    std::fs::create_dir_all(&root)
        .with_context(|| format!("create contribution root {}", root.display()))?;
    let suffix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    Ok(root.join(format!(
        "{}-{}-{suffix}",
        safe_component(slug),
        safe_component(task_ref)
    )))
}

fn clone_repository(repository: &str, destination: &Path) -> Result<()> {
    let status = Command::new("git")
        .args(["clone", "--", repository])
        .arg(destination)
        .status()
        .context("launch git clone")?;
    if !status.success() {
        bail!("git clone failed with {status}");
    }
    Ok(())
}

fn run_git(workspace: &Path, args: &[&str]) -> Result<()> {
    let status = Command::new("git")
        .arg("-C")
        .arg(workspace)
        .args(args)
        .status()
        .with_context(|| format!("launch git {}", args.first().copied().unwrap_or("command")))?;
    if !status.success() {
        bail!(
            "git {} failed with {status}",
            args.first().copied().unwrap_or("command")
        );
    }
    Ok(())
}

fn git_has_changes(workspace: &Path) -> Result<bool> {
    let output = Command::new("git")
        .arg("-C")
        .arg(workspace)
        .args(["status", "--porcelain"])
        .output()
        .context("inspect contribution changes")?;
    if !output.status.success() {
        bail!("git status failed with {}", output.status);
    }
    Ok(!output.stdout.is_empty())
}

fn validate_github_repository(value: &str) -> Result<()> {
    let clean = value.trim().trim_end_matches(".git");
    if !clean.starts_with("https://github.com/") {
        bail!("task repository must be an HTTPS github.com URL");
    }
    let path = clean.trim_start_matches("https://github.com/");
    let segments: Vec<_> = path.split('/').collect();
    if segments.len() != 2
        || segments.iter().any(|segment| {
            segment.is_empty()
                || !segment
                    .chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || "._-".contains(ch))
        })
    {
        bail!("task repository URL is invalid");
    }
    Ok(())
}

fn validate_branch(value: &str) -> Result<()> {
    if value.len() > 180
        || !value.starts_with("fractal/")
        || !value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || "/._-".contains(ch))
        || value.contains("..")
        || value.ends_with('/')
    {
        bail!("task review branch is invalid");
    }
    Ok(())
}

fn safe_component(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' {
                ch.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .to_owned()
}

fn percent_encode(value: &str) -> String {
    value.bytes().fold(String::new(), |mut encoded, byte| {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            encoded.push(byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
        encoded
    })
}

#[cfg(test)]
mod tests {
    use super::{safe_component, validate_branch, validate_github_repository};

    #[test]
    fn contribution_inputs_are_narrowly_validated() {
        validate_github_repository("https://github.com/fractalsociety/example.git").unwrap();
        assert!(validate_github_repository("file:///tmp/repo").is_err());
        validate_branch("fractal/alex/1.2").unwrap();
        assert!(validate_branch("main").is_err());
        assert_eq!(safe_component("Wave 1.2"), "wave-1-2");
    }
}
