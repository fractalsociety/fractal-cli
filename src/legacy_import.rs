//! One-time migration from the retired Python board state.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

use crate::project_file::ExecutionAssignment;

pub(crate) fn run(state_path: &Path, workspace: &Path) -> Result<()> {
    let marker = workspace.join(".fractal").join("legacy-import-v1.json");
    if marker.exists() {
        bail!(
            "legacy graph state was already imported for this project ({})",
            marker.display()
        );
    }
    let legacy: Value = serde_json::from_slice(
        &fs::read(state_path)
            .with_context(|| format!("read legacy state {}", state_path.display()))?,
    )
    .with_context(|| format!("parse legacy state {}", state_path.display()))?;
    if legacy.get("schema").and_then(Value::as_str) != Some("fractal.execution_graph_view_state.v1")
    {
        bail!("unsupported legacy graph-state schema");
    }

    let now = crate::project_file::project_timestamp();
    let active: BTreeSet<&str> = legacy
        .get("active")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    let legacy_assignments = legacy
        .get("assignments")
        .and_then(Value::as_object)
        .context("legacy graph state has no assignments object")?;
    let mut assignments = BTreeMap::new();
    for (node, value) in legacy_assignments {
        let raw_state = value.get("state").and_then(Value::as_str).unwrap_or(
            if active.contains(node.as_str()) {
                "checked_out"
            } else {
                "released"
            },
        );
        let state = match raw_state {
            "active" | "checked_out" => "checked_out",
            "complete" | "completed" => "completed",
            _ => "released",
        };
        assignments.insert(
            node.clone(),
            ExecutionAssignment {
                agent_id: value
                    .get("agent_id")
                    .and_then(Value::as_str)
                    .unwrap_or("legacy/import")
                    .to_owned(),
                agent_label: value
                    .get("agent_label")
                    .and_then(Value::as_str)
                    .unwrap_or("Legacy import")
                    .to_owned(),
                state: state.to_owned(),
                checked_out_at: value
                    .get("checked_out_at")
                    .and_then(Value::as_str)
                    .unwrap_or(&now)
                    .to_owned(),
                completed_at: value
                    .get("completed_at")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                released_at: value
                    .get("released_at")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                extra: BTreeMap::new(),
            },
        );
    }

    let created_placeholder = !crate::project_file::path(workspace).exists();
    if created_placeholder {
        let mut node_ids: BTreeSet<String> = assignments.keys().cloned().collect();
        node_ids.extend(active.into_iter().map(str::to_owned));
        let mut graph = json!({
            "schema": "fractal.execution_graph.v1",
            "graph_id": format!(
                "legacy_{}",
                legacy.get("work_id").and_then(Value::as_str).unwrap_or("project")
            ),
            "nodes": node_ids.into_iter().map(|id| json!({
                "id": id,
                "title": id,
                "capability": "legacy.imported",
                "instruction": "Imported from the retired Python execution board."
            })).collect::<Vec<_>>(),
            "edges": []
        });
        crate::graph_store::rehash_graph(&mut graph)?;
        let title = legacy
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or("Imported Fractal project");
        crate::project_file::persist(workspace, &graph, title)?;
        // A state-only legacy file has no trustworthy dependency DAG. Preserve
        // its history, but release active claims and halt until Fractal replans
        // or a compiled canonical graph is supplied.
        for assignment in assignments.values_mut() {
            if assignment.state == "checked_out" {
                assignment.state = "released".to_owned();
                assignment.released_at = Some(now.clone());
            }
        }
    }

    let imported = crate::project_file::import_legacy_assignments(workspace, assignments)?;
    if created_placeholder {
        crate::project_file::set_execution_phase(workspace, "halted")?;
    }
    fs::create_dir_all(marker.parent().expect("marker has parent"))?;
    let marker_document = json!({
        "schema": "fractal.legacy_import.v1",
        "source": state_path,
        "imported_assignments": imported,
        "requires_replan": created_placeholder,
        "imported_at": now,
    });
    fs::write(&marker, serde_json::to_vec_pretty(&marker_document)?)
        .with_context(|| format!("write import marker {}", marker.display()))?;
    println!(
        "Imported {imported} legacy assignment(s) into {}",
        crate::project_file::path(workspace).display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn imports_legacy_state_once_into_the_portable_project() -> Result<()> {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| duration.as_nanos());
        let workspace = std::env::temp_dir().join(format!("fractal-legacy-import-{nonce}"));
        fs::create_dir_all(&workspace)?;
        let state = workspace.join("graph-state.json");
        fs::write(
            &state,
            serde_json::to_vec_pretty(&json!({
                "schema": "fractal.execution_graph_view_state.v1",
                "work_id": "old-project",
                "title": "Old project",
                "active": ["M2"],
                "assignments": {
                    "M1": {
                        "agent_id": "codex/root",
                        "agent_label": "Codex",
                        "state": "completed",
                        "checked_out_at": "2026-07-20T10:00:00Z",
                        "completed_at": "2026-07-20T10:01:00Z"
                    },
                    "M2": {
                        "agent_id": "cursor/auto",
                        "agent_label": "Cursor",
                        "state": "checked_out",
                        "checked_out_at": "2026-07-20T10:02:00Z"
                    }
                }
            }))?,
        )?;
        run(&state, &workspace)?;
        let project = crate::project_file::load(&workspace)?;
        let execution = project.execution.context("imported execution")?;
        assert_eq!(execution.assignments["M1"].state, "completed");
        assert_eq!(execution.assignments["M2"].state, "released");
        assert_eq!(execution.phase, "halted");
        assert!(run(&state, &workspace).is_err());
        fs::remove_dir_all(workspace)?;
        Ok(())
    }
}
