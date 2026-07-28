use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};

use crate::cli::VisibilityArgs;

pub(crate) fn run(args: &VisibilityArgs) -> Result<()> {
    let workspace = resolve_workspace(&args.project)?;
    let document = crate::project_file::load(&workspace)?;
    let target = if args.public { "public" } else { "private" };
    let repository = github_repository(&workspace)?;

    println!("Project visibility warning:");
    println!("  Project: {}", document.project.slug);
    println!("  Fractal Society graph: {target}");
    println!("  GitHub repository: {repository} → {target}");
    if target == "public" {
        println!("  Anyone will be able to view the graph, repository, files, and commit history.");
        println!(
            "  Review the full Git history for secrets and personal information before confirming."
        );
    } else {
        println!("  Only authorized project members and GitHub collaborators will retain access.");
    }
    if !args.yes {
        bail!(
            "visibility unchanged; after the user explicitly answers yes to this exact warning, repeat with `--yes`"
        );
    }

    let previous_local = document.project.visibility;
    let previous_github = github_visibility(&workspace, &repository)?;
    if previous_github != target {
        edit_github_visibility(&workspace, &repository, target)?;
    }
    if let Err(error) = crate::project_file::set_visibility(&workspace, target)
        .and_then(|_| crate::project_sync::publish_visibility(&workspace))
    {
        let _ = crate::project_file::set_visibility(&workspace, &previous_local);
        if previous_github != target {
            let _ = edit_github_visibility(&workspace, &repository, &previous_github);
        }
        return Err(error)
            .context("visibility synchronization failed; prior visibility was restored");
    }
    println!("Visibility updated: project graph and GitHub repository are now {target}.");
    Ok(())
}

fn resolve_workspace(project: &str) -> Result<PathBuf> {
    let direct = PathBuf::from(project);
    if direct.join(".fractal/project.fractal").is_file() {
        return Ok(direct);
    }
    if let Ok(current) = std::env::current_dir() {
        if crate::project_file::load(&current)
            .ok()
            .is_some_and(|document| document.project.slug == project)
        {
            return Ok(current);
        }
    }
    crate::projects::list()
        .into_iter()
        .find_map(|entry| {
            let workspace = PathBuf::from(&entry.workspace);
            crate::project_file::load(&workspace)
                .ok()
                .filter(|document| {
                    document.project.slug == project
                        || entry.label.eq_ignore_ascii_case(project)
                        || entry.workspace == project
                })
                .map(|_| workspace)
        })
        .with_context(|| format!("project `{project}` was not found; run `fractal projects`"))
}

fn github_repository(workspace: &Path) -> Result<String> {
    let output = Command::new("git")
        .current_dir(workspace)
        .args(["remote", "get-url", "origin"])
        .output()
        .context("read GitHub origin")?;
    if !output.status.success() {
        bail!("this project has no GitHub origin; run `fractal sync --github --repo PATH` first");
    }
    canonical_repository(String::from_utf8_lossy(&output.stdout).trim())
        .context("origin must point to github.com")
}

fn canonical_repository(remote: &str) -> Option<String> {
    let path = remote
        .strip_prefix("git@github.com:")
        .or_else(|| remote.strip_prefix("ssh://git@github.com/"))
        .or_else(|| remote.strip_prefix("https://github.com/"))?
        .trim_end_matches('/')
        .trim_end_matches(".git");
    let mut pieces = path.split('/');
    let owner = pieces.next()?;
    let repository = pieces.next()?;
    if pieces.next().is_some() || !safe_segment(owner) || !safe_segment(repository) {
        return None;
    }
    Some(format!("{owner}/{repository}"))
}

fn safe_segment(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 100
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn github_visibility(workspace: &Path, repository: &str) -> Result<String> {
    let output = Command::new("gh")
        .current_dir(workspace)
        .args([
            "repo",
            "view",
            repository,
            "--json",
            "visibility",
            "--jq",
            ".visibility",
        ])
        .output()
        .context(
            "inspect GitHub repository visibility; install GitHub CLI and run `gh auth login`",
        )?;
    if !output.status.success() {
        bail!(
            "inspect GitHub repository visibility: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    match String::from_utf8(output.stdout)?
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "public" => Ok("public".to_owned()),
        "private" | "internal" => Ok("private".to_owned()),
        other => bail!("GitHub returned unsupported repository visibility `{other}`"),
    }
}

fn edit_github_visibility(workspace: &Path, repository: &str, visibility: &str) -> Result<()> {
    let status = Command::new("gh")
        .current_dir(workspace)
        .args([
            "repo",
            "edit",
            repository,
            "--visibility",
            visibility,
            "--accept-visibility-change-consequences",
        ])
        .status()
        .context("change GitHub repository visibility; run `gh auth login` first")?;
    if !status.success() {
        bail!("GitHub repository visibility update failed with {status}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_common_github_remote_forms() {
        assert_eq!(
            canonical_repository("git@github.com:owner/repo.git").as_deref(),
            Some("owner/repo")
        );
        assert_eq!(
            canonical_repository("https://github.com/owner/repo").as_deref(),
            Some("owner/repo")
        );
        assert_eq!(canonical_repository("https://example.com/owner/repo"), None);
    }
}
