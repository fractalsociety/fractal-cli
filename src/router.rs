//! Router evolution — the closing arc of the loop. Each verified run's outcome is
//! persisted to durable memory (via the DataEvol ingest bridge). Before a run, the
//! router asks DataEvol's real `build_cheapest_acceptable_rows` which model is the
//! cheapest *acceptable* one for this task-kind, and pins it. So selection
//! genuinely improves across runs instead of every run starting from the same
//! static default.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

/// The embedded router-recommendation bridge (run under `python3`).
const RECOMMEND_PY: &str = include_str!("../scripts/router_recommend.py");

/// Routing-relevant facts about one run, used both to record its outcome and to
/// key the cheapest-acceptable lookup.
pub(crate) struct RunFacts {
    pub task_group: String,
    pub capabilities: Vec<String>,
    pub risk: String,
    pub effort: String,
    pub estimated_input_tokens: u32,
    /// Primary agent (the model family for the capability cell), e.g. `claude`.
    pub model_family: String,
    /// The executed model as `agent:model`, e.g. `claude:fable`.
    pub option_id: String,
    /// Relative cost proxy (usd-micros) for the executed model.
    pub cost_micros: u64,
    /// Stable per-task-kind counterfactual group (independent of the model).
    pub group_id: String,
    /// Unique per-run outcome id + completion tick.
    pub outcome_id: String,
    pub completed_at: u64,
}

fn sha256_hex(text: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(text.as_bytes());
    let mut out = String::new();
    for byte in hasher.finalize() {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// A relative cost proxy per model, so "cheapest acceptable" is meaningful even
/// without real usage receipts. Cheaper models score lower.
pub(crate) fn cost_micros(agent: &str, model: &str) -> u64 {
    match (agent, model) {
        ("claude", "fable") => 1_000,
        ("claude", "haiku") => 1_500,
        ("claude", "sonnet") => 3_000,
        ("claude", "opus") => 6_000,
        ("claude", _) => 3_000,
        ("cursor", _) => 2_000,
        ("codex", _) => 3_000,
        _ => 3_000,
    }
}

/// Durable, append-only outcome memory shared across runs.
pub(crate) fn memory_path() -> PathBuf {
    let root = match std::env::var_os("FRACTAL_HOME") {
        Some(home) => PathBuf::from(home),
        None => match std::env::var_os("HOME") {
            Some(home) => PathBuf::from(home).join(".fractal"),
            None => PathBuf::from(".fractal"),
        },
    };
    root.join("outcome-memory.jsonl")
}

/// A stable comparison-pin hash for a task-kind (not evidence-derived), so runs
/// of the same kind with different models group together in DataEvol.
pub(crate) fn pin_hash(task_group: &str) -> String {
    format!("sha256:{}", sha256_hex(&format!("pin:{task_group}")))
}

/// Build the routing facts for a run: the task-kind, its capability cell inputs,
/// and the executed model + cost. `group_id` deliberately excludes the model so
/// different-model runs of the same kind compare.
pub(crate) fn facts_for(
    task_group: &str,
    capabilities: &[String],
    agent: &str,
    model: &str,
    graph_id: &str,
) -> RunFacts {
    let risk = "low".to_owned();
    let effort = "medium".to_owned();
    let tokens = 2_000u32;
    let mut caps = capabilities.to_vec();
    caps.sort();
    caps.dedup();
    let descriptor = format!(
        "{task_group}|{}|{risk}|{effort}|{agent}|{tokens}",
        caps.join("+")
    );
    let group_id = format!("cf_{}", &sha256_hex(&descriptor)[..20]);
    let completed_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    RunFacts {
        task_group: task_group.to_owned(),
        capabilities: caps,
        risk,
        effort,
        estimated_input_tokens: tokens,
        model_family: agent.to_owned(),
        option_id: format!("{agent}:{model}"),
        cost_micros: cost_micros(agent, model),
        group_id,
        outcome_id: format!("fractal-cli-{graph_id}-{completed_at}"),
        completed_at,
    }
}

/// A cheapest-acceptable recommendation drawn from outcome memory.
pub(crate) struct Recommendation {
    pub chosen_option_id: String,
    pub observed: Vec<String>,
    pub samples: usize,
}

/// Ask DataEvol's real cheapest-acceptable machinery for the best model for this
/// task-kind, given accumulated memory. `None` when there is not yet a causal
/// cheapest-acceptable target (e.g. only one model has been tried).
pub(crate) fn recommend(group_id: &str) -> Option<Recommendation> {
    let src = super::dataevol::dataevol_src()?;
    let memory = memory_path();
    if !memory.is_file() {
        return None;
    }
    let script = std::env::temp_dir().join("fractal_router_recommend.py");
    std::fs::write(&script, RECOMMEND_PY).ok()?;
    let payload = json!({
        "dataevol_src": src.to_string_lossy(),
        "memory_path": memory.to_string_lossy(),
        "counterfactual_group_id": group_id,
    })
    .to_string();

    let mut child = Command::new("python3")
        .arg(&script)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    child.stdin.take()?.write_all(payload.as_bytes()).ok()?;
    let output = child.wait_with_output().ok()?;
    if !output.status.success() {
        return None;
    }
    let line = String::from_utf8_lossy(&output.stdout);
    let value: Value = serde_json::from_str(line.lines().last().unwrap_or("").trim()).ok()?;
    let chosen = value.get("chosen_option_id")?.as_str()?.to_owned();
    Some(Recommendation {
        chosen_option_id: chosen,
        observed: value
            .get("observed_options")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default(),
        samples: value.get("samples").and_then(Value::as_u64).unwrap_or(0) as usize,
    })
}
