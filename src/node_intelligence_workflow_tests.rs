//! Real deterministic workflow and coordinator-restart tests for INT-165.
#![cfg(unix)]

use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const MASTER: &str = "/Users/jamesstar/fractalmaster";
const CHILD_TEST: &str = "node_intelligence_workflow_tests::workflow_coordinator_child";

#[path = "node_intelligence_network_workflow_tests.rs"]
mod node_intelligence_network_workflow_tests;

struct EnvGuard(&'static str, Option<OsString>);
impl EnvGuard {
    fn set(key: &'static str, value: &str) -> Self {
        let previous = std::env::var_os(key);
        std::env::set_var(key, value);
        Self(key, previous)
    }
}
impl Drop for EnvGuard {
    fn drop(&mut self) {
        match &self.1 {
            Some(value) => std::env::set_var(self.0, value),
            None => std::env::remove_var(self.0),
        }
    }
}

struct Project {
    root: PathBuf,
    workspace: PathBuf,
    policy: PathBuf,
    bin: PathBuf,
    preserve: bool,
}
impl Drop for Project {
    fn drop(&mut self) {
        if std::thread::panicking() {
            eprintln!("Preserved failed workflow fixture: {}", self.root.display());
        } else if !self.preserve {
            let _ = fs::remove_dir_all(&self.root);
        }
    }
}

impl Project {
    fn new(review: bool, ordinary: bool, feedback: bool) -> Self {
        Self::with_head(review, ordinary, feedback, None)
    }

    fn with_head(
        review: bool,
        ordinary: bool,
        feedback: bool,
        head_config: Option<&PathBuf>,
    ) -> Self {
        let storage = if head_config.is_some() {
            PathBuf::from(
                std::env::var_os("FRACTAL_WORKFLOW_TEST_ARTIFACT_ROOT")
                    .expect("explicit diagnostic artifact root"),
            )
        } else {
            std::env::temp_dir()
        };
        let root = fs::canonicalize(storage).unwrap().join(format!(
            "fractal-real-workflow-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        let workspace = root.join("project");
        let bin = root.join("bin");
        fs::create_dir_all(&workspace).unwrap();
        fs::create_dir_all(&bin).unwrap();
        let mut nodes = vec![];
        for (name, suffix) in [
            ("intake", "intake"),
            ("analysis", "analyze"),
            ("check", "check"),
        ] {
            nodes.push(json!({
                "id": name, "capability": format!("intelligence.measurement.{suffix}"),
                "title": format!("Real measurement {name}"),
                "instruction": format!("Run the pinned local measurement {name} adapter."),
                "hard_limits": {"max_calls": 2,
                    "max_elapsed_ms": if head_config.is_some() && name == "analysis" {60000} else {15000},
                    "max_retries": 2}
            }));
        }
        if ordinary {
            nodes.push(json!({"id":"ordinary", "capability":"code.generate",
                "title":"Independent fixture action", "instruction":"Write the local fixture marker.",
                "hard_limits":{"max_elapsed_ms":15000,"max_retries":0}}));
        }
        let mut graph = json!({"schema":"fractal.execution_graph.v1", "graph_id":"fg_real_measurement_workflow",
            "nodes": nodes, "edges":[{"from":"intake","to":"analysis"},
                {"from":"intake","to":"check"},{"from":"analysis","to":"check"}]});
        graph["graph_hash"] = json!(fractal_contracts::canonical_sha256(&graph).unwrap());
        crate::project_file::persist(&workspace, &graph, "Real Measurement Workflow Fixture")
            .unwrap();
        let document = crate::project_file::load(&workspace).unwrap();
        let policy = root.join("host-policy.json");
        let mut command = Command::new("python3");
        command
            .arg(format!("{MASTER}/runs/jev-network/workflow_fixture.py"))
            .arg("--workspace")
            .arg(&workspace)
            .arg("--project-id")
            .arg(&document.project.slug)
            .arg("--host-policy")
            .arg(&policy)
            .env("PYTHONPATH", MASTER)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if review {
            command.arg("--pending-review");
        }
        if feedback {
            command
                .arg("--feedback-policy")
                .arg(root.join("feedback-policy.json"));
        }
        if let Some(path) = head_config {
            command.arg("--analysis-head-config").arg(path);
        }
        let mut child = command.spawn().unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(&fractal_contracts::canonical_json(&document.graph).unwrap())
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "fixture: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        // Every possible Codex launch is intercepted, including accidental
        // routing of a measurement node to a provider. Only the explicitly
        // unrelated scheduling fixture is permitted to succeed.
        let fake = bin.join("codex");
        fs::write(&fake, r#"#!/usr/bin/python3
import json, os, pathlib, sys
root = pathlib.Path(os.environ['FRACTAL_WORKFLOW_TEST_ROOT'])
node = os.environ.get('FRACTAL_NODE_ID')
if node != 'ordinary':
    (root / 'unexpected-provider-launch.json').write_text(json.dumps({'node': node, 'args': sys.argv[1:]}))
    raise SystemExit(91)
document = json.loads(pathlib.Path('.fractal/project.fractal').read_text())
(root / 'ordinary.json').write_text(json.dumps({'analysis_attempt_count': document['learning']['nodes']['analysis']['attempt_count']}))
"#).unwrap();
        fs::set_permissions(&fake, fs::Permissions::from_mode(0o700)).unwrap();
        Self {
            root,
            workspace,
            policy,
            bin,
            preserve: head_config.is_some(),
        }
    }

    fn command(&self, phase: &str, resume: bool) -> Command {
        let mut command = Command::new(std::env::current_exe().unwrap());
        let path = std::env::join_paths(std::iter::once(self.bin.clone()).chain(
            std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()),
        ))
        .unwrap();
        command
            .args([
                "--ignored",
                "--exact",
                CHILD_TEST,
                "--nocapture",
                "--test-threads=1",
            ])
            .env("FRACTAL_WORKFLOW_TEST_ROOT", &self.root)
            .env("FRACTAL_WORKFLOW_TEST_WORKSPACE", &self.workspace)
            .env(
                "FRACTAL_WORKFLOW_TEST_RESUME",
                if resume { "1" } else { "0" },
            )
            .env("FRACTAL_WORKFLOW_TEST_PHASE", phase)
            .env("FRACTAL_HOME", self.root.join("fractal-home"))
            .env("FRACTAL_OFFLINE", "1")
            .env("FRACTAL_MIDRUN", "0")
            .env("FRACTAL_AGENT_TIMEOUT_MS", "15000")
            .env("FRACTAL_NODE_INTELLIGENCE_POLICY", &self.policy)
            .env("PYTHONPATH", MASTER)
            .env("PATH", path)
            .env_remove("FRACTAL_ACTIVE_RUN_ID")
            .env_remove("FRACTAL_NODE_ATTEMPT_REF")
            .env_remove("FRACTAL_NODE_CONTEXT_REF")
            .env_remove("FRACTAL_NODE_CONTEXT_PATH")
            .env_remove("FRACTAL_NODE_NETWORK_RESOLVER")
            .env_remove("FRACTAL_NODE_FEEDBACK_POLICY")
            .env_remove("FRACTAL_NODE_INTELLIGENCE_TEST_CRASH_AFTER_EFFECT")
            .current_dir(&self.workspace)
            .stdout(Stdio::from(
                fs::File::create(self.root.join(format!("{phase}.stdout"))).unwrap(),
            ))
            .stderr(Stdio::from(
                fs::File::create(self.root.join(format!("{phase}.stderr"))).unwrap(),
            ));
        let feedback_policy = self.root.join("feedback-policy.json");
        if feedback_policy.exists() {
            command.env("FRACTAL_NODE_FEEDBACK_POLICY", feedback_policy);
        }
        command
    }

    fn wait(&self, child: &mut Child, phase: &str) -> ExitStatus {
        let deadline = Instant::now() + Duration::from_secs(90);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                return status;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("coordinator timeout: {}", self.logs(phase));
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn logs(&self, phase: &str) -> String {
        format!(
            "{}\n{}",
            fs::read_to_string(self.root.join(format!("{phase}.stdout"))).unwrap_or_default(),
            fs::read_to_string(self.root.join(format!("{phase}.stderr"))).unwrap_or_default()
        )
    }

    fn python(&self, script: &str) -> Value {
        let output = Command::new("python3")
            .args(["-c", script])
            .arg(&self.workspace)
            .env("PYTHONPATH", MASTER)
            .env("FRACTAL_NODE_INTELLIGENCE_POLICY", &self.policy)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "inspection: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }

    fn actions(&self) -> Value {
        self.python(r#"import json, pathlib, sqlite3, sys
p=pathlib.Path(sys.argv[1])/'.fractal/node-actions/actions.sqlite3'
if not p.exists(): print('[]')
else:
 c=sqlite3.connect(p);c.row_factory=sqlite3.Row
 print(json.dumps([dict(r) for r in c.execute('SELECT action_id,adapter_id,attempt_ref,state,output_refs_json,verified_outcome FROM actions ORDER BY adapter_id')]))
"#)
    }

    fn feedback(&self) -> Value {
        self.python(
            r#"import dataclasses,json,pathlib,sys
from intelligence_graph.node_feedback_store import FeedbackStore
from intelligence_graph.node_runtime import ReceiptStore
w=pathlib.Path(sys.argv[1]);policy=json.loads((w.parent/'feedback-policy.json').read_text())
feed=FeedbackStore(w/policy['feedback_database_path'],project_id=policy['project_id'])
receipts=ReceiptStore(w,policy['receipt_store_path'])
values=[]
for event in feed.read_feedback(after_seq=0,limit=16).events:
 records=[receipts.get(ref) for ref in event.payload_refs if (receipts.root/(ref[7:]+'.json')).is_file()]
 provenance=next(record for record in records if record.get('schema')=='fractal.node.feedback.publisher_provenance.v1')
 values.append({'event':dataclasses.asdict(event),'provenance':provenance})
print(json.dumps(values))
"#,
        )
    }

    fn assert_complete(&self) {
        let document = crate::project_file::load(&self.workspace).unwrap();
        for node in ["intake", "analysis", "check"] {
            assert_eq!(
                document.execution.as_ref().unwrap().assignments[node].state,
                "completed",
                "{node}"
            );
        }
        let rows = self.actions();
        assert_eq!(rows.as_array().unwrap().len(), 3, "{rows}");
        for row in rows.as_array().unwrap() {
            assert_eq!(row["state"], "complete");
        }
        let checker = rows
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["adapter_id"] == "deterministic_measurement_checker")
            .unwrap();
        assert_eq!(checker["verified_outcome"], 1);
        assert!(!self.root.join("unexpected-provider-launch.json").exists());
    }
}

#[test]
#[ignore = "child-process helper; invoked by bounded workflow tests"]
fn workflow_coordinator_child() {
    let workspace = PathBuf::from(
        std::env::var_os("FRACTAL_WORKFLOW_TEST_WORKSPACE").expect("owned test workspace"),
    );
    let root = PathBuf::from(std::env::var_os("FRACTAL_WORKFLOW_TEST_ROOT").unwrap());
    let phase = std::env::var("FRACTAL_WORKFLOW_TEST_PHASE").unwrap();
    let document = crate::project_file::load(&workspace).unwrap();
    let completed: BTreeSet<String> = document
        .execution
        .as_ref()
        .map(|state| {
            state
                .assignments
                .iter()
                .filter(|(_, assignment)| assignment.state == "completed")
                .map(|(id, _)| id.clone())
                .collect()
        })
        .unwrap_or_default();
    if std::env::var("FRACTAL_WORKFLOW_TEST_RESUME").as_deref() == Ok("1") {
        // Parent observed the prior coordinator terminate before this child
        // uses the production stale-assignment/reopen lifecycle.
        crate::project_file::release_stale_assignments(&workspace).unwrap();
        crate::checkpoint::prepare_resume(&workspace, &document.graph, &completed).unwrap();
    }
    let outcome = crate::execute::run_multi_agent(
        &document.graph,
        &workspace,
        &["codex".to_owned()],
        None,
        &completed,
    )
    .unwrap();
    fs::write(root.join(format!("{phase}.outcome.json")), serde_json::to_vec_pretty(&json!({
        "failed_node": outcome.failed_node, "detail": outcome.detail,
        "runs": outcome.log.iter().map(|run| json!({"node":run.node,"ok":run.ok,"latency_ms":run.latency_ms})).collect::<Vec<_>>()
    })).unwrap()).unwrap();
    assert!(outcome.failed_node.is_none(), "{}", outcome.detail);
}

#[test]
fn real_three_node_workflow_runs_through_canonical_scheduler() {
    let _lock = crate::graph_store::ENV_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let _offline = EnvGuard::set("FRACTAL_OFFLINE", "1");
    let project = Project::new(false, false, true);
    let mut child = project.command("complete", false).spawn().unwrap();
    assert!(
        project.wait(&mut child, "complete").success(),
        "{}",
        project.logs("complete")
    );
    project.assert_complete();
    let captured = project.feedback();
    assert_eq!(captured.as_array().unwrap().len(), 1, "{captured}");
    let actions = project.actions();
    let analysis = actions
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["adapter_id"] == "deterministic_measurement")
        .unwrap();
    assert_eq!(captured[0]["event"]["attempt_ref"], analysis["attempt_ref"]);
    assert_eq!(captured[0]["event"]["kind"], "independently_verified");
    assert_eq!(
        captured[0]["provenance"]["payload_classification"],
        "synthetic"
    );
    let mut replay = project.command("feedback-replay", true).spawn().unwrap();
    assert!(
        project.wait(&mut replay, "feedback-replay").success(),
        "{}",
        project.logs("feedback-replay")
    );
    assert_eq!(project.feedback(), captured);
    assert_eq!(project.actions(), actions);
}

#[test]
fn real_workflow_review_pauses_only_dependents_and_resumes_same_attempt() {
    let _lock = crate::graph_store::ENV_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let _offline = EnvGuard::set("FRACTAL_OFFLINE", "1");
    let project = Project::new(true, true, false);
    let mut child = project.command("review", false).spawn().unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut packet = None;
    while Instant::now() < deadline {
        if child.try_wait().unwrap().is_some() {
            panic!(
                "coordinator exited before review: {}",
                project.logs("review")
            );
        }
        let database = project
            .workspace
            .join(".fractal/node-receipts/calls.sqlite3");
        if database.exists() && project.root.join("ordinary.json").exists() {
            let value = project.python(r#"import json,pathlib,sqlite3,sys
c=sqlite3.connect(pathlib.Path(sys.argv[1])/'.fractal/node-receipts/calls.sqlite3')
r=c.execute('SELECT packet_ref FROM materializations').fetchone();print(json.dumps(r[0] if r else None))
"#);
            if let Some(value) = value.as_str() {
                packet = Some(value.to_owned());
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let Some(packet) = packet else {
        let _ = child.kill();
        let _ = child.wait();
        panic!("no version-bound review packet: {}", project.logs("review"));
    };
    let before = crate::project_file::load(&project.workspace).unwrap();
    assert_eq!(before.learning.nodes["analysis"].attempt_count, 0);
    assert_eq!(before.learning.nodes["check"].attempt_count, 0);
    assert_eq!(project.actions().as_array().unwrap().len(), 1);
    let approval = Command::new("python3")
        .arg(format!("{MASTER}/runs/jev-network/workflow_fixture.py"))
        .arg("--workspace")
        .arg(&project.workspace)
        .arg("--approve-packet")
        .arg(packet)
        .env("PYTHONPATH", MASTER)
        .env("FRACTAL_NODE_INTELLIGENCE_POLICY", &project.policy)
        .output()
        .unwrap();
    assert!(
        approval.status.success(),
        "{}",
        String::from_utf8_lossy(&approval.stderr)
    );
    assert!(
        project.wait(&mut child, "review").success(),
        "{}",
        project.logs("review")
    );
    project.assert_complete();
    assert_eq!(
        crate::project_file::load(&project.workspace)
            .unwrap()
            .learning
            .nodes["analysis"]
            .attempt_count,
        1
    );
}

#[test]
#[ignore = "explicitly budgeted diagnostic; requires pinned local assets and saved head"]
fn real_saved_head_runs_once_in_canonical_measurement_workflow() {
    let _lock = crate::graph_store::ENV_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let _offline = EnvGuard::set("FRACTAL_OFFLINE", "1");
    let config = PathBuf::from(
        std::env::var_os("FRACTAL_WORKFLOW_TEST_HEAD_CONFIG")
            .expect("explicit frozen head configuration"),
    );
    let report_path = PathBuf::from(
        std::env::var_os("FRACTAL_WORKFLOW_TEST_HEAD_REPORT")
            .expect("explicit diagnostic report path"),
    );
    assert!(
        !report_path.exists(),
        "diagnostic may not overwrite prior evidence"
    );
    let project = Project::with_head(false, false, true, Some(&config));
    let mut child = project.command("saved-head", false).spawn().unwrap();
    assert!(
        project.wait(&mut child, "saved-head").success(),
        "{}",
        project.logs("saved-head")
    );
    project.assert_complete();
    let captured = project.feedback();
    assert_eq!(captured.as_array().unwrap().len(), 1);
    let signal = project.python(
        r#"import json,pathlib,sys
from intelligence_graph.node_runtime import ReceiptStore
w=pathlib.Path(sys.argv[1]);store=ReceiptStore(w,'.fractal/node-receipts')
signals={}
for path in store.root.glob('*.json'):
 body=json.loads(path.read_text())
 if body.get('schema')=='fractal.node_runtime_receipt.v1' and body.get('analysis'):
  signals[body['signals_ref']]=store.get(body['signals_ref'])
assert len(signals)==1
print(json.dumps(next(iter(signals.values()))['signals'][0]))
"#,
    );
    assert_eq!(signal["backend"], "local", "{signal}");
    assert_eq!(signal["head"]["usage"]["worker_starts"], 1);
    assert_eq!(signal["head"]["usage"]["forward_calls"], 1);
    assert_eq!(signal["prediction"]["usage"]["provider_calls"], 0);
    assert_eq!(signal["execution_choice"], json!({"decision":"analyze"}));
    let actions = project.actions();
    let mut replay = project.command("saved-head-replay", true).spawn().unwrap();
    assert!(
        project.wait(&mut replay, "saved-head-replay").success(),
        "{}",
        project.logs("saved-head-replay")
    );
    assert_eq!(project.actions(), actions);
    assert_eq!(project.feedback(), captured);
    let report = json!({"schema":"fractal.node_intelligence.saved_head_workflow_smoke.v1",
        "workspace":project.workspace, "signal":signal, "actions":actions, "feedback":captured,
        "completed_replay_unchanged":true, "hosted_calls":0, "synthetic":true,
        "production_promotion":false, "quality_claim":false});
    fs::write(report_path, serde_json::to_vec_pretty(&report).unwrap()).unwrap();
}

#[test]
fn unit_mismatch_blocks_real_workflow_and_repaired_source_resumes() {
    let _lock = crate::graph_store::ENV_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let _offline = EnvGuard::set("FRACTAL_OFFLINE", "1");
    let project = Project::new(false, false, false);
    let config_path = project.workspace.join(".fractal/node-intelligence.json");
    let original_config = fs::read(&config_path).unwrap();
    let original_policy = fs::read(&project.policy).unwrap();
    project.python(r#"import json,os,pathlib,sys
from intelligence_graph.canonical import canonical_bytes
from intelligence_graph.node_contracts import ContractStore,Record
w=pathlib.Path(sys.argv[1]);store=ContractStore(w/'.fractal/node-contracts')
path=w/'.fractal/node-intelligence.json';config=json.loads(path.read_text())
task=config['tasks']['intake'];old=task['input_refs'][0];body=store.get(old,'artifact').to_dict()
bad=Record({**body,'units':{'value':'mmol/L'}});ref=store.put(bad)
task['input_refs']=[ref if item==old else item for item in task['input_refs']]
auth=task['runtime']['authorization'];auth['authorized_artifacts']=[ref if item==old else item for item in auth['authorized_artifacts']]
auth['current_artifacts'][body['id']]=ref
path.write_bytes(canonical_bytes(config)+b'\n')
policy_path=pathlib.Path(os.environ['FRACTAL_NODE_INTELLIGENCE_POLICY']);policy=json.loads(policy_path.read_text())
policy['tasks']['intake']['authorization']=auth;policy_path.write_bytes(canonical_bytes(policy)+b'\n')
print(json.dumps({'injected_ref':ref}))
"#);
    let mut child = project.command("unit-defect", false).spawn().unwrap();
    assert!(
        !project.wait(&mut child, "unit-defect").success(),
        "unexpected success"
    );
    let outcome: Value =
        serde_json::from_slice(&fs::read(project.root.join("unit-defect.outcome.json")).unwrap())
            .unwrap();
    assert_eq!(outcome["failed_node"], "intake", "{outcome}");
    assert!(project
        .logs("unit-defect")
        .contains("runtime_validation_failed"));
    assert_eq!(project.actions(), json!([]));
    let document = crate::project_file::load(&project.workspace).unwrap();
    for node in ["intake", "analysis", "check"] {
        assert!(document
            .execution
            .as_ref()
            .unwrap()
            .assignments
            .get(node)
            .is_none_or(|assignment| assignment.state != "completed"));
    }
    assert_eq!(document.learning.nodes["analysis"].attempt_count, 0);
    assert_eq!(document.learning.nodes["check"].attempt_count, 0);
    fs::write(config_path, original_config).unwrap();
    fs::write(&project.policy, original_policy).unwrap();
    let mut repaired = project.command("unit-repaired", true).spawn().unwrap();
    assert!(
        project.wait(&mut repaired, "unit-repaired").success(),
        "{}",
        project.logs("unit-repaired")
    );
    project.assert_complete();
    assert_eq!(
        crate::project_file::load(&project.workspace)
            .unwrap()
            .learning
            .nodes["intake"]
            .attempt_count,
        2
    );
    assert!(!project
        .root
        .join("unexpected-provider-launch.json")
        .exists());
}

#[test]
fn committed_effect_survives_actual_coordinator_exit_without_duplicate_action() {
    let _lock = crate::graph_store::ENV_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let _offline = EnvGuard::set("FRACTAL_OFFLINE", "1");
    let project = Project::new(false, false, false);
    let mut child = project
        .command("interrupted", false)
        .env(
            "FRACTAL_NODE_INTELLIGENCE_TEST_CRASH_AFTER_EFFECT",
            "analysis",
        )
        .spawn()
        .unwrap();
    assert_eq!(
        project.wait(&mut child, "interrupted").code(),
        Some(86),
        "{}",
        project.logs("interrupted")
    );
    let before = crate::project_file::load(&project.workspace).unwrap();
    assert_eq!(
        before.execution.as_ref().unwrap().assignments["intake"].state,
        "completed"
    );
    assert_eq!(
        before.execution.as_ref().unwrap().assignments["analysis"].state,
        "checked_out"
    );
    assert_eq!(before.learning.nodes["analysis"].attempt_count, 1);
    let actions_before = project.actions();
    assert_eq!(actions_before.as_array().unwrap().len(), 2);
    let old_analysis = actions_before
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["adapter_id"] == "deterministic_measurement")
        .unwrap()
        .clone();
    assert_eq!(old_analysis["state"], "complete");
    let mut resumed = project.command("resumed", true).spawn().unwrap();
    assert!(
        project.wait(&mut resumed, "resumed").success(),
        "{}",
        project.logs("resumed")
    );
    project.assert_complete();
    let actions_after = project.actions();
    let recovered = actions_after
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["adapter_id"] == "deterministic_measurement")
        .unwrap();
    assert_eq!(
        &old_analysis, recovered,
        "recovery changed or repeated the committed effect"
    );
    assert_eq!(
        crate::project_file::load(&project.workspace)
            .unwrap()
            .learning
            .nodes["analysis"]
            .attempt_count,
        2
    );
    let stable = project.actions();
    let mut replay = project.command("replay", true).spawn().unwrap();
    assert!(
        project.wait(&mut replay, "replay").success(),
        "{}",
        project.logs("replay")
    );
    assert_eq!(project.actions(), stable);
}
