//! End-to-end, self-evolving interactive run. Wraps the multi-agent executor to:
//!   1. lease each node (one owner) and emit a signed chain receipt,
//!   2. anchor a signed receipt for every node execution + verifier verdict,
//!   3. on a *verified failure*, run governed evolution — anchor a repair
//!      developmental step, persist a new child graph with parent lineage, and
//!      re-run (re-enqueuing the nodes),
//!   4. on success, auto-export the sanitized outcome to DataEvol.
//!   5. optionally (`--settle` / `FRACTAL_SETTLE=1`) submit a bound
//!      `SettleOutcomeReceipt` only after the independent verification floor.
//!
//! Everything is committed to a per-run signed [`crate::chain::RunLedger`].
//! Default offline CLI outcome + routing behavior is preserved; settlement is
//! opt-in and never invents finality.

use std::path::Path;

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};

use fractal_chain::{
    payload_hash_str, sanitized_export, Consent, DevelopmentalOp, DevelopmentalStep, EvidenceEntry,
    Hash256, OutcomeField, ScaleLevel, Sensitivity,
};

use crate::chain::RunLedger;
use crate::{execute, graph_store};

/// Team-19 bounded FractalChain settlement client (owned module path).
#[path = "chain_client19.rs"]
pub(crate) mod chain_client19;

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
    hybrid: bool,
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
        hybrid,
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
    hybrid: bool,
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
    if let Err(error) = crate::checkpoint::prepare_resume(workspace, &graph, &run_completed) {
        eprintln!("  resume learning note: {error:#}");
    }

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
            Backend::InProcess if hybrid => {
                // Hybrid resume deliberately bypasses the mid-run supervisor:
                // every remaining model-driven node must cross the isolated
                // worktree integration boundary instead of sharing a checkout.
                let default_efficiency = crate::efficiency_config::EfficiencyConfig {
                    mode: crate::efficiency::EfficiencyMode::Suggest,
                    approved: Vec::new(),
                    overridden: Vec::new(),
                    high_impact_autonomy: Vec::new(),
                };
                let efficiency = efficiency.unwrap_or(&default_efficiency);
                let mut runtime = execute::EfficiencyRuntime::default();
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
                execute::run_multi_agent_hybrid(&graph, workspace, agents, board, &run_completed)?
            }
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
            if let Err(error) = execute::reopen_for_retry(workspace, &failed_node) {
                eprintln!("  reopen learning note: {error:#}");
            }

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
    // Offline default keeps settle=false / mode=offline (frozen capability-settlement19
    // schema). Opt-in settle flips the flag only; P1.1 economics fields stay out of
    // this export file so DataEvol remains normalization authority via ingest.
    let settle = chain_client19::settle_opt_in();
    let payload = offline_export_document(
        graph_id,
        &hex(&export.evidence_root),
        &hex(&export.export_commitment),
        &export.consent_scope,
        export
            .public_fields
            .iter()
            .map(|(k, d)| (k.as_str(), hex(d)))
            .collect(),
        export.redacted_count,
        settle,
    );
    std::fs::write(&path, canonical_offline_export_bytes(&payload)?)
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
    // P1.1 settlement fields round-trip via ingest only — not via the export file.
    let export_ok = outcome.verified != Some(false) && outcome.failed_node.is_none();
    // Settlement requires the independent verification floor (explicit verified=true).
    let verified_floor = outcome.verified == Some(true) && outcome.failed_node.is_none();
    let independent = verified_floor || outcome.log.iter().any(|run| run.is_verify && run.ok);
    let settlement_fields =
        crate::dataevol::SettlementFields::from_run(facts, &hex(&export.evidence_root), false);
    match crate::dataevol::ingest_with_settlement(
        graph_id,
        &hex(&export.evidence_root),
        &hex(&export.export_commitment),
        export_ok,
        facts,
        Some(&settlement_fields),
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
            // P1.4: opt-in settle only after independent verification floor.
            maybe_settle_verified_outcome(
                workspace,
                graph_id,
                facts,
                &settlement_fields,
                verified_floor,
                independent,
                result.accepted,
                &result.outcome_id,
            );
        }
        None => {
            println!("  (DataEvol not installed here — kept the sanitized export file)");
            // Still allow configured local-devnet settle when evidence is verified
            // and capability config is present (tests / explicit local-devnet).
            maybe_settle_verified_outcome(
                workspace,
                graph_id,
                facts,
                &settlement_fields,
                verified_floor,
                independent,
                verified_floor,
                &facts.outcome_id,
            );
        }
    }
    Ok(())
}

/// Build the sanitized DataEvol export document. Settlement economics fields are
/// intentionally absent; only the opt-in settle mode flag is recorded.
fn offline_export_document(
    graph_id: &str,
    evidence_root: &str,
    export_commitment: &str,
    consent_scope: &str,
    public_fields: Vec<(&str, String)>,
    redacted_count: usize,
    settle: bool,
) -> Value {
    json!({
        "schema": "fractal.dataevol_export.v1",
        "graph_id": graph_id,
        "evidence_root": evidence_root,
        "public_fields": public_fields
            .into_iter()
            .map(|(k, digest)| json!([k, digest]))
            .collect::<Vec<_>>(),
        "redacted_count": redacted_count,
        "consent_scope": consent_scope,
        "export_commitment": export_commitment,
        "mode": if settle { "settle" } else { "offline" },
        "settle": settle,
    })
}

/// Python-compatible `json.dumps(..., indent=2, sort_keys=True)` + trailing newline
/// so offline exports stay byte-stable across rollback.
fn canonical_offline_export_bytes(payload: &Value) -> Result<String> {
    let sorted = sort_json_keys(payload.clone());
    Ok(serde_json::to_string_pretty(&sorted)? + "\n")
}

fn sort_json_keys(value: Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<String> = map.keys().cloned().collect();
            keys.sort();
            let mut out = serde_json::Map::new();
            for key in keys {
                if let Some(v) = map.get(&key) {
                    out.insert(key, sort_json_keys(v.clone()));
                }
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.into_iter().map(sort_json_keys).collect()),
        other => other,
    }
}

/// Opt-in FractalChain settlement. Missing config, uncertain finality, or a
/// failed verification floor leaves work pending and claims zero settlement.
/// Ordinary CLI execution remains usable regardless of settle outcome.
#[allow(clippy::too_many_arguments)]
fn maybe_settle_verified_outcome(
    workspace: &Path,
    graph_id: &str,
    facts: &crate::router::RunFacts,
    fields: &crate::dataevol::SettlementFields,
    verified: bool,
    independent_verifier: bool,
    accepted: bool,
    outcome_id: &str,
) {
    if !chain_client19::settle_opt_in() {
        return;
    }
    let Some(cfg) = chain_client19::SettlementConfig::from_env(workspace) else {
        eprintln!(
            "  settle note: missing explicit FractalChain configuration — leave pending, claim zero"
        );
        return;
    };
    let gate = chain_client19::SettlementGate {
        verified,
        independent_verifier,
        accepted,
        fallback_used: fields.fallback_used,
        fallback_allowed: false,
        schema_ok: true,
        replay: false,
        malformed: false,
        mismatched: false,
        unsupported: false,
    };
    if !gate.allows_submit_strict() {
        eprintln!(
            "  settle note: verification floor not met — zero submissions (verified={verified} independent={independent_verifier} accepted={accepted} fallback={})",
            fields.fallback_used
        );
        return;
    }

    let request_binding = format!(
        "graph:{graph_id}|outcome:{outcome_id}|option:{}|price:{}",
        facts.option_id, fields.price_paid_frac
    );
    let record_hash = chain_client19::keccak_like_record_hash(&request_binding);
    let receipt = chain_client19::build_bound_receipt(
        &cfg,
        record_hash,
        fields.price_paid_frac,
        10_000,
        accepted,
        facts.completed_at,
    );

    let journal = match chain_client19::PendingJournal::open(&cfg.journal_path) {
        Ok(j) => j,
        Err(error) => {
            eprintln!("  settle note: persistence failure ({error:#}) — leave pending, claim zero");
            return;
        }
    };

    let settle_result = if cfg.use_local_devnet {
        let mut net = chain_client19::LocalDevnet::new(cfg.chain_identity.clone());
        net.fund(cfg.payer, fields.price_paid_frac.saturating_mul(2).max(1));
        chain_client19::settle_verified_outcome(
            &mut net,
            &cfg,
            &journal,
            &receipt,
            &request_binding,
            &gate,
        )
    } else {
        let mut transport = chain_client19::JsonRpcTransport::new(&cfg);
        chain_client19::settle_verified_outcome(
            &mut transport,
            &cfg,
            &journal,
            &receipt,
            &request_binding,
            &gate,
        )
    };

    match settle_result {
        Ok(Some(claim)) if claim.settled => {
            println!(
                "  ⛓  capability settlement finalized · receipt {} · height {} · chain {}",
                &claim.receipt_hash_hex[..16.min(claim.receipt_hash_hex.len())],
                claim.height,
                claim.chain_identity
            );
        }
        Ok(_) => {
            eprintln!("  settle note: pending / unfinalized — claim zero settlement");
        }
        Err(error) => {
            eprintln!("  settle note: {error:#} — leave pending, claim zero");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::BTreeSet;
    use std::fs;
    use std::process::Command;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_workspace(name: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "fractal-orchestrate-{name}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn sha256_hex(bytes: &[u8]) -> String {
        let mut child = Command::new("shasum")
            .args(["-a", "256"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("spawn shasum");
        {
            use std::io::Write;
            child
                .stdin
                .as_mut()
                .expect("stdin")
                .write_all(bytes)
                .expect("write");
        }
        let out = child.wait_with_output().expect("shasum");
        assert!(out.status.success());
        String::from_utf8_lossy(&out.stdout)
            .split_whitespace()
            .next()
            .unwrap()
            .to_owned()
    }

    #[test]
    fn offline_export_bytes_match_frozen_verified_run_trace() {
        let payload = offline_export_document(
            "freeze-offline-verified",
            "sha256:abababababababababababababababababababababababababababababababab",
            "sha256:efefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefef",
            "dataevol:promotion",
            vec![(
                "summary",
                "sha256:cdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd".into(),
            )],
            1,
            false,
        );
        let bytes = canonical_offline_export_bytes(&payload).unwrap();
        assert_eq!(
            sha256_hex(bytes.as_bytes()),
            "8c71dab46a93eae49871874f3a4c885d00e350abbc0512b36843b274f8b1d917"
        );
        // The real producer must emit the owned golden trace byte for byte, so a
        // rollback or refactor that changes offline output fails here rather than
        // silently shifting what "offline" means.
        let golden = fs::read(
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/capability_settlement19/offline_verified_run_trace.json"),
        )
        .expect("owned offline trace fixture");
        assert_eq!(
            bytes.as_bytes(),
            golden.as_slice(),
            "offline export bytes drifted from the frozen owned trace"
        );
        assert_eq!(payload["settle"], false);
        assert_eq!(payload["mode"], "offline");
    }

    #[test]
    fn resume_and_retry_reopen_preserve_attempt_count() {
        let workspace = temp_workspace("retry");
        let mut graph = json!({
            "schema": "fractal.execution_graph.v1",
            "graph_id": "fg_orch",
            "nodes": [
                {"id": "build", "capability": "code.generate", "instruction": "Build", "title": "Build"}
            ],
            "edges": []
        });
        graph["graph_hash"] = Value::String(fractal_contracts::canonical_sha256(&graph).unwrap());
        crate::project_file::persist(&workspace, &graph, "Orch").unwrap();
        crate::project_file::checkout_start_node(&workspace, "build", "cursor", "cursor").unwrap();
        crate::project_file::release_node(
            &workspace,
            "build",
            "cursor",
            Some((
                crate::learning_data::NodeOutcome::FailedExecution,
                crate::learning_data::FailureCode::Timeout,
            )),
        )
        .unwrap();

        crate::checkpoint::prepare_resume(&workspace, &graph, &BTreeSet::new()).unwrap();
        execute::reopen_for_retry(&workspace, "build").ok();
        let document = crate::project_file::load(&workspace).unwrap();
        assert!(document.learning.nodes["build"].outcome.is_none());
        assert_eq!(document.learning.nodes["build"].attempt_count, 1);
        assert!(document.learning.nodes["build"].reopen_count >= 1);

        crate::project_file::checkout_start_node(&workspace, "build", "cursor", "cursor").unwrap();
        let retried = crate::project_file::load(&workspace).unwrap();
        assert_eq!(retried.learning.nodes["build"].attempt_count, 2);
        let started = retried.learning.nodes["build"]
            .started_at
            .clone()
            .expect("started");
        crate::project_file::finish_node(
            &workspace,
            "build",
            "cursor",
            crate::learning_data::NodeOutcome::UnverifiedSuccess,
        )
        .unwrap();
        let finished = crate::project_file::load(&workspace).unwrap();
        assert!(
            finished.learning.nodes["build"]
                .finished_at
                .as_ref()
                .unwrap()
                >= &started
        );
        let _ = fs::remove_dir_all(workspace);
    }
}
