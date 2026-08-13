//! Disposable, canonical execution graphs for exercising multi-window joins.

use std::fs;

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

use crate::cli::GraphSeedParallelTestArgs;

const TEST_AGENT_INSTRUCTIONS: &str = r#"# Parallel Join Test Agent Instructions

When asked to join this project as a worker, run `fractal join --role worker`.
Do not run `squad join` or `squad receive` directly.

Implement and validate the assigned node, then complete it with the exact agent
identity printed by `fractal join`. Completion automatically checks out the next
dependency-ready node. If the output contains `Next assigned: TASK_ID`, inspect
that node, implement it, complete it, and repeat until Fractal reports that the
graph is complete or no dependency-ready task is available.
"#;

pub(crate) fn seed(args: &GraphSeedParallelTestArgs) -> Result<()> {
    if args.first_wave > args.nodes {
        bail!("--first-wave cannot exceed --nodes");
    }
    if crate::project_file::path(&args.repo).exists() {
        bail!(
            "refusing to replace existing project graph at {}",
            crate::project_file::path(&args.repo).display()
        );
    }
    fs::create_dir_all(&args.repo)
        .with_context(|| format!("create test workspace {}", args.repo.display()))?;

    let mut graph = build_graph(args.nodes, args.first_wave);
    crate::graph_store::rehash_graph(&mut graph)?;
    let hash = graph
        .get("graph_hash")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    crate::graph_store::commit_graph(&graph)?;
    crate::project_file::persist(&args.repo, &graph, &args.title)?;
    let instructions = args.repo.join("AGENTS.md");
    if !instructions.exists() {
        fs::write(&instructions, TEST_AGENT_INSTRUCTIONS)
            .with_context(|| format!("write {}", instructions.display()))?;
    }
    println!(
        "Seeded {} with {} tasks across {} parallel lanes ({hash})",
        args.repo.display(),
        args.nodes,
        args.first_wave
    );
    Ok(())
}

fn build_graph(node_count: u32, lanes: u32) -> Value {
    let mut nodes = Vec::with_capacity(node_count as usize);
    let mut edges = Vec::with_capacity(node_count.saturating_sub(lanes) as usize);
    for index in 0..node_count {
        let ordinal = index + 1;
        let id = format!("parallel_{ordinal:02}");
        let wave = index / lanes + 1;
        let depends_on = if index < lanes {
            Vec::new()
        } else {
            let parent = format!("parallel_{:02}", index - lanes + 1);
            edges.push(json!({"from": parent, "to": id, "condition": "success"}));
            vec![parent]
        };
        let instruction = format!(
            "Create artifacts/task_{ordinal:02}.md with a concise implementation note, one concrete validation example, and the command or observation used as evidence. Stay within that file so this task is safe to execute concurrently."
        );
        nodes.push(json!({
            "id": id,
            "title": format!("Independent parallel test task {ordinal:02}"),
            "kind": "codex",
            "node_type": "implementation",
            "capability": "code.generate",
            "objective": instruction,
            "instruction": instruction,
            "depends_on": depends_on,
            "execution": {
                "mode": "parallel",
                "parallel_group": format!("join-test-wave-{wave}"),
                "wave": wave,
                "task_number": format!("{wave}.{}", index % lanes + 1)
            },
            "budget": {"timeout_ms": 600_000},
            "memory_scopes": ["workspace:root", "work:goal"]
        }));
    }
    json!({
        "schema": "fractal.execution_graph.v1",
        "graph_id": format!("fg_parallel_join_test_{node_count}_{lanes}"),
        "target": "darwin-arm64",
        "nodes": nodes,
        "edges": edges
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_broad_parallel_waves() {
        let graph = build_graph(36, 12);
        assert_eq!(graph["nodes"].as_array().map(Vec::len), Some(36));
        assert_eq!(graph["edges"].as_array().map(Vec::len), Some(24));
        assert!(graph["nodes"].as_array().unwrap()[..12]
            .iter()
            .all(|node| node["depends_on"].as_array().is_some_and(Vec::is_empty)));
        assert_eq!(graph["nodes"][12]["depends_on"][0], "parallel_01");
        assert_eq!(graph["nodes"][24]["depends_on"][0], "parallel_13");
    }
}
