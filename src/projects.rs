//! Stable per-project numbering so a build can be listed and resumed by number —
//! including by voice, e.g. "please resume project 3".
//!
//! Every workspace that runs a build is registered once and keeps its number for
//! good (numbers are never reused), so `fractal projects` shows a stable list and
//! `fractal resume <N>` / "resume project #N" always mean the same project.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// One registered project.
#[derive(Serialize, Deserialize, Clone)]
pub(crate) struct Project {
    pub number: u32,
    pub workspace: String,
    pub label: String,
    pub updated_at_ms: u64,
}

fn fractal_home() -> PathBuf {
    match std::env::var_os("FRACTAL_HOME") {
        Some(home) => PathBuf::from(home),
        None => match std::env::var_os("HOME") {
            Some(home) => PathBuf::from(home).join(".fractal"),
            None => PathBuf::from(".fractal"),
        },
    }
}

fn registry_path() -> PathBuf {
    fractal_home().join("projects.json")
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// A short, human-friendly label — the workspace folder name.
fn label_for(workspace: &Path) -> String {
    validated_label_for(workspace).unwrap_or_else(|| fallback_label_for(workspace))
}

fn validated_label_for(workspace: &Path) -> Option<String> {
    let document = crate::project_file::load(workspace).ok()?;
    let title = document.project.title.trim();
    (!title.is_empty()).then(|| title.chars().take(240).collect())
}

fn fallback_label_for(workspace: &Path) -> String {
    workspace
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| workspace.to_string_lossy().into_owned())
}

pub(crate) fn load() -> Vec<Project> {
    load_from(&registry_path())
}

fn load_from(path: &Path) -> Vec<Project> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

fn save_to(home: &Path, path: &Path, projects: &[Project]) {
    let _ = std::fs::create_dir_all(home);
    if let Ok(text) = serde_json::to_string_pretty(projects) {
        let _ = std::fs::write(path, text);
    }
}

/// Register `workspace` if new (assigning the next number) or refresh it; returns
/// its stable number. Keyed by the same workspace string the checkpoint uses.
pub(crate) fn register(workspace: &Path) -> u32 {
    register_in(workspace, &fractal_home(), &registry_path())
}

fn register_in(workspace: &Path, home: &Path, registry: &Path) -> u32 {
    let key = workspace.to_string_lossy().into_owned();
    let mut projects = load_from(registry);
    if let Some(existing) = projects.iter_mut().find(|project| project.workspace == key) {
        existing.updated_at_ms = now_ms();
        if let Some(label) = validated_label_for(workspace) {
            existing.label = label;
        }
        let number = existing.number;
        save_to(home, registry, &projects);
        return number;
    }
    let number = projects.iter().map(|p| p.number).max().unwrap_or(0) + 1;
    projects.push(Project {
        number,
        workspace: key,
        label: label_for(workspace),
        updated_at_ms: now_ms(),
    });
    save_to(home, registry, &projects);
    number
}

pub(crate) fn by_number(number: u32) -> Option<Project> {
    sync();
    load().into_iter().find(|project| project.number == number)
}

/// Backfill the registry from any resumable checkpoints not yet numbered — so
/// projects started before the registry existed still get stable numbers.
pub(crate) fn sync() {
    let known: std::collections::BTreeSet<String> = load()
        .into_iter()
        .map(|project| project.workspace)
        .collect();
    for cp in crate::checkpoint::list_resumable() {
        if !known.contains(&cp.workspace) {
            register(Path::new(&cp.workspace));
        }
    }
}

/// All projects, ordered by number (registry synced with checkpoints first).
pub(crate) fn list() -> Vec<Project> {
    sync();
    let mut projects = load();
    projects.sort_by_key(|project| project.number);
    projects
}

/// Recognize a spoken/typed "resume project N" control command and extract N.
/// Requires a resume verb AND a project/# reference AND a number, so ordinary
/// build requests are never misread as a resume.
pub(crate) fn parse_resume_command(text: &str) -> Option<u32> {
    let lower = text.to_ascii_lowercase();
    let is_resume =
        lower.contains("resume") || lower.contains("continue") || lower.contains("pick up");
    let is_project = lower.contains("project") || lower.contains('#');
    if !(is_resume && is_project) {
        return None;
    }
    lower
        .split(|c: char| !c.is_ascii_digit())
        .find(|part| !part.is_empty())
        .and_then(|digits| digits.parse().ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    fn temp_dir(prefix: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "{prefix}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    fn valid_graph() -> Value {
        let mut graph = json!({
            "schema": "fractal.execution_graph.v1",
            "graph_id": "fg_project_discovery",
            "nodes": [{"id": "build", "capability": "code.generate", "instruction": "Build", "future_node_field": true}],
            "edges": [],
            "future_graph_field": {"must": "survive"}
        });
        graph["graph_hash"] = Value::String(fractal_contracts::canonical_sha256(&graph).unwrap());
        graph
    }

    #[test]
    fn register_refreshes_project_label_from_validated_current_project_file() -> anyhow::Result<()>
    {
        let home = temp_dir("fractal-projects-home");
        let registry = home.join("projects.json");
        let workspace = temp_dir("fractal-projects-workspace");
        std::fs::create_dir_all(&workspace)?;

        let graph = valid_graph();
        crate::project_file::persist(&workspace, &graph, "Folder Label")?;
        let number = register_in(&workspace, &home, &registry);
        let mut raw: Value =
            serde_json::from_slice(&std::fs::read(crate::project_file::path(&workspace))?)?;
        raw["project"]["title"] = json!("Current Enriched Title");
        raw["learning"]["nodes"]["build"]["outcome"] = json!("unverified_success");
        raw["learning"]["nodes"]["build"]["finished_at"] = json!("2024-01-01T00:00:00Z");
        std::fs::write(
            crate::project_file::path(&workspace),
            serde_json::to_vec_pretty(&raw)?,
        )?;

        assert_eq!(register_in(&workspace, &home, &registry), number);
        let project = load_from(&registry)
            .into_iter()
            .find(|project| project.number == number)
            .expect("registered project");
        assert_eq!(project.label, "Current Enriched Title");
        assert_eq!(
            crate::project_file::load(&workspace)?.graph["future_graph_field"],
            json!({"must": "survive"})
        );
        assert_eq!(
            serde_json::to_value(crate::project_file::load(&workspace)?.learning)?["nodes"]
                ["build"]["outcome"],
            json!("unverified_success")
        );

        let _ = std::fs::remove_dir_all(home);
        let _ = std::fs::remove_dir_all(workspace);
        Ok(())
    }

    #[test]
    fn invalid_current_project_file_does_not_rewrite_registry_metadata() -> anyhow::Result<()> {
        let home = temp_dir("fractal-projects-invalid-home");
        let registry = home.join("projects.json");
        let workspace = temp_dir("fractal-projects-invalid-workspace");
        std::fs::create_dir_all(&workspace)?;

        let graph = valid_graph();
        crate::project_file::persist(&workspace, &graph, "Safe Title")?;
        let number = register_in(&workspace, &home, &registry);
        let mut raw: Value =
            serde_json::from_slice(&std::fs::read(crate::project_file::path(&workspace))?)?;
        raw["learning"]["nodes"]["build"]["notes"] = json!("x".repeat(1001));
        raw["project"]["title"] = json!("Invalid Title Must Not Land");
        std::fs::write(
            crate::project_file::path(&workspace),
            serde_json::to_vec_pretty(&raw)?,
        )?;

        assert_eq!(register_in(&workspace, &home, &registry), number);
        assert_eq!(
            load_from(&registry)
                .into_iter()
                .find(|project| project.number == number)
                .expect("registered project")
                .label,
            "Safe Title"
        );

        let _ = std::fs::remove_dir_all(home);
        let _ = std::fs::remove_dir_all(workspace);
        Ok(())
    }

    #[test]
    fn parses_resume_commands_but_not_builds() {
        assert_eq!(parse_resume_command("please resume project #3"), Some(3));
        assert_eq!(parse_resume_command("resume project 12"), Some(12));
        assert_eq!(parse_resume_command("continue project 2"), Some(2));
        assert_eq!(parse_resume_command("pick up project #7 please"), Some(7));
        // Not resume commands:
        assert_eq!(
            parse_resume_command("build an expense tracker with 3 tabs"),
            None
        );
        assert_eq!(parse_resume_command("resume the workout"), None); // no number/#
        assert_eq!(parse_resume_command("make project alpha"), None); // no resume verb
    }
}
