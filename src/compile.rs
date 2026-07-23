//! Selected-harness compilation into `fractal.execution_graph.v1`.

use std::collections::BTreeSet;

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Map, Value};

use crate::harness::HarnessSelection;

const CODE_HARNESS_FIXTURE: &str = include_str!(
    "../../../FractalRuntime/contracts/v1/fixtures/fractal-compiled-harness-v1-python-repair.json"
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

/// Heuristic: does the goal ask to create something new (vs. repair existing
/// code)? Greenfield verbs with no repair signal route to the build harness.
fn goal_is_greenfield_build(goal: &str) -> bool {
    let goal = goal.to_ascii_lowercase();
    const REPAIR: [&str; 6] = ["fix", "repair", "bug", "regression", "debug", "failing"];
    if REPAIR.iter().any(|word| goal.contains(word)) {
        return false;
    }
    const BUILD: [&str; 6] = ["build", "create", "write", "implement", "make", "generate"];
    BUILD.iter().any(|word| goal.contains(word))
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
        "Plan the build for: {goal}. Write INTERFACE.md stating the single module filename (e.g. solution.py), the public function name(s) and signatures, and 3-5 behaviors/edge cases it must satisfy: {criteria}. Keep it short and concrete so two agents can build the code and the tests in parallel from it."
    );
    let implement = format!(
        "Read INTERFACE.md and implement the module file exactly as specified for: {goal}. Self-contained, no network. Do not write tests — another agent is doing that in parallel."
    );
    let author_tests = format!(
        "Read INTERFACE.md and write a Python `unittest` test file (test_<module>.py) that imports the module and tests every behavior/edge case listed for: {goal}. Do not write the implementation — another agent is doing that in parallel."
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
                "budget": {"timeout_ms": 60_000}
            },
            {
                "id": "implement",
                "capability": "code.generate",
                "memory_scopes": ["work:goal", "workspace:root"],
                "preconditions": ["plan_ready"],
                "produced_state": ["implementation_ready"],
                "instruction": implement,
                "budget": {"timeout_ms": 180_000}
            },
            {
                "id": "author_tests",
                "capability": "code.generate",
                "memory_scopes": ["work:goal", "workspace:root"],
                "preconditions": ["plan_ready"],
                "produced_state": ["tests_ready"],
                "instruction": author_tests,
                "budget": {"timeout_ms": 180_000}
            },
            {
                "id": "review",
                "capability": "code.edit",
                "memory_scopes": ["work:goal", "workspace:root"],
                "preconditions": ["implementation_ready", "tests_ready"],
                "produced_state": ["reviewed"],
                "instruction": review,
                "budget": {"timeout_ms": 180_000}
            },
            {
                "id": "acceptance",
                "capability": "python.tests.execute",
                "memory_scopes": ["work:goal", "workspace:root", "acceptance:spec"],
                "preconditions": ["reviewed"],
                "produced_state": ["acceptance_passed"],
                "instruction": acceptance,
                "budget": {"timeout_ms": 120_000}
            },
            {
                "id": "complete",
                "capability": "control.complete",
                "memory_scopes": ["work:goal"],
                "preconditions": ["acceptance_passed"],
                "produced_state": ["outcome_verified"],
                "instruction": complete,
                "budget": {"timeout_ms": 5_000}
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
                "budget": {"timeout_ms": 30_000}
            },
            {
                "id": "implement",
                "capability": "code.generate",
                "memory_scopes": ["work:goal"],
                "preconditions": ["analysis_complete"],
                "produced_state": ["implementation_complete"],
                "budget": {"timeout_ms": 120_000}
            },
            {
                "id": "verify",
                "capability": "result.verify",
                "memory_scopes": ["work:goal"],
                "preconditions": ["implementation_complete"],
                "produced_state": ["result_verified"],
                "budget": {"timeout_ms": 60_000}
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
    let registry = build_registry(&harness);
    let mut authorized_work = work.clone();

    // NL work describes intent-level requirements. Compilation additionally
    // authorizes the concrete selected harness's capabilities and memory scopes
    // on this copy, leaving the submitted FractalWork object unchanged.
    augment_work_authorizations(&mut authorized_work, &harness)?;

    fractal_harnessc::compile(&authorized_work, &harness, &registry, target)
        .map_err(|error| anyhow!("fractal-harnessc compile failed: {error}"))
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

    use super::compile_graph;
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
    }

    #[test]
    fn greenfield_build_uses_task_faithful_acceptance_harness() {
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
    }

    #[test]
    fn repair_goal_keeps_python_repair_fixture() {
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
}
