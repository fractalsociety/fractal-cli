use std::ffi::OsString;
use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

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

    match apply_visibility(
        &workspace,
        &repository,
        target,
        &document.project.visibility,
    ) {
        Ok(()) => {
            println!("Visibility updated: project graph and GitHub repository are now {target}.");
            Ok(())
        }
        Err(error) if std::env::var_os("FRACTAL_VISIBILITY_RECEIVER").is_some() => Err(error),
        Err(error) => {
            let handoff = queue_visibility(&workspace, target)?;
            let launched = launch_fractal_voice(&handoff.path);
            eprintln!("Direct GitHub access was unavailable: {error:#}");
            if launched {
                if let Some(result) = wait_for_visibility_result(&handoff.result_path)? {
                    if result.success {
                        println!("{message}", message = result.message);
                        return Ok(());
                    }
                    bail!("{}", result.message);
                }
            }
            println!(
                "{} confirmed visibility change for Fractal Voice. The app will update GitHub and Fractal Society.",
                if launched { "Sent" } else { "Queued" }
            );
            Ok(())
        }
    }
}

fn apply_visibility(
    workspace: &Path,
    repository: &str,
    target: &str,
    previous_local: &str,
) -> Result<()> {
    let previous_github = github_visibility(workspace, repository)?;
    if previous_github != target {
        edit_github_visibility(workspace, repository, target)?;
    }
    if let Err(error) = crate::project_file::set_visibility(workspace, target)
        .and_then(|_| publish_visibility_with_retry(workspace))
    {
        let _ = crate::project_file::set_visibility(workspace, previous_local);
        if previous_github != target {
            let _ = edit_github_visibility(workspace, repository, &previous_github);
        }
        return Err(error)
            .context("visibility synchronization failed; prior visibility was restored");
    }
    Ok(())
}

fn publish_visibility_with_retry(workspace: &Path) -> Result<()> {
    let mut last_error = None;
    for attempt in 0..3 {
        match crate::project_sync::publish_visibility(workspace) {
            Ok(()) => return Ok(()),
            Err(error) => {
                last_error = Some(error);
                if attempt < 2 {
                    std::thread::sleep(std::time::Duration::from_millis(500));
                }
            }
        }
    }
    Err(last_error.expect("visibility publication attempted at least once"))
}

#[derive(Serialize)]
struct VisibilityHandoff<'a> {
    schema: &'static str,
    workspace: &'a str,
    target: &'a str,
    created_at_ms: u128,
}

struct QueuedVisibility {
    path: PathBuf,
    result_path: PathBuf,
}

#[derive(Deserialize)]
struct VisibilityResult {
    success: bool,
    message: String,
}

fn queue_visibility(workspace: &Path, target: &str) -> Result<QueuedVisibility> {
    let workspace = workspace
        .canonicalize()
        .context("resolve visibility project workspace")?;
    let workspace = workspace
        .to_str()
        .context("visibility project path must be UTF-8")?;
    let created_at_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before Unix epoch")?
        .as_millis();
    let envelope = VisibilityHandoff {
        schema: "fractal.external_visibility.v1",
        workspace,
        target,
        created_at_ms,
    };
    let bytes = serde_json::to_vec(&envelope)?;
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
        "fractal-visibility-{}-{nonce}.fractalvisibility",
        std::process::id()
    ));
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(&path)
        .with_context(|| format!("create secure visibility handoff {}", path.display()))?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    let result_path = path.with_extension("result");
    Ok(QueuedVisibility { path, result_path })
}

fn wait_for_visibility_result(path: &Path) -> Result<Option<VisibilityResult>> {
    for _ in 0..80 {
        match std::fs::read(path) {
            Ok(bytes) => {
                std::fs::remove_file(path).ok();
                return serde_json::from_slice(&bytes)
                    .context("decode Fractal Voice visibility result")
                    .map(Some);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                std::thread::sleep(std::time::Duration::from_millis(250));
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(None)
}

fn launch_fractal_voice(path: &Path) -> bool {
    Command::new("/usr/bin/open")
        .args(["-a", "/Applications/Fractal Voice.app"])
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn resolve_workspace(project: &str) -> Result<PathBuf> {
    let direct = PathBuf::from(project);
    if direct.join(".fractal/project.fractal").is_file() {
        return Ok(direct);
    }
    if let Ok(current) = std::env::current_dir() {
        if crate::project_file::load(&current)
            .ok()
            .is_some_and(|document| {
                project_identity_matches(
                    project,
                    &document.project.slug,
                    &document.project.title,
                    current
                        .file_name()
                        .and_then(|name| name.to_str())
                        .unwrap_or(""),
                )
            })
        {
            return Ok(current);
        }
    }
    let matches: Vec<_> = crate::projects::list()
        .into_iter()
        .filter_map(|entry| {
            let workspace = PathBuf::from(&entry.workspace);
            crate::project_file::load(&workspace)
                .ok()
                .filter(|document| {
                    entry.workspace.eq_ignore_ascii_case(project)
                        || project_identity_matches(
                            project,
                            &document.project.slug,
                            &document.project.title,
                            &entry.label,
                        )
                })
                .map(|_| workspace)
        })
        .collect();
    match matches.as_slice() {
        [workspace] => Ok(workspace.clone()),
        [] => bail!("project `{project}` was not found; run `fractal projects`"),
        _ => bail!(
            "project name `{project}` matches multiple projects; use the exact workspace path from `fractal projects`"
        ),
    }
}

fn project_identity_matches(project: &str, slug: &str, title: &str, label: &str) -> bool {
    let needles = project_keys(project);
    [slug, title, label].into_iter().any(|candidate| {
        let candidates = project_keys(candidate);
        needles.iter().any(|needle| candidates.contains(needle))
    })
}

fn project_keys(value: &str) -> Vec<String> {
    let mut words: Vec<String> = value
        .split(|character: char| !character.is_ascii_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(|word| word.to_ascii_lowercase())
        .collect();
    let mut keys = vec![words.join("")];
    if words.first().is_some_and(|word| word == "the") {
        words.remove(0);
    }
    if words
        .last()
        .is_some_and(|word| matches!(word.as_str(), "app" | "build" | "project"))
    {
        words.pop();
    }
    for word in &mut words {
        *word = match word.as_str() {
            "zero" => "0",
            "one" => "1",
            "two" => "2",
            "three" => "3",
            "four" => "4",
            "five" => "5",
            "six" => "6",
            "seven" => "7",
            "eight" => "8",
            "nine" => "9",
            "ten" => "10",
            _ => continue,
        }
        .to_owned();
    }
    let conversational = words.join("");
    if !conversational.is_empty() && !keys.contains(&conversational) {
        keys.push(conversational);
    }
    keys
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
    let github_cli = github_cli_path();
    let output = Command::new(&github_cli)
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
        .with_context(|| {
            format!(
                "launch GitHub CLI at {}; install GitHub CLI, run `gh auth login`, or set FRACTAL_GH_BIN",
                github_cli.display()
            )
        })?;
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
    let github_cli = github_cli_path();
    let output = Command::new(&github_cli)
        .current_dir(workspace)
        .args([
            "repo",
            "edit",
            repository,
            "--visibility",
            visibility,
            "--accept-visibility-change-consequences",
        ])
        .output()
        .with_context(|| {
            format!(
                "launch GitHub CLI at {}; install GitHub CLI, run `gh auth login`, or set FRACTAL_GH_BIN",
                github_cli.display()
            )
        })?;
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr);
        bail!(
            "GitHub repository visibility update failed: {}",
            detail.trim()
        );
    }
    Ok(())
}

fn github_cli_path() -> PathBuf {
    github_cli_path_from(std::env::var_os("FRACTAL_GH_BIN"), |path| path.is_file())
}

fn github_cli_path_from(
    override_path: Option<OsString>,
    is_file: impl Fn(&Path) -> bool,
) -> PathBuf {
    if let Some(path) = override_path.filter(|value| !value.is_empty()) {
        return PathBuf::from(path);
    }
    ["/opt/homebrew/bin/gh", "/usr/local/bin/gh", "/usr/bin/gh"]
        .into_iter()
        .map(PathBuf::from)
        .find(|path| is_file(path))
        .unwrap_or_else(|| PathBuf::from("gh"))
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

    #[test]
    fn discovers_github_cli_outside_restricted_desktop_path() {
        let selected = github_cli_path_from(None, |path| path == Path::new("/opt/homebrew/bin/gh"));
        assert_eq!(selected, PathBuf::from("/opt/homebrew/bin/gh"));

        let selected = github_cli_path_from(Some(OsString::from("/custom/gh")), |_| false);
        assert_eq!(selected, PathBuf::from("/custom/gh"));
    }

    #[test]
    fn visibility_handoff_is_private_and_contains_only_confirmed_target() {
        let root =
            std::env::temp_dir().join(format!("fractal-visibility-test-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let handoff = queue_visibility(&root, "public").unwrap();
        let metadata = std::fs::metadata(&handoff.path).unwrap();
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
        let value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&handoff.path).unwrap()).unwrap();
        assert_eq!(value["schema"], "fractal.external_visibility.v1");
        assert_eq!(value["target"], "public");
        std::fs::remove_file(handoff.path).unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn visibility_project_lookup_accepts_title_and_spoken_number() {
        assert!(project_identity_matches(
            "Coffee Five app",
            "coffee5",
            "Coffee5",
            "coffee5-1785198755992"
        ));
        assert!(!project_identity_matches(
            "Coffee Three",
            "coffee5",
            "Coffee5",
            "coffee5-1785198755992"
        ));
    }
}
