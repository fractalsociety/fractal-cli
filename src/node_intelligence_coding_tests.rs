//! Real managed-coding seam tests for the node-intelligence bridge.
//!
//! These tests launch only a temporary fake `codex` executable. The Python
//! bridge and canonical scheduler are real; no provider or credential is used.

#![cfg(unix)]

use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::chain::jev_receipt::UsageValue;
use crate::execute::run_multi_agent;

const FRACTALMASTER: &str = "/Users/jamesstar/fractalmaster";
const TASK_ID: &str = "coding";

struct TempTree(PathBuf);

impl TempTree {
    fn new(label: &str) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        // `ContractStore` rejects symlinks in every ancestor. macOS commonly
        // exposes the temp directory through `/var` or `/tmp` symlinks, so use
        // its canonical physical path for the synthetic project root.
        let temp_root = fs::canonicalize(std::env::temp_dir())
            .expect("canonicalize system temporary directory");
        let path = temp_root.join(format!(
            "fractal-node-coding-{label}-{}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system clock")
                .as_nanos()
        ));
        fs::create_dir_all(&path).expect("create private test root");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).expect("protect test root");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempTree {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct EnvGuard {
    key: &'static str,
    previous: Option<OsString>,
}

impl EnvGuard {
    fn set(key: &'static str, value: impl AsRef<OsStr>) -> Self {
        let previous = std::env::var_os(key);
        std::env::set_var(key, value);
        Self { key, previous }
    }

    fn remove(key: &'static str) -> Self {
        let previous = std::env::var_os(key);
        std::env::remove_var(key);
        Self { key, previous }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        if let Some(previous) = &self.previous {
            std::env::set_var(self.key, previous);
        } else {
            std::env::remove_var(self.key);
        }
    }
}

struct TestProject {
    workspace: PathBuf,
    graph: Value,
    config_path: PathBuf,
    policy_path: PathBuf,
}

fn execution_graph(required_gate: Option<&str>) -> Value {
    let mut node = json!({
        "id": TASK_ID,
        "capability": "code.generate",
        "title": "Exercise the managed coding bridge",
        "instruction": "Inspect the pinned local context and report that this synthetic worker ran.",
        "budget": {"timeout_ms": 5_000},
        "hard_limits": {"max_calls": 1, "max_elapsed_ms": 5_000, "max_retries": 0}
    });
    if let Some(gate) = required_gate {
        node["required_gates"] = json!([gate]);
    }
    let mut graph = json!({
        "schema": "fractal.execution_graph.v1",
        "graph_id": "fg_node_intelligence_coding",
        "goal": "Exercise the local managed coding integration with a fake CLI.",
        "nodes": [node],
        "edges": []
    });
    graph["graph_hash"] =
        Value::String(fractal_contracts::canonical_sha256(&graph).expect("hash graph"));
    graph
}

fn review_race_graph() -> Value {
    let mut graph = json!({
        "schema": "fractal.execution_graph.v1",
        "graph_id": "fg_node_intelligence_late_review_rejection",
        "goal": "Block a reviewed local analysis if owner review changes before preparation, while unrelated work proceeds.",
        "nodes": [
            {
                "id": "analysis",
                "capability": "intelligence.measurement.analyze",
                "title": "Analyze approved synthetic measurements",
                "instruction": "Summarize the fixture measurements locally.",
                "budget": {"timeout_ms": 5_000},
                "hard_limits": {"max_calls": 1, "max_elapsed_ms": 5_000, "max_retries": 0}
            },
            {
                "id": "ordinary",
                "capability": "code.generate",
                "title": "Run independent synthetic coding work",
                "instruction": "Report that this unrelated synthetic coding task ran.",
                "budget": {"timeout_ms": 5_000},
                "hard_limits": {"max_calls": 1, "max_elapsed_ms": 5_000, "max_retries": 0}
            }
        ],
        "edges": []
    });
    graph["graph_hash"] =
        Value::String(fractal_contracts::canonical_sha256(&graph).expect("hash graph"));
    graph
}

fn prepare_pending_review_project(root: &Path) -> TestProject {
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).expect("create workspace");
    fs::set_permissions(&workspace, fs::Permissions::from_mode(0o700)).expect("protect workspace");
    let graph = review_race_graph();
    crate::project_file::persist(&workspace, &graph, "Node review race test")
        .expect("persist canonical graph");
    let document = crate::project_file::load(&workspace).expect("load canonical graph");
    let config_path = workspace.join(".fractal/node-intelligence.json");
    let policy_path = root.join("node-intelligence-host-policy.json");

    let mut child = Command::new("python3")
        .arg(Path::new(FRACTALMASTER).join("runs/jev-network/runtime_fixture.py"))
        .args([
            "--workspace",
            workspace.to_str().expect("UTF-8 workspace"),
            "--project-id",
            &document.project.slug,
            "--task-id",
            "analysis",
            "--pending-review",
        ])
        .env("PYTHONPATH", FRACTALMASTER)
        .current_dir(&workspace)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start pending-review fixture");
    child
        .stdin
        .take()
        .expect("fixture stdin")
        .write_all(&fractal_contracts::canonical_json(&document.graph).expect("encode graph"))
        .expect("write fixture graph");
    let output = child
        .wait_with_output()
        .expect("wait for pending-review fixture");
    assert!(
        output.status.success(),
        "pending-review fixture failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let fixture: Value = serde_json::from_slice(&output.stdout).expect("decode fixture result");
    assert!(fixture["packet_ref"].as_str().is_some());

    let config: Value =
        serde_json::from_slice(&fs::read(&config_path).expect("read fixture config"))
            .expect("decode fixture config");
    let task = &config["tasks"]["analysis"];
    let policy = json!({
        "schema": "fractal.node_intelligence.host_policy.v1",
        "project_id": format!("fractal:project:{}", document.project.slug),
        "graph_hash": document.graph_hash,
        "tasks": {
            "analysis": {
                "authorization": task["runtime"]["authorization"],
                "authorized_approvers": task["authorized_approvers"],
                "qualified_reviewers": task["qualified_reviewers"],
                "memory": task["runtime"]["memory"]
            }
        }
    });
    write_private_json(&policy_path, &policy);

    let output = Command::new("python3")
        .arg(Path::new(FRACTALMASTER).join("runs/jev-network/runtime_fixture.py"))
        .args([
            "--workspace",
            workspace.to_str().expect("UTF-8 workspace"),
            "--project-id",
            &document.project.slug,
            "--task-id",
            "analysis",
            "--approve-review",
        ])
        .env("PYTHONPATH", FRACTALMASTER)
        .env("FRACTAL_NODE_INTELLIGENCE_POLICY", &policy_path)
        .current_dir(&workspace)
        .output()
        .expect("approve fixture packet through production review API");
    assert!(
        output.status.success(),
        "fixture approval failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    TestProject {
        workspace,
        graph: document.graph,
        config_path,
        policy_path,
    }
}

fn install_late_review_rejection_shim(root: &Path) {
    let bin = root.join("fake-bin");
    fs::create_dir_all(&bin).expect("create fake binary directory");
    let real_python = find_python3();
    let real_python_json = serde_json::to_string(&real_python.to_string_lossy())
        .expect("encode Python executable path");
    let shim_path = root.join("late-review-python-shim.py");
    let policy_name = "node-intelligence-host-policy.json";
    let marker_name = "late-review-rejection.json";
    let marker_name_json = serde_json::to_string(marker_name).expect("encode marker name");
    let policy_name_json = serde_json::to_string(policy_name).expect("encode policy name");
    let fractalmaster_json = serde_json::to_string(FRACTALMASTER).expect("encode source path");
    let script = format!(
        r#"import json, os, pathlib, subprocess, sys

REAL_PYTHON = {real_python}
payload = sys.stdin.buffer.read()
args = sys.argv[1:]
if args[:2] == ["-m", "intelligence_graph.node_runtime"]:
    try:
        request = json.loads(payload)
    except Exception:
        request = {{}}
    attempt = request.get("attempt", {{}}) if isinstance(request, dict) else {{}}
    if (request.get("operation", "prepare") == "prepare"
            and attempt.get("node_id") == "analysis"):
        workspace = pathlib.Path(request["workspace"])
        root = workspace.parent
        marker = root / {marker_name}
        if not marker.exists():
            config = json.loads((workspace / ".fractal/node-intelligence.json").read_text())
            task = config["tasks"]["analysis"]
            os.environ["FRACTAL_NODE_INTELLIGENCE_POLICY"] = str(root / {policy_name})
            sys.path.insert(0, {fractalmaster})
            from intelligence_graph.node_review import record_decision
            decision = record_decision(
                action="reject", workspace=str(workspace),
                contract_store=task["runtime"]["store_path"],
                workflow_store=task["runtime"]["workflow_path"],
                packet_ref=task["review_packet_refs"][0],
                actor_id="fractal:agent:fixture-reviewer",
                decision_id="fractal:review_decision:runtime-fixture-late-rejection",
                rationale="Synthetic integration test: revoke approval before preparation.",
            )
            marker.write_text(json.dumps({{
                "injected": True,
                "workflow_review": decision["review"]["workflow_review"],
                "attempt_ref": decision["attempt_ref"],
            }}, sort_keys=True))
completed = subprocess.run(
    [REAL_PYTHON, *args], input=payload, stdout=sys.stdout.buffer,
    stderr=sys.stderr.buffer, env=os.environ.copy(), check=False,
)
raise SystemExit(completed.returncode)
"#,
        real_python = real_python_json,
        marker_name = marker_name_json,
        policy_name = policy_name_json,
        fractalmaster = fractalmaster_json,
    );
    fs::write(&shim_path, script).expect("write Python shim");
    let launcher = format!(
        "#!/bin/sh\nexec {} {} \"$@\"\n",
        shell_quote(real_python.to_str().expect("UTF-8 Python path")),
        shell_quote(shim_path.to_str().expect("UTF-8 shim path")),
    );
    let executable = bin.join("python3");
    fs::write(&executable, launcher).expect("write python3 shim launcher");
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700))
        .expect("make python3 shim executable");
}

fn find_python3() -> PathBuf {
    let path = std::env::var_os("PATH").expect("test PATH");
    std::env::split_paths(&path)
        .map(|directory| directory.join("python3"))
        .find(|candidate| candidate.is_file())
        .and_then(|candidate| fs::canonicalize(candidate).ok())
        .expect("find real python3 before installing shim")
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn prepare_project(root: &Path, configured: bool, required_gate: Option<&str>) -> TestProject {
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).expect("create workspace");
    fs::set_permissions(&workspace, fs::Permissions::from_mode(0o700)).expect("protect workspace");
    let graph = execution_graph(required_gate);
    crate::project_file::persist(&workspace, &graph, "Node coding test")
        .expect("persist canonical graph");
    let document = crate::project_file::load(&workspace).expect("load canonical graph");
    let config_path = workspace.join(".fractal/node-intelligence.json");
    let policy_path = root.join("node-intelligence-host-policy.json");

    if configured {
        prepare_python_fixture(&workspace, &document.graph, &document.project.slug);
        let config_bytes = fs::read(&config_path).expect("read fixture config");
        let config: Value = serde_json::from_slice(&config_bytes).expect("decode fixture config");
        let task = &config["tasks"][TASK_ID];
        let mut policy_tasks = serde_json::Map::new();
        policy_tasks.insert(
            TASK_ID.to_owned(),
            json!({
                "authorization": task["runtime"]["authorization"],
                "authorized_approvers": task["authorized_approvers"],
                "qualified_reviewers": task["qualified_reviewers"]
            }),
        );
        let policy = json!({
            "schema": "fractal.node_intelligence.host_policy.v1",
            "project_id": format!("fractal:project:{}", document.project.slug),
            "graph_hash": document.graph_hash,
            "tasks": Value::Object(policy_tasks)
        });
        write_private_json(&policy_path, &policy);
    }

    TestProject {
        workspace,
        graph: document.graph,
        config_path,
        policy_path,
    }
}

fn prepare_python_fixture(workspace: &Path, graph: &Value, project_id: &str) {
    let mut child = Command::new("python3")
        .arg(Path::new(FRACTALMASTER).join("runs/jev-network/runtime_fixture.py"))
        .args([
            "--workspace",
            workspace.to_str().expect("UTF-8 workspace"),
            "--project-id",
            project_id,
            "--task-id",
            TASK_ID,
            "--coding-task",
        ])
        .env("PYTHONPATH", FRACTALMASTER)
        .current_dir(workspace)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start node-intelligence runtime fixture");
    child
        .stdin
        .take()
        .expect("fixture stdin")
        .write_all(&fractal_contracts::canonical_json(graph).expect("encode graph"))
        .expect("write fixture graph");
    let output = child.wait_with_output().expect("wait for runtime fixture");
    assert!(
        output.status.success(),
        "runtime fixture failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(workspace.join(".fractal/node-intelligence.json").is_file());
}

fn write_private_json(path: &Path, value: &Value) {
    fs::write(
        path,
        fractal_contracts::canonical_json(value).expect("encode private policy"),
    )
    .expect("write host policy");
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).expect("protect host policy");
}

fn install_fake_codex(root: &Path) -> (PathBuf, PathBuf) {
    let bin = root.join("fake-bin");
    fs::create_dir_all(&bin).expect("create fake binary directory");
    let executable = bin.join("codex");
    fs::write(
        &executable,
        r##"#!/bin/sh
{
  for arg do printf 'ARG_BEGIN\n%s\nARG_END\n' "$arg"; done
  printf 'ATTEMPT_REF=%s\n' "${FRACTAL_NODE_ATTEMPT_REF-}"
  printf 'NODE_ID=%s\n' "${FRACTAL_NODE_ID-}"
  printf 'CONTEXT_REF=%s\n' "${FRACTAL_NODE_CONTEXT_REF-}"
  printf 'CONTEXT_PATH=%s\n' "${FRACTAL_NODE_CONTEXT_PATH-}"
  if [ -n "${FRACTAL_NODE_CONTEXT_PATH-}" ] && [ -f "$FRACTAL_NODE_CONTEXT_PATH" ]; then
    printf 'CONTEXT_FILE_PRESENT=yes\n'
  else
    printf 'CONTEXT_FILE_PRESENT=no\n'
  fi
} > "$FRACTAL_TEST_CODEX_TRACE"
exit 0
"##,
    )
    .expect("write fake codex executable");
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700))
        .expect("make fake codex executable");
    (bin, executable)
}

fn test_environment(bin: &Path, trace: &Path, policy: Option<&Path>) -> Vec<EnvGuard> {
    let mut paths = vec![bin.to_path_buf()];
    if let Some(existing) = std::env::var_os("PATH") {
        paths.extend(std::env::split_paths(&existing));
    }
    let path = std::env::join_paths(paths).expect("join test PATH");
    let mut guards = vec![
        EnvGuard::set("PATH", path),
        EnvGuard::set("PYTHONPATH", FRACTALMASTER),
        EnvGuard::set("FRACTAL_TEST_CODEX_TRACE", trace),
        EnvGuard::set("FRACTAL_AGENT_TIMEOUT_MS", "10000"),
        EnvGuard::set("FRACTAL_OFFLINE", "1"),
        EnvGuard::remove("FRACTAL_ACTIVE_RUN_ID"),
        EnvGuard::remove("FRACTAL_NODE_ATTEMPT_REF"),
        EnvGuard::remove("FRACTAL_NODE_CONTEXT_REF"),
        EnvGuard::remove("FRACTAL_NODE_CONTEXT_PATH"),
    ];
    guards.push(match policy {
        Some(path) => EnvGuard::set("FRACTAL_NODE_INTELLIGENCE_POLICY", path),
        None => EnvGuard::remove("FRACTAL_NODE_INTELLIGENCE_POLICY"),
    });
    guards
}

fn run_graph(project: &TestProject) -> crate::execute::RunOutcome {
    run_multi_agent(
        &project.graph,
        &project.workspace,
        &["codex".to_owned()],
        None,
        &BTreeSet::new(),
    )
    .expect("run canonical test graph")
}

#[test]
fn configured_coding_node_receives_pinned_context_and_keeps_unknown_usage() {
    let _env_lock = crate::graph_store::ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let tree = TempTree::new("configured");
    let project = prepare_project(tree.path(), true, None);
    let trace_path = tree.path().join("codex-trace.txt");
    let (bin, _) = install_fake_codex(tree.path());
    let _environment = test_environment(&bin, &trace_path, Some(&project.policy_path));

    let outcome = run_graph(&project);
    let trace = fs::read_to_string(&trace_path)
        .unwrap_or_else(|error| format!("trace unavailable: {error}"));
    let assignment = crate::project_file::assignment(&project.workspace, TASK_ID)
        .expect("read canonical assignment");
    let log_summary = outcome
        .log
        .iter()
        .map(|entry| {
            entry.receipt.as_ref().map_or_else(
                || format!("ok={}; receipt=none", entry.ok),
                |receipt| {
                    format!(
                        "ok={}; process_status={}; exit={:?}; process_success={}; reasons={:?}; refs={:?}",
                        entry.ok,
                        receipt.process_status,
                        receipt.exit_status,
                        receipt.process_success,
                        receipt.reason_codes,
                        receipt.authorization_evidence_refs
                    )
                },
            )
        })
        .collect::<Vec<_>>();
    assert!(
        outcome.built,
        "managed coding outcome: {}; assignment={assignment:#?}; log={log_summary:?}; trace:\n{trace}",
        outcome.detail,
    );
    assert_eq!(outcome.log.len(), 1);
    let receipt = outcome.log[0]
        .receipt
        .as_ref()
        .expect("scheduler stores host route receipt");
    assert!(receipt.process_success);
    assert_eq!(receipt.process_status, "exited_success");
    assert_eq!(receipt.invocation.cli_family, "codex");
    assert_eq!(
        receipt.invocation.selected_model.as_deref(),
        Some("gpt-5.6-luna")
    );
    assert_eq!(receipt.invocation.selected_effort, None);
    assert_eq!(receipt.actual_route_provenance.provider, "unknown");
    assert_eq!(receipt.usage.input_tokens, UsageValue::Unknown);
    assert_eq!(receipt.usage.output_tokens, UsageValue::Unknown);
    assert_eq!(receipt.usage.cached_input_tokens, UsageValue::Unknown);
    assert_eq!(receipt.usage.cost_micros, UsageValue::Unknown);
    assert_eq!(receipt.usage.usage_source, "unavailable");
    assert_eq!(receipt.usage.provider_receipt_ref, None);
    assert_eq!(receipt.task_correctness, "unknown");
    assert_eq!(receipt.verification.verification_status, "not_applicable");

    assert!(
        trace.contains("ARG_BEGIN\ngpt-5.6-luna\nARG_END"),
        "{trace}"
    );
    let attempt_ref = trace_value(&trace, "ATTEMPT_REF");
    let context_ref = trace_value(&trace, "CONTEXT_REF");
    let context_path = trace_value(&trace, "CONTEXT_PATH");
    assert!(attempt_ref.starts_with("sha256:"));
    assert!(context_ref.starts_with("sha256:"));
    assert_eq!(trace_value(&trace, "CONTEXT_FILE_PRESENT"), "yes");
    assert!(trace.contains("Fractal pinned the authorized inputs"));
    assert!(trace.contains(&context_ref));
    let manifest: Value = serde_json::from_slice(
        &fs::read(project.workspace.join(context_path)).expect("read pinned context manifest"),
    )
    .expect("decode pinned context manifest");
    assert_eq!(manifest["schema"], "fractal.node_context.v1");
    assert_eq!(manifest["attempt_ref"], attempt_ref);

    let evidence_refs = receipt
        .authorization_evidence_refs
        .iter()
        .map(|evidence| evidence.reference.as_str())
        .collect::<BTreeSet<_>>();
    for expected in [
        format!("node-intelligence:attempt:{attempt_ref}"),
        format!("node-intelligence:context:{context_ref}"),
        format!(
            "node-intelligence:network:{}",
            fixture_task(&project)["network_ref"].as_str().unwrap()
        ),
        format!(
            "node-intelligence:model:{}",
            fixture_task(&project)["model_ref"].as_str().unwrap()
        ),
        format!(
            "node-intelligence:policy:{}",
            fixture_task(&project)["policy_ref"].as_str().unwrap()
        ),
        format!(
            "node-intelligence:input:{}",
            fixture_task(&project)["input_refs"][0].as_str().unwrap()
        ),
    ] {
        assert!(
            evidence_refs.contains(expected.as_str()),
            "missing {expected} in {evidence_refs:?}"
        );
    }
    assert!(evidence_refs
        .iter()
        .any(|value| value.starts_with("node-intelligence:host-policy:")));
}

#[test]
fn late_review_rejection_blocks_analysis_but_allows_unrelated_coding() {
    let _env_lock = crate::graph_store::ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let tree = TempTree::new("late-review-rejection");
    let project = prepare_pending_review_project(tree.path());
    let trace_path = tree.path().join("codex-trace.txt");
    let marker_path = tree.path().join("late-review-rejection.json");
    let actions_path = project
        .workspace
        .join(".fractal/node-actions/actions.sqlite3");
    let (bin, _) = install_fake_codex(tree.path());
    install_late_review_rejection_shim(tree.path());
    let _environment = test_environment(&bin, &trace_path, Some(&project.policy_path));

    let outcome = run_graph(&project);
    let marker: Value = serde_json::from_slice(&fs::read(&marker_path).unwrap_or_else(|error| {
        panic!(
            "late review rejection was not injected: {error}; outcome={}",
            outcome.detail
        )
    }))
    .expect("decode late review rejection evidence");
    assert_eq!(marker["injected"], true);
    assert_eq!(marker["workflow_review"], "conflict");
    assert!(marker["attempt_ref"].as_str().is_some());

    assert!(
        outcome.built,
        "independent coding should finish despite blocked review: {}",
        outcome.detail
    );
    assert_eq!(
        outcome.failed_node.as_deref(),
        Some("analysis"),
        "after unrelated work completes, the blocked analysis is the terminal scheduler result"
    );
    let analysis_runs = outcome
        .log
        .iter()
        .filter(|entry| entry.node == "analysis")
        .collect::<Vec<_>>();
    let ordinary_runs = outcome
        .log
        .iter()
        .filter(|entry| entry.node == "ordinary")
        .collect::<Vec<_>>();
    assert_eq!(
        analysis_runs.len(),
        1,
        "scheduler records one blocked analysis attempt"
    );
    assert!(!analysis_runs[0].ok, "rejected attempt must not complete");
    assert_eq!(
        ordinary_runs.len(),
        1,
        "unrelated coding node should execute once"
    );
    assert!(ordinary_runs[0].ok, "fake coding node should complete");

    let assignment = crate::project_file::assignment(&project.workspace, "analysis")
        .expect("read analysis assignment");
    assert!(
        assignment
            .as_ref()
            .and_then(|assignment| assignment.released_at.as_ref())
            .is_some(),
        "blocked analysis checkout must be released: {assignment:#?}"
    );
    assert!(
        !actions_path.exists(),
        "analysis action journal must not be created after pre-effect rejection"
    );
    let trace = fs::read_to_string(&trace_path).expect("read fake coding trace");
    assert_eq!(trace_value(&trace, "NODE_ID"), "ordinary");
    assert!(trace.contains("Report that this unrelated synthetic coding task ran."));
    assert!(!trace.contains("Summarize the fixture measurements locally."));
}

#[test]
fn unconfigured_coding_command_keeps_legacy_prompt_without_node_context() {
    let _env_lock = crate::graph_store::ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let tree = TempTree::new("unconfigured");
    let project = prepare_project(tree.path(), false, None);
    let trace_path = tree.path().join("codex-trace.txt");
    let (bin, _) = install_fake_codex(tree.path());
    let _environment = test_environment(&bin, &trace_path, None);

    let outcome = run_graph(&project);
    let trace = fs::read_to_string(&trace_path)
        .unwrap_or_else(|error| format!("trace unavailable: {error}"));
    let assignment = crate::project_file::assignment(&project.workspace, TASK_ID)
        .expect("read canonical assignment");
    assert!(
        outcome.built,
        "unconfigured coding outcome: {}; assignment={assignment:#?}; trace:\n{trace}",
        outcome.detail,
    );
    assert_eq!(outcome.log.len(), 1);
    let receipt = outcome.log[0]
        .receipt
        .as_ref()
        .expect("legacy scheduler route receipt");
    assert_eq!(receipt.invocation.cli_family, "codex");
    assert_eq!(
        receipt.invocation.selected_model.as_deref(),
        Some("gpt-5.6-luna")
    );
    assert_eq!(receipt.invocation.selected_effort, None);
    assert_eq!(receipt.usage.input_tokens, UsageValue::Unknown);
    assert_eq!(receipt.usage.output_tokens, UsageValue::Unknown);
    assert!(trace.contains("ARG_BEGIN\ngpt-5.6-luna\nARG_END"));
    assert_eq!(trace_value(&trace, "ATTEMPT_REF"), "");
    assert_eq!(trace_value(&trace, "CONTEXT_REF"), "");
    assert_eq!(trace_value(&trace, "CONTEXT_PATH"), "");
    assert_eq!(trace_value(&trace, "CONTEXT_FILE_PRESENT"), "no");
    assert!(!trace.contains("Fractal pinned the authorized inputs"));
    assert!(trace
        .contains("Inspect the pinned local context and report that this synthetic worker ran."));
    assert!(!project.config_path.exists());
}

#[test]
fn host_permission_mismatch_and_external_gate_stop_before_fake_worker() {
    let _env_lock = crate::graph_store::ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);

    let policy_tree = TempTree::new("permission-denied");
    let policy_project = prepare_project(&policy_tree.0, true, None);
    let mut config: Value = serde_json::from_slice(
        &fs::read(&policy_project.config_path).expect("read fixture config"),
    )
    .expect("decode fixture config");
    config["tasks"][TASK_ID]["runtime"]["authorization"]["current_sources"] = json!({});
    fs::write(
        &policy_project.config_path,
        fractal_contracts::canonical_json(&config).expect("encode altered config"),
    )
    .expect("alter task authorization for fail-closed test");
    let policy_trace = policy_tree.path().join("codex-trace.txt");
    let (policy_bin, _) = install_fake_codex(policy_tree.path());
    let _policy_environment = test_environment(
        &policy_bin,
        &policy_trace,
        Some(&policy_project.policy_path),
    );
    let policy_outcome = run_graph(&policy_project);
    assert!(!policy_outcome.built);
    assert!(
        !policy_trace.exists(),
        "worker launched with mismatched host permission"
    );
    drop(_policy_environment);

    let gate_tree = TempTree::new("gate-denied");
    let gate_project = prepare_project(&gate_tree.0, true, Some("security_review"));
    let gate_trace = gate_tree.path().join("codex-trace.txt");
    let (gate_bin, _) = install_fake_codex(gate_tree.path());
    let _gate_environment =
        test_environment(&gate_bin, &gate_trace, Some(&gate_project.policy_path));
    let gate_outcome = run_graph(&gate_project);
    assert!(!gate_outcome.built);
    assert!(
        !gate_trace.exists(),
        "worker launched without required external gate"
    );
}

fn fixture_task(project: &TestProject) -> Value {
    let config: Value =
        serde_json::from_slice(&fs::read(&project.config_path).expect("read fixture config"))
            .expect("decode fixture config");
    config["tasks"][TASK_ID].clone()
}

fn trace_value<'a>(trace: &'a str, key: &str) -> &'a str {
    trace
        .lines()
        .find_map(|line| line.strip_prefix(&format!("{key}=")))
        .expect("trace value")
}
