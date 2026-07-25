//! Lead-planned project decomposition.
//!
//! Every ordinary interactive build asks the selected lead to expand the request
//! (or a referenced PRD/spec) into a structured product contract plus a concrete
//! task DAG. Fractal validates both artifacts before compiling the DAG.
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
use serde::{Deserialize, Serialize};
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

#[derive(Clone, Debug, Deserialize, Serialize)]
struct StructuredPrd {
    schema: String,
    title: String,
    summary: String,
    architecture: Architecture,
    acceptance_criteria: Vec<AcceptanceCriterion>,
    #[serde(default)]
    non_goals: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct Architecture {
    approach: String,
    rationale: String,
    components: Vec<ArchitectureComponent>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct ArchitectureComponent {
    name: String,
    responsibility: String,
    technology: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct AcceptanceCriterion {
    id: String,
    criterion: String,
    verification: String,
}

#[derive(Debug)]
struct PlannedProject {
    prd: StructuredPrd,
    tasks: Vec<Task>,
}

/// One task from the lead's decomposition.
#[derive(Clone, Debug)]
struct Task {
    id: String,
    title: String,
    capability: String,
    instruction: String,
    depends_on: Vec<String>,
}

/// Expand any ordinary request into a structured PRD and committed task graph.
/// A referenced PRD/spec becomes the source; otherwise the user's request is the
/// source. The caller owns deterministic fallback when planning is unavailable.
pub(crate) fn plan_and_commit(
    request: &str,
    workspace: &Path,
    agents: &[String],
) -> Result<String> {
    if agents.is_empty() {
        return Err(anyhow!(
            "no agent is enabled to plan the PRD (enable a builder at launch)"
        ));
    }
    let referenced = detect_prd_file(request, workspace);
    let (source_text, source_name) = match referenced {
        Some(path) => {
            let text = std::fs::read_to_string(&path)
                .with_context(|| format!("read PRD file {}", path.display()))?;
            let name = path
                .file_name()
                .map(|value| value.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string());
            (text, name)
        }
        None => (request.to_owned(), "user request".to_owned()),
    };
    decompose_and_commit(&source_text, &source_name, request, workspace, &agents[0])
}

/// Commit a minimal graph before the lead starts its potentially multi-minute
/// planning call. This lets authenticated users open the permanent project URL
/// immediately; the same project document is replaced with the full DAG later.
pub(crate) fn commit_planning_preview(
    request: &str,
    workspace: &Path,
    lead_agent: &str,
) -> Result<String> {
    let source_name = detect_prd_file(request, workspace)
        .and_then(|path| path.file_name().map(|name| name.to_string_lossy().into_owned()))
        .unwrap_or_else(|| "user request".to_owned());
    let harness = planning_preview_harness(&source_name, lead_agent);
    let work = build_work_value(&format!(
        "Lead agent `{lead_agent}` is planning {source_name}: {request}"
    ))?;
    let target_id = "darwin-arm64";
    let graph = crate::compile::recompile(&work, &harness, target_id)
        .context("compile the planning preview graph")?;
    let record =
        graph_store::commit_graph(&graph).context("commit the planning preview graph")?;
    if let Some(hash) = graph.get("graph_hash").and_then(Value::as_str) {
        graph_store::persist_source(hash, &harness, &work, target_id).ok();
    }
    Ok(record.graph_hash)
}

fn planning_preview_harness(source_name: &str, lead_agent: &str) -> Value {
    json!({
        "schema": "fractal.compiled_harness.v1",
        "version": 1,
        "harness_id": "harness.lead_planning_preview.v1",
        "goal": format!("Expand {source_name} into a structured project plan."),
        "nodes": [{
            "id": "lead_planning",
            "title": "Lead agent is planning the project",
            "capability": "control.plan",
            "status": "active",
            "agent": lead_agent,
            "agent_label": lead_agent,
            "memory_scopes": ["work:goal", "workspace:root"],
            "preconditions": [],
            "produced_state": ["lead_plan.ready"],
            "instruction": format!(
                "The lead is reading {source_name}, selecting architecture, defining acceptance criteria, and decomposing the execution graph."
            ),
            "budget": {"timeout_ms": 900_000},
        }],
        "edges": [],
    })
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
    source_text: &str,
    source_name: &str,
    request: &str,
    workspace: &Path,
    lead_agent: &str,
) -> Result<String> {
    println!(
        "  🧠 [{lead_agent}] expanding {source_name} into a PRD, architecture, acceptance contract, and task graph…"
    );

    let planned = plan_project(source_text, source_name, lead_agent, workspace)?;
    persist_structured_prd(workspace, &planned.prd)?;
    println!(
        "  ✓ lead proposed {} validated tasks and {} acceptance criteria; compiling the execution graph…",
        planned.tasks.len(),
        planned.prd.acceptance_criteria.len(),
    );

    let harness = build_harness_genome(&planned.tasks, source_name);

    // Reuse the exact compile + commit path the built-in harnesses use, so the
    // decomposed graph is evolution- and supervisor-ready.
    let goal = format!(
        "Execute the lead-planned project `{}` from {source_name}: {request}",
        planned.prd.title
    );
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

/// Invoke the lead agent to write `fractal-plan.json`, then validate its
/// structured product contract and task DAG.
fn plan_project(
    source_text: &str,
    source_name: &str,
    lead_agent: &str,
    workspace: &Path,
) -> Result<PlannedProject> {
    let plan_path = workspace.join("fractal-plan.json");
    // Start clean so we never read a stale plan from a previous attempt.
    std::fs::remove_file(&plan_path).ok();

    let prompt = planning_prompt(source_text, source_name);
    // Planning gets a generous budget; a hung planner is killed rather than
    // stalling forever.
    let planner_timeout_ms = std::env::var("FRACTAL_AGENT_TIMEOUT_MS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(900_000);
    let heartbeat = crate::ui::ProgressHeartbeat::planning(lead_agent, source_name);
    let run = crate::execute::run_agent_prompt(lead_agent, &prompt, workspace, planner_timeout_ms);
    heartbeat.stop();
    let run = run.with_context(|| format!("lead planner `{lead_agent}` failed to run"))?;
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
    parse_and_validate(&raw)
}

fn planning_prompt(source_text: &str, source_name: &str) -> String {
    format!(
        "You are the LEAD PLANNER for an autonomous multi-agent build team. Expand the source below \
         ({source_name}) into a product-ready structured PRD, select a concrete architecture, define \
         verifiable acceptance criteria, and decompose it into an executable task DAG.\n\n\
         Optimize the plan for the PERFORMANCE and QUALITY of the product being built — correctness, \
         robustness, and the behaviors that matter — NOT for the lowest cost or least effort. Prefer \
         a decomposition that maximizes safe parallelism (independent tasks share no dependency).\n\n\
         WRITE A FILE named exactly `fractal-plan.json` in the current directory, and nothing else. \
         It MUST be valid JSON of this exact shape:\n\
         {{\n\
           \"prd\": {{\n\
             \"schema\": \"fractal.prd.v1\",\n\
             \"title\": \"specific product title\",\n\
             \"summary\": \"what is being built and for whom\",\n\
             \"architecture\": {{\n\
               \"approach\": \"chosen architecture\",\n\
               \"rationale\": \"why it fits\",\n\
               \"components\": [{{\"name\":\"component\", \"responsibility\":\"what it owns\", \"technology\":\"concrete technology\"}}]\n\
             }},\n\
             \"acceptance_criteria\": [{{\"id\":\"AC-1\", \"criterion\":\"observable behavior\", \"verification\":\"exact test or check\"}}],\n\
             \"non_goals\": [\"explicitly excluded scope\"]\n\
           }},\n\
           \"tasks\": [{{\n\
             \"id\": \"short_snake_case_id\",\n\
             \"title\": \"one line\",\n\
             \"capability\": \"code.generate|code.edit|project.tests.execute|content.analyze\",\n\
             \"instruction\": \"a self-contained directive an agent can execute in this workspace with no other context\",\n\
             \"depends_on\": [\"ids of tasks that must finish first\"]\n\
           }}]\n\
         }}\n\n\
         Rules: {min}-{max} tasks; ids unique; `depends_on` must reference earlier task ids only and form a DAG (no cycles); \
         every `instruction` must be concrete and standalone (name the files to create/edit and what they must contain/do); \
         architecture must name at least one component; include at least one observable acceptance criterion; \
         include at least one `project.tests.execute` task that depends on implementation and verifies the stated acceptance criteria using the repository's real native/package test suite; \
         put the tasks in a sensible build order. Do NOT implement anything now — only write the plan file.\n\n\
         --- SOURCE ({source_name}) ---\n{source}\n--- END SOURCE ---",
        min = MIN_TASKS,
        max = MAX_TASKS,
        source = truncate_prd(source_text),
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

/// Parse the planner's JSON and fail closed unless the product contract and DAG
/// are complete, bounded, and acyclic.
fn parse_and_validate(raw: &str) -> Result<PlannedProject> {
    let json = extract_json(raw)?;
    let doc: Value = serde_json::from_str(&json)
        .context("planner produced invalid JSON in fractal-plan.json")?;
    let prd: StructuredPrd = serde_json::from_value(
        doc.get("prd")
            .cloned()
            .context("fractal-plan.json has no `prd` object")?,
    )
    .context("decode structured PRD")?;
    validate_structured_prd(&prd)?;
    let array = doc
        .get("tasks")
        .and_then(Value::as_array)
        .context("fractal-plan.json has no `tasks` array")?;

    let mut tasks: Vec<Task> = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for item in array {
        let id = sanitize_id(item.get("id").and_then(Value::as_str).unwrap_or(""));
        if id.is_empty() {
            bail!("planner produced a task with an empty id");
        }
        if !seen.insert(id.clone()) {
            bail!("planner produced duplicate task id `{id}`");
        }
        let instruction = item
            .get("instruction")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_owned();
        if instruction.is_empty() {
            bail!("task `{id}` has no instruction");
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
    if tasks.len() > MAX_TASKS {
        bail!(
            "planner produced {} tasks — maximum is {MAX_TASKS}",
            tasks.len()
        );
    }
    let ids: std::collections::BTreeSet<String> = tasks.iter().map(|t| t.id.clone()).collect();
    for task in &tasks {
        for dependency in &task.depends_on {
            if dependency == &task.id {
                bail!("task `{}` depends on itself", task.id);
            }
            if !ids.contains(dependency) {
                bail!("task `{}` depends on unknown task `{dependency}`", task.id);
            }
        }
    }
    tasks = topological_sort(tasks)?;
    if !tasks
        .iter()
        .any(|task| task.capability.ends_with("tests.execute"))
    {
        bail!("lead plan must contain a gating tests task");
    }
    Ok(PlannedProject { prd, tasks })
}

fn validate_structured_prd(prd: &StructuredPrd) -> Result<()> {
    if prd.schema != "fractal.prd.v1" {
        bail!("structured PRD schema must be `fractal.prd.v1`");
    }
    if prd.title.trim().is_empty()
        || prd.summary.trim().is_empty()
        || prd.architecture.approach.trim().is_empty()
        || prd.architecture.rationale.trim().is_empty()
        || prd.architecture.components.is_empty()
        || prd.acceptance_criteria.is_empty()
    {
        bail!("structured PRD is missing required product or architecture detail");
    }
    if prd.architecture.components.iter().any(|component| {
        component.name.trim().is_empty()
            || component.responsibility.trim().is_empty()
            || component.technology.trim().is_empty()
    }) {
        bail!("every architecture component needs name, responsibility, and technology");
    }
    let mut criteria = std::collections::BTreeSet::new();
    for criterion in &prd.acceptance_criteria {
        if criterion.id.trim().is_empty()
            || criterion.criterion.trim().is_empty()
            || criterion.verification.trim().is_empty()
            || !criteria.insert(criterion.id.clone())
        {
            bail!("acceptance criteria require unique ids, behavior, and verification");
        }
    }
    Ok(())
}

fn topological_sort(tasks: Vec<Task>) -> Result<Vec<Task>> {
    let mut remaining = tasks;
    let mut ordered = Vec::with_capacity(remaining.len());
    let mut completed = std::collections::BTreeSet::new();
    while !remaining.is_empty() {
        let Some(index) = remaining
            .iter()
            .position(|task| task.depends_on.iter().all(|dep| completed.contains(dep)))
        else {
            bail!("lead plan contains a dependency cycle");
        };
        let task = remaining.remove(index);
        completed.insert(task.id.clone());
        ordered.push(task);
    }
    Ok(ordered)
}

fn structured_prd_path(workspace: &Path) -> PathBuf {
    workspace.join(".fractal").join("lead-prd.json")
}

fn persist_structured_prd(workspace: &Path, prd: &StructuredPrd) -> Result<()> {
    let path = structured_prd_path(workspace);
    let directory = path.parent().expect("structured PRD has parent");
    std::fs::create_dir_all(directory)?;
    let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
    std::fs::write(&temporary, serde_json::to_vec_pretty(prd)?)
        .with_context(|| format!("write {}", temporary.display()))?;
    std::fs::rename(&temporary, &path).with_context(|| format!("replace {}", path.display()))?;
    println!("  ◇ Structured PRD: {}", path.display());
    Ok(())
}

/// Assemble a `fractal.compiled_harness.v1` genome from the task DAG. Each task is
/// a node carrying its instruction; a durable lead-plan root makes all workers
/// non-root; and a lead-only closeout node reviews every completed sink.
fn build_harness_genome(tasks: &[Task], prd_name: &str) -> Value {
    let ready = |id: &str| format!("{id}.ready");
    let mut nodes = vec![json!({
        "id": "lead_plan",
        "title": "Lead PRD and architecture approved",
        "capability": "control.plan",
        "memory_scopes": ["work:goal", "workspace:root"],
        "preconditions": [],
        "produced_state": [ready("lead_plan")],
        "instruction": "The lead-authored .fractal/lead-prd.json and validated task DAG are ready.",
        "budget": {"timeout_ms": 5_000},
    })];
    let mut edges = Vec::new();

    for task in tasks {
        let dependencies = if task.depends_on.is_empty() {
            vec!["lead_plan".to_owned()]
        } else {
            task.depends_on.clone()
        };
        let preconditions: Vec<String> = dependencies.iter().map(|dep| ready(dep)).collect();
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
        for dep in &dependencies {
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
        edges.push(json!({"from": task.id, "to": "lead_closeout", "condition": "success"}));
    }
    nodes.push(json!({
        "id": "lead_closeout",
        "title": "Lead acceptance review and closeout",
        "capability": "control.closeout",
        "memory_scopes": ["work:goal", "workspace:root"],
        "preconditions": closer_preconditions,
        "produced_state": ["outcome_verified"],
        "instruction": "Review the finished implementation against .fractal/lead-prd.json. Inspect the changes and verification evidence, run any final checks needed, then write .fractal/closeout.json with schema fractal.closeout.v1, status approved, a non-empty summary, an acceptance array containing every PRD acceptance id with passed=true and concrete evidence, and a risks array. Do not approve if any criterion is unsupported.",
        "budget": {"timeout_ms": 180_000},
    }));

    json!({
        "schema": "fractal.compiled_harness.v1",
        "version": 1,
        "harness_id": "harness.lead_planned_project.v1",
        "goal": format!("Execute and close out the lead-planned project derived from {prd_name}."),
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

    fn valid_plan(tasks: &str) -> String {
        format!(
            r#"{{
              "prd": {{
                "schema": "fractal.prd.v1",
                "title": "Expense tracker",
                "summary": "Track personal expenses.",
                "architecture": {{
                  "approach": "local-first application",
                  "rationale": "simple and testable",
                  "components": [
                    {{"name":"app","responsibility":"user workflows","technology":"Rust"}}
                  ]
                }},
                "acceptance_criteria": [
                  {{"id":"AC-1","criterion":"expenses can be recorded","verification":"automated test"}}
                ],
                "non_goals": []
              }},
              "tasks": {tasks}
            }}"#
        )
    }

    #[test]
    fn parses_a_task_dag_and_synthesizes_a_closer() {
        let raw = valid_plan(
            r#"[
          {"id": "core", "title": "core", "capability": "code.generate", "instruction": "write core.py", "depends_on": []},
          {"id": "tests", "title": "tests", "capability": "python.tests.execute", "instruction": "run pytest", "depends_on": ["core"]}
        ]"#,
        );
        let planned = parse_and_validate(&raw).expect("valid plan");
        assert_eq!(planned.tasks.len(), 2);
        let genome = build_harness_genome(&planned.tasks, "X.md");
        let ids: Vec<&str> = genome["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n["id"].as_str().unwrap())
            .collect();
        assert!(
            ids.contains(&"lead_plan")
                && ids.contains(&"core")
                && ids.contains(&"tests")
                && ids.contains(&"lead_closeout")
        );
        assert!(genome["edges"]
            .as_array()
            .unwrap()
            .iter()
            .any(|edge| edge["from"] == "lead_plan" && edge["to"] == "core"));
        let closer_edge = genome["edges"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["from"] == "tests" && e["to"] == "lead_closeout");
        assert!(closer_edge);
    }

    #[test]
    fn rejects_cycles_and_unknown_dependencies() {
        let cycle = valid_plan(
            r#"[
          {"id": "a", "capability": "code.generate", "instruction": "a", "depends_on": ["b"]},
          {"id": "b", "capability": "code.generate", "instruction": "b", "depends_on": ["a"]},
          {"id": "tests", "capability": "project.tests.execute", "instruction": "test", "depends_on": ["a"]}
        ]"#,
        );
        assert!(parse_and_validate(&cycle)
            .unwrap_err()
            .to_string()
            .contains("cycle"));

        let unknown = valid_plan(
            r#"[
          {"id": "core", "capability": "code.generate", "instruction": "core", "depends_on": ["ghost"]},
          {"id": "tests", "capability": "project.tests.execute", "instruction": "test", "depends_on": ["core"]}
        ]"#,
        );
        assert!(parse_and_validate(&unknown)
            .unwrap_err()
            .to_string()
            .contains("unknown task"));
    }

    #[test]
    fn rejects_too_few_tasks() {
        let raw = valid_plan(
            r#"[{"id": "only", "capability": "code.generate", "instruction": "x", "depends_on": []}]"#,
        );
        assert!(parse_and_validate(&raw).is_err());
    }

    #[test]
    fn topologically_orders_forward_dependencies_and_requires_tests() {
        let raw = valid_plan(
            r#"[
          {"id": "tests", "capability": "project.tests.execute", "instruction": "test", "depends_on": ["core"]},
          {"id": "core", "capability": "code.generate", "instruction": "core", "depends_on": []}
        ]"#,
        );
        let planned = parse_and_validate(&raw).expect("valid");
        assert_eq!(planned.tasks[0].id, "core");
        assert_eq!(planned.tasks[1].id, "tests");

        let no_tests = valid_plan(
            r#"[
          {"id": "core", "capability": "code.generate", "instruction": "core", "depends_on": []},
          {"id": "docs", "capability": "content.analyze", "instruction": "docs", "depends_on": ["core"]}
        ]"#,
        );
        assert!(parse_and_validate(&no_tests)
            .unwrap_err()
            .to_string()
            .contains("gating tests"));
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

    #[test]
    fn planning_preview_exposes_one_active_lead_node() {
        let harness = planning_preview_harness("APP_PRD.md", "claude");
        let node = &harness["nodes"][0];
        assert_eq!(harness["harness_id"], "harness.lead_planning_preview.v1");
        assert_eq!(node["id"], "lead_planning");
        assert_eq!(node["capability"], "control.plan");
        assert_eq!(node["status"], "active");
        assert_eq!(node["agent"], "claude");
    }

    #[test]
    fn planning_preview_compiles_to_a_publishable_execution_graph() {
        let harness = planning_preview_harness("APP_PRD.md", "claude");
        let work = build_work_value("Plan the application").expect("work");
        let graph =
            crate::compile::recompile(&work, &harness, "darwin-arm64").expect("planning graph");
        crate::graph_store::verify_graph_document(&graph).expect("valid graph hash");
        let nodes = graph["nodes"].as_array().expect("nodes");
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0]["capability"], "control.plan");
    }
}
