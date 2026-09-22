//! Opt-in source capture for the INT-165 managed-learning diagnostic.
//!
//! The normal test suite never runs the cohort. The ignored test creates three
//! canonical Rust projects from frozen source-only cases, then launches the
//! existing managed coordinator child for each project. No model is configured
//! or started by this workflow.

#![cfg(unix)]

use serde_json::{json, Value};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const MASTER: &str = "/Users/jamesstar/fractalmaster";
const CHILD_TEST: &str =
    "node_intelligence_workflow_tests::node_intelligence_learning_workflow_tests::learning_cohort_coordinator_child";
const CASES_SCRIPT: &str = r#"
import json, pathlib, sys
sys.path.insert(0, str(pathlib.Path(sys.argv[1]) / 'runs/jev-network'))
from managed_learning_cases import cases, manifest
from intelligence_graph.canonical import sha256_bytes
generator = pathlib.Path(sys.argv[1]) / 'runs/jev-network/managed_learning_cases.py'
print(json.dumps({'manifest': manifest(), 'cases': cases(),
                  'generator_ref': sha256_bytes(generator.read_bytes())},
                 sort_keys=True))
"#;
const FIXTURE_SCRIPT: &str = r#"
import json, pathlib, sys
sys.path.insert(0, str(pathlib.Path(sys.argv[1]) / 'runs/jev-network'))
from learning_workflow_fixture import prepare_cases
request = json.load(sys.stdin)
result = prepare_cases(
    pathlib.Path(request['workspace']), request['graph'], request['project_id'],
    request['case_document'],
    host_policy_path=pathlib.Path(request['host_policy_path']),
    feedback_policy_path=pathlib.Path(request['feedback_policy_path']),
    rollout_policy_path=pathlib.Path(request['rollout_policy_path']))
print(json.dumps(result, sort_keys=True))
"#;
const AUDIT_SCRIPT: &str = r#"
import dataclasses, json, pathlib, sqlite3, sys
from intelligence_graph.node_feedback_store import FeedbackStore

projects = json.load(sys.stdin)
results = []
all_actions = []
all_events = []
for item in projects:
    workspace = pathlib.Path(item['workspace'])
    document = json.loads((workspace / '.fractal/project.fractal').read_text())
    nodes = document['graph']['nodes']
    assignments = document['execution']['assignments']
    completed = [n['id'] for n in nodes
                 if assignments.get(n['id'], {}).get('state') == 'completed']
    if len(completed) != len(nodes):
        raise RuntimeError('canonical_tasks_incomplete')
    config = json.loads((workspace / '.fractal/node-intelligence.json').read_text())
    for case_id in item['case_ids']:
        runtime = config['tasks'][case_id + '-analysis']['runtime']
        if runtime.get('local_backend') is not None:
            raise RuntimeError('unexpected_local_model_configuration')
        if 'network_resolver_config' in runtime:
            raise RuntimeError('cohort_must_keep_frozen_network_pins')

    database = workspace / '.fractal/node-actions/actions.sqlite3'
    connection = sqlite3.connect(database)
    connection.row_factory = sqlite3.Row
    actions = [dict(row) for row in connection.execute(
        'SELECT action_id,adapter_id,attempt_ref,state,output_refs_json,verified_outcome '
        'FROM actions ORDER BY adapter_id,action_id')]
    connection.close()
    if any(row['state'] != 'complete' for row in actions):
        raise RuntimeError('action_not_complete')
    outputs_by_action = {}
    for row in actions:
        try:
            refs = json.loads(row['output_refs_json'])
        except (TypeError, ValueError):
            raise RuntimeError('action_output_invalid') from None
        if not isinstance(refs, list) or not refs or any(
                not isinstance(ref, str) or not ref.startswith('sha256:') for ref in refs):
            raise RuntimeError('action_output_missing')
        outputs_by_action[row['action_id']] = refs
    output_refs = [ref for refs in outputs_by_action.values() for ref in refs]
    if len(output_refs) != len(set(output_refs)):
        raise RuntimeError('duplicate_output_artifact')
    expected_action_count = len(item['case_ids']) * 3
    expected_output_count = len(item['case_ids']) * 5
    if len(actions) != expected_action_count or len(output_refs) != expected_output_count:
        raise RuntimeError('action_output_coverage')
    adapter_counts = {name: sum(row['adapter_id'] == name for row in actions)
                      for name in ('deterministic_measurement_intake',
                                   'deterministic_measurement',
                                   'deterministic_measurement_checker')}
    if adapter_counts != {name: len(item['case_ids']) for name in adapter_counts}:
        raise RuntimeError('action_adapter_coverage')
    checker_rows = [row for row in actions
                    if row['adapter_id'] == 'deterministic_measurement_checker']
    passed_checks = sum(row['verified_outcome'] == 1 for row in checker_rows)
    failed_checks = sum(row['verified_outcome'] == 0 for row in checker_rows)
    unknown_checks = sum(row['verified_outcome'] is None for row in checker_rows)
    if (len(checker_rows) != len(item['case_ids'])
            or passed_checks != len(item['case_ids'])
            or failed_checks != 0 or unknown_checks != 0):
        raise RuntimeError('checker_outcome_coverage')
    policy = json.loads(pathlib.Path(item['feedback_policy_path']).read_text())
    feed = FeedbackStore(workspace / policy['feedback_database_path'],
                         project_id=item['project_id'])
    events = [dataclasses.asdict(event)
              for event in feed.read_feedback(after_seq=0, limit=64).events]
    if any(event['kind'] != 'independently_verified' for event in events):
        raise RuntimeError('feedback_kind_mismatch')
    analyses = {row['attempt_ref'] for row in actions
                if row['adapter_id'] == 'deterministic_measurement'}
    if {event['attempt_ref'] for event in events} != analyses:
        raise RuntimeError('feedback_attempt_binding')
    if len({row['action_id'] for row in actions}) != len(actions):
        raise RuntimeError('duplicate_action_id')
    results.append({
        'split': item['split'], 'project_id': item['project_id'],
        'graph_hash': document['graph']['graph_hash'],
        'canonical_tasks_completed': len(completed),
        'action_rows': len(actions), 'feedback_events': len(events),
        'output_artifacts': len(output_refs),
        'checker_passed': passed_checks, 'checker_failed': failed_checks,
        'checker_unknown': unknown_checks,
        'attempt_refs': sorted(analyses), 'action_ids': sorted(row['action_id'] for row in actions),
        'event_refs': sorted(event['event_ref'] for event in events),
    })
    all_actions.extend(actions)
    all_events.extend(events)
print(json.dumps({
    'projects': results,
    'canonical_tasks_completed': sum(row['canonical_tasks_completed'] for row in results),
    'action_rows': len(all_actions),
    'output_artifacts': sum(row['output_artifacts'] for row in results),
    'feedback_events': len(all_events),
    'checker_passed': sum(row['checker_passed'] for row in results),
    'checker_failed': sum(row['checker_failed'] for row in results),
    'checker_unknown': sum(row['checker_unknown'] for row in results),
    'provider_calls': 0,
    'local_model_calls': 0,
}, sort_keys=True))
"#;

struct SplitProject {
    split: String,
    root: PathBuf,
    workspace: PathBuf,
    project_id: String,
    host_policy: PathBuf,
    feedback_policy: PathBuf,
    rollout_policy: PathBuf,
    case_ids: Vec<String>,
    fixture: Value,
}

fn python_json(script: &str, input: Option<&Value>) -> Value {
    let mut command = Command::new("python3");
    command
        .arg("-c")
        .arg(script)
        .arg(MASTER)
        .env("PYTHONPATH", MASTER)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if input.is_some() {
        command.stdin(Stdio::piped());
    }
    let mut child = command
        .spawn()
        .expect("start bounded Python fixture helper");
    if let Some(input) = input {
        child
            .stdin
            .take()
            .expect("fixture stdin")
            .write_all(&fractal_contracts::canonical_json(input).expect("canonical fixture JSON"))
            .expect("write fixture request");
    }
    let output = child.wait_with_output().expect("wait for fixture helper");
    assert!(
        output.status.success(),
        "fixture helper failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("fixture helper JSON")
}

fn graph_for_split(split: &str, case_document: &Value) -> Value {
    let cases = case_document["cases"].as_array().expect("case rows");
    let mut nodes = Vec::new();
    let mut edges = Vec::new();
    for case in cases {
        assert_eq!(case["split"], split);
        let case_id = case["case_id"].as_str().expect("case id");
        for (role, capability) in [
            ("intake", "intelligence.measurement.intake"),
            ("analysis", "intelligence.measurement.analyze"),
            ("check", "intelligence.measurement.check"),
        ] {
            let id = format!("{case_id}-{role}");
            nodes.push(json!({
                "id": id,
                "capability": capability,
                "title": format!("{case_id} {role}"),
                "instruction": "Run the configured deterministic measurement adapter.",
                "hard_limits": {"max_calls": 2, "max_elapsed_ms": 30000, "max_retries": 0}
            }));
        }
        edges.push(json!({"from":format!("{case_id}-intake"),"to":format!("{case_id}-analysis")}));
        edges.push(json!({"from":format!("{case_id}-intake"),"to":format!("{case_id}-check")}));
        edges.push(json!({"from":format!("{case_id}-analysis"),"to":format!("{case_id}-check")}));
    }
    if split == "train" {
        // The feed's two frozen pages are case00..03 and case04..07. Make the
        // second batch wait for all first-batch checker publications, even if
        // the coordinator schedules independent cases concurrently.
        assert_eq!(cases.len(), 8);
        for next in &cases[4..8] {
            let next_id = next["case_id"].as_str().unwrap();
            for prior in &cases[0..4] {
                let prior_id = prior["case_id"].as_str().unwrap();
                edges.push(
                    json!({"from":format!("{prior_id}-check"),"to":format!("{next_id}-intake")}),
                );
            }
        }
    }
    let mut graph = json!({
        "schema":"fractal.execution_graph.v1",
        "graph_id":format!("fg_managed_learning_{split}_v1"),
        "nodes":nodes,
        "edges":edges
    });
    graph["graph_hash"] =
        json!(fractal_contracts::canonical_sha256(&graph).expect("canonical graph hash"));
    graph
}

fn wait_bounded(child: &mut Child, stdout: &Path) -> ExitStatus {
    let deadline = Instant::now() + Duration::from_secs(300);
    loop {
        if let Some(status) = child.try_wait().expect("poll coordinator") {
            return status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!(
                "cohort coordinator exceeded 300 seconds; see {}",
                stdout.display()
            );
        }
        thread::sleep(Duration::from_millis(50));
    }
}

fn run_project(project: &SplitProject, fake_bin: &Path, home: &Path) {
    let stdout = project.root.join("coordinator.stdout");
    let stderr = project.root.join("coordinator.stderr");
    let path = std::env::join_paths(std::iter::once(fake_bin.to_path_buf()).chain(
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()),
    ))
    .expect("coordinator PATH");
    let mut child = Command::new(std::env::current_exe().expect("test executable"))
        .args([
            "--ignored",
            "--exact",
            CHILD_TEST,
            "--nocapture",
            "--test-threads=1",
        ])
        .env("FRACTAL_WORKFLOW_TEST_ROOT", &project.root)
        .env("FRACTAL_WORKFLOW_TEST_WORKSPACE", &project.workspace)
        .env("FRACTAL_WORKFLOW_TEST_RESUME", "0")
        .env("FRACTAL_WORKFLOW_TEST_PHASE", "cohort")
        .env(
            "FRACTAL_WORKFLOW_TEST_PROVIDER_MARKER",
            project.root.join("unexpected-provider.json"),
        )
        .env("FRACTAL_HOME", home)
        .env("FRACTAL_OFFLINE", "1")
        .env("FRACTAL_MIDRUN", "0")
        .env("FRACTAL_AGENT_TIMEOUT_MS", "30000")
        .env("FRACTAL_NODE_INTELLIGENCE_POLICY", &project.host_policy)
        .env("FRACTAL_NODE_FEEDBACK_POLICY", &project.feedback_policy)
        .env("PYTHONPATH", MASTER)
        .env("PATH", path)
        .env_remove("FRACTAL_NODE_NETWORK_RESOLVER")
        .env_remove("FRACTAL_NODE_INTELLIGENCE_TEST_CRASH_AFTER_EFFECT")
        .current_dir(&project.workspace)
        .stdout(Stdio::from(
            File::create(&stdout).expect("coordinator stdout"),
        ))
        .stderr(Stdio::from(
            File::create(&stderr).expect("coordinator stderr"),
        ))
        .spawn()
        .expect("launch canonical managed coordinator test child");
    let status = wait_bounded(&mut child, &stdout);
    assert!(
        status.success(),
        "{} split coordinator failed; stdout={} stderr={}",
        project.split,
        fs::read_to_string(stdout).unwrap_or_default(),
        fs::read_to_string(stderr).unwrap_or_default()
    );
    let outcome_path = project.root.join("cohort.outcome.json");
    let outcome: Value =
        serde_json::from_slice(&fs::read(&outcome_path).expect("coordinator outcome"))
            .expect("coordinator outcome JSON");
    assert!(outcome["failed_node"].is_null(), "{outcome}");
}

#[test]
fn training_graph_has_frozen_batch_barrier() {
    let cases = json!({"schema":"fractal.node.learning_workflow.cases.v1","cases":[
        {"case_id":"case00","split":"train"},{"case_id":"case01","split":"train"},
        {"case_id":"case02","split":"train"},{"case_id":"case03","split":"train"},
        {"case_id":"case04","split":"train"},{"case_id":"case05","split":"train"},
        {"case_id":"case06","split":"train"},{"case_id":"case07","split":"train"}
    ]});
    let graph = graph_for_split("train", &cases);
    assert_eq!(graph["nodes"].as_array().unwrap().len(), 24);
    assert_eq!(graph["edges"].as_array().unwrap().len(), 3 * 8 + 4 * 4);
    for next in 4..8 {
        for prior in 0..4 {
            assert!(graph["edges"].as_array().unwrap().contains(&json!({
                "from":format!("case{prior:02}-check"),
                "to":format!("case{next:02}-intake")
            })));
        }
    }
}

#[test]
fn managed_learning_split_projects_have_distinct_canonical_ids() {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock after epoch")
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "fractal-managed-learning-project-ids-{}-{stamp}",
        std::process::id()
    ));
    fs::create_dir(&root).expect("exclusive canonical-project test root");
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();

    let mut project_ids = Vec::new();
    for split in ["train", "validation", "test"] {
        let workspace = root.join(format!("jev-managed-learning-{split}-v1"));
        fs::create_dir(&workspace).expect("exclusive split workspace");
        let case_count = if split == "train" { 8 } else { 1 };
        let cases = json!({
            "schema":"fractal.node.learning_workflow.cases.v1",
            "cases":(0..case_count).map(|index| json!({
                "case_id":format!("{split}-case{index:02}"),"split":split
            })).collect::<Vec<_>>()
        });
        let graph = graph_for_split(split, &cases);
        crate::project_file::persist(&workspace, &graph, "Managed Learning Split")
            .expect("persist split through canonical project API");
        let document = crate::project_file::load(&workspace).expect("load canonical project");
        assert_eq!(document.graph["graph_hash"], graph["graph_hash"]);
        project_ids.push(document.project.slug);
    }

    assert_eq!(project_ids.len(), 3);
    assert_eq!(
        project_ids
            .iter()
            .collect::<std::collections::HashSet<_>>()
            .len(),
        3,
        "split workspaces must not collapse to one canonical project ID"
    );
    assert_eq!(
        project_ids,
        [
            "jev-managed-learning-train-v1",
            "jev-managed-learning-validation-v1",
            "jev-managed-learning-test-v1"
        ]
    );
    fs::remove_dir_all(root).expect("remove canonical-project test root");
}

#[test]
fn managed_learning_three_split_fixture_preflight_roundtrips_without_actions() {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock after epoch")
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "fractal-managed-learning-preflight-{}-{stamp}",
        std::process::id()
    ));
    fs::create_dir(&root).expect("exclusive fixture preflight root");
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
    let root = fs::canonicalize(root).expect("canonical fixture preflight root");
    let policies = root.join("policies");
    fs::create_dir(&policies).expect("private fixture policy directory");
    fs::set_permissions(&policies, fs::Permissions::from_mode(0o700)).unwrap();

    let source_snapshot = python_json(CASES_SCRIPT, None);
    let source_cases = source_snapshot["cases"]["cases"]
        .as_array()
        .expect("frozen source cases");
    let mut project_ids = std::collections::BTreeSet::new();
    let mut network_refs = std::collections::BTreeSet::new();
    let mut policy_refs = std::collections::BTreeSet::new();
    let mut owner_policy_paths = std::collections::BTreeSet::new();
    let mut feedback_policy_paths = std::collections::BTreeSet::new();
    let mut rollout_policy_paths = std::collections::BTreeSet::new();

    for split in ["train", "validation", "test"] {
        let case_document = json!({
            "schema":source_snapshot["cases"]["schema"],
            "cases":source_cases.iter().filter(|case| case["split"] == split)
                .cloned().collect::<Vec<_>>()
        });
        let cases = case_document["cases"].as_array().unwrap();
        let expected_count = source_snapshot["manifest"]["split_counts"][split]
            .as_u64()
            .expect("frozen split count") as usize;
        assert_eq!(cases.len(), expected_count, "{split} source cases");
        let case_ids = cases
            .iter()
            .map(|case| case["case_id"].as_str().expect("case ID"))
            .collect::<Vec<_>>();
        let graph = graph_for_split(split, &case_document);
        let project_root = root.join(split);
        fs::create_dir(&project_root).expect("exclusive preflight split root");
        fs::set_permissions(&project_root, fs::Permissions::from_mode(0o700)).unwrap();
        let workspace = project_root.join(format!("jev-managed-learning-{split}-v1"));
        fs::create_dir(&workspace).expect("exclusive canonical split workspace");
        crate::project_file::persist(
            &workspace,
            &graph,
            source_snapshot["manifest"]["project_titles"][split]
                .as_str()
                .unwrap(),
        )
        .expect("persist via canonical Rust project API");
        let document = crate::project_file::load(&workspace).expect("canonical project roundtrip");
        let slug = format!("jev-managed-learning-{split}-v1");
        assert_eq!(document.project.slug, slug);
        assert_eq!(document.graph["graph_hash"], graph["graph_hash"]);
        let project_id = format!("fractal:project:{slug}");
        project_ids.insert(project_id.clone());

        let host_policy = policies.join(format!("{split}-host-policy.json"));
        let feedback_policy = policies.join(format!("{split}-feedback-policy.json"));
        let rollout_policy = policies.join(format!("{split}-rollout-policy.json"));
        owner_policy_paths.insert(host_policy.clone());
        feedback_policy_paths.insert(feedback_policy.clone());
        rollout_policy_paths.insert(rollout_policy.clone());
        let fixture_request = json!({
            "workspace":workspace,
            "graph":document.graph,
            "project_id":document.project.slug,
            "case_document":case_document,
            "host_policy_path":host_policy,
            "feedback_policy_path":feedback_policy,
            "rollout_policy_path":rollout_policy
        });
        let fixture = python_json(FIXTURE_SCRIPT, Some(&fixture_request));
        assert_eq!(fixture["project_id"], project_id);
        assert_eq!(fixture["graph_id"], graph["graph_id"]);
        assert_eq!(fixture["graph_hash"], graph["graph_hash"]);
        assert_eq!(fixture["split"], split);
        assert_eq!(
            fixture["case_ids"].as_array().unwrap().len(),
            expected_count
        );
        assert_eq!(fixture["source_count"], (expected_count * 3) as u64);
        assert_eq!(fixture["synthetic"], true);
        assert_eq!(fixture["training_eligible"], false);
        assert_eq!(fixture["provider_calls"], 0);
        assert_eq!(fixture["model_calls"], 0);
        network_refs.insert(fixture["network_ref"].as_str().unwrap().to_owned());
        policy_refs.insert(fixture["rollout_policy_ref"].as_str().unwrap().to_owned());

        let task_refs = fixture["task_refs"].as_object().expect("task pins");
        assert_eq!(task_refs.len(), expected_count * 3);
        for case_id in &case_ids {
            for role in ["intake", "analysis", "check"] {
                let task_id = format!("{case_id}-{role}");
                let task = task_refs.get(&task_id).expect("fixture task pin");
                assert_eq!(task["network_ref"], fixture["network_ref"]);
                assert_eq!(task["policy_ref"], fixture["rollout_policy_ref"]);
            }
        }
        assert_eq!(fixture["host_policy_path"], host_policy.to_str().unwrap());
        assert_eq!(
            fixture["feedback_policy_path"],
            feedback_policy.to_str().unwrap()
        );
        assert_eq!(
            fixture["rollout_policy_path"],
            rollout_policy.to_str().unwrap()
        );
        for path in [&host_policy, &feedback_policy, &rollout_policy] {
            let metadata = fs::metadata(path).expect("fixture owner policy exists");
            assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
        }
        let host_policy_body: Value =
            serde_json::from_slice(&fs::read(&host_policy).unwrap()).expect("host policy JSON");
        assert_eq!(host_policy_body["project_id"], project_id);
        assert_eq!(host_policy_body["graph_hash"], graph["graph_hash"]);
        let config_path = PathBuf::from(fixture["config_path"].as_str().unwrap());
        assert_eq!(
            config_path,
            workspace.join(".fractal/node-intelligence.json")
        );
        assert_eq!(
            fs::metadata(&config_path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let config: Value =
            serde_json::from_slice(&fs::read(&config_path).unwrap()).expect("runtime config JSON");
        assert_eq!(config["schema"], "fractal.node_intelligence.config.v1");
        assert_eq!(config["enabled"], true);
        assert_eq!(
            config["tasks"].as_object().unwrap().len(),
            expected_count * 3
        );
        for case_id in &case_ids {
            for role in ["intake", "analysis", "check"] {
                let task_id = format!("{case_id}-{role}");
                let configured = &config["tasks"][&task_id];
                let fixture_pins = &task_refs[&task_id];
                assert_eq!(configured["capability_id"], fixture_pins["capability_id"]);
                assert_eq!(configured["network_ref"], fixture_pins["network_ref"]);
                assert_eq!(configured["node_ref"], fixture_pins["node_ref"]);
                assert_eq!(configured["model_ref"], fixture_pins["model_ref"]);
                assert_eq!(configured["policy_ref"], fixture_pins["policy_ref"]);
                assert_eq!(
                    configured["capability_id"],
                    format!("fractal:capability:workflow-{role}")
                );
                assert_eq!(configured["runtime"][role]["enabled"], true);
                if role == "intake" {
                    assert_eq!(configured["input_refs"].as_array().unwrap().len(), 3);
                } else {
                    assert!(configured["input_refs"].as_array().unwrap().is_empty());
                }
            }
        }
        assert!(!workspace
            .join(".fractal/node-actions/actions.sqlite3")
            .exists());
    }

    assert_eq!(project_ids.len(), 3);
    assert_eq!(network_refs.len(), 3);
    assert_eq!(policy_refs.len(), 3);
    assert_eq!(owner_policy_paths.len(), 3);
    assert_eq!(feedback_policy_paths.len(), 3);
    assert_eq!(rollout_policy_paths.len(), 3);
    fs::remove_dir_all(root).expect("remove fixture preflight root");
}

#[test]
#[ignore = "one-run frozen source cohort capture; invoke only after root freezes protocol and binary/source fingerprints"]
fn generate_frozen_managed_learning_cohort() {
    let _lock = crate::graph_store::ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let artifact_root = PathBuf::from(
        std::env::var_os("FRACTAL_MANAGED_LEARNING_COHORT_ROOT")
            .expect("explicit cohort artifact storage directory"),
    );
    assert!(
        artifact_root.is_absolute(),
        "artifact root must be absolute"
    );
    let artifact_root = fs::canonicalize(artifact_root).expect("canonical artifact root");
    assert!(artifact_root.is_dir(), "artifact root must already exist");
    let protocol_path =
        PathBuf::from(MASTER).join("docs/jev-network/managed_learning_protocol_v1.json");
    let protocol: Value =
        serde_json::from_slice(&fs::read(&protocol_path).expect("frozen protocol"))
            .expect("frozen protocol JSON");
    let protocol_ref = protocol["protocol_ref"].as_str().expect("protocol ref");
    let mut protocol_body = protocol.clone();
    protocol_body
        .as_object_mut()
        .unwrap()
        .remove("protocol_ref");
    assert_eq!(
        fractal_contracts::canonical_sha256(&protocol_body).unwrap(),
        protocol_ref,
        "frozen protocol digest"
    );
    assert_eq!(protocol["task"], "INT-165");
    assert_eq!(protocol["budget"]["model_calls_during_workflow"], 0);
    assert_eq!(protocol["budget"]["max_hosted_calls"], 0);
    assert_eq!(protocol["budget"]["max_workflow_actions"], 48);
    assert_eq!(protocol["budget"]["max_workflow_cases"], 16);

    let source_snapshot = python_json(CASES_SCRIPT, None);
    assert_eq!(source_snapshot["manifest"], protocol["source_manifest"]);
    assert_eq!(
        source_snapshot["generator_ref"],
        protocol["source_generator_ref"]
    );
    assert_eq!(
        fractal_contracts::canonical_sha256(&source_snapshot["cases"]).unwrap(),
        protocol["source_cases_ref"]
    );
    assert_eq!(source_snapshot["manifest"]["real_verified_tasks"], 0);
    assert_eq!(
        source_snapshot["manifest"]["model_calls_during_workflow"],
        0
    );
    assert_eq!(source_snapshot["manifest"]["production_promotion"], false);

    let run_root = artifact_root.join("cohort-run");
    fs::create_dir(&run_root)
        .expect("exclusive cohort-run reservation; never overwrite prior evidence");
    fs::set_permissions(&run_root, fs::Permissions::from_mode(0o700)).unwrap();
    write_new_json(&run_root.join("source-snapshot.json"), &source_snapshot);
    write_new_json(&run_root.join("protocol.json"), &protocol);

    let policies = run_root.join("policies");
    fs::create_dir(&policies).expect("cohort policies directory");
    fs::set_permissions(&policies, fs::Permissions::from_mode(0o700)).unwrap();
    let mut projects = Vec::new();
    for split in ["train", "validation", "test"] {
        let case_document = json!({
            "schema":source_snapshot["cases"]["schema"],
            "cases":source_snapshot["cases"]["cases"].as_array().unwrap().iter()
                .filter(|case| case["split"] == split).cloned().collect::<Vec<_>>()
        });
        let cases = case_document["cases"].as_array().unwrap();
        let expected_count = source_snapshot["manifest"]["split_counts"][split]
            .as_u64()
            .expect("split count") as usize;
        assert_eq!(cases.len(), expected_count, "{split} cases");
        let case_ids: Vec<String> = cases
            .iter()
            .map(|case| case["case_id"].as_str().unwrap().to_owned())
            .collect();
        let graph = graph_for_split(split, &case_document);
        let project_root = run_root.join(split);
        fs::create_dir(&project_root).expect("exclusive split project directory");
        fs::set_permissions(&project_root, fs::Permissions::from_mode(0o700)).unwrap();
        let workspace = project_root.join(format!("jev-managed-learning-{split}-v1"));
        fs::create_dir(&workspace).expect("new canonical project workspace");
        crate::project_file::persist(
            &workspace,
            &graph,
            source_snapshot["manifest"]["project_titles"][split]
                .as_str()
                .unwrap(),
        )
        .expect("persist split graph via canonical Rust project API");
        let document = crate::project_file::load(&workspace).expect("load canonical project");
        assert_eq!(document.graph["graph_hash"], graph["graph_hash"]);
        let host_policy = policies.join(format!("{split}-host-policy.json"));
        let feedback_policy = policies.join(format!("{split}-feedback-policy.json"));
        let rollout_policy = policies.join(format!("{split}-rollout-policy.json"));
        let fixture_request = json!({
            "workspace":workspace,
            "graph":document.graph,
            "project_id":document.project.slug,
            "case_document":case_document,
            "host_policy_path":host_policy,
            "feedback_policy_path":feedback_policy,
            "rollout_policy_path":rollout_policy
        });
        let fixture = python_json(FIXTURE_SCRIPT, Some(&fixture_request));
        assert_eq!(fixture["synthetic"], true);
        assert_eq!(fixture["provider_calls"], 0);
        assert_eq!(fixture["model_calls"], 0);
        let expected_project_id = format!(
            "fractal:project:{}",
            fixture_request["project_id"]
                .as_str()
                .expect("canonical project slug")
        );
        assert_eq!(fixture["project_id"], expected_project_id);
        assert_eq!(fixture["graph_hash"], graph["graph_hash"]);
        assert_eq!(fixture["rollout_policy_ref"], fixture["policy_ref"]);
        assert_eq!(fixture["provider_calls"], 0);
        assert_eq!(fixture["model_calls"], 0);
        for task in fixture["task_refs"].as_object().unwrap().values() {
            assert_eq!(task["network_ref"], fixture["network_ref"]);
            assert_eq!(task["policy_ref"], fixture["rollout_policy_ref"]);
        }
        write_new_json(&project_root.join("fixture-receipt.json"), &fixture);
        write_new_json(&project_root.join("graph.json"), &graph);
        projects.push(SplitProject {
            split: split.to_owned(),
            root: project_root,
            workspace,
            project_id: fixture["project_id"].as_str().unwrap().to_owned(),
            host_policy,
            feedback_policy,
            rollout_policy,
            case_ids,
            fixture,
        });
    }

    let fake_bin = run_root.join("fake-bin");
    fs::create_dir(&fake_bin).expect("fake provider bin");
    fs::set_permissions(&fake_bin, fs::Permissions::from_mode(0o700)).unwrap();
    let fake_provider = fake_bin.join("codex");
    fs::write(
        &fake_provider,
        concat!(
            "#!/usr/bin/python3\n",
            "import os, pathlib\n",
            "pathlib.Path(os.environ['FRACTAL_WORKFLOW_TEST_PROVIDER_MARKER']).write_text('blocked provider launch')\n",
            "raise SystemExit(91)\n",
        ),
    )
    .expect("provider trap");
    fs::set_permissions(&fake_provider, fs::Permissions::from_mode(0o700)).unwrap();
    let home = run_root.join("fractal-home");
    fs::create_dir(&home).expect("private Fractal home");
    fs::set_permissions(&home, fs::Permissions::from_mode(0o700)).unwrap();
    for project in &projects {
        run_project(project, &fake_bin, &home);
        assert!(
            !project.root.join("unexpected-provider.json").exists(),
            "measurement workflow attempted a provider for {}",
            project.split
        );
    }

    let audit_input = json!(projects
        .iter()
        .map(|project| json!({
            "split":project.split,
            "workspace":project.workspace,
            "project_id":project.project_id,
            "feedback_policy_path":project.feedback_policy,
            "case_ids":project.case_ids,
        }))
        .collect::<Vec<_>>());
    let audit = python_json(AUDIT_SCRIPT, Some(&audit_input));
    assert_eq!(audit["canonical_tasks_completed"], 48, "{audit}");
    assert_eq!(audit["action_rows"], 48, "{audit}");
    assert_eq!(audit["output_artifacts"], 80, "{audit}");
    assert_eq!(audit["feedback_events"], 16, "{audit}");
    assert_eq!(audit["checker_passed"], 16, "{audit}");
    assert_eq!(audit["checker_failed"], 0, "{audit}");
    assert_eq!(audit["checker_unknown"], 0, "{audit}");
    assert_eq!(audit["provider_calls"], 0);
    assert_eq!(audit["local_model_calls"], 0);
    for project in &projects {
        let project_audit = audit["projects"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["split"] == project.split)
            .unwrap();
        assert_eq!(project_audit["project_id"], project.project_id);
        assert_eq!(
            project_audit["canonical_tasks_completed"],
            (project.case_ids.len() * 3) as u64
        );
        assert_eq!(
            project_audit["action_rows"],
            (project.case_ids.len() * 3) as u64
        );
        assert_eq!(
            project_audit["output_artifacts"],
            (project.case_ids.len() * 5) as u64
        );
        assert_eq!(
            project_audit["feedback_events"],
            project.case_ids.len() as u64
        );
        assert_eq!(
            project_audit["checker_passed"],
            project.case_ids.len() as u64
        );
        assert_eq!(project_audit["checker_failed"], 0);
        assert_eq!(project_audit["checker_unknown"], 0);
    }
    let report = json!({
        "schema":"fractal.node.managed_learning.cohort_capture.v1",
        "protocol_ref":protocol_ref,
        "source_manifest_ref":source_snapshot["manifest"]["manifest_ref"],
        "source_cases_ref":protocol["source_cases_ref"],
        "synthetic":true,
        "real_verified_tasks":0,
        "model_calls":0,
        "hosted_calls":0,
        "production_promotion":false,
        "projects":projects.iter().map(|project| json!({
            "split":project.split,
            "project_id":project.project_id,
            "workspace":project.workspace,
            "graph_hash":project.fixture["graph_hash"],
            "network_ref":project.fixture["network_ref"],
            "host_policy_path":project.host_policy,
            "host_policy_ref":project.fixture["host_policy_ref"],
            "feedback_policy_path":project.feedback_policy,
            "feedback_policy_ref":project.fixture["feedback_policy_ref"],
            "rollout_policy_ref":project.fixture["rollout_policy_ref"],
            "rollout_policy_path":project.rollout_policy,
        })).collect::<Vec<_>>(),
        "observed":audit,
        "provider_launch_marker_present":false,
    });
    write_new_json(&run_root.join("cohort-report.json"), &report);
}

#[test]
#[ignore = "invoked only as a child by generate_frozen_managed_learning_cohort"]
fn learning_cohort_coordinator_child() {
    let workspace = PathBuf::from(
        std::env::var_os("FRACTAL_WORKFLOW_TEST_WORKSPACE").expect("owned project workspace"),
    );
    let root = PathBuf::from(std::env::var_os("FRACTAL_WORKFLOW_TEST_ROOT").unwrap());
    let document = crate::project_file::load(&workspace).expect("load child project");
    let completed = std::collections::BTreeSet::new();
    let outcome = crate::execute::run_multi_agent(
        &document.graph,
        &workspace,
        &["codex".to_owned()],
        None,
        &completed,
    )
    .expect("run canonical managed scheduler");
    let serialized = json!({
        "failed_node":outcome.failed_node,
        "detail":outcome.detail,
        "runs":outcome.log.iter().map(|run| json!({
            "node":run.node,"ok":run.ok,"latency_ms":run.latency_ms
        })).collect::<Vec<_>>()
    });
    write_new_json(&root.join("cohort.outcome.json"), &serialized);
    assert!(serialized["failed_node"].is_null(), "{serialized}");
}

fn write_new_json(path: &Path, value: &Value) {
    let bytes = serde_json::to_vec_pretty(value).expect("serialize evidence JSON");
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .unwrap_or_else(|error| panic!("exclusive evidence write {}: {error}", path.display()));
    file.write_all(&bytes).expect("write evidence JSON");
    file.write_all(b"\n").expect("finish evidence JSON");
    file.sync_all().expect("sync evidence JSON");
}
