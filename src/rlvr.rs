//! Real GRPO training via the `fractal-rlvr` crate. The CLI's accumulated
//! *verifiable* rewards — the harness-evolution arm rewards (verified success vs.
//! failure per failure-cause) and the router outcome rewards (verifier score per
//! task-kind) — are turned into `DialogueTrace` rollouts (`task_id` = the group,
//! `final_reward` = the verifiable reward) and handed to `fractal-rlvr`'s real
//! GRPO trainer, which computes group-normalized advantages and writes an adapter
//! checkpoint. This is RLVR: the reward is the independently-verified floor
//! verdict, not a hand-tuned proxy.
//!
//! The trainer runs as a subprocess (the prebuilt `fractal-rlvr` binary), so no
//! cross-workspace dependency is needed. When the binary is absent, training is
//! skipped gracefully.

use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

/// Locate the `fractal-rlvr` binary (env override, the fractalchain build dir,
/// or `PATH`).
fn rlvr_bin() -> Option<PathBuf> {
    if let Some(explicit) = std::env::var_os("FRACTAL_RLVR_BIN") {
        let path = PathBuf::from(explicit);
        if path.is_file() {
            return Some(path);
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        for rel in [
            "fractalchain/target/release/fractal-rlvr",
            "fractalchain/target/debug/fractal-rlvr",
        ] {
            let path = PathBuf::from(&home).join(rel);
            if path.is_file() {
                return Some(path);
            }
        }
    }
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|dir| dir.join("fractal-rlvr"))
            .find(|candidate| candidate.is_file())
    })
}

fn fractal_home() -> PathBuf {
    match std::env::var_os("FRACTAL_HOME") {
        Some(home) => PathBuf::from(home),
        None => match std::env::var_os("HOME") {
            Some(home) => PathBuf::from(home).join(".fractal"),
            None => PathBuf::from(".fractal"),
        },
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// One rollout distilled from a verifiable outcome.
struct Rollout {
    trace_id: String,
    task_id: String,
    reward: f64,
    actor: String,
    summary: String,
}

fn read_jsonl(path: &std::path::Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .map(|text| {
            text.lines()
                .filter_map(|line| serde_json::from_str::<Value>(line.trim()).ok())
                .collect()
        })
        .unwrap_or_default()
}

/// Gather rollouts from both durable reward memories.
fn gather_rollouts() -> Vec<Rollout> {
    let home = fractal_home();
    let mut rollouts = Vec::new();

    // Harness-evolution arm rewards — real reward variance (0 vs 10000bp) per
    // failure cause: the GRPO signal for which mutation arm pays off.
    for (index, record) in read_jsonl(&home.join("harness-evolution-memory.jsonl"))
        .iter()
        .enumerate()
    {
        let arm = record.get("arm").and_then(Value::as_str).unwrap_or("arm");
        let cause = record
            .get("cause")
            .and_then(Value::as_str)
            .unwrap_or("harness");
        let reward = record
            .get("reward_bp")
            .and_then(Value::as_f64)
            .unwrap_or(0.0)
            / 10_000.0;
        rollouts.push(Rollout {
            trace_id: format!(
                "evo-{index}-{}",
                record
                    .get("recorded_at")
                    .and_then(Value::as_u64)
                    .unwrap_or(0)
            ),
            task_id: format!("evolution:{cause}"),
            reward,
            actor: arm.to_owned(),
            summary: format!("arm {arm} on cause {cause}: reward {reward}"),
        });
    }

    // Router outcome rewards — verifier score per task-kind and model.
    for (index, record) in read_jsonl(&home.join("outcome-memory.jsonl"))
        .iter()
        .enumerate()
    {
        let task = record
            .get("task_group")
            .and_then(Value::as_str)
            .unwrap_or("fractal-cli");
        let actor = record
            .get("executed_option_id")
            .and_then(Value::as_str)
            .unwrap_or("model");
        let reward = record
            .get("verifier_score")
            .and_then(Value::as_f64)
            .or_else(|| {
                record
                    .get("verified")
                    .and_then(Value::as_bool)
                    .map(|verified| if verified { 1.0 } else { 0.0 })
            })
            .unwrap_or(0.0);
        let id = record
            .get("outcome_id")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .unwrap_or_else(|| format!("outcome-{index}"));
        rollouts.push(Rollout {
            trace_id: format!("route-{id}"),
            task_id: format!("route:{task}"),
            reward,
            actor: actor.to_owned(),
            summary: format!("model {actor} on {task}: reward {reward}"),
        });
    }

    rollouts
}

/// Build a valid `fractal-rlvr` DialogueTrace document for one rollout.
fn trace_document(rollout: &Rollout) -> Value {
    let reward = rollout.reward;
    json!({
        "trace_id": rollout.trace_id,
        "task_id": rollout.task_id,
        "turns": [
            { "role": "user", "content": rollout.task_id, "model_id": null,
              "route_decision": null, "latency_ms": null, "cost_estimate": null },
            { "role": "assistant", "content": rollout.summary, "model_id": rollout.actor,
              "route_decision": null, "latency_ms": null, "cost_estimate": null }
        ],
        "verifier_outputs": [],
        "reward_vector": {
            "correctness": reward, "checkpoint_coverage": reward,
            "clarification_quality": reward, "false_premise_detection": reward,
            "route_correctness": reward, "tool_use_correctness": reward,
            "cost_efficiency": reward, "latency_efficiency": reward,
            "privacy_compliance": reward, "non_redundancy": reward
        },
        "final_reward": reward
    })
}

/// The number of accumulated verifiable rollouts available for training.
pub(crate) fn available_rollouts() -> usize {
    gather_rollouts().len()
}

/// Train a GRPO adapter from the accumulated verifiable rewards. Returns the
/// trainer's report line, or `Ok(None)` when the `fractal-rlvr` binary is absent
/// or there is not yet enough reward data (GRPO needs at least two rollouts).
pub(crate) fn train() -> Result<Option<String>> {
    let Some(bin) = rlvr_bin() else {
        return Ok(None);
    };
    let rollouts = gather_rollouts();
    if rollouts.len() < 2 {
        return Ok(None);
    }

    let stamp = now_ms();
    let base = fractal_home().join("rlvr").join(format!("train-{stamp}"));
    let rollouts_dir = base.join("rollouts");
    let out_dir = base.join("out");
    std::fs::create_dir_all(&rollouts_dir).context("create rollouts dir")?;
    std::fs::create_dir_all(&out_dir).context("create out dir")?;

    for rollout in &rollouts {
        let path = rollouts_dir.join(format!("{}.json", sanitize(&rollout.trace_id)));
        std::fs::write(&path, serde_json::to_vec_pretty(&trace_document(rollout))?)
            .with_context(|| format!("write rollout {}", path.display()))?;
    }

    let adapter = format!("fractal-cli-{stamp}");
    let output = Command::new(&bin)
        .args(["train", "--method", "grpo", "--rollouts"])
        .arg(&rollouts_dir)
        .args([
            "--adapter",
            &adapter,
            "--base",
            "fractal-specialist",
            "--out",
        ])
        .arg(&out_dir)
        .output()
        .with_context(|| format!("launch fractal-rlvr ({})", bin.display()))?;
    if !output.status.success() {
        bail!(
            "fractal-rlvr GRPO training failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let report = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    Ok(Some(report))
}

fn sanitize(id: &str) -> String {
    id.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}
