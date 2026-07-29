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
    annotate_execution_flow(&mut graph, harness)?;
    Ok(graph)
}

fn annotate_execution_flow(graph: &mut Value, harness: &Value) -> Result<()> {
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
    for edge in &graph_edges {
        if edge.get("condition").and_then(Value::as_str) == Some("failure") {
            continue;
        }
        if let (Some(from), Some(to)) = (
            edge.get("from").and_then(Value::as_str),
            edge.get("to").and_then(Value::as_str),
        ) {
            if let Some(values) = predecessors.get_mut(to) {
                values.push(from.to_owned());
            }
        }
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
        annotate_execution_flow(&mut graph, &harness).unwrap();
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
}
