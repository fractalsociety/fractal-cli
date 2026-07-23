use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};

use crate::cli::{Mode, Provider};
use crate::work_builder::{IntentClassification, NlWorkRequest, build_work_from_nl};

const STAGES: [(&str, &str); 8] = [
    (
        "Intent",
        "classifier.ts wires intent classification in P0.2",
    ),
    (
        "FractalWork",
        "work_builder::build_work_from_nl constructs fractal.work.v1 (P0.3)",
    ),
    (
        "Harness",
        "starter harness selected; fractal-harnessc compiles it to a graph in P1.1",
    ),
    (
        "Compile",
        "fractal-harnessc deterministically compiles work + harness",
    ),
    (
        "Graph",
        "fractal.execution_graph.v1 is ready for live-board projection in P1.3",
    ),
    (
        "Coordinate",
        "squad-coordinate wires supervised graph execution in P2",
    ),
    (
        "Verify",
        "fractal-verify wires evidence floors and verdicts in P2.4",
    ),
    (
        "Evolve",
        "fractal-evolution wires governed morphogenesis in P4",
    ),
];

/// The outcome of rendering a submission preview.
///
/// `text` is the human-readable plan; `committed_graph_hash` is `Some` only when
/// build mode compiled and durably committed a graph, so the caller can auto-open
/// that graph's board without the pure builder performing any side effects.
pub(crate) struct SubmitPlan {
    pub(crate) text: String,
    pub(crate) committed_graph_hash: Option<String>,
}

/// Render the submission preview, including a constructed FractalWork object.
pub(crate) fn render_submit_plan(
    request: &str,
    mode: Option<Mode>,
    provider: Option<Provider>,
    repo: Option<&Path>,
    classification: Option<IntentClassification>,
) -> Result<SubmitPlan> {
    let created_at_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0);
    let (work, _source) = build_work_from_nl(&NlWorkRequest {
        request: request.to_owned(),
        requester: "local:cli".to_owned(),
        created_at_ms,
        work_id: None,
        classification,
        repo: repo.map(|path| path.display().to_string()),
        success_criteria: None,
        max_cost_microunits: Some(0),
    })
    .context("construct FractalWorkV1 from natural-language request")?;

    let mut lines = vec![format!("Request: {request}")];
    if let Some(mode) = mode {
        lines.push(format!("Mode: {mode}"));
    }
    if let Some(provider) = provider {
        lines.push(format!("Provider: {provider}"));
    }
    if let Some(repo) = repo {
        lines.push(format!("Repo: {}", repo.display()));
    }
    lines.push(format!("Work id: {}", work.work_id));
    lines.push(format!("Intent: {}", work.intent));
    lines.push(format!("Goal: {}", work.goal));
    lines.push(format!("Content hash: {}", work.content_hash));
    // P0.4 — deterministically select a starter harness for the intent family.
    let selection = crate::harness::select_harness(&work.intent);
    lines.push(format!(
        "Harness: {} (family: {}, source: {})",
        selection.harness_id, selection.family, selection.source
    ));
    let mut graph_compiled = false;
    let mut committed_graph_hash = None;
    // Only an explicit `--mode build` compiles + durably commits + auto-opens.
    // The default (no mode) and plan mode preview only — a bare `fractal "…"`
    // must not silently write a graph to disk and launch a viewer.
    if mode != Some(Mode::Build) {
        lines.push(
            "Preview mode: stopping before execution (work + harness selected). Pass --mode build to compile."
                .to_owned(),
        );
    } else {
        let compilation = serde_json::to_value(&work)
            .context("encode FractalWork for compilation")
            .and_then(|work_value| {
                crate::compile::compile_graph(
                    &work_value,
                    &selection,
                    fractal_harnessc::Target::DarwinMlxApple,
                )
            });
        match compilation {
            Ok(graph) => {
                graph_compiled = true;
                match graph_summary(&graph) {
                    Ok(summary) => lines.extend(summary),
                    Err(error) => lines.push(format!("Compile error: {error:#}")),
                }
                match crate::graph_store::commit_graph(&graph) {
                    Ok(record) => {
                        lines.push(format!("Committed: {}", record.path.display()));
                        committed_graph_hash = Some(record.graph_hash);
                    }
                    Err(error) => lines.push(format!("Commit error: {error:#}")),
                }
            }
            Err(error) => lines.push(format!("Compile error: {error:#}")),
        }
    }
    lines.push("FractalWork:".to_owned());
    lines.push(serde_json::to_string_pretty(&work).context("encode FractalWork JSON")?);
    lines.push("Pipeline plan:".to_owned());
    lines.extend(STAGES.iter().enumerate().map(|(index, (stage, todo))| {
        let marker = if *stage == "FractalWork"
            || *stage == "Harness"
            || (graph_compiled && matches!(*stage, "Compile" | "Graph"))
        {
            "DONE"
        } else {
            "STUB"
        };
        format!("{}. {stage} [{marker}] TODO: {todo}", index + 1)
    }));
    Ok(SubmitPlan {
        text: lines.join("\n"),
        committed_graph_hash,
    })
}

fn graph_summary(graph: &serde_json::Value) -> Result<Vec<String>> {
    let graph_id = graph
        .get("graph_id")
        .and_then(serde_json::Value::as_str)
        .context("compiled graph is missing graph_id")?;
    let graph_hash = graph
        .get("graph_hash")
        .and_then(serde_json::Value::as_str)
        .context("compiled graph is missing graph_hash")?;
    let nodes = graph
        .get("nodes")
        .and_then(serde_json::Value::as_array)
        .context("compiled graph is missing nodes")?;
    let edges = graph
        .get("edges")
        .and_then(serde_json::Value::as_array)
        .context("compiled graph is missing edges")?;
    Ok(vec![
        format!("Graph id: {graph_id}"),
        format!("Graph hash: {graph_hash}"),
        format!("Nodes: {}  Edges: {}", nodes.len(), edges.len()),
    ])
}

/// Print the future Coordinate runner boundary.
pub(crate) fn print_run_stub(work: Option<&str>) {
    let work = work.map_or("<latest>", std::convert::identity);
    println!("Run request: {work}");
    println!(
        "[STUB] TODO: squad-coordinate-sync will drive Coordinate over the compiled graph in P2."
    );
}

/// Print the morphogenesis-loop boundary (superseded by [`crate::evolve`] in P4.7).
#[allow(dead_code)]
pub(crate) fn print_evolve_stub(once: bool, watch: bool) {
    let mode = if watch {
        "watch"
    } else if once {
        "once"
    } else {
        "unspecified"
    };
    println!("Evolution mode: {mode}");
    println!(
        "[STUB] TODO: fractal-evolution will wire the grow/differentiate/repair morphogenesis loop in P4."
    );
}

/// Print the future per-node control boundary.
pub(crate) fn print_node_stub(id: &str, show: bool, retry: bool, cancel: bool) {
    let action = if show {
        "show"
    } else if retry {
        "retry"
    } else if cancel {
        "cancel"
    } else {
        "unspecified"
    };
    println!("Node: {id}");
    println!("Action: {action}");
    println!(
        "[STUB] TODO: the execution-graph board plus squad-coordinate will wire per-node control in P2."
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn submit_plan_includes_constructed_work() {
        let plan = render_submit_plan(
            "build a tiny CLI that reverses a string",
            Some(Mode::Plan),
            None,
            Some(Path::new("/tmp/reverse-cli")),
            None,
        )
        .expect("plan");
        assert!(plan.committed_graph_hash.is_none());
        let plan = plan.text;
        assert!(plan.contains("\"schema\": \"fractal.work.v1\""));
        assert!(plan.contains("Content hash: sha256:"));
        assert!(plan.contains("2. FractalWork [DONE]"));
        assert!(plan.contains("Preview mode: stopping before execution"));
        assert!(plan.contains("1. Intent [STUB]"));
        assert!(plan.contains("work_builder::build_work_from_nl"));
        assert!(!plan.contains("Graph hash:"));
        assert!(plan.contains("4. Compile [STUB]"));
        assert!(plan.contains("5. Graph [STUB]"));
    }

    #[test]
    fn default_mode_previews_and_does_not_commit() {
        // A bare `fractal "…"` (mode = None) must preview, not compile/commit.
        let plan = render_submit_plan(
            "build a tiny CLI that reverses a string",
            None,
            None,
            Some(Path::new("/tmp/reverse-cli")),
            None,
        )
        .expect("plan");
        assert!(plan.committed_graph_hash.is_none());
        assert!(plan.text.contains("Preview mode: stopping before execution"));
        assert!(!plan.text.contains("Graph hash:"));
        assert!(!plan.text.contains("Committed:"));
    }

    #[test]
    fn build_mode_includes_compiled_graph() {
        let _environment_lock = crate::graph_store::ENV_LOCK
            .lock()
            .expect("environment lock");
        let _home = crate::graph_store::TestHome::new("pipeline")
            .expect("temporary FRACTAL_HOME");
        let plan = render_submit_plan(
            "build a tiny CLI that reverses a string",
            Some(Mode::Build),
            None,
            Some(Path::new("/tmp/reverse-cli")),
            None,
        )
        .expect("build plan");

        let committed = plan
            .committed_graph_hash
            .clone()
            .expect("build mode commits a graph");
        assert!(committed.starts_with("sha256:"));
        let plan = plan.text;
        assert!(plan.contains("Graph id: fg_"));
        assert!(plan.contains(&format!("Graph hash: {committed}")));
        assert!(plan.contains("Nodes: "));
        assert!(plan.contains("Committed: "), "{plan}");
        assert!(plan.contains("4. Compile [DONE]"));
        assert!(plan.contains("5. Graph [DONE]"));
    }
}
