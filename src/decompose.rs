//! PRD-decomposition planner.
//!
//! When a request points at a PRD / spec file, the lead planner *reads it* and
//! decomposes it into a concrete task DAG (N tasks + dependencies), which compiles
//! into a real execution graph the agent team + mid-run morphogenesis execute —
//! instead of collapsing the whole PRD into one fixed template harness.
//!
//! Pipeline: detect the PRD file → lead agent writes `fractal-plan.json` (a task
//! DAG optimized for the product's performance) → validate (acyclic, capabilities,
//! deps) → assemble a `fractal.compiled_harness.v1` genome whose nodes carry the
//! planner's per-task instructions → run it through the SAME `recompile` +
//! `commit_graph` path as the built-in harnesses, so evolution, the recompile hop,
//! and the mid-run supervisor all work on the decomposed graph for free.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};

use crate::graph_store;
use crate::work_builder::{build_work_from_nl, NlWorkRequest};

/// Capabilities a planned task may declare; anything else is coerced to
/// `code.generate` so a wayward plan still compiles.
const ALLOWED_CAPS: [&str; 6] = [
    "code.generate",
    "code.edit",
    "project.tests.execute",
    "python.tests.execute",
    "content.analyze",
    "control.complete",
];

/// Bounds so a runaway plan cannot produce an unusable graph.
const MAX_TASKS: usize = 40;
const MIN_TASKS: usize = 2;

/// One task from the planner's decomposition of the PRD.
#[derive(Clone)]
struct Task {
    id: String,
    title: String,
    capability: String,
    instruction: String,
    depends_on: Vec<String>,
}

/// If the request references a PRD/spec file, decompose it into a committed graph
/// and return its hash. Returns `None` when no PRD file is referenced (the caller
/// then uses the standard harness). The inner `Result` is `Err` only when a PRD
/// *was* found but planning failed — the caller falls back with a note.
pub(crate) fn maybe_decompose(
    request: &str,
    workspace: &Path,
    agents: &[String],
) -> Option<Result<String>> {
    let prd = detect_prd_file(request, workspace)?;
    if agents.is_empty() {
        return Some(Err(anyhow!(
            "no agent is enabled to plan the PRD (enable a builder at launch)"
        )));
    }
    Some(decompose_and_commit(&prd, request, workspace, &agents[0]))
}

/// Find a PRD/spec file the request explicitly names (a token that resolves to an
/// existing file). Requires an explicit filename so we never guess among many PRDs.
fn detect_prd_file(request: &str, workspace: &Path) -> Option<PathBuf> {
    for raw in request.split_whitespace() {
        let token = raw.trim_matches(|c: char| "\"'`,;:()[]<>".contains(c));
        if token.len() < 3 || !token.contains('.') {
            continue; // needs an extension to look like a file
        }
        for candidate in candidate_paths(token, workspace) {
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

fn candidate_paths(token: &str, workspace: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    let expanded = if let Some(rest) = token.strip_prefix("~/") {
        std::env::var_os("HOME").map(|home| PathBuf::from(home).join(rest))
    } else {
        None
    };
    if let Some(expanded) = expanded {
        paths.push(expanded);
    }
    let raw = PathBuf::from(token);
    if raw.is_absolute() {
        paths.push(raw);
    } else {
        paths.push(workspace.join(token));
        if let Ok(cwd) = std::env::current_dir() {
            paths.push(cwd.join(token));
        }
    }
    paths
}

fn decompose_and_commit(
    prd_path: &Path,
    request: &str,
    workspace: &Path,
    lead_agent: &str,
) -> Result<String> {
    let prd_text = std::fs::read_to_string(prd_path)
        .with_context(|| format!("read PRD file {}", prd_path.display()))?;
    let prd_name = prd_path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| prd_path.display().to_string());

    println!(
        "  🧠 [{lead_agent}] decomposing {prd_name} into a task graph (optimizing for performance)…"
    );

    let tasks = plan_tasks(&prd_text, &prd_name, lead_agent, workspace)?;
    println!(
        "  ✓ planner produced {} tasks; compiling the execution graph…",
        tasks.len()
    );

    let harness = build_harness_genome(&tasks, &prd_name);

    // Reuse the exact compile + commit path the built-in harnesses use, so the
    // decomposed graph is evolution- and supervisor-ready.
    let goal = format!("Execute the plan decomposed from the PRD {prd_name}: {request}");
    let work = build_work_value(&goal)?;
    let target_id = "darwin-arm64";
    let graph = crate::compile::recompile(&work, &harness, target_id)
        .context("compile the decomposed task graph")?;
    let record = graph_store::commit_graph(&graph).context("commit the decomposed graph")?;
    if let Some(hash) = graph.get("graph_hash").and_then(Value::as_str) {
        graph_store::persist_source(hash, &harness, &work, target_id).ok();
    }
    Ok(record.graph_hash)
}

/// Build a `fractal.work.v1` value for the decomposed goal (reusing the standard
/// NL → work constructor so authorizations/validation match the normal path).
fn build_work_value(goal: &str) -> Result<Value> {
    let created_at_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0);
    let (work, _source) = build_work_from_nl(&NlWorkRequest {
        request: goal.to_owned(),
        requester: "local:cli".to_owned(),
        created_at_ms,
        work_id: None,
        classification: None,
        repo: None,
        success_criteria: None,
        max_cost_microunits: Some(0),
    })
    .map_err(|error| anyhow!("construct work from PRD goal: {error}"))?;
    serde_json::to_value(&work).context("encode decomposed FractalWork")
}

/// Invoke the lead agent to write `fractal-plan.json`, then parse + validate it
/// into a task DAG.
fn plan_tasks(
    prd_text: &str,
    prd_name: &str,
    lead_agent: &str,
    workspace: &Path,
) -> Result<Vec<Task>> {
    let plan_path = workspace.join("fractal-plan.json");
    // Start clean so we never read a stale plan from a previous attempt.
    std::fs::remove_file(&plan_path).ok();

    let prompt = planning_prompt(prd_text, prd_name);
    // Planning gets a generous budget; a hung planner is killed rather than
    // stalling forever.
    let planner_timeout_ms = std::env::var("FRACTAL_AGENT_TIMEOUT_MS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(900_000);
    let run = crate::execute::run_agent_prompt(lead_agent, &prompt, workspace, planner_timeout_ms)
        .with_context(|| format!("lead planner `{lead_agent}` failed to run"))?;
    if !run.ok {
        bail!(
            "lead planner `{lead_agent}` {}",
            if run.timed_out {
                "timed out"
            } else {
                "exited with an error"
            }
        );
    }
    let raw = std::fs::read_to_string(&plan_path).map_err(|_| {
        anyhow!("planner did not write fractal-plan.json (the PRD may be too large or unclear)")
    })?;
    let tasks = parse_and_validate(&raw)?;
    Ok(tasks)
}

fn planning_prompt(prd_text: &str, prd_name: &str) -> String {
    format!(
        "You are the LEAD PLANNER for an autonomous multi-agent build team. Read the PRD below \
         ({prd_name}) and decompose it into a concrete, executable task DAG that a team of coding \
         agents will run in this workspace.\n\n\
         Optimize the plan for the PERFORMANCE and QUALITY of the product being built — correctness, \
         robustness, and the behaviors that matter — NOT for the lowest cost or least effort. Prefer \
         a decomposition that maximizes safe parallelism (independent tasks share no dependency).\n\n\
         WRITE A FILE named exactly `fractal-plan.json` in the current directory, and nothing else. \
         It MUST be valid JSON of this exact shape:\n\
         {{\n  \"tasks\": [\n    {{\n      \"id\": \"short_snake_case_id\",\n      \"title\": \"one line\",\n      \"capability\": \"code.generate|code.edit|project.tests.execute|content.analyze\",\n      \"instruction\": \"a self-contained directive an agent can execute in this workspace with no other context\",\n      \"depends_on\": [\"ids of tasks that must finish first\"]\n    }}\n  ]\n}}\n\n\
         Rules: {min}-{max} tasks; ids unique; `depends_on` must reference earlier task ids only and form a DAG (no cycles); \
         every `instruction` must be concrete and standalone (name the files to create/edit and what they must contain/do); \
         include at least one `project.tests.execute` task that depends on the implementation and gates it (it should run the repository's real native/package test suite and fail if anything fails); \
         put the tasks in a sensible build order. Do NOT implement anything now — only write the plan file.\n\n\
         --- PRD ({prd_name}) ---\n{prd}\n--- END PRD ---",
        min = MIN_TASKS,
        max = MAX_TASKS,
        prd = truncate_prd(prd_text),
    )
}

/// Keep the prompt bounded for smaller-context agents.
fn truncate_prd(text: &str) -> String {
    const LIMIT: usize = 24_000;
    if text.len() <= LIMIT {
        return text.to_owned();
    }
    let mut cut = LIMIT;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}\n\n[PRD truncated for planning]", &text[..cut])
}

/// Parse the planner's JSON (tolerating markdown fences) and validate it into a
/// clean, acyclic task list with a synthesized `complete` node.
fn parse_and_validate(raw: &str) -> Result<Vec<Task>> {
    let json = extract_json(raw)?;
    let doc: Value = serde_json::from_str(&json)
        .context("planner produced invalid JSON in fractal-plan.json")?;
    let array = doc
        .get("tasks")
        .and_then(Value::as_array)
        .context("fractal-plan.json has no `tasks` array")?;

    let mut tasks: Vec<Task> = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for item in array {
        let id = sanitize_id(item.get("id").and_then(Value::as_str).unwrap_or(""));
        if id.is_empty() || !seen.insert(id.clone()) {
            continue; // skip empty or duplicate ids
        }
        let instruction = item
            .get("instruction")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_owned();
        if instruction.is_empty() {
            continue;
        }
        let capability = normalize_capability(item.get("capability").and_then(Value::as_str));
        let title = item
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or(&id)
            .trim()
            .to_owned();
        let depends_on = item
            .get("depends_on")
            .and_then(Value::as_array)
            .map(|deps| {
                deps.iter()
                    .filter_map(|dep| dep.as_str().map(sanitize_id))
                    .filter(|dep| !dep.is_empty())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        tasks.push(Task {
            id,
            title,
            capability,
            instruction,
            depends_on,
        });
    }

    if tasks.len() < MIN_TASKS {
        bail!(
            "planner produced only {} usable task(s) — need at least {MIN_TASKS}",
            tasks.len()
        );
    }
    tasks.truncate(MAX_TASKS);

    // Drop dependency references to unknown/dropped ids, then break any cycles so
    // the graph is a DAG the executor can topo-order.
    let ids: std::collections::BTreeSet<String> = tasks.iter().map(|t| t.id.clone()).collect();
    for task in &mut tasks {
        task.depends_on
            .retain(|dep| ids.contains(dep) && dep != &task.id);
    }
    break_cycles(&mut tasks);
    Ok(tasks)
}

/// Remove edges that would form a cycle, keeping the first-seen order as the DAG
/// spine (a dependency is only honored if it points to an already-ordered task).
fn break_cycles(tasks: &mut [Task]) {
    let order: std::collections::BTreeMap<String, usize> = tasks
        .iter()
        .enumerate()
        .map(|(index, task)| (task.id.clone(), index))
        .collect();
    for task in tasks.iter_mut() {
        let self_index = order[&task.id];
        task.depends_on.retain(|dep| {
            order
                .get(dep)
                .is_some_and(|dep_index| *dep_index < self_index)
        });
    }
}

/// Assemble a `fractal.compiled_harness.v1` genome from the task DAG. Each task is
/// a node carrying its instruction; edges come from `depends_on`; a synthesized
/// `complete` control node depends on every sink task so the run has one closer.
fn build_harness_genome(tasks: &[Task], prd_name: &str) -> Value {
    let ready = |id: &str| format!("{id}.ready");
    let mut nodes = Vec::new();
    let mut edges = Vec::new();

    for task in tasks {
        let preconditions: Vec<String> = task.depends_on.iter().map(|dep| ready(dep)).collect();
        let budget = if task.capability.ends_with("tests.execute") {
            120_000
        } else {
            180_000
        };
        nodes.push(json!({
            "id": task.id,
            "title": task.title,
            "capability": task.capability,
            "memory_scopes": ["work:goal", "workspace:root"],
            "preconditions": preconditions,
            "produced_state": [ready(&task.id)],
            "instruction": task.instruction,
            "budget": {"timeout_ms": budget},
        }));
        for dep in &task.depends_on {
            edges.push(json!({"from": dep, "to": task.id, "condition": "success"}));
        }
    }

    // Sinks = tasks nothing depends on; the closer waits on all of them.
    let has_dependents: std::collections::BTreeSet<&String> = tasks
        .iter()
        .flat_map(|task| task.depends_on.iter())
        .collect();
    let sinks: Vec<&Task> = tasks
        .iter()
        .filter(|task| !has_dependents.contains(&task.id))
        .collect();
    let closer_preconditions: Vec<String> = sinks.iter().map(|task| ready(&task.id)).collect();
    for task in &sinks {
        edges.push(json!({"from": task.id, "to": "complete", "condition": "success"}));
    }
    nodes.push(json!({
        "id": "complete",
        "capability": "control.complete",
        "memory_scopes": ["work:goal"],
        "preconditions": closer_preconditions,
        "produced_state": ["outcome_verified"],
        "instruction": "Every planned task and its gating tests have passed — mark the outcome complete.",
        "budget": {"timeout_ms": 5_000},
    }));

    json!({
        "schema": "fractal.compiled_harness.v1",
        "version": 1,
        "harness_id": "harness.prd_decomposition.v1",
        "goal": format!("Execute the task DAG decomposed from {prd_name}."),
        "nodes": nodes,
        "edges": edges,
    })
}

fn normalize_capability(capability: Option<&str>) -> String {
    match capability {
        Some(value) if ALLOWED_CAPS.contains(&value) => value.to_owned(),
        // Common synonyms the planner might emit.
        Some(value) if value.contains("test") || value.contains("verif") => {
            "project.tests.execute".to_owned()
        }
        Some(value) if value.contains("edit") || value.contains("review") => "code.edit".to_owned(),
        Some(value) if value.contains("analy") || value.contains("plan") => {
            "content.analyze".to_owned()
        }
        _ => "code.generate".to_owned(),
    }
}

fn sanitize_id(raw: &str) -> String {
    let cleaned: String = raw
        .trim()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-' {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect();
    let trimmed = cleaned.trim_matches('_').to_owned();
    // `complete` is reserved for the synthesized closer node.
    if trimmed == "complete" {
        "task_complete".to_owned()
    } else {
        trimmed
    }
}

/// Pull the JSON object out of the planner's file, tolerating ```json fences or
/// surrounding prose by taking the outermost `{ … }`.
fn extract_json(raw: &str) -> Result<String> {
    let start = raw
        .find('{')
        .context("no JSON object in fractal-plan.json")?;
    let end = raw
        .rfind('}')
        .context("no JSON object in fractal-plan.json")?;
    if end <= start {
        bail!("fractal-plan.json is not a JSON object");
    }
    Ok(raw[start..=end].to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_task_dag_and_synthesizes_a_closer() {
        let raw = r#"```json
        {"tasks": [
          {"id": "core", "title": "core", "capability": "code.generate", "instruction": "write core.py", "depends_on": []},
          {"id": "tests", "title": "tests", "capability": "python.tests.execute", "instruction": "run pytest", "depends_on": ["core"]}
        ]}
        ```"#;
        let tasks = parse_and_validate(raw).expect("valid plan");
        assert_eq!(tasks.len(), 2);
        let genome = build_harness_genome(&tasks, "X.md");
        let ids: Vec<&str> = genome["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n["id"].as_str().unwrap())
            .collect();
        assert!(ids.contains(&"core") && ids.contains(&"tests") && ids.contains(&"complete"));
        // tests is the sink → the closer depends on it.
        let closer_edge = genome["edges"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["from"] == "tests" && e["to"] == "complete");
        assert!(closer_edge);
    }

    #[test]
    fn breaks_cycles_and_drops_unknown_deps() {
        let raw = r#"{"tasks": [
          {"id": "a", "capability": "code.generate", "instruction": "a", "depends_on": ["b"]},
          {"id": "b", "capability": "code.generate", "instruction": "b", "depends_on": ["a"]},
          {"id": "c", "capability": "python.tests.execute", "instruction": "c", "depends_on": ["ghost"]}
        ]}"#;
        let tasks = parse_and_validate(raw).expect("valid");
        // a↔b cycle broken (a keeps nothing since b is later; b keeps a); ghost dropped.
        for task in &tasks {
            for dep in &task.depends_on {
                assert!(tasks.iter().any(|t| &t.id == dep), "dep {dep} must exist");
            }
        }
        // Must be a DAG: no task depends on a later-or-equal task.
        let index: std::collections::BTreeMap<_, _> = tasks
            .iter()
            .enumerate()
            .map(|(i, t)| (t.id.clone(), i))
            .collect();
        for (i, task) in tasks.iter().enumerate() {
            for dep in &task.depends_on {
                assert!(index[dep] < i, "cycle remained");
            }
        }
    }

    #[test]
    fn rejects_too_few_tasks() {
        let raw = r#"{"tasks": [{"id": "only", "capability": "code.generate", "instruction": "x", "depends_on": []}]}"#;
        assert!(parse_and_validate(raw).is_err());
    }

    #[test]
    fn detects_only_referenced_existing_files() {
        let dir = std::env::temp_dir().join(format!("fractal-prd-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let prd = dir.join("MY_PRD.md");
        std::fs::write(&prd, "# spec").unwrap();
        assert_eq!(
            detect_prd_file("work on MY_PRD.md please", &dir),
            Some(prd.clone())
        );
        assert_eq!(detect_prd_file("work on the prd", &dir), None);
        std::fs::remove_dir_all(&dir).ok();
    }
}
