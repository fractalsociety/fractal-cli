//! Selected-harness compilation into `fractal.execution_graph.v1`.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Map, Value};

use crate::efficiency::{validate_node_metadata, NodeEfficiencyMetadata, MAX_BASIS_BYTES};
use crate::harness::HarnessSelection;

const CODE_HARNESS_FIXTURE: &str = include_str!(
    "../../FractalRuntime/contracts/v1/fixtures/fractal-compiled-harness-v1-python-repair.json"
);

/// Resolve a selected starter harness to a compiler-ready harness document.
///
/// A `code` goal that *builds something new* (greenfield) compiles to a
/// task-faithful build harness (analyze → implement → acceptance → complete)
/// whose acceptance node is graded by the work's own acceptance test, rather
/// than the repository-repair fixture (which assumes an existing repo to edit).
/// A `code` goal that reads as a repair keeps the python-repair fixture.
fn harness_for(selection: &HarnessSelection, goal: &str, success_criteria: &[String]) -> Value {
    if selection.family != "code" {
        return minimal_harness(selection);
    }
    if goal_is_greenfield_build(goal) {
        return build_harness(selection, goal, success_criteria);
    }
    match serde_json::from_str(CODE_HARNESS_FIXTURE) {
        Ok(harness) => harness,
        Err(_) => minimal_harness(selection),
    }
}

/// Heuristic: should this compile to the task-faithful build harness (which has a
/// real planning node and grades on the work's own acceptance) rather than the
/// repository-repair fixture (which assumes an existing repo to edit)?
///
/// Only a clear repair signal keeps the repair fixture. Everything else — an
/// explicit build verb, or an open-ended directive like "work on the PRD" / "ship
/// the roadmap" / "execute the spec" — routes to the build harness, because those
/// are greenfield planning tasks, not edits to a failing repo. (Previously an
/// open-ended goal with no build verb fell through to the repair fixture, whose
/// passive `analyze` node does no planning at all — which is why "work on the prd"
/// never produced a task breakdown.)
fn goal_is_greenfield_build(goal: &str) -> bool {
    let goal = goal.to_ascii_lowercase();
    const REPAIR: [&str; 6] = ["fix", "repair", "bug", "regression", "debug", "failing"];
    !REPAIR.iter().any(|word| goal.contains(word))
}

/// Baseline planning metadata for nodes Fractal synthesizes itself. Planner
/// tasks declare their own metadata; legacy harnesses without any stay valid.
pub(crate) fn baseline_node_efficiency(
    estimated_remaining_tokens: u64,
    dependencies: Vec<String>,
    expected_artifact: &str,
    files_or_systems_affected: Vec<String>,
    verification_plan: &str,
) -> NodeEfficiencyMetadata {
    NodeEfficiencyMetadata {
        estimated_remaining_tokens,
        dependencies,
        expected_artifact: bounded_basis(expected_artifact),
        files_or_systems_affected,
        verification_plan: bounded_basis(verification_plan),
        current_assumptions: Vec::new(),
        similarity_to_other_active_nodes: BTreeMap::new(),
        confidence_still_useful: 1.0,
    }
}

/// Baseline metadata already encoded for direct embedding in a harness node.
pub(crate) fn baseline_efficiency_value(
    estimated_remaining_tokens: u64,
    dependencies: Vec<String>,
    expected_artifact: &str,
    files_or_systems_affected: Vec<String>,
    verification_plan: &str,
) -> Value {
    node_efficiency_to_graph_value(&baseline_node_efficiency(
        estimated_remaining_tokens,
        dependencies,
        expected_artifact,
        files_or_systems_affected,
        verification_plan,
    ))
}

/// Encode planning metadata for embedding in canonically hashed documents.
/// `fractal-cjson-v1` rejects floating-point numbers, so the two unit-interval
/// fields travel as decimal strings; field names and everything else match
/// `NodeEfficiencyMetadata` exactly. The caller passes validated metadata.
pub(crate) fn node_efficiency_to_graph_value(meta: &NodeEfficiencyMetadata) -> Value {
    let mut value = serde_json::to_value(meta).expect("encode node efficiency metadata");
    value["confidence_still_useful"] = Value::String(meta.confidence_still_useful.to_string());
    value["similarity_to_other_active_nodes"] = Value::Object(
        meta.similarity_to_other_active_nodes
            .iter()
            .map(|(peer, score)| (peer.clone(), Value::String(score.to_string())))
            .collect(),
    );
    value
}

/// Decode node planning metadata, accepting the canonical string form for the
/// unit-interval fields as well as plain JSON numbers.
pub(crate) fn node_efficiency_from_graph_value(raw: &Value) -> Result<NodeEfficiencyMetadata> {
    let mut value = raw.clone();
    if let Some(text) = value
        .get("confidence_still_useful")
        .and_then(Value::as_str)
        .map(str::to_owned)
    {
        value["confidence_still_useful"] = parse_unit_number(&text, "confidence_still_useful")?;
    }
    if let Some(scores) = value
        .get("similarity_to_other_active_nodes")
        .and_then(Value::as_object)
        .cloned()
    {
        let mut converted = Map::new();
        for (peer, score) in scores {
            let score = match score {
                Value::String(text) => {
                    parse_unit_number(&text, "similarity_to_other_active_nodes")?
                }
                other => other,
            };
            converted.insert(peer, score);
        }
        value["similarity_to_other_active_nodes"] = Value::Object(converted);
    }
    serde_json::from_value(value)
        .map_err(|error| anyhow!("efficiency metadata is malformed: {error}"))
}

fn parse_unit_number(text: &str, field: &str) -> Result<Value> {
    let parsed: f64 = text
        .parse()
        .map_err(|_| anyhow!("{field} must be a decimal number, got `{text}`"))?;
    serde_json::Number::from_f64(parsed)
        .map(Value::Number)
        .ok_or_else(|| anyhow!("{field} must be a finite number, got `{text}`"))
}

/// Clamp free text to the efficiency contract's basis size bound.
fn bounded_basis(text: &str) -> String {
    let text = text.trim();
    let mut cut = text.len().min(MAX_BASIS_BYTES);
    while cut > 0 && !text.is_char_boundary(cut) {
        cut -= 1;
    }
    text[..cut].to_owned()
}

/// A task-faithful greenfield build harness. The `implement` node produces the
/// artifact from the work goal in the workspace; the `acceptance` node runs the
/// work's acceptance test (wired by the runtime node-verifier at run time) so a
/// "verified outcome" means the built artifact actually passes.
fn build_harness(selection: &HarnessSelection, goal: &str, success_criteria: &[String]) -> Value {
    let goal = goal.trim();
    let criteria = if success_criteria.is_empty() {
        "the artifact satisfies the goal".to_owned()
    } else {
        success_criteria.join("; ")
    };
    // A parallel decomposition: the lead plans a shared interface, then the
    // implementation and the tests are written *simultaneously* by two different
    // agents, a third cross-checks them, and finally the suite is verified.
    let plan = format!(
        "Plan the build for: {goal}. Optimize for the BEST performance and quality of the product itself — correctness, robustness, and the behaviors that matter — NOT for the lowest cost or the least effort. \
         First reason briefly through 2-3 alternative implementation approaches and choose the one that maximizes product performance, noting why the others are weaker. \
         Then write INTERFACE.md stating: the single module filename (e.g. solution.py); the public function name(s) and signatures; the chosen approach (1-2 lines) and the alternatives you rejected; and — as your own acceptance BENCHMARKS — 5-8 concrete, measurable behaviors and edge cases the product must satisfy to count as high-performing: {criteria}. \
         Make the benchmarks demanding and specific so they genuinely test performance, not just happy-path. Keep it concrete so two agents can build the code and the tests in parallel from it."
    );
    let implement = format!(
        "Read INTERFACE.md and implement the module file exactly as specified for: {goal}. Self-contained, no network. Do not write tests — another agent is doing that in parallel."
    );
    let author_tests = format!(
        "Read INTERFACE.md and write a Python `unittest` test file (test_<module>.py) that imports the module and rigorously tests EVERY benchmark behavior and edge case the plan listed for: {goal}. The plan's benchmarks are the bar for high performance, so make the tests demanding — cover the edge cases and failure modes, not just the happy path. Do not write the implementation — another agent is doing that in parallel."
    );
    let review = format!(
        "The implementation and the tests were written in parallel by two agents. Run `python3 -m unittest`, then reconcile them: fix any import/name/signature mismatch so the tests exercise the implementation and all tests pass. Criteria: {criteria}."
    );
    let acceptance =
        "Run the whole unittest suite; every test must pass before completion.".to_owned();
    let complete = "Implementation and tests were built in parallel, cross-checked, and verified — mark the outcome complete.".to_owned();
    json!({
        "schema": "fractal.compiled_harness.v1",
        "version": 1,
        "harness_id": selection.harness_id,
        "goal": "Decompose the build into parallel implementation + tests, cross-check them, and prove the suite passes.",
        "nodes": [
            {
                "id": "plan",
                "capability": "code.generate",
                "memory_scopes": ["work:goal", "workspace:root"],
                "preconditions": [],
                "produced_state": ["plan_ready"],
                "instruction": plan,
                "budget": {"timeout_ms": 60_000},
                "efficiency": baseline_efficiency_value(
                    6_000,
                    vec![],
                    "INTERFACE.md",
                    vec!["INTERFACE.md".to_owned()],
                    "The review and acceptance nodes prove the interface is implemented and tested.",
                )
            },
            {
                "id": "implement",
                "capability": "code.generate",
                "memory_scopes": ["work:goal", "workspace:root"],
                "preconditions": ["plan_ready"],
                "produced_state": ["implementation_ready"],
                "instruction": implement,
                "budget": {"timeout_ms": 180_000},
                "efficiency": baseline_efficiency_value(
                    20_000,
                    vec!["plan".to_owned()],
                    "The module file specified by INTERFACE.md.",
                    vec![],
                    "The acceptance node runs the whole unittest suite.",
                )
            },
            {
                "id": "author_tests",
                "capability": "code.generate",
                "memory_scopes": ["work:goal", "workspace:root"],
                "preconditions": ["plan_ready"],
                "produced_state": ["tests_ready"],
                "instruction": author_tests,
                "budget": {"timeout_ms": 180_000},
                "efficiency": baseline_efficiency_value(
                    16_000,
                    vec!["plan".to_owned()],
                    "A unittest file covering every INTERFACE.md benchmark.",
                    vec![],
                    "The acceptance node runs the whole unittest suite.",
                )
            },
            {
                "id": "review",
                "capability": "code.edit",
                "memory_scopes": ["work:goal", "workspace:root"],
                "preconditions": ["implementation_ready", "tests_ready"],
                "produced_state": ["reviewed"],
                "instruction": review,
                "budget": {"timeout_ms": 180_000},
                "efficiency": baseline_efficiency_value(
                    10_000,
                    vec!["implement".to_owned(), "author_tests".to_owned()],
                    "The reconciled implementation and test files.",
                    vec![],
                    "python3 -m unittest passes after reconciliation.",
                )
            },
            {
                "id": "acceptance",
                "capability": "python.tests.execute",
                "memory_scopes": ["work:goal", "workspace:root", "acceptance:spec"],
                "preconditions": ["reviewed"],
                "produced_state": ["acceptance_passed"],
                "instruction": acceptance,
                "budget": {"timeout_ms": 120_000},
                "efficiency": baseline_efficiency_value(
                    4_000,
                    vec!["review".to_owned()],
                    "A fully passing unittest run.",
                    vec![],
                    "python3 -m unittest exits successfully.",
                )
            },
            {
                "id": "complete",
                "capability": "control.complete",
                "memory_scopes": ["work:goal"],
                "preconditions": ["acceptance_passed"],
                "produced_state": ["outcome_verified"],
                "instruction": complete,
                "budget": {"timeout_ms": 5_000},
                "efficiency": baseline_efficiency_value(
                    500,
                    vec!["acceptance".to_owned()],
                    "The verified-outcome completion marker.",
                    vec![],
                    "The acceptance_passed state is present before completion.",
                )
            }
        ],
        "edges": [
            {"from": "plan", "to": "implement", "condition": "success"},
            {"from": "plan", "to": "author_tests", "condition": "success"},
            {"from": "implement", "to": "review", "condition": "success"},
            {"from": "author_tests", "to": "review", "condition": "success"},
            {"from": "review", "to": "acceptance", "condition": "success"},
            {"from": "acceptance", "to": "complete", "condition": "success"}
        ]
    })
}

fn minimal_harness(selection: &HarnessSelection) -> Value {
    json!({
        "schema": "fractal.compiled_harness.v1",
        "version": 1,
        "harness_id": selection.harness_id,
        "goal": "Analyze the work, implement the outcome, and verify the result.",
        "nodes": [
            {
                "id": "analyze",
                "capability": "content.analyze",
                "memory_scopes": ["work:goal"],
                "preconditions": [],
                "produced_state": ["analysis_complete"],
                "budget": {"timeout_ms": 30_000},
                "efficiency": baseline_efficiency_value(
                    4_000,
                    vec![],
                    "An analysis of the work goal.",
                    vec![],
                    "The verify node checks the implemented result.",
                )
            },
            {
                "id": "implement",
                "capability": "code.generate",
                "memory_scopes": ["work:goal"],
                "preconditions": ["analysis_complete"],
                "produced_state": ["implementation_complete"],
                "budget": {"timeout_ms": 120_000},
                "efficiency": baseline_efficiency_value(
                    12_000,
                    vec!["analyze".to_owned()],
                    "The implemented outcome for the work goal.",
                    vec![],
                    "The verify node checks the implemented result.",
                )
            },
            {
                "id": "verify",
                "capability": "result.verify",
                "memory_scopes": ["work:goal"],
                "preconditions": ["implementation_complete"],
                "produced_state": ["result_verified"],
                "budget": {"timeout_ms": 60_000},
                "efficiency": baseline_efficiency_value(
                    4_000,
                    vec!["implement".to_owned()],
                    "A verified result for the work goal.",
                    vec![],
                    "The result.verify capability confirms the outcome.",
                )
            }
        ],
        "edges": [
            {"from": "analyze", "to": "implement", "condition": "success"},
            {"from": "implement", "to": "verify", "condition": "success"}
        ]
    })
}

/// Build the compiler's capability-keyed candidate registry.
fn build_registry(harness: &Value) -> Value {
    let greenfield_build = harness
        .get("nodes")
        .and_then(Value::as_array)
        .is_some_and(|nodes| {
            nodes
                .iter()
                .any(|node| node.get("id").and_then(Value::as_str) == Some("acceptance"))
        });
    let capabilities = harness
        .get("nodes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|node| node.get("capability").and_then(Value::as_str))
        .collect::<BTreeSet<_>>();

    let mut registry = Map::new();
    for capability in capabilities {
        let kind = if greenfield_build {
            greenfield_worker_kind(capability)
        } else {
            registry_kind(capability)
        };
        let mut candidate = Map::new();
        candidate.insert(
            "id".to_owned(),
            Value::String(format!("{}-default", route_suffix(capability))),
        );
        candidate.insert("kind".to_owned(), Value::String(kind.to_owned()));
        if matches!(kind, "container" | "verification") {
            candidate.insert(
                "sandbox_profile".to_owned(),
                Value::String("local-work-v1".to_owned()),
            );
        }
        registry.insert(
            capability.to_owned(),
            Value::Array(vec![Value::Object(candidate)]),
        );
    }
    Value::Object(registry)
}

fn greenfield_worker_kind(capability: &str) -> &'static str {
    match capability {
        "content.analyze" | "python.tests.execute" => "cursor",
        "code.generate" | "control.complete" => "codex",
        _ => registry_kind(capability),
    }
}

fn registry_kind(capability: &str) -> &'static str {
    if capability.starts_with("control.") {
        "control"
    } else if capability.contains("execute") || capability.ends_with(".edit") {
        "container"
    } else if capability.contains("verify") || capability.contains("tests") {
        "verification"
    } else {
        "inference"
    }
}

fn route_suffix(capability: &str) -> String {
    capability
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character
            } else {
                '-'
            }
        })
        .collect()
}

/// Compile the selected harness and work into a deterministic execution graph.
pub(crate) fn compile_graph(
    work: &Value,
    selection: &HarnessSelection,
    target: fractal_harnessc::Target,
) -> Result<Value> {
    let goal = work.get("goal").and_then(Value::as_str).unwrap_or_default();
    let success_criteria: Vec<String> = work
        .get("success_criteria")
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(|value| value.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    let harness = harness_for(selection, goal, &success_criteria);
    let target_id = target_id(target);
    let graph = recompile(work, &harness, target_id)?;

    // Persist the compile inputs (harness genome + work + target) alongside the
    // graph so harness evolution can mutate the genome and recompile it.
    if let Some(hash) = graph.get("graph_hash").and_then(Value::as_str) {
        crate::graph_store::persist_source(hash, &harness, work, target_id).ok();
    }
    Ok(graph)
}

/// Stable identifier for a compile target (round-trips through the source sidecar).
fn target_id(target: fractal_harnessc::Target) -> &'static str {
    match target {
        fractal_harnessc::Target::CudaVllmOci => "cuda-linux",
        fractal_harnessc::Target::DarwinMlxApple => "darwin-arm64",
    }
}

fn target_from_id(target_id: &str) -> fractal_harnessc::Target {
    match target_id {
        "cuda-linux" => fractal_harnessc::Target::CudaVllmOci,
        _ => fractal_harnessc::Target::DarwinMlxApple,
    }
}

/// Compile a harness genome + work into an execution graph. This is the single
/// genome→graph hop, shared by the initial compile and by harness evolution's
/// recompile-after-mutation path.
pub(crate) fn recompile(work: &Value, harness: &Value, target_id: &str) -> Result<Value> {
    let registry = build_registry(harness);
    let mut authorized_work = work.clone();

    // NL work describes intent-level requirements. Compilation additionally
    // authorizes the concrete selected harness's capabilities and memory scopes
    // on this copy, leaving the submitted FractalWork object unchanged.
    augment_work_authorizations(&mut authorized_work, harness)?;

    let mut graph = fractal_harnessc::compile(
        &authorized_work,
        harness,
        &registry,
        target_from_id(target_id),
    )
    .map_err(|error| anyhow!("fractal-harnessc compile failed: {error}"))?;
    annotate_execution_flow(&mut graph, harness, Some(work))?;
    Ok(graph)
}

/// Annotate a compiled graph with deterministic execution-flow and structural
/// learning metadata.  `work` is optional for the small unit fixtures that
/// exercise flow layout in isolation; real compilation always supplies it so
/// node creation/ready timestamps derive from the work's recorded creation
/// time rather than a fresh wall-clock value.
fn annotate_execution_flow(graph: &mut Value, harness: &Value, work: Option<&Value>) -> Result<()> {
    let graph_edges = graph
        .get("edges")
        .and_then(Value::as_array)
        .cloned()
        .context("compiled execution graph edges must be an array")?;
    let graph_nodes = graph
        .get_mut("nodes")
        .and_then(Value::as_array_mut)
        .context("compiled execution graph nodes must be an array")?;
    let node_ids: BTreeSet<String> = graph_nodes
        .iter()
        .filter_map(|node| node.get("id").and_then(Value::as_str).map(str::to_owned))
        .collect();
    let lead_planning_roots: BTreeSet<String> = graph_nodes
        .iter()
        .filter(|node| node.get("capability").and_then(Value::as_str) == Some("control.plan"))
        .filter_map(|node| node.get("id").and_then(Value::as_str).map(str::to_owned))
        .collect();
    let mut predecessors: BTreeMap<String, Vec<String>> =
        node_ids.iter().map(|id| (id.clone(), Vec::new())).collect();
    let mut incoming_dependencies: BTreeMap<String, Vec<String>> =
        node_ids.iter().map(|id| (id.clone(), Vec::new())).collect();
    for edge in &graph_edges {
        if let (Some(from), Some(to)) = (
            edge.get("from").and_then(Value::as_str),
            edge.get("to").and_then(Value::as_str),
        ) {
            if let Some(values) = incoming_dependencies.get_mut(to) {
                values.push(from.to_owned());
            }
            if edge.get("condition").and_then(Value::as_str) == Some("failure") {
                continue;
            }
            if let Some(values) = predecessors.get_mut(to) {
                values.push(from.to_owned());
            }
        }
    }
    for values in predecessors.values_mut() {
        values.sort();
        values.dedup();
    }
    for values in incoming_dependencies.values_mut() {
        values.sort();
        values.dedup();
    }
    let mut waves: BTreeMap<String, u32> = BTreeMap::new();
    while waves.len() < node_ids.len() {
        let before = waves.len();
        for id in &node_ids {
            if waves.contains_key(id) {
                continue;
            }
            let dependencies = &predecessors[id];
            if dependencies
                .iter()
                .all(|dependency| waves.contains_key(dependency))
            {
                let wave = if dependencies.is_empty() && lead_planning_roots.contains(id) {
                    0
                } else {
                    dependencies
                        .iter()
                        .filter_map(|dependency| waves.get(dependency))
                        .copied()
                        .max()
                        .unwrap_or(0)
                        + 1
                };
                waves.insert(id.clone(), wave);
            }
        }
        if waves.len() == before {
            bail!("compiled graph execution flow contains a dependency cycle");
        }
    }
    let mut wave_sizes = BTreeMap::new();
    for wave in waves.values() {
        *wave_sizes.entry(*wave).or_insert(0_usize) += 1;
    }
    let titles: BTreeMap<&str, &str> = harness
        .get("nodes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|node| {
            Some((
                node.get("id")?.as_str()?,
                node.get("title")?.as_str()?.trim(),
            ))
        })
        .filter(|(_, title)| !title.is_empty())
        .collect();
    let harness_nodes: BTreeMap<&str, &Value> = harness
        .get("nodes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|node| Some((node.get("id")?.as_str()?, node)))
        .collect();
    let created_at = work
        .and_then(|value| value.get("created_at_ms"))
        .and_then(Value::as_u64)
        .map(crate::work_builder::rfc3339_from_unix_millis);
    let amendment_metadata: BTreeMap<String, Value> = harness
        .get("fractal_amendments")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .map(|(id, metadata)| (id.clone(), metadata.clone()))
        .collect();
    // Planning-time efficiency metadata declared on harness nodes is validated
    // (ranges + dependency consistency) BEFORE the graph is hashed and
    // committed. Legacy harnesses whose nodes carry no metadata stay valid.
    let mut efficiency_by_id: BTreeMap<String, NodeEfficiencyMetadata> = BTreeMap::new();
    for node in harness
        .get("nodes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(id) = node.get("id").and_then(Value::as_str) else {
            continue;
        };
        let Some(raw) = node.get("efficiency") else {
            continue;
        };
        let meta = node_efficiency_from_graph_value(raw)
            .map_err(|error| anyhow!("node `{id}` efficiency metadata: {error}"))?;
        validate_node_metadata(&meta)
            .map_err(|error| anyhow!("node `{id}` efficiency metadata: {error}"))?;
        efficiency_by_id.insert(id.to_owned(), meta);
    }
    let mut wave_positions: BTreeMap<u32, u32> = BTreeMap::new();
    for node in graph_nodes {
        let id = node
            .get("id")
            .and_then(Value::as_str)
            .context("compiled graph node is missing id")?
            .to_owned();
        let wave = waves[&id];
        let position = wave_positions.entry(wave).or_insert(0);
        *position += 1;
        let task_number = format!("{wave}.{position}");
        let parallel = wave_sizes[&wave] > 1;
        node["execution"] = json!({
            "mode": if parallel { "parallel" } else { "sequential" },
            "wave": wave,
            "task_number": task_number,
            "parallel_group": if parallel {
                Value::String(format!("wave-{wave}"))
            } else {
                Value::Null
            },
        });
        if let Some(metadata) = amendment_metadata.get(&id) {
            for key in [
                "amendment_kind",
                "branch_id",
                "branch_parent",
                "branch_depth",
            ] {
                if let Some(value) = metadata.get(key) {
                    node["execution"][key] = value.clone();
                }
            }
        }
        if let Some(title) = titles.get(id.as_str()) {
            node["title"] = Value::String((*title).to_owned());
        }
        initialize_node_learning(
            node,
            id.as_str(),
            incoming_dependencies
                .get(&id)
                .map(Vec::as_slice)
                .unwrap_or_default(),
            predecessors.get(&id).is_none_or(Vec::is_empty),
            created_at.as_deref(),
            harness_nodes.get(id.as_str()).copied(),
        );
        if let Some(meta) = efficiency_by_id.get(&id) {
            for dependency in &meta.dependencies {
                if !predecessors
                    .get(&id)
                    .is_some_and(|values| values.contains(dependency))
                {
                    bail!(
                        "node `{id}` efficiency dependency `{dependency}` is not an execution dependency of the node"
                    );
                }
            }
            for peer in meta.similarity_to_other_active_nodes.keys() {
                if peer == &id || !node_ids.contains(peer) {
                    bail!(
                        "node `{id}` efficiency similarity peer `{peer}` must name another graph node"
                    );
                }
            }
            node["efficiency"] = node_efficiency_to_graph_value(meta);
        }
    }
    let flow_waves = wave_sizes
        .iter()
        .map(|(wave, size)| {
            json!({
                "wave": wave,
                "mode": if *size > 1 { "parallel" } else { "sequential" },
                "parallel_group": if *size > 1 {
                    Value::String(format!("wave-{wave}"))
                } else {
                    Value::Null
                },
                "nodes": waves.iter()
                    .filter(|(_, node_wave)| *node_wave == wave)
                    .map(|(id, _)| Value::String(id.clone()))
                    .collect::<Vec<_>>(),
            })
        })
        .collect::<Vec<_>>();
    graph["execution_flow"] = json!({
        "schema": "fractal.execution_flow.v1",
        "waves": flow_waves,
    });
    let object = graph
        .as_object_mut()
        .context("compiled execution graph must be an object")?;
    object.remove("graph_hash");
    let graph_hash = fractal_contracts::canonical_sha256(&Value::Object(object.clone()))
        .map_err(|error| anyhow!("execution flow graph hashing failed: {error}"))?;
    object.insert("graph_hash".to_owned(), Value::String(graph_hash));
    Ok(())
}

/// Populate only facts known at graph-production time.  Runtime timestamps,
/// outcomes, verification, costs, artifacts, and intervention records remain
/// absent until the lifecycle recorder observes them.
fn initialize_node_learning(
    node: &mut Value,
    id: &str,
    dependencies: &[String],
    ready: bool,
    created_at: Option<&str>,
    harness_node: Option<&Value>,
) {
    let capability = node
        .get("capability")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let node_type = node_type_for_capability(capability);
    let objective = harness_node
        .and_then(|source| source.get("title"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .or_else(|| node.get("title").and_then(Value::as_str))
        .or_else(|| node.get("instruction").and_then(Value::as_str))
        .unwrap_or(id)
        .to_owned();

    node["node_type"] = Value::String(node_type.to_owned());
    node["objective"] = Value::String(bounded_objective(&objective));
    node["depends_on"] = Value::Array(dependencies.iter().cloned().map(Value::String).collect());
    if let Some(created_at) = created_at {
        node["created_at"] = Value::String(created_at.to_owned());
        if ready {
            node["ready_at"] = Value::String(created_at.to_owned());
        } else {
            node.as_object_mut().map(|object| object.remove("ready_at"));
        }
    }

    if let Some(executor) = executor_configuration(harness_node) {
        node["executor"] = Value::Object(executor);
    } else {
        node.as_object_mut().map(|object| object.remove("executor"));
    }

    if let Some(estimated_cost) = estimated_cost(harness_node) {
        node["estimated_cost"] = estimated_cost;
    } else {
        node.as_object_mut()
            .map(|object| object.remove("estimated_cost"));
    }

    // These are initialized collections/defaults, not lifecycle facts.  Keep
    // terminal and historical fields absent so downstream code cannot mistake
    // a planned node for one that already ran.
    node["attempt_count"] = Value::from(0_u64);
    node["artifacts_produced"] = Value::Array(Vec::new());
    node["consumed_by"] = Value::Array(Vec::new());
    node["human_intervention"] = Value::Bool(false);
    node["reopen_count"] = Value::from(0_u64);
    if let Some(object) = node.as_object_mut() {
        for key in [
            "started_at",
            "finished_at",
            "outcome",
            "failure_code",
            "verification",
            "actual_cost",
            "notes",
        ] {
            object.remove(key);
        }
    }
}

fn node_type_for_capability(capability: &str) -> &'static str {
    if capability.starts_with("control.") {
        "control"
    } else if capability.starts_with("project.tests")
        || capability.starts_with("python.tests")
        || capability.contains("verify")
        || capability.contains("test")
    {
        "verification"
    } else {
        "implementation"
    }
}

fn bounded_objective(value: &str) -> String {
    let value = value.trim();
    let mut cut = value.len().min(1_000);
    while cut > 0 && !value.is_char_boundary(cut) {
        cut -= 1;
    }
    value[..cut].to_owned()
}

fn executor_configuration(harness_node: Option<&Value>) -> Option<Map<String, Value>> {
    let source = harness_node?;
    let mut executor = Map::new();
    for (destination, source_key) in [
        ("agent", "agent"),
        ("model", "model"),
        ("version", "version"),
        ("config_hash", "config_hash"),
        ("label", "agent_label"),
    ] {
        if let Some(value) = source
            .get(source_key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            executor.insert(destination.to_owned(), Value::String(value.to_owned()));
        }
    }
    for container_key in ["executor", "executor_config"] {
        let Some(config) = source.get(container_key).and_then(Value::as_object) else {
            continue;
        };
        for key in ["agent", "model", "version", "config_hash", "label"] {
            if executor.contains_key(key) {
                continue;
            }
            if let Some(value) = config
                .get(key)
                .or_else(|| {
                    (key == "label")
                        .then(|| config.get("agent_label"))
                        .flatten()
                })
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
            {
                executor.insert(key.to_owned(), Value::String(value.to_owned()));
            }
        }
    }
    (!executor.is_empty()).then_some(executor)
}

fn estimated_cost(harness_node: Option<&Value>) -> Option<Value> {
    let source = harness_node?;
    let raw = source.get("estimated_cost").or_else(|| {
        source
            .get("budget")
            .and_then(|budget| budget.get("estimated_cost"))
    })?;
    // Canonical JSON deliberately rejects floating-point values.  Costs that
    // are available at planning time are therefore accepted only in the
    // integer/microunit form used by the work contract.
    raw.as_u64()
        .map(Value::from)
        .or_else(|| raw.as_i64().filter(|value| *value >= 0).map(Value::from))
}

fn augment_work_authorizations(work: &mut Value, harness: &Value) -> Result<()> {
    let nodes = harness
        .get("nodes")
        .and_then(Value::as_array)
        .context("compiled harness nodes must be an array")?;
    let object = work
        .as_object_mut()
        .context("FractalWork must be a JSON object")?;

    let mut capabilities =
        string_set(object.get("required_capabilities"), "required_capabilities")?;
    let mut scopes = string_set(object.get("memory_scopes"), "memory_scopes")?;

    for node in nodes {
        let node_object = node
            .as_object()
            .context("compiled harness node must be an object")?;
        let capability = node_object
            .get("capability")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .context("compiled harness node capability must be a non-empty string")?;
        capabilities.insert(capability.to_owned());

        let node_scopes = node_object
            .get("memory_scopes")
            .and_then(Value::as_array)
            .context("compiled harness node memory_scopes must be an array")?;
        for scope in node_scopes {
            let scope = scope
                .as_str()
                .filter(|value| !value.is_empty())
                .context("compiled harness memory scope must be a non-empty string")?;
            scopes.insert(scope.to_owned());
        }
    }

    object.insert(
        "required_capabilities".to_owned(),
        Value::Array(capabilities.into_iter().map(Value::String).collect()),
    );
    object.insert(
        "memory_scopes".to_owned(),
        Value::Array(scopes.into_iter().map(Value::String).collect()),
    );
    Ok(())
}

fn string_set(value: Option<&Value>, field: &str) -> Result<BTreeSet<String>> {
    let values = value
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("FractalWork {field} must be an array"))?;
    let mut strings = BTreeSet::new();
    for value in values {
        let Some(value) = value.as_str().filter(|value| !value.is_empty()) else {
            bail!("FractalWork {field} must contain non-empty strings");
        };
        strings.insert(value.to_owned());
    }
    Ok(strings)
}

#[cfg(test)]
mod tests {
    use fractal_harnessc::Target;
    use serde_json::{json, Value};

    use super::{
        annotate_execution_flow, compile_graph, harness_for, node_efficiency_from_graph_value,
        recompile,
    };

    /// Isolate `FRACTAL_HOME` (compile now persists a genome sidecar) and
    /// serialize with the other tests that mutate the environment.
    fn isolate() -> (
        std::sync::MutexGuard<'static, ()>,
        crate::graph_store::TestHome,
    ) {
        let lock = crate::graph_store::ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let home = crate::graph_store::TestHome::new("compile").expect("test home");
        (lock, home)
    }
    use crate::harness::{select_harness, HarnessSelection};

    fn representative_work() -> Value {
        json!({
            "schema": "fractal.work.v1",
            "work_id": "fw_reverse_cli",
            "intent": "nl.code",
            "goal": "A string-reversing CLI is implemented and verified.",
            "inputs": [],
            "constraints": {
                "privacy": "local_only",
                "deadline_ms": 300_000,
                "max_memory_mib": 4096,
                "max_tokens": 4096,
                "max_cost_microunits": 0,
                "network_policy": "deny"
            },
            "required_capabilities": ["code.generate"],
            "risk": "medium",
            "success_criteria": ["the CLI reverses a representative string"],
            "memory_scopes": ["work:goal"],
            "requester": "local:test",
            "created_at_ms": 0,
            "content_hash": "sha256:test"
        })
    }

    #[test]
    fn code_family_compiles_to_execution_graph() {
        let _guard = isolate();
        let graph = compile_graph(
            &representative_work(),
            &select_harness("nl.code"),
            Target::DarwinMlxApple,
        )
        .expect("code harness should compile");

        assert_eq!(graph["schema"], "fractal.execution_graph.v1");
        assert!(graph["graph_hash"]
            .as_str()
            .is_some_and(|hash| !hash.is_empty()));
        assert!(graph["nodes"]
            .as_array()
            .is_some_and(|nodes| !nodes.is_empty()));
        assert_eq!(
            graph["execution_flow"]["schema"],
            "fractal.execution_flow.v1"
        );
        assert!(graph["nodes"].as_array().unwrap().iter().all(|node| {
            node["execution"]["wave"].as_u64().is_some()
                && node["execution"]["task_number"]
                    .as_str()
                    .is_some_and(|value| value.contains('.'))
        }));
    }

    #[test]
    fn compiled_nodes_initialize_structural_learning_fields() {
        let _guard = isolate();
        let mut work = representative_work();
        work["created_at_ms"] = json!(1_000);
        work["goal"] = json!("Build a tiny CLI that reverses a string, with a passing test.");
        let graph = compile_graph(&work, &select_harness("nl.code"), Target::DarwinMlxApple)
            .expect("build harness compiles");
        let nodes = graph["nodes"].as_array().expect("nodes");
        let node = |id: &str| {
            nodes
                .iter()
                .find(|node| node["id"] == id)
                .unwrap_or_else(|| panic!("missing node {id}"))
        };
        let created = json!("1970-01-01T00:00:01Z");

        let plan = node("plan");
        assert_eq!(plan["node_type"], "implementation");
        assert!(plan["objective"]
            .as_str()
            .is_some_and(|value| value.contains("Plan the build")));
        assert_eq!(plan["depends_on"], json!([]));
        assert_eq!(plan["created_at"], created);
        assert_eq!(plan["ready_at"], created);

        let implement = node("implement");
        assert_eq!(implement["node_type"], "implementation");
        assert_eq!(implement["depends_on"], json!(["plan"]));
        assert_eq!(implement["created_at"], created);
        assert!(implement.get("ready_at").is_none());
        assert_eq!(implement["execution"]["mode"], "parallel");

        let acceptance = node("acceptance");
        assert_eq!(acceptance["node_type"], "verification");
        assert_eq!(acceptance["depends_on"], json!(["review"]));
        let complete = node("complete");
        assert_eq!(complete["node_type"], "control");
        assert_eq!(complete["depends_on"], json!(["acceptance"]));

        for current in nodes {
            assert!(current["id"].as_str().is_some_and(|id| !id.is_empty()));
            assert!(current["objective"]
                .as_str()
                .is_some_and(|objective| !objective.is_empty()));
            assert_eq!(current["attempt_count"], 0);
            assert_eq!(current["artifacts_produced"], json!([]));
            assert_eq!(current["consumed_by"], json!([]));
            assert_eq!(current["human_intervention"], false);
            assert_eq!(current["reopen_count"], 0);
            for absent in [
                "started_at",
                "finished_at",
                "outcome",
                "failure_code",
                "verification",
                "actual_cost",
                "notes",
            ] {
                assert!(
                    current.get(absent).is_none(),
                    "{absent} invented for {}",
                    current["id"]
                );
            }
        }
    }

    #[test]
    fn greenfield_build_uses_task_faithful_acceptance_harness() {
        let _guard = isolate();
        let mut work = representative_work();
        work["goal"] = json!("Build a tiny CLI that reverses a string, with a passing test.");
        let graph = compile_graph(&work, &select_harness("nl.code"), Target::DarwinMlxApple)
            .expect("build harness compiles");
        let ids: Vec<&str> = graph["nodes"]
            .as_array()
            .expect("nodes")
            .iter()
            .filter_map(|node| node["id"].as_str())
            .collect();
        // Decomposed into parallel implementation + tests, a review, and verify.
        for expected in [
            "plan",
            "implement",
            "author_tests",
            "review",
            "acceptance",
            "complete",
        ] {
            assert!(ids.contains(&expected), "missing {expected} in {ids:?}");
        }
        assert!(!serde_json::to_string(&graph)
            .unwrap()
            .contains("python_repair"));
        let requires = |id: &str| -> Vec<String> {
            graph["nodes"]
                .as_array()
                .unwrap()
                .iter()
                .find(|node| node["id"] == json!(id))
                .and_then(|node| node["requires"].as_array())
                .map(|list| {
                    list.iter()
                        .filter_map(|v| v.as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default()
        };
        // implement and author_tests both depend only on `plan` → parallel.
        assert_eq!(requires("implement"), vec!["plan_ready".to_owned()]);
        assert_eq!(requires("author_tests"), vec!["plan_ready".to_owned()]);
        // review waits for both parallel builds.
        let mut review_deps = requires("review");
        review_deps.sort();
        assert_eq!(
            review_deps,
            vec!["implementation_ready".to_owned(), "tests_ready".to_owned()]
        );
        let node = |id: &str| {
            graph["nodes"]
                .as_array()
                .unwrap()
                .iter()
                .find(|node| node["id"] == id)
                .unwrap()
        };
        let implement = node("implement");
        let author_tests = node("author_tests");
        assert_eq!(implement["execution"]["mode"], "parallel");
        assert_eq!(author_tests["execution"]["mode"], "parallel");
        assert_eq!(
            implement["execution"]["parallel_group"],
            author_tests["execution"]["parallel_group"]
        );
    }

    #[test]
    fn repair_goal_keeps_python_repair_fixture() {
        let _guard = isolate();
        let mut work = representative_work();
        work["goal"] = json!("Fix the failing reverse test in the parser module.");
        let graph = compile_graph(&work, &select_harness("nl.code"), Target::DarwinMlxApple)
            .expect("repair harness compiles");
        assert!(serde_json::to_string(&graph)
            .unwrap()
            .contains("python_repair"));
    }

    #[test]
    fn repeated_compilation_is_deterministic() {
        let _guard = isolate();
        let work = representative_work();
        let selection = select_harness("nl.code");
        let first = compile_graph(&work, &selection, Target::DarwinMlxApple)
            .expect("first compile should succeed");
        let second = compile_graph(&work, &selection, Target::DarwinMlxApple)
            .expect("second compile should succeed");

        assert_eq!(first["graph_hash"], second["graph_hash"]);
    }

    #[test]
    fn unknown_family_uses_valid_minimal_harness() {
        let _guard = isolate();
        let selection = HarnessSelection {
            harness_id: "harness.generic_task.v1".to_owned(),
            family: "unknown-family".to_owned(),
            source: "test".to_owned(),
        };
        let graph = compile_graph(&representative_work(), &selection, Target::DarwinMlxApple)
            .expect("fallback harness should compile");

        assert_eq!(graph["schema"], "fractal.execution_graph.v1");
        assert_eq!(graph["nodes"].as_array().map(Vec::len), Some(3));
        assert_eq!(graph["edges"].as_array().map(Vec::len), Some(2));
        let nodes = graph["nodes"].as_array().expect("nodes");
        let node = |id: &str| nodes.iter().find(|node| node["id"] == id).unwrap();
        assert_eq!(node("analyze")["node_type"], "implementation");
        assert_eq!(node("analyze")["depends_on"], json!([]));
        assert_eq!(node("implement")["depends_on"], json!(["analyze"]));
        assert_eq!(node("verify")["node_type"], "verification");
        assert_eq!(node("verify")["depends_on"], json!(["implement"]));
        assert!(nodes.iter().all(|node| node["created_at"].is_string()));
    }

    #[test]
    fn compiled_execution_flow_carries_branch_layout_metadata() {
        let mut graph = json!({
            "nodes": [
                {"id":"anchor","capability":"code.generate"},
                {"id":"branch.build","capability":"code.generate"}
            ],
            "edges": [
                {"from":"anchor","to":"branch.build","condition":"success"}
            ]
        });
        let harness = json!({
            "nodes": [
                {"id":"anchor","title":"Anchor"},
                {"id":"branch.build","title":"Build branch"}
            ],
            "fractal_amendments": {
                "branch.build": {
                    "amendment_kind":"branch",
                    "branch_id":"branch.amend_1",
                    "branch_parent":"anchor",
                    "branch_depth":1
                }
            }
        });
        annotate_execution_flow(&mut graph, &harness, None).unwrap();
        let branch = graph["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|node| node["id"] == "branch.build")
            .unwrap();
        assert_eq!(branch["execution"]["amendment_kind"], "branch");
        assert_eq!(branch["execution"]["branch_parent"], "anchor");
        assert_eq!(branch["execution"]["branch_depth"], 1);
    }

    #[test]
    fn node_dependency_mirror_includes_each_normalized_incoming_edge() {
        let mut graph = json!({
            "nodes": [
                {"id":"root","capability":"code.generate"},
                {"id":"retry","capability":"code.generate"},
                {"id":"verify","capability":"project.tests.execute"}
            ],
            "edges": [
                {"from":"root","to":"verify","condition":"failure"},
                {"from":"root","to":"retry","condition":"success"},
                {"from":"retry","to":"verify","condition":"success"}
            ]
        });
        annotate_execution_flow(&mut graph, &json!({"nodes": []}), None).expect("flow metadata");
        let verify = graph["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|node| node["id"] == "verify")
            .unwrap();
        assert_eq!(verify["depends_on"], json!(["retry", "root"]));
    }

    #[test]
    fn compiled_nodes_expose_validated_efficiency_metadata() {
        let selection = select_harness("nl.code");
        let harness = harness_for(&selection, "Build a tiny CLI that reverses a string.", &[]);
        let graph =
            recompile(&representative_work(), &harness, "darwin-arm64").expect("build compiles");
        let node = graph["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|node| node["id"] == "implement")
            .unwrap();
        assert_eq!(node["efficiency"]["dependencies"], json!(["plan"]));
        assert_eq!(
            node["efficiency"]["estimated_remaining_tokens"],
            json!(20_000)
        );
        // Unit-interval fields travel as canonical-JSON-safe decimal strings.
        assert_eq!(node["efficiency"]["confidence_still_useful"], json!("1"));
        let decoded = node_efficiency_from_graph_value(&node["efficiency"]).expect("round-trip");
        crate::efficiency::validate_node_metadata(&decoded).expect("decoded metadata is valid");
        assert!((decoded.confidence_still_useful - 1.0).abs() < f64::EPSILON);
        assert!(graph["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .all(|node| node.get("efficiency").is_some()));
    }

    #[test]
    fn out_of_range_efficiency_metadata_fails_before_commitment() {
        let selection = select_harness("nl.code");
        let mut harness = harness_for(&selection, "Build a tiny CLI.", &[]);
        harness["nodes"][1]["efficiency"]["confidence_still_useful"] = json!("1.5");
        let error = recompile(&representative_work(), &harness, "darwin-arm64")
            .unwrap_err()
            .to_string();
        assert!(error.contains("confidence_still_useful"), "{error}");
    }

    #[test]
    fn efficiency_dependencies_must_match_graph_edges() {
        let selection = select_harness("nl.code");
        let mut harness = harness_for(&selection, "Build a tiny CLI.", &[]);
        harness["nodes"][1]["efficiency"]["dependencies"] = json!(["acceptance"]);
        let error = recompile(&representative_work(), &harness, "darwin-arm64")
            .unwrap_err()
            .to_string();
        assert!(error.contains("not an execution dependency"), "{error}");
    }

    #[test]
    fn efficiency_similarity_peers_must_name_other_nodes() {
        let selection = select_harness("nl.code");
        let mut harness = harness_for(&selection, "Build a tiny CLI.", &[]);
        harness["nodes"][1]["efficiency"]["similarity_to_other_active_nodes"] =
            json!({"ghost": "0.4"});
        let error = recompile(&representative_work(), &harness, "darwin-arm64")
            .unwrap_err()
            .to_string();
        assert!(error.contains("similarity peer"), "{error}");

        let mut harness = harness_for(&selection, "Build a tiny CLI.", &[]);
        harness["nodes"][1]["efficiency"]["similarity_to_other_active_nodes"] =
            json!({"implement": "0.4"});
        assert!(recompile(&representative_work(), &harness, "darwin-arm64").is_err());
    }

    #[test]
    fn legacy_harness_without_efficiency_metadata_still_compiles() {
        let selection = select_harness("nl.code");
        let mut harness = harness_for(&selection, "Build a tiny CLI.", &[]);
        for node in harness["nodes"].as_array_mut().unwrap() {
            node.as_object_mut().unwrap().remove("efficiency");
        }
        let graph = recompile(&representative_work(), &harness, "darwin-arm64")
            .expect("legacy harness still compiles");
        assert!(graph["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .all(|node| node.get("efficiency").is_none()));
    }

    #[test]
    fn recompilation_accepts_persisted_branch_metadata() {
        let selection = select_harness("nl.code");
        let mut harness = harness_for(
            &selection,
            "Build a tiny CLI that reverses a string.",
            &["the CLI reverses a string".to_owned()],
        );
        harness["fractal_amendments"] = json!({
            "implement": {
                "amendment_kind":"branch",
                "branch_id":"branch.amend_1",
                "branch_parent":"plan",
                "branch_depth":1
            }
        });
        let graph = recompile(&representative_work(), &harness, "darwin-arm64").unwrap();
        let implement = graph["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|node| node["id"] == "implement")
            .unwrap();
        assert_eq!(implement["execution"]["branch_id"], "branch.amend_1");
    }

    #[test]
    fn cross_boundary_compile_persist_reload_asserts_ac1_fields_and_hash() {
        let _guard = isolate();
        std::env::set_var("FRACTAL_OFFLINE", "1");
        let workspace = std::env::temp_dir().join(format!(
            "fractal-compile-e2e-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&workspace).unwrap();

        let mut work = representative_work();
        work["created_at_ms"] = json!(1_000);
        work["goal"] = json!("Build a tiny CLI that reverses a string, with a passing test.");
        let graph = compile_graph(&work, &select_harness("nl.code"), Target::DarwinMlxApple)
            .expect("build harness compiles");
        crate::graph_store::verify_graph_document(&graph).unwrap();
        let hash = graph["graph_hash"].as_str().unwrap().to_owned();
        crate::project_file::persist(&workspace, &graph, "Compile E2E").unwrap();

        let project = crate::project_file::load(&workspace).unwrap();
        assert_eq!(project.graph_hash, hash);
        assert_eq!(project.learning.schema, "fractal.learning.v1");
        let plan_graph = project.graph["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|node| node["id"] == "plan")
            .unwrap();
        assert_eq!(plan_graph["created_at"], json!("1970-01-01T00:00:01Z"));
        assert_eq!(plan_graph["ready_at"], json!("1970-01-01T00:00:01Z"));
        assert_eq!(plan_graph["attempt_count"], 0);
        assert_eq!(plan_graph["artifacts_produced"], json!([]));
        assert_eq!(plan_graph["consumed_by"], json!([]));
        assert_eq!(plan_graph["human_intervention"], false);

        let plan = &project.learning.nodes["plan"];
        assert_eq!(plan.node_id, "plan");
        assert!(!plan.node_type.is_empty());
        assert!(!plan.objective.is_empty());
        assert_eq!(plan.depends_on, Vec::<String>::new());
        assert!(plan.created_at.is_some());
        assert!(plan.ready_at.is_some());
        assert!(plan.started_at.is_none());
        assert!(plan.finished_at.is_none());
        assert_eq!(plan.attempt_count, 0);
        assert!(plan.outcome.is_none());
        assert!(plan.failure_code.is_none());
        assert!(plan.verification.is_none());
        assert!(plan.artifacts_produced.is_empty());
        assert!(plan.consumed_by.is_empty());
        assert!(!plan.human_intervention);
        assert!(plan.actual_cost.is_none());
        assert!(plan.notes.is_none());

        let implement = &project.learning.nodes["implement"];
        assert_eq!(implement.depends_on, vec!["plan".to_owned()]);
        assert!(implement.ready_at.is_none());

        let raw = std::fs::read(crate::project_file::path(&workspace)).unwrap();
        let encoded: Value = serde_json::from_slice(&raw).unwrap();
        assert_eq!(encoded["graph"]["graph_hash"], json!(hash));
        assert!(!serde_json::to_string(&encoded["learning"])
            .unwrap()
            .contains("chain_of_thought"));
        let _ = std::fs::remove_dir_all(workspace);
    }
}
