//! Canonical scheduler coverage for host-selected network pins.
#![cfg(unix)]

use super::{Project, MASTER};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::process::Command;

fn receipt_for_node<'a>(receipts: &'a [Value], id: &str) -> &'a Value {
    receipts
        .iter()
        .find(|receipt| receipt["node_id"] == id)
        .unwrap_or_else(|| panic!("missing completed receipt for {id}: {receipts:?}"))
}

fn install_network_resolver(project: &Project, disabled: bool) -> (PathBuf, Value) {
    let resolver = project.root.join("network-resolver.json");
    let rollout = project.root.join("network-rollout-policy.json");
    let document = crate::project_file::load(&project.workspace).unwrap();
    let output = Command::new("python3")
        .arg(format!(
            "{MASTER}/runs/jev-network/network_workflow_fixture.py"
        ))
        .arg("--workspace")
        .arg(&project.workspace)
        .arg("--project-id")
        .arg(format!("fractal:project:{}", document.project.slug))
        .arg("--host-policy")
        .arg(&project.policy)
        .arg("--resolver-config")
        .arg(&resolver)
        .arg("--rollout-policy")
        .arg(&rollout)
        .args(if disabled { vec!["--disabled"] } else { vec![] })
        .env("PYTHONPATH", MASTER)
        .output()
        .expect("network workflow fixture process");
    assert!(
        output.status.success(),
        "network fixture: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: Value = serde_json::from_slice(&output.stdout).expect("fixture JSON");
    assert_eq!(value["disabled"], disabled);
    assert_eq!(value["hosted_calls"], 0);
    assert_eq!(value["model_starts"], 0);
    (resolver, value)
}

#[test]
fn resolver_selected_network_flows_through_actual_workflow_handoffs() {
    let _lock = crate::graph_store::ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let _offline = super::EnvGuard::set("FRACTAL_OFFLINE", "1");
    let project = Project::new(false, false, false);
    let (resolver, fixture) = install_network_resolver(&project, false);

    let mut command = project.command("network-workflow", false);
    command.env("FRACTAL_NODE_NETWORK_RESOLVER", &resolver);
    let mut child = command.spawn().unwrap();
    assert!(
        project.wait(&mut child, "network-workflow").success(),
        "{}",
        project.logs("network-workflow")
    );
    project.assert_complete();

    let observed = project.python(
        r#"import json,pathlib,sqlite3,sys
from intelligence_graph.node_runtime import ReceiptStore
w=pathlib.Path(sys.argv[1]); store=ReceiptStore(w,'.fractal/node-receipts')
receipts=[]
for path in store.root.glob('*.json'):
 body=json.loads(path.read_text())
 if body.get('schema')!='fractal.node_runtime_receipt.v1': continue
 attempt=body.get('attempt') or {}
 operation=next((name for name in ('intake','analysis','check') if isinstance(body.get(name),dict)),None)
 if operation and body[operation].get('action',{}).get('state')=='complete':
  receipts.append({'node_id':attempt.get('node_id'),'attempt_ref':body.get('attempt_ref'),
   'network_ref':body.get('network_ref'),'network_resolution_ref':body.get('network_resolution_ref'),
   'materialization_ref':body.get('materialization_ref'),'input_refs':body.get('input_refs'),
   'handoff_refs':body.get('handoff_refs'),'operation':operation})
db=sqlite3.connect(w/'.fractal/node-actions/actions.sqlite3'); db.row_factory=sqlite3.Row
actions=[{'adapter_id':r['adapter_id'],'output_refs':json.loads(r['output_refs_json'])}
 for r in db.execute('SELECT adapter_id,output_refs_json FROM actions ORDER BY adapter_id')]
print(json.dumps({'receipts':receipts,'actions':actions}))
"#,
    );

    let selected = fixture["selected_network_ref"].as_str().unwrap();
    let baseline = fixture["baseline_network_ref"].as_str().unwrap();
    assert_ne!(selected, baseline);
    let receipts = observed["receipts"].as_array().unwrap();
    let intake = receipt_for_node(receipts, "intake");
    let analysis = receipt_for_node(receipts, "analysis");
    let check = receipt_for_node(receipts, "check");
    for receipt in [intake, analysis, check] {
        assert_eq!(receipt["network_ref"], selected, "{receipt}");
    }
    assert!(intake["network_resolution_ref"].is_string(), "{intake}");
    assert!(analysis["materialization_ref"].is_string(), "{analysis}");
    assert!(check["materialization_ref"].is_string(), "{check}");
    assert!(!analysis["handoff_refs"].as_array().unwrap().is_empty());
    assert!(!check["handoff_refs"].as_array().unwrap().is_empty());

    let actions = observed["actions"].as_array().unwrap();
    let output = |adapter: &str| -> Vec<String> {
        actions
            .iter()
            .find(|row| row["adapter_id"] == adapter)
            .unwrap()["output_refs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|reference| reference.as_str().unwrap().to_owned())
            .collect()
    };
    let intake_outputs = output("deterministic_measurement_intake");
    let analysis_outputs = output("deterministic_measurement");
    let mut expected_check_inputs = intake_outputs.clone();
    expected_check_inputs.extend(analysis_outputs.iter().cloned());
    expected_check_inputs.sort();
    let mut expected_analysis_inputs = intake_outputs;
    expected_analysis_inputs.sort();
    assert_eq!(analysis["input_refs"], json!(expected_analysis_inputs));
    assert_eq!(check["input_refs"], json!(expected_check_inputs));

    let resolution = project.python(
        r#"import json,pathlib,sys
from intelligence_graph.node_runtime import ReceiptStore
w=pathlib.Path(sys.argv[1]); store=ReceiptStore(w,'.fractal/node-receipts')
ref=next(json.loads(p.read_text())['network_resolution_ref'] for p in store.root.glob('*.json')
 if json.loads(p.read_text()).get('schema')=='fractal.node_runtime_receipt.v1'
 and json.loads(p.read_text()).get('network_resolution_ref'))
print(json.dumps(store.get(ref)))
"#,
    );
    assert_eq!(resolution["network_ref"], selected);
    assert_eq!(resolution["attempt"]["node_id"], "intake");
    assert_eq!(resolution["provider_calls"], 0);
    assert_eq!(resolution["model_starts"], 0);
}

#[test]
fn disabled_owner_rollout_blocks_before_any_measurement_action() {
    let _lock = crate::graph_store::ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let _offline = super::EnvGuard::set("FRACTAL_OFFLINE", "1");
    let project = Project::new(false, false, false);
    let (resolver, _) = install_network_resolver(&project, true);
    let mut command = project.command("network-policy-disabled", false);
    command.env("FRACTAL_NODE_NETWORK_RESOLVER", &resolver);
    let mut child = command.spawn().unwrap();
    assert!(
        !project
            .wait(&mut child, "network-policy-disabled")
            .success(),
        "disabled owner rollout unexpectedly ran"
    );
    assert_eq!(project.actions().as_array().unwrap().len(), 0);
    assert!(!project
        .root
        .join("unexpected-provider-launch.json")
        .exists());
}
