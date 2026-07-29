//! End-to-end, self-evolving interactive run. Wraps the multi-agent executor to:
//!   1. lease each node (one owner) and emit a signed chain receipt,
//!   2. anchor a signed receipt for every node execution + verifier verdict,
//!   3. on a *verified failure*, run governed evolution — anchor a repair
//!      developmental step, persist a new child graph with parent lineage, and
//!      re-run (re-enqueuing the nodes),
//!   4. on success, auto-export the sanitized outcome to DataEvol.
//!
//! Everything is committed to a per-run signed [`crate::chain::RunLedger`].

use std::path::Path;

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};

use fractal_chain::{
    payload_hash_str, sanitized_export, Consent, DevelopmentalOp, DevelopmentalStep, EvidenceEntry,
    Hash256, OutcomeField, ScaleLevel, Sensitivity,
};

use crate::chain::RunLedger;
use crate::{execute, graph_store};

/// Bounded so evolution always terminates. Three governed repair/grow attempts
/// before giving up (was two) — more resilience on multi-task builds.
const MAX_REPAIRS: u32 = 3;

/// Which executor drives the graph. The default is turnkey and in-process; the
/// Coordinate backend reconciles into the real durable Coordinate queue first.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Backend {
    InProcess,
    Coordinate,
}

impl Backend {
    /// Backend from `$FRACTAL_BACKEND` (`coordinate`) or an explicit flag.
    pub(crate) fn resolve(coordinate_flag: bool) -> Self {
        let env_coordinate = std::env::var("FRACTAL_BACKEND")
            .map(|value| value.eq_ignore_ascii_case("coordinate"))
            .unwrap_or(false);
        if coordinate_flag || env_coordinate {
            Self::Coordinate
        } else {
            Self::InProcess
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::InProcess => "in-process",
            Self::Coordinate => "coordinate",
        }
    }
}

fn hex(hash: &Hash256) -> String {
    let mut s = String::from("sha256:");
    for byte in hash {
        s.push_str(&format!("{byte:02x}"));
    }
    s
}

/// Extract the port from a board URL like `http://127.0.0.1:8092`.
pub(crate) fn board_port(url: &str) -> Option<u16> {
    url.trim_end_matches('/')
        .rsplit(':')
        .next()
        .and_then(|port| port.parse::<u16>().ok())
}

pub(crate) fn hex_to_hash(hexstr: &str) -> Hash256 {
    let hexstr = hexstr.strip_prefix("sha256:").unwrap_or(hexstr);
    let mut out = [0u8; 32];
    for (index, chunk) in hexstr.as_bytes().chunks(2).take(32).enumerate() {
        out[index] = std::str::from_utf8(chunk)
            .ok()
            .and_then(|s| u8::from_str_radix(s, 16).ok())
            .unwrap_or(0);
    }
    out
}

/// Run a committed graph end-to-end with signed receipts, governed evolution on
/// failure, and a DataEvol export on success. Returns the final outcome.
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_end_to_end(
    graph_hash: &str,
    workspace: &Path,
    agents: &[String],
    board: Option<&str>,
    backend: Backend,
    facts: &crate::router::RunFacts,
    request: &str,
    resume_completed: &std::collections::BTreeSet<String>,
) -> Result<execute::RunOutcome> {
    run_end_to_end_with_efficiency(
        graph_hash,
        workspace,
        agents,
        board,
        backend,
        facts,
        request,
        resume_completed,
        None,
    )
}

/// End-to-end run with explicit efficiency governance threaded from CLI/native
/// controls. `None` selects the product default (suggest mode).
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_end_to_end_with_efficiency(
    graph_hash: &str,
    workspace: &Path,
    agents: &[String],
    board: Option<&str>,
    backend: Backend,
    facts: &crate::router::RunFacts,
    request: &str,
    resume_completed: &std::collections::BTreeSet<String>,
    efficiency: Option<&crate::efficiency_config::EfficiencyConfig>,
) -> Result<execute::RunOutcome> {
    let mut graph = graph_store::load_graph(graph_hash)?;
    let mut current_hash = graph_hash.to_owned();
    let graph_id = graph
        .get("graph_id")
        .and_then(Value::as_str)
        .unwrap_or("graph")
        .to_owned();
    let ledger = RunLedger::new(&graph_id);
    // Durable checkpoint so this run can be stopped and resumed. The supervisor
    // records progress per wave; here we clear it on success and keep it on
    // failure so an interrupted or failed run can be picked back up.
    let recorder = crate::checkpoint::Recorder::new(workspace, &graph_id, request);
    let mut run_completed: std::collections::BTreeSet<String> = resume_completed.clone();

    let mut attempt = 0u32;
    // The pending harness evolution awaiting its verifiable reward (RL feedback):
    // (arm, bandit context, cause, governance session). Settled by the next run's
    // verdict: the bandit is rewarded and the canary is activated or rolled back.
    let mut pending: Option<(
        String,
        Vec<i32>,
        String,
        crate::harness_evolution::Governance,
    )> = None;
    let outcome = loop {
        let outcome = match backend {
            // Mid-run morphogenesis supervisor: drives the graph wave-by-wave and
            // fires proactive governed morphogens between waves (adapting the graph
            // continuously), returning the possibly-evolved graph to continue from.
            Backend::InProcess if crate::supervise::enabled() => {
                let supervised = crate::supervise::run_supervised_with_efficiency(
                    graph.clone(),
                    &current_hash,
                    &graph_id,
                    workspace,
                    agents,
                    board,
                    &ledger,
                    &run_completed,
                    Some(&recorder),
                    efficiency,
                )?;
                graph = supervised.graph;
                current_hash = supervised.hash;
                supervised.outcome
            }
            Backend::InProcess => {
                // Non-supervised whole-graph path: still run one efficiency
                // boundary before checkout so resume/inspect stays consistent.
                let default_efficiency = crate::efficiency_config::EfficiencyConfig {
                    mode: crate::efficiency::EfficiencyMode::Suggest,
                    approved: Vec::new(),
                    overridden: Vec::new(),
                    high_impact_autonomy: Vec::new(),
                };
                let efficiency = efficiency.unwrap_or(&default_efficiency);
                let mut runtime = execute::EfficiencyRuntime::default();
                if !run_completed.is_empty() {
                    if let Err(error) = execute::run_efficiency_boundary(
                        &graph,
                        &current_hash,
                        &run_completed,
                        workspace,
                        efficiency,
                        &mut runtime,
                    ) {
                        eprintln!("  efficiency boundary note: {error:#}");
                    }
                }
                execute::run_multi_agent(&graph, workspace, agents, board, &run_completed)?
            }
            Backend::Coordinate => crate::coordinate::run_via_coordinate(
                &current_hash,
                &graph,
                workspace,
                agents,
                board,
            )?,
        };

        // (1)+(2) Signed receipts for every lifecycle event: lease, execution,
        // and (for verify nodes) the verdict.
        for run in &outcome.log {
            ledger.lease(&run.node, &run.agent);
            ledger.execution(&run.node, hex_to_hash(&run.evidence_hex));
            if run.is_verify {
                ledger.verdict(&run.node, run.ok);
            }
            if run.ok {
                run_completed.insert(run.node.clone());
            }
        }
        // Checkpoint the accumulated progress (covers the non-supervised executors;
        // the supervisor also records per wave).
        recorder.record(
            &current_hash,
            &run_completed,
            graph
                .get("nodes")
                .and_then(Value::as_array)
                .map_or(0, Vec::len),
        );

        let succeeded = outcome.verified != Some(false) && outcome.failed_node.is_none();

        // RLVR feedback: reward the *previous* harness evolution with this run's
        // verifiable outcome, so the bandit learns which mutation fixes the failure.
        if let Some((arm, context_bp, cause, governance)) = pending.take() {
            crate::harness_evolution::record_reward(&arm, &context_bp, &cause, succeeded);
            // Settle the governed canary with this run's verifiable verdict.
            let settle = crate::harness_evolution::settle(governance, succeeded);
            if !settle.is_empty() {
                println!("  ⟳ {settle}");
            }
            ledger.promotion(
                &format!("{graph_id}:evolution:{arm}"),
                &format!("reward:{}:{settle}", if succeeded { "10000" } else { "0" }),
            );
        }

        // (3) Self-evolving harness after a verified failure: attribute → bandit-
        // select a governed morphogen (grow/repair) → apply → re-run.
        if !succeeded && attempt < MAX_REPAIRS {
            let failed_node = outcome
                .failed_node
                .clone()
                .unwrap_or_else(|| "acceptance".to_owned());
            let evolution = match crate::harness_evolution::evolve(
                &graph,
                &current_hash,
                &failed_node,
                attempt,
            ) {
                Ok(evolution) => evolution,
                Err(error) => {
                    // A failed evolution must NEVER destroy the whole build. Log
                    // it, stop evolving, and return the partial outcome — the
                    // checkpoint is kept so the run can be resumed.
                    eprintln!(
                            "  ⟳ harness evolution unavailable ({error:#}); keeping progress and stopping evolution"
                        );
                    break outcome;
                }
            };
            println!(
                "  ⟳ verified failure at `{failed_node}` (cause: {}) — harness evolution: {} [{}]",
                evolution.cause, evolution.note, evolution.arm
            );
            println!("  ⟳ {}", evolution.verdict);

            // (4) Anchor the developmental step + child lineage on the signed chain.
            let motivating = outcome
                .log
                .iter()
                .find(|run| run.node == failed_node)
                .map(|run| hex_to_hash(&run.evidence_hex))
                .unwrap_or([0u8; 32]);
            let produced = payload_hash_str(&format!(
                "evolve:{graph_id}:{failed_node}:{}:{attempt}",
                evolution.arm
            ));
            let step = DevelopmentalStep {
                scale: ScaleLevel::Graph,
                subject: format!("{graph_id}#{failed_node}"),
                operation: match evolution.arm.as_str() {
                    "grow.verification" => DevelopmentalOp::Grow,
                    "differentiate.specialize" => DevelopmentalOp::Differentiate,
                    _ => DevelopmentalOp::Repair,
                },
                step_id: format!("{}-{failed_node}-{attempt}", evolution.arm),
                motivating_outcome: motivating,
                produced_outcome: produced,
            };
            ledger.developmental(&step);
            ledger.promotion(
                &format!("{graph_id}->{}", evolution.child_hash),
                &format!("lineage:{}:{}", evolution.arm, step.step_id),
            );
            println!(
                "  ⟳ persisted evolved harness {} (lineage on-chain)",
                &evolution.child_hash[..23.min(evolution.child_hash.len())]
            );
            if let Err(error) =
                crate::project_file::persist(workspace, &evolution.child_graph, request)
            {
                eprintln!("  project graph note: {error:#}");
            } else {
                let _ = crate::project_sync::maybe_sync(workspace);
            }
            crate::run_control::set_graph(&evolution.child_hash, board.unwrap_or_default());

            // Board follows the evolution: re-point the live board to the child
            // graph so the grown / differentiated / repaired tasks appear on the
            // board (with the child's `parent_graph` / `evolution_arm` lineage),
            // instead of the board staying on the original planned tasks.
            if let Some(url) = board {
                if let Some(port) = board_port(url) {
                    println!("  ⟳ board now following the evolved graph…");
                    if let Err(error) =
                        crate::board::serve_graph(&evolution.child_hash, port, None, true, None)
                    {
                        eprintln!("  (board follow unavailable: {error:#})");
                    }
                }
            }

            // (5) Re-run the evolved harness; reward + canary settle next iteration.
            pending = Some((
                evolution.arm,
                evolution.context_bp,
                evolution.cause,
                evolution.governance,
            ));
            graph = evolution.child_graph;
            current_hash = evolution.child_hash;
            attempt += 1;
            continue;
        }
        break outcome;
    };

    // (6) Auto-export the sanitized outcome to DataEvol on success — and persist
    // it to durable outcome memory so the router can learn from it (7).
    if outcome.verified != Some(false) && outcome.failed_node.is_none() {
        // Run finished cleanly — clear the checkpoint so it is not offered for
        // resume. A failed run keeps its checkpoint so it can be picked back up.
        recorder.finish();
        if let Err(error) = export_to_dataevol(&graph_id, &outcome, workspace, &ledger, facts) {
            eprintln!("  export note: {error:#}");
        }
    }

    let (blocks, root, ok) = ledger.summary();
    println!(
        "  ⛓  chain: {blocks} signed receipt block(s) · root {} · {}",
        &root[..23.min(root.len())],
        if ok { "verified" } else { "INVALID" }
    );

    // Fold this run's signed head into the durable machine-scale chain so the
    // receipts accumulate, verifiably, across runs.
    let run_ok = outcome.verified != Some(false) && outcome.failed_node.is_none();
    if let Some(fold) = ledger.fold_into_machine(run_ok) {
        println!(
            "  ⛓  folded into machine chain · {} run(s) anchored · machine root {} · {}",
            fold.runs,
            &fold.machine_root[..23.min(fold.machine_root.len())],
            if fold.verified { "verified" } else { "INVALID" }
        );
    }
    Ok(outcome)
}

/// Build the replayable evidence root + consent-gated sanitized export and write
/// it as the DataEvol handoff.
fn export_to_dataevol(
    graph_id: &str,
    outcome: &execute::RunOutcome,
    workspace: &Path,
    ledger: &RunLedger,
    facts: &crate::router::RunFacts,
) -> Result<()> {
    let entries: Vec<EvidenceEntry> = outcome
        .log
        .iter()
        .map(|run| EvidenceEntry::new(format!("node:{}", run.node), hex_to_hash(&run.evidence_hex)))
        .collect();
    if entries.is_empty() {
        return Ok(());
    }
    let fields = vec![
        OutcomeField {
            key: "summary".to_owned(),
            value_digest: payload_hash_str(&outcome.detail),
            sensitivity: Sensitivity::Public,
        },
        OutcomeField {
            key: "workspace_path".to_owned(),
            value_digest: payload_hash_str(workspace.to_string_lossy().as_ref()),
            sensitivity: Sensitivity::Private,
        },
    ];
    let consent = Consent {
        granted: true,
        scope: "dataevol:promotion".to_owned(),
    };
    let export = sanitized_export(&entries, &fields, &consent)
        .map_err(|error| anyhow!("consent-gated export denied: {error}"))?;

    let dir = workspace.join(".fractal");
    std::fs::create_dir_all(&dir).ok();
    let path = dir.join("dataevol-export.json");
    let payload = json!({
        "schema": "fractal.dataevol_export.v1",
        "graph_id": graph_id,
        "evidence_root": hex(&export.evidence_root),
        "public_fields": export.public_fields.iter().map(|(k, d)| json!([k, hex(d)])).collect::<Vec<_>>(),
        "redacted_count": export.redacted_count,
        "consent_scope": export.consent_scope,
        "export_commitment": hex(&export.export_commitment),
    });
    std::fs::write(&path, serde_json::to_string_pretty(&payload)? + "\n")
        .with_context(|| format!("write {}", path.display()))?;
    ledger.promotion(
        graph_id,
        &format!("dataevol:exported:{}", hex(&export.export_commitment)),
    );
    println!(
        "  ⇢ exported sanitized outcome to DataEvol · {} public field(s), {} redacted · {}",
        export.public_fields.len(),
        export.redacted_count,
        path.display()
    );

    // Genuine ingest: hand the sanitized outcome to DataEvol's *real* normalizer
    // and confirm it is accepted (fail-closed if DataEvol is present and rejects).
    let verified = outcome.verified != Some(false) && outcome.failed_node.is_none();
    match crate::dataevol::ingest(
        graph_id,
        &hex(&export.evidence_root),
        &hex(&export.export_commitment),
        verified,
        facts,
    )? {
        Some(result) => {
            ledger.promotion(
                graph_id,
                &format!(
                    "dataevol:accepted:{}:{}",
                    result.accepted, result.outcome_id
                ),
            );
            println!(
                "  ⇢ DataEvol normalizer {} outcome {} · recorded to outcome memory ({})",
                if result.accepted {
                    "accepted"
                } else {
                    "recorded (not acceptable)"
                },
                result.outcome_id,
                facts.option_id,
            );
        }
        None => println!("  (DataEvol not installed here — kept the sanitized export file)"),
    }
    Ok(())
}
