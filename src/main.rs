mod amendments;
mod architect;
mod auth;
mod board;
mod bridge;
mod chain;
mod checkpoint;
mod cli;
mod compile;
mod contribute;
mod coordinate;
mod coordinator;
mod dataevol;
mod decompose;
mod efficiency;
mod efficiency_accounting;
mod efficiency_config;
mod efficiency_detector;
mod efficiency_policy;
mod evolve;
mod execute;
mod external_gates;
mod failure_graph;
mod graph_store;
mod handoff;
mod harness;
mod harness_evolution;
mod harness_policy;
mod ingest;
mod intent;
mod interactive;
mod join;
mod learning_data;
mod legacy_import;
mod lessons;
mod master_graph;
mod mobile;
mod node;
mod orchestrate;
mod pipeline;
mod prd_graph;
mod project_audit;
mod project_file;
mod project_sync;
mod projects;
mod reconcile;
mod rlvr;
mod router;
mod run;
mod run_control;
mod safety;
mod social;
mod supervise;
mod test_graph;
mod ui;
mod verify;
mod visibility;
mod voice;
mod work_builder;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Component, Path};
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use sha2::{Digest, Sha256};

use crate::cli::{
    AmendmentCommand, BridgeCommand, Cli, Command, GateCommand, GraphCommand, HarnessCommand,
};
use crate::work_builder::IntentClassification;

fn main() -> ExitCode {
    let cli = Cli::parse();
    let json_diagnostics = uses_stable_json_diagnostics(&cli);
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            if json_diagnostics {
                eprintln!(
                    "{}",
                    serde_json::json!({
                        "schema": "fractal.cli.diagnostic.v1",
                        "status": "error",
                        "message": format!("{error:#}"),
                    })
                );
            } else {
                eprintln!("error: {error:#}");
            }
            ExitCode::FAILURE
        }
    }
}

fn uses_stable_json_diagnostics(cli: &Cli) -> bool {
    matches!(
        cli.command,
        Some(Command::Graph(crate::cli::GraphArgs {
            command: GraphCommand::Audit(_)
                | GraphCommand::Compose(_)
                | GraphCommand::Reconcile(_)
                | GraphCommand::PlanPrd(crate::cli::GraphPlanPrdArgs { json: true, .. }),
        })) | Some(Command::Harness(crate::cli::HarnessArgs {
            command: HarnessCommand::Validate(crate::cli::HarnessPolicyArgs { json: true, .. })
                | HarnessCommand::Show(crate::cli::HarnessPolicyArgs { json: true, .. }),
        })) | Some(Command::Join(crate::cli::JoinArgs { json: true, .. }))
            | Some(Command::Gate(crate::cli::GateArgs {
                command: GateCommand::Record(crate::cli::GateRecordArgs { json: true, .. })
                    | GateCommand::Show(crate::cli::GateShowArgs { json: true, .. })
                    | GateCommand::Revoke(crate::cli::GateRevokeArgs { json: true, .. }),
            }))
            | Some(Command::Amendment(crate::cli::AmendmentArgs {
                command: AmendmentCommand::List(crate::cli::AmendmentListArgs { json: true, .. })
                    | AmendmentCommand::Reject(crate::cli::AmendmentRejectArgs { json: true, .. }),
            }))
    )
}

fn run(cli: Cli) -> Result<()> {
    let Cli {
        fractalwork,
        coordinate,
        offline,
        request,
        command,
    } = cli;
    match (request, command) {
        (None, None) => {
            if offline {
                std::env::set_var("FRACTAL_OFFLINE", "1");
                println!("Offline mode: Fractal Society login and cloud sync are disabled.\n");
            } else {
                auth::ensure_login()?;
            }
            interactive::run(fractalwork.as_deref(), coordinate)
        }
        (Some(request), None) => print_submit_plan(
            &request,
            None,
            None,
            None,
            fractalwork.as_deref(),
            crate::cli::DEFAULT_GRAPH_PORT,
            false,
        ),
        (None, Some(Command::Submit(args))) => print_submit_plan(
            &args.request,
            args.mode,
            args.provider,
            args.repo.as_deref(),
            fractalwork.as_deref(),
            args.port,
            args.no_open,
        ),
        (None, Some(Command::Ios(args))) => {
            mobile::run_ios(&args, fractalwork.as_deref(), coordinate)
        }
        (None, Some(Command::Mobile(args))) => {
            mobile::run_mobile(&args, fractalwork.as_deref(), coordinate)
        }
        (None, Some(Command::Ingest(args))) => {
            ingest::run(&args, fractalwork.as_deref(), coordinate)
        }
        (None, Some(Command::Voice(args))) => {
            voice::run(&args, false, fractalwork.as_deref(), coordinate)
        }
        (None, Some(Command::Dictate(args))) => {
            voice::run(&args, true, fractalwork.as_deref(), coordinate)
        }
        (None, Some(Command::Graph(args))) => match args.command {
            GraphCommand::Open => board::open(),
            GraphCommand::Board(args) => board::serve_graph(
                &args.graph_hash,
                args.port,
                args.exec_graph_dir.as_deref(),
                args.no_open,
                None,
            ),
            GraphCommand::Status(args) => board::status(&args.url, args.json),
            GraphCommand::Show(args) => graph_store::show(&args.graph_hash, args.json),
            GraphCommand::PlanPrd(args) => run_graph_plan_prd(&args),
            GraphCommand::Audit(args) => run_graph_audit(&args),
            GraphCommand::Compose(args) => run_graph_compose(&args),
            GraphCommand::Reconcile(args) => reconcile::run(&reconcile::ReconcileOptions {
                inventory: args.inventory,
                audits: args.audits,
                baseline: args.baseline,
                output: args.output,
            }),
            GraphCommand::Master(args) => {
                board::serve_master(&args.inventory, args.port, None, args.no_open)
            }
            GraphCommand::ImportLegacy(args) => legacy_import::run(&args.state, &args.repo),
            GraphCommand::SeedParallelTest(args) => test_graph::seed(&args),
            GraphCommand::Serve(args) => board::serve_project_foreground(
                &args.repo,
                args.port,
                args.exec_graph_dir.as_deref(),
            ),
        },
        (None, Some(Command::Harness(args))) => match args.command {
            HarnessCommand::Validate(args) => harness_policy::validate(&args.repo, args.json),
            HarnessCommand::Show(args) => harness_policy::show(&args.repo, args.json),
        },
        (None, Some(Command::Gate(args))) => run_gate(&args),
        (None, Some(Command::Amendment(args))) => run_amendment(&args),
        (None, Some(Command::Run(args))) if args.local => {
            let efficiency = efficiency_config::resolve(&args.efficiency)?;
            run_local(&args, &efficiency)
        }
        (None, Some(Command::Run(args))) => {
            let efficiency = efficiency_config::resolve(&args.efficiency)?;
            match args.graph.as_deref() {
                Some(graph_hash) => {
                    println!("{}", efficiency_config::banner(&efficiency));
                    run::run_graph(
                        graph_hash,
                        args.db.as_deref(),
                        args.squad_bin.as_deref(),
                        args.watch,
                        args.dry_run,
                    )
                }
                None => {
                    pipeline::print_run_stub(args.work.as_deref());
                    Ok(())
                }
            }
        }
        (None, Some(Command::Evolve(args))) => evolve::run_evolve(&args),
        (None, Some(Command::Efficiency(args))) => efficiency_config::run(&args),
        (None, Some(Command::Node(args))) => node::run(&args),
        (None, Some(Command::Join(args))) => join::run(&args),
        (None, Some(Command::Coordinator(args))) => coordinator::run(&args),
        (None, Some(Command::Architect(args))) => architect::run(&args),
        (None, Some(Command::Clean(args))) => {
            let removed = safety::guarded_clear(&args.dir, args.yes)?;
            println!("Cleared {removed} item(s) from {}", args.dir.display());
            Ok(())
        }
        (None, Some(Command::Train)) => {
            let count = rlvr::available_rollouts();
            match rlvr::train()? {
                Some(report) => {
                    println!("GRPO training over {count} verifiable rollout(s):");
                    println!("  {report}");
                }
                None if count < 2 => println!(
                    "Not enough accumulated verifiable rewards yet ({count}); run a few builds first."
                ),
                None => println!(
                    "fractal-rlvr not found. Build it (cargo build -p fractal-rlvr) or set $FRACTAL_RLVR_BIN."
                ),
            }
            Ok(())
        }
        (None, Some(Command::Chain)) => {
            let (runs, root, verified) = chain::machine_summary();
            if runs == 0 {
                println!("Machine chain is empty — no runs have been folded in yet.");
            } else {
                println!(
                    "Machine chain: {runs} run(s) anchored · root {} · {}",
                    &root[..23.min(root.len())],
                    if verified { "verified" } else { "INVALID" }
                );
            }
            Ok(())
        }
        (None, Some(Command::Projects)) => {
            let projects = projects::list();
            if projects.is_empty() {
                println!("No projects yet — start a build and it will be numbered.");
            } else {
                println!("Projects (resume by number, or say \"resume project N\"):");
                for project in projects {
                    let workspace = std::path::Path::new(&project.workspace);
                    let status = match checkpoint::from_project_state(workspace)
                        .or_else(|| checkpoint::find_resumable(workspace))
                    {
                        Some(cp) => format!("{}/{} done — resumable", cp.completed.len(), cp.total),
                        None => "complete / idle".to_owned(),
                    };
                    println!("  #{:<3} {:<40} {}", project.number, project.label, status);
                }
            }
            Ok(())
        }
        (None, Some(Command::Resume(args))) => {
            interactive::resume_project(args.number, fractalwork.as_deref(), args.port, coordinate)
                .map(|_| ())
        }
        (None, Some(Command::Stop(args))) => run_control::stop(&args),
        (None, Some(Command::Status(args))) => run_control::status(&args),
        (None, Some(Command::Login(args))) => auth::run_login(&args),
        (None, Some(Command::Logout)) => auth::logout(),
        (None, Some(Command::Sync(args))) => project_sync::run(&args),
        (None, Some(Command::Handoff(args))) => handoff::run(&args),
        (None, Some(Command::Contribute(args))) => {
            contribute::run(&args, fractalwork.as_deref(), coordinate)
        }
        (None, Some(Command::Invite(args))) => social::invite(&args),
        (None, Some(Command::ShareX(args))) => social::share_x(&args),
        (None, Some(Command::ConnectX(args))) => social::connect_x(&args),
        (None, Some(Command::Visibility(args))) => visibility::run(&args),
        (None, Some(Command::Bridge(args))) => match args.command {
            BridgeCommand::Serve { port } => {
                bridge::serve(port, fractalwork.as_deref(), coordinate)
            }
            BridgeCommand::Install { port } => bridge::install(port),
            BridgeCommand::Token => bridge::print_token(),
            BridgeCommand::Status { port } => bridge::status(port),
        },
        (None, Some(Command::Version)) => {
            println!("fractal {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        _ => Ok(()),
    }
}

const AMENDMENT_CLI_SCHEMA: &str = "fractal.amendment.cli.v1";
const AMENDMENT_REJECTION_PREVIEW_READY: &str = "AMENDMENT_REJECTION_PREVIEW_READY";
const AMENDMENT_REJECTION_APPLIED: &str = "AMENDMENT_REJECTION_APPLIED";

const PRD_GRAPH_PLAN_SCHEMA: &str = "fractal.prd_graph_plan.v1";
const PRD_GRAPH_REPLACEMENT_PREVIEW_READY: &str = "PRD_GRAPH_REPLACEMENT_PREVIEW_READY";
const PRD_GRAPH_REPLACEMENT_APPLIED: &str = "PRD_GRAPH_REPLACEMENT_APPLIED";
const EXTERNAL_GATE_CLI_SCHEMA: &str = "fractal.external_gate.cli.v1";
const EXTERNAL_GATE_RECORD_PREVIEW_READY: &str = "EXTERNAL_GATE_RECORD_PREVIEW_READY";
const EXTERNAL_GATE_RECORD_APPLIED: &str = "EXTERNAL_GATE_RECORD_APPLIED";
const EXTERNAL_GATE_REVOKE_PREVIEW_READY: &str = "EXTERNAL_GATE_REVOKE_PREVIEW_READY";
const EXTERNAL_GATE_REVOKE_APPLIED: &str = "EXTERNAL_GATE_REVOKE_APPLIED";

fn gate_preview_guidance(operation: &str, token: &str) -> String {
    let mut guidance = format!(
        "Preview token: {token}\nRe-run this exact command with --yes --expected-content-hash {token} to append the exact {operation}."
    );
    if operation == "revocation" {
        guidance.push_str(
            " The token binds the exact approval, project snapshot, and ledger; revocation remains possible if the old approval evidence drifts or is deleted.",
        );
    }
    guidance
}

fn run_gate(args: &crate::cli::GateArgs) -> Result<()> {
    match &args.command {
        GateCommand::Record(args) => run_gate_record(args),
        GateCommand::Show(args) => run_gate_show(args),
        GateCommand::Revoke(args) => run_gate_revoke(args),
    }
}

fn gate_record_input(args: &crate::cli::GateRecordArgs) -> external_gates::RecordApprovalInput {
    external_gates::RecordApprovalInput {
        node_id: args.node.clone(),
        gate: args.gate.clone(),
        evidence_path: args.evidence.clone(),
        reviewer_id: args.reviewer_id.clone(),
        reviewer_label: if args.reviewer_label.trim().is_empty() {
            args.reviewer_id.clone()
        } else {
            args.reviewer_label.clone()
        },
        role: args.role.clone(),
        attestation: args.attestation.clone(),
    }
}

fn gate_revoke_input(args: &crate::cli::GateRevokeArgs) -> external_gates::RevokeApprovalInput {
    external_gates::RevokeApprovalInput {
        approval_hash: args.approval_hash.clone(),
        reviewer_id: args.reviewer_id.clone(),
        reviewer_label: if args.reviewer_label.trim().is_empty() {
            args.reviewer_id.clone()
        } else {
            args.reviewer_label.clone()
        },
        role: args.role.clone(),
        attestation: args.attestation.clone(),
    }
}

fn run_gate_record(args: &crate::cli::GateRecordArgs) -> Result<()> {
    let input = gate_record_input(args);
    if !args.yes {
        let record = external_gates::preview_approval(&args.repo, &input)?;
        if args.json {
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "schema": EXTERNAL_GATE_CLI_SCHEMA,
                    "operation": "record",
                    "status": "preview",
                    "mutation": "none",
                    "ready": true,
                    "readiness_marker": EXTERNAL_GATE_RECORD_PREVIEW_READY,
                    "record": record,
                }))?
            );
        } else {
            println!("External gate approval preview (no mutation performed)");
            println!("  Node: {}", record.node_id);
            println!("  Gate: {}", record.gate);
            println!("  Graph hash: {}", record.graph_hash);
            println!(
                "  Evidence: {} ({} bytes)",
                record.evidence_path, record.evidence_length
            );
            println!("  Evidence hash: {}", record.evidence_hash);
            println!(
                "  Reviewer: {} ({})",
                record.reviewer_label, record.reviewer_id
            );
            println!("  Role: {}", record.role);
            println!("  Authority: {} / {}", record.authority, record.assurance);
            println!(
                "{}",
                gate_preview_guidance("approval", &record.content_hash)
            );
            println!("{EXTERNAL_GATE_RECORD_PREVIEW_READY}");
        }
        return Ok(());
    }
    let expected = args
        .expected_content_hash
        .as_deref()
        .context("--yes requires --expected-content-hash from the preview")?;
    let record = external_gates::record_approval_with_expected(&args.repo, input, expected)?;
    if record.content_hash != expected {
        anyhow::bail!("applied external gate record does not match preview token");
    }
    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "schema": EXTERNAL_GATE_CLI_SCHEMA,
                "operation": "record",
                "status": "recorded",
                "mutation": "applied",
                "ready": true,
                "readiness_marker": EXTERNAL_GATE_RECORD_APPLIED,
                "record": record,
            }))?
        );
    } else {
        println!("Recorded external gate approval");
        println!("  Node: {}  Gate: {}", record.node_id, record.gate);
        println!("  Content hash: {}", record.content_hash);
        println!("  Graph hash: {}", record.graph_hash);
        println!("{EXTERNAL_GATE_RECORD_APPLIED}");
    }
    Ok(())
}

fn run_gate_show(args: &crate::cli::GateShowArgs) -> Result<()> {
    let workspace = args
        .repo
        .canonicalize()
        .context("resolve gate repository")?;
    let document = project_file::load(&workspace)?;
    let ledger = document
        .external_gate_ledger
        .clone()
        .unwrap_or_else(external_gates::ExternalGateLedger::empty);
    external_gates::validate_ledger(&ledger)?;
    let records = ledger
        .records
        .iter()
        .filter(|record| {
            args.node
                .as_deref()
                .is_none_or(|node| node == record.node_id)
        })
        .filter(|record| args.gate.as_deref().is_none_or(|gate| gate == record.gate))
        .cloned()
        .collect::<Vec<_>>();
    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "schema": EXTERNAL_GATE_CLI_SCHEMA,
                "operation": "show",
                "status": "ok",
                "repo": workspace.display().to_string(),
                "graph_hash": document.graph_hash,
                "ledger_present": document.external_gate_ledger.is_some(),
                "records": records,
            }))?
        );
    } else if records.is_empty() {
        println!(
            "No external gate ledger records in {} (ledger {} ).",
            workspace.display(),
            if document.external_gate_ledger.is_some() {
                "present"
            } else {
                "missing; gated checkout remains denied"
            }
        );
    } else {
        println!("External gate ledger in {}:", workspace.display());
        for record in records {
            println!(
                "  {} {} · {} · {} · {}",
                record.kind, record.content_hash, record.node_id, record.gate, record.reviewer_id
            );
            println!(
                "    graph={} evidence={} ({}, {} bytes)",
                record.graph_hash,
                record.evidence_path,
                record.evidence_hash,
                record.evidence_length
            );
            if let Some(target) = record.revokes {
                println!("    revokes={target}");
            }
        }
    }
    Ok(())
}

fn run_gate_revoke(args: &crate::cli::GateRevokeArgs) -> Result<()> {
    let input = gate_revoke_input(args);
    if !args.yes {
        let record = external_gates::preview_revoke(&args.repo, &input)?;
        if args.json {
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "schema": EXTERNAL_GATE_CLI_SCHEMA,
                    "operation": "revoke",
                    "status": "preview",
                    "mutation": "none",
                    "ready": true,
                    "readiness_marker": EXTERNAL_GATE_REVOKE_PREVIEW_READY,
                    "record": record,
                }))?
            );
        } else {
            println!("External gate revocation preview (no mutation performed)");
            println!("  Target approval: {}", args.approval_hash);
            println!("  Node: {}  Gate: {}", record.node_id, record.gate);
            println!(
                "  Revoker: {} ({})",
                record.reviewer_label, record.reviewer_id
            );
            println!(
                "{}",
                gate_preview_guidance("revocation", &record.content_hash)
            );
            println!("{EXTERNAL_GATE_REVOKE_PREVIEW_READY}");
        }
        return Ok(());
    }
    let expected = args
        .expected_content_hash
        .as_deref()
        .context("--yes requires --expected-content-hash from the preview")?;
    let record = external_gates::revoke_approval_with_expected(&args.repo, input, expected)?;
    if record.content_hash != expected {
        anyhow::bail!("applied external gate revocation does not match preview token");
    }
    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "schema": EXTERNAL_GATE_CLI_SCHEMA,
                "operation": "revoke",
                "status": "revoked",
                "mutation": "applied",
                "ready": true,
                "readiness_marker": EXTERNAL_GATE_REVOKE_APPLIED,
                "record": record,
            }))?
        );
    } else {
        println!("Revoked external gate approval");
        println!("  Revocation hash: {}", record.content_hash);
        println!("  Revokes: {}", record.revokes.unwrap_or_default());
        println!("{EXTERNAL_GATE_REVOKE_APPLIED}");
    }
    Ok(())
}

fn run_amendment(args: &crate::cli::AmendmentArgs) -> Result<()> {
    match &args.command {
        AmendmentCommand::List(args) => run_amendment_list(args),
        AmendmentCommand::Reject(args) => run_amendment_reject(args),
    }
}

fn run_amendment_list(args: &crate::cli::AmendmentListArgs) -> Result<()> {
    let records = amendments::list_pending(&args.repo)
        .with_context(|| format!("list pending amendments in {}", args.repo.display()))?;
    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "schema": AMENDMENT_CLI_SCHEMA,
                "operation": "list",
                "status": "ok",
                "repo": args.repo.display().to_string(),
                "count": records.len(),
                "amendments": records,
            }))?
        );
        return Ok(());
    }

    if records.is_empty() {
        println!("No pending amendments in {}.", args.repo.display());
        return Ok(());
    }
    println!(
        "Pending amendments in {} ({}):",
        args.repo.display(),
        records.len()
    );
    for record in records {
        println!(
            "  {} · {} · task {}",
            record.amendment.command_id, record.amendment.action, record.amendment.task_ref
        );
        println!("    Content hash: {}", record.content_hash);
        println!("    Queue: {} ({})", record.queue, record.queue_file);
        println!("    Instruction: {}", record.amendment.instruction);
    }
    Ok(())
}

fn run_amendment_reject(args: &crate::cli::AmendmentRejectArgs) -> Result<()> {
    let reason = args.reason.trim();
    validate_amendment_rejection_reason(reason)?;
    let records = amendments::list_pending(&args.repo)
        .with_context(|| format!("inspect pending amendments in {}", args.repo.display()))?;
    let target = records
        .iter()
        .find(|record| record.amendment.command_id == args.command_id)
        .with_context(|| {
            format!(
                "pending amendment command_id `{}` was not found in {}",
                args.command_id,
                args.repo.display()
            )
        })?;

    if !args.yes {
        if args.json {
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "schema": AMENDMENT_CLI_SCHEMA,
                    "operation": "reject",
                    "status": "preview",
                    "mutation": "none",
                    "repo": args.repo.display().to_string(),
                    "ready": true,
                    "readiness_marker": AMENDMENT_REJECTION_PREVIEW_READY,
                    "target": target,
                    "reason": reason,
                }))?
            );
        } else {
            println!("Amendment rejection preview (no mutation performed)");
            println!("  Target command ID: {}", target.amendment.command_id);
            println!("  Content hash: {}", target.content_hash);
            println!("  Queue: {} ({})", target.queue, target.queue_file);
            println!("  Reason: {reason}");
            println!("{AMENDMENT_REJECTION_PREVIEW_READY}");
            println!("Re-run this exact command with --yes to reject only this pending amendment.");
        }
        return Ok(());
    }

    let rejection = amendments::reject_pending_amendment(&args.repo, &args.command_id, reason)
        .with_context(|| {
            format!(
                "reject pending amendment `{}` in {}",
                args.command_id,
                args.repo.display()
            )
        })?;
    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "schema": AMENDMENT_CLI_SCHEMA,
                "operation": "reject",
                "status": "rejected",
                "mutation": "applied",
                "repo": args.repo.display().to_string(),
                "ready": true,
                "readiness_marker": AMENDMENT_REJECTION_APPLIED,
                "rejection": rejection,
            }))?
        );
    } else {
        println!("Rejected pending amendment");
        println!("  Target command ID: {}", rejection.command_id);
        println!("  Content hash: {}", rejection.content_hash);
        println!("  Queue: {} ({})", rejection.queue, rejection.queue_file);
        println!("  Reason: {}", rejection.reason);
        println!("{AMENDMENT_REJECTION_APPLIED}");
    }
    Ok(())
}

fn validate_amendment_rejection_reason(reason: &str) -> Result<()> {
    if reason.is_empty() {
        anyhow::bail!("rejection reason must not be empty");
    }
    if reason.len() > 1_024 {
        anyhow::bail!("rejection reason exceeds 1024 bytes");
    }
    if reason.chars().any(char::is_control) {
        anyhow::bail!("rejection reason contains control characters");
    }
    Ok(())
}

fn run_graph_plan_prd(args: &crate::cli::GraphPlanPrdArgs) -> Result<()> {
    let repo = canonicalize_prd_repo(&args.repo)?;
    let prd_relative = validate_prd_relative_path(&args.prd)?;
    let prd_path = resolve_prd_path(&repo, &prd_relative)?;
    let expected_parent_hash = args.expected_parent_hash.trim();
    validate_prd_parent_hash(expected_parent_hash)?;

    // The first load/check is deliberately before reading or compiling the PRD.
    // A stale caller therefore cannot accidentally compile against an obsolete
    // project graph, even when the PRD itself is unchanged.
    let project = project_file::load(&repo)
        .with_context(|| format!("load canonical project graph in {}", repo.display()))?;
    ensure_prd_parent_hash(&project, expected_parent_hash)?;

    let markdown = fs::read_to_string(&prd_path)
        .with_context(|| format!("read PRD {}", prd_relative.display()))?;
    let preview = prd_graph::compile_prd(
        &markdown,
        &prd_relative.to_string_lossy(),
        &args.from,
        &args.through,
        Some(expected_parent_hash),
    )
    .with_context(|| format!("compile PRD {}", prd_relative.display()))?;
    let graph_hash = preview
        .graph
        .get("graph_hash")
        .and_then(serde_json::Value::as_str)
        .context("compiled PRD graph is missing graph_hash")?;
    let summary = preview
        .graph
        .get("summary")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));

    if !args.yes {
        print_prd_graph_plan_preview(
            args,
            &repo,
            &prd_relative,
            expected_parent_hash,
            graph_hash,
            &summary,
        )?;
        return Ok(());
    }

    // Re-check after compilation before touching the content-addressed store.
    // This is also useful for callers that hold the project lock only briefly.
    let current = project_file::load(&repo)
        .with_context(|| format!("recheck canonical project graph in {}", repo.display()))?;
    ensure_prd_parent_hash(&current, expected_parent_hash)?;
    let commit = graph_store::commit_graph(&preview.graph)
        .context("commit compiled PRD graph to the content-addressed store")?;

    // Commit may involve filesystem I/O. The final parent check and project
    // replacement must therefore share one canonical project-file lock.
    let project_path =
        project_file::persist_evolved_if_parent(&repo, &preview.graph, expected_parent_hash)
            .context("persist evolved PRD graph through the canonical project boundary")?;

    print_prd_graph_plan_applied(
        args,
        &repo,
        &prd_relative,
        expected_parent_hash,
        graph_hash,
        &summary,
        &commit,
        &project_path,
    )
}

fn canonicalize_prd_repo(repo: &Path) -> Result<std::path::PathBuf> {
    let canonical = fs::canonicalize(repo)
        .with_context(|| format!("canonicalize project repository {}", repo.display()))?;
    if !canonical.is_dir() {
        anyhow::bail!("project repository {} is not a directory", repo.display());
    }
    Ok(canonical)
}

fn validate_prd_relative_path(path: &Path) -> Result<std::path::PathBuf> {
    if path.as_os_str().is_empty() {
        anyhow::bail!("PRD path must not be empty");
    }
    if path.is_absolute() {
        anyhow::bail!("PRD path must be relative to --repo");
    }

    let mut normalized = std::path::PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(segment) => normalized.push(segment),
            Component::CurDir => {}
            Component::ParentDir => {
                anyhow::bail!("PRD path must not contain traversal component `..`");
            }
            Component::RootDir | Component::Prefix(_) => {
                anyhow::bail!("PRD path must be relative to --repo");
            }
        }
    }
    if normalized.as_os_str().is_empty() {
        anyhow::bail!("PRD path must name a file relative to --repo");
    }
    Ok(normalized)
}

fn resolve_prd_path(repo: &Path, relative: &Path) -> Result<std::path::PathBuf> {
    let mut candidate = repo.to_path_buf();
    for component in relative.components() {
        let Component::Normal(segment) = component else {
            continue;
        };
        candidate.push(segment);
        let metadata = fs::symlink_metadata(&candidate)
            .with_context(|| format!("inspect PRD path component {}", candidate.display()))?;
        if metadata.file_type().is_symlink() {
            anyhow::bail!(
                "PRD path must not traverse a symlink: {}",
                relative.display()
            );
        }
    }

    let metadata = fs::metadata(&candidate)
        .with_context(|| format!("inspect PRD file {}", relative.display()))?;
    if !metadata.is_file() {
        anyhow::bail!("PRD path {} is not a regular file", relative.display());
    }
    let canonical = fs::canonicalize(&candidate)
        .with_context(|| format!("canonicalize PRD {}", relative.display()))?;
    if !canonical.starts_with(repo) {
        anyhow::bail!("PRD path must remain inside --repo: {}", relative.display());
    }
    Ok(canonical)
}

fn validate_prd_parent_hash(hash: &str) -> Result<()> {
    let Some(digest) = hash.strip_prefix("sha256:") else {
        anyhow::bail!("expected parent hash must start with `sha256:`");
    };
    if digest.len() != 64
        || !digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        anyhow::bail!("expected parent hash must contain 64 lowercase hexadecimal characters");
    }
    Ok(())
}

fn ensure_prd_parent_hash(
    project: &project_file::FractalProject,
    expected_parent_hash: &str,
) -> Result<()> {
    if project.graph_hash != expected_parent_hash {
        anyhow::bail!(
            "current project graph hash mismatch: expected {expected_parent_hash}, found {}",
            project.graph_hash
        );
    }
    Ok(())
}

fn print_prd_graph_plan_preview(
    args: &crate::cli::GraphPlanPrdArgs,
    repo: &Path,
    prd_relative: &Path,
    expected_parent_hash: &str,
    graph_hash: &str,
    summary: &serde_json::Value,
) -> Result<()> {
    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "schema": PRD_GRAPH_PLAN_SCHEMA,
                "operation": "plan-prd",
                "status": "preview",
                "mutation": "none",
                "repo": repo.display().to_string(),
                "prd": prd_relative.to_string_lossy(),
                "from": args.from,
                "through": args.through,
                "expected_parent_hash": expected_parent_hash,
                "graph_hash": graph_hash,
                "summary": summary,
                "readiness_marker": PRD_GRAPH_REPLACEMENT_PREVIEW_READY,
            }))?
        );
    } else {
        println!("PRD graph replacement preview (no mutation performed)");
        println!("  Repo: {}", repo.display());
        println!("  PRD: {}", prd_relative.display());
        println!("  Range: {} through {}", args.from, args.through);
        println!("  Expected parent graph: {expected_parent_hash}");
        println!("  Compiled graph: {graph_hash}");
        print_prd_graph_summary(summary);
        println!("{PRD_GRAPH_REPLACEMENT_PREVIEW_READY}");
        println!("Re-run this exact command with --yes to commit the child graph.");
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn print_prd_graph_plan_applied(
    args: &crate::cli::GraphPlanPrdArgs,
    repo: &Path,
    prd_relative: &Path,
    expected_parent_hash: &str,
    graph_hash: &str,
    summary: &serde_json::Value,
    commit: &graph_store::CommitRecord,
    project_path: &Path,
) -> Result<()> {
    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "schema": PRD_GRAPH_PLAN_SCHEMA,
                "operation": "plan-prd",
                "status": "applied",
                "mutation": "applied",
                "repo": repo.display().to_string(),
                "prd": prd_relative.to_string_lossy(),
                "from": args.from,
                "through": args.through,
                "expected_parent_hash": expected_parent_hash,
                "graph_hash": graph_hash,
                "summary": summary,
                "commit": {
                    "graph_hash": commit.graph_hash,
                    "path": commit.path,
                    "bytes": commit.bytes,
                },
                "readiness_marker": PRD_GRAPH_REPLACEMENT_APPLIED,
            }))?
        );
    } else {
        println!("PRD graph replacement applied");
        println!("  Repo: {}", repo.display());
        println!("  PRD: {}", prd_relative.display());
        println!("  Range: {} through {}", args.from, args.through);
        println!("  Parent graph: {expected_parent_hash}");
        println!("  Child graph: {graph_hash}");
        print_prd_graph_summary(summary);
        println!(
            "  Commit: {} ({} bytes)",
            commit.path.display(),
            commit.bytes
        );
        println!("  Project: {}", project_path.display());
        println!("{PRD_GRAPH_REPLACEMENT_APPLIED}");
    }
    Ok(())
}

fn print_prd_graph_summary(summary: &serde_json::Value) {
    let count = |field: &str| {
        summary
            .get(field)
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0)
    };
    println!(
        "  Summary: tasks={} nodes={} edges={} waves={} initial-ready={}",
        count("selected_tasks"),
        count("node_count"),
        count("edge_count"),
        count("wave_count"),
        count("initial_ready_nodes")
    );
}

#[allow(clippy::too_many_arguments)]
fn print_submit_plan(
    request: &str,
    mode: Option<crate::cli::Mode>,
    provider: Option<crate::cli::Provider>,
    repo: Option<&std::path::Path>,
    fractalwork_override: Option<&std::path::Path>,
    port: u16,
    no_open: bool,
) -> Result<()> {
    let classification = intent::fractalwork_dir(fractalwork_override)
        .and_then(|directory| intent::classify(request, &directory));
    let mapped = classification.as_ref().ok().map(map_classification);
    let plan = pipeline::render_submit_plan(request, mode, provider, repo, mapped)?;
    match &classification {
        Ok(classification) => {
            println!(
                "{}",
                intent::render_submit_plan(&plan.text, Ok(classification))
            );
        }
        Err(error) => {
            let reason = format!("{error:#}");
            println!("{}", intent::render_submit_plan(&plan.text, Err(&reason)));
        }
    }
    // P1.4 — auto-open the viewer on submit: when build mode durably committed a
    // graph, launch its own board and open it (unless suppressed with --no-open).
    if let Some(graph_hash) = plan.committed_graph_hash {
        let mut project_url = None;
        if let Some(workspace) = repo {
            match graph_store::load_graph(&graph_hash)
                .and_then(|graph| project_file::persist(workspace, &graph, request))
            {
                Ok(path) => {
                    println!("Project graph: {}", path.display());
                    project_url = project_sync::maybe_sync(workspace);
                }
                Err(error) => eprintln!("project graph note: {error:#}"),
            }
        }
        if no_open {
            println!(
                "Skipping viewer (--no-open). Open it with:\n  fractal graph board {graph_hash} --port {port}"
            );
        } else {
            let (browser_target, is_cloud_target) =
                board::browser_target(project_url.as_deref(), port);
            if let Err(error) = board::serve_graph(&graph_hash, port, None, is_cloud_target, None) {
                eprintln!("warning: could not serve the execution-graph viewer: {error:#}");
            }
            if is_cloud_target {
                if let Err(error) = board::open_url(&browser_target) {
                    eprintln!(
                        "warning: could not open Fractal Society: {error:#}; opening local board"
                    );
                    let (local, _) = board::browser_target(None, port);
                    if let Err(fallback_error) = board::open_url(&local) {
                        eprintln!(
                            "warning: could not open the local execution graph: {fallback_error:#}"
                        );
                    }
                }
            }
        }
    }
    Ok(())
}

/// Execute a committed or file-based graph in-process with the local agent team,
/// serving a live board that turns green as nodes complete. The efficiency
/// configuration is threaded in explicitly (no global mutable state); it is
/// surfaced here but does not yet mutate scheduling.
fn run_local(
    args: &crate::cli::RunArgs,
    efficiency: &efficiency_config::EfficiencyConfig,
) -> Result<()> {
    let (graph, graph_file): (serde_json::Value, std::path::PathBuf) =
        if let Some(file) = &args.graph_file {
            (serde_json::from_slice(&std::fs::read(file)?)?, file.clone())
        } else if let Some(hash) = &args.graph {
            (
                graph_store::load_graph(hash)?,
                graph_store::graph_path(hash),
            )
        } else {
            anyhow::bail!("--local requires --graph <hash> or --graph-file <path>");
        };
    let workspace = std::env::current_dir()?;
    let agents = execute::detect_agents();
    if agents.is_empty() {
        anyhow::bail!("no agents (claude/codex/cursor/hermes) on PATH");
    }

    let port = crate::cli::DEFAULT_GRAPH_PORT;
    let board_url = format!("http://127.0.0.1:{port}");
    if let Err(error) = board::serve_graph_file(&graph_file, port, None, args.dry_run) {
        eprintln!("(board unavailable: {error:#})");
    }
    efficiency_accounting::ensure_envelope(&workspace, efficiency.mode, &efficiency.config_hash())?;
    std::thread::sleep(std::time::Duration::from_millis(1500));

    println!("{}", efficiency_config::banner(efficiency));
    println!(
        "Running graph with agent team: {} in {}",
        agents.join(", "),
        workspace.display()
    );
    let outcome = execute::run_multi_agent(
        &graph,
        &workspace,
        &agents,
        Some(&board_url),
        &std::collections::BTreeSet::new(),
    )?;
    println!("⇒ {}", outcome.detail);
    Ok(())
}

fn map_classification(classification: &intent::TaskClassification) -> IntentClassification {
    IntentClassification {
        intent: classification.intent.clone(),
        topic: String::new(),
        privacy_level: classification.privacy.clone(),
        difficulty: classification.difficulty.clone(),
        verification_level: classification.verification.clone(),
        likely_tools: classification.tools.clone(),
        external_calls_allowed: classification.external_calls,
    }
}
fn run_graph_audit(args: &cli::GraphAuditArgs) -> Result<()> {
    let inventory = master_graph::load_inventory(&args.inventory).with_context(|| {
        "graph audit requires a frozen fractal.repository_inventory.v1 artifact"
    })?;
    let selected: Vec<_> = inventory
        .records
        .iter()
        .enumerate()
        .filter(|(index, _)| *index % args.shard.total as usize == args.shard.index as usize)
        .map(|(_, record)| record)
        .collect();

    let mut project_reports = Vec::new();
    let mut persisted_catalogs = Vec::new();
    let mut warnings = Vec::new();
    for record in selected {
        if !record.exists {
            warnings.push(format!(
                "skipped unavailable workspace from frozen inventory: {}",
                record.canonical_workspace
            ));
            continue;
        }
        let workspace = Path::new(&record.canonical_workspace);
        let mut options = project_audit::AuditOptions::new(workspace, 0, 1);
        if args.run_tests {
            options.native_test_commands = native_test_commands_for(workspace);
            options.command_timeout = Duration::from_secs(120);
        }
        let report = project_audit::load_project_audit_shard(options)
            .with_context(|| format!("audit workspace {}", workspace.display()))?;
        if record
            .project_fractal
            .as_ref()
            .is_some_and(|project| project.available)
        {
            let catalog = catalog_from_audit_report(&inventory, record, &report)?;
            project_file::replace_catalog(workspace, &catalog)
                .with_context(|| format!("persist catalog for {}", workspace.display()))?;
            persisted_catalogs.push(record.canonical_workspace.clone());
        }
        project_reports.push(report);
    }

    if let Some(parent) = args.report.parent() {
        if parent != Path::new("") {
            fs::create_dir_all(parent)
                .with_context(|| format!("create audit report directory {}", parent.display()))?;
        }
    }
    let payload = serde_json::json!({
        "schema": "fractal.graph_audit_report.v1",
        "inventory_path": args.inventory,
        "inventory_hash": inventory.inventory_hash,
        "shard": {"index": args.shard.index, "total": args.shard.total},
        "run_tests": args.run_tests,
        "selected_projects": project_reports.len(),
        "persisted_catalogs": persisted_catalogs,
        "warnings": warnings,
        "reports": project_reports,
    });
    fs::write(&args.report, serde_json::to_vec_pretty(&payload)?)
        .with_context(|| format!("write audit report {}", args.report.display()))?;
    println!("Wrote audit report {}", args.report.display());
    Ok(())
}

fn run_graph_compose(args: &cli::GraphComposeArgs) -> Result<()> {
    let result = master_graph::compose_path(
        &args.inventory,
        master_graph::ComposeOptions {
            validate_only: args.validate_only,
            cache: None,
        },
    )?;
    let value = serde_json::to_value(&result)?;
    if args.json || args.validate_only {
        println!("{}", serde_json::to_string_pretty(&value)?);
    } else if let Some(view_hash) = value.get("view_hash").and_then(serde_json::Value::as_str) {
        println!("Composed master graph view_hash={view_hash}");
    } else {
        println!("{}", serde_json::to_string_pretty(&value)?);
    }
    Ok(())
}

fn native_test_commands_for(workspace: &Path) -> Vec<Vec<String>> {
    let mut commands = Vec::new();
    if workspace.join("Cargo.toml").is_file() {
        commands.push(vec!["cargo".to_owned(), "test".to_owned()]);
    } else if workspace.join("package.json").is_file() {
        commands.push(vec!["npm".to_owned(), "test".to_owned()]);
    } else if workspace.join("pyproject.toml").is_file()
        || workspace.join("requirements.txt").is_file()
    {
        commands.push(vec![
            "python".to_owned(),
            "-m".to_owned(),
            "pytest".to_owned(),
        ]);
    } else if workspace.join("go.mod").is_file() {
        commands.push(vec!["go".to_owned(), "test".to_owned(), "./...".to_owned()]);
    } else if workspace.join("Package.swift").is_file() {
        commands.push(vec!["swift".to_owned(), "test".to_owned()]);
    }
    commands
}

/// A relationship extracted from source text is not automatically a
/// cross-project edge.  Most `use`/`import` signals point at a local module or
/// a third-party package and must remain represented by the catalog's local
/// component/dependency fields.  This small record is used only for explicit,
/// inventory-backed project references (for example a Cargo repository URL or
/// a documented `~/fractal-cli` source path).
#[derive(Clone, Debug)]
struct ExplicitProjectReference {
    target_project_key: String,
    target_alias: String,
    evidence_path: String,
    evidence_hash: String,
    confidence: project_file::project_catalog::CatalogConfidence,
    rationale: String,
}

fn explicit_project_references(
    inventory: &master_graph::RepositoryInventory,
    record: &master_graph::InventoryRecord,
    report: &project_audit::CatalogShardReport,
) -> Vec<ExplicitProjectReference> {
    let origin_key = project_file::project_catalog::project_key(&record.canonical_workspace);
    let mut target_aliases: Vec<(String, String, String)> = inventory
        .records
        .iter()
        .filter(|candidate| {
            candidate.exists
                && candidate
                    .project_fractal
                    .as_ref()
                    .is_some_and(|project| project.available)
                && candidate.canonical_workspace != record.canonical_workspace
        })
        .flat_map(|candidate| {
            let target_key =
                project_file::project_catalog::project_key(&candidate.canonical_workspace);
            let mut aliases = candidate.labels.clone();
            aliases.push(target_key.clone());
            aliases.push(
                Path::new(&candidate.canonical_workspace)
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default(),
            );
            if let Some(git) = &candidate.git {
                for remote in &git.remotes {
                    if let Some(url) = &remote.sanitized_url {
                        if let Some(repo) = url
                            .trim_end_matches('/')
                            .trim_end_matches(".git")
                            .rsplit('/')
                            .next()
                            .filter(|value| !value.is_empty())
                        {
                            aliases.push(repo.to_owned());
                        }
                    }
                }
            }
            aliases
                .into_iter()
                .filter(|alias| alias.chars().count() >= 5)
                .map(move |alias| {
                    (
                        target_key.clone(),
                        alias,
                        candidate.canonical_workspace.clone(),
                    )
                })
        })
        .collect();
    target_aliases.sort_by(|left, right| {
        // Prefer the most specific alias at a given location.  The final
        // tuple keeps processing deterministic when aliases have equal length.
        right
            .1
            .len()
            .cmp(&left.1.len())
            .then_with(|| left.0.cmp(&right.0))
            .then_with(|| left.1.cmp(&right.1))
    });
    target_aliases.dedup_by(|left, right| left.0 == right.0 && left.1 == right.1);

    let workspace = Path::new(&report.workspace);
    let mut references = Vec::new();
    for file in &report.inventory.files {
        let relative = Path::new(&file.path);
        if relative
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.eq_ignore_ascii_case("AGENTS.md"))
        {
            // Every generated project carries the same operating contract,
            // whose examples mention unrelated project names.  It is not
            // relationship evidence for the audited project.
            continue;
        }
        if relative.is_absolute()
            || relative.components().any(|component| {
                matches!(
                    component,
                    std::path::Component::ParentDir
                        | std::path::Component::Prefix(_)
                        | std::path::Component::RootDir
                )
            })
            || relative.components().any(|component| {
                matches!(
                    component,
                    std::path::Component::Normal(name)
                        if matches!(
                            name.to_string_lossy().to_ascii_lowercase().as_str(),
                            ".git"
                                | ".fractal"
                                | "target"
                                | "node_modules"
                                | ".venv"
                                | ".build"
                                | ".xcode-build"
                                | "artifacts"
                                | "dist"
                                | "build"
                        )
                )
            })
        {
            continue;
        }
        let extension = relative
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        if !matches!(
            extension.as_str(),
            "md" | "txt" | "toml" | "yaml" | "yml" | "json" | "rs" | "py" | "sh"
        ) {
            continue;
        }
        let path = workspace.join(relative);
        let Ok(canonical) = path.canonicalize() else {
            continue;
        };
        if !canonical.starts_with(workspace) {
            continue;
        }
        let Ok(contents) = fs::read_to_string(&canonical) else {
            continue;
        };
        for (target_project_key, alias, _) in &target_aliases {
            if target_project_key == &origin_key {
                continue;
            }
            let Some(matching_line) = contents
                .lines()
                .find(|line| contains_project_alias(line, alias))
            else {
                continue;
            };
            let lower_path = file.path.to_ascii_lowercase();
            let lower_line = matching_line.to_ascii_lowercase();
            let alias_lower = alias.to_ascii_lowercase();
            let repository_url =
                lower_line.contains("github.com/") && lower_line.contains(&alias_lower);
            let path_reference = lower_line.contains(&format!("~/{alias_lower}"))
                || (lower_line.contains("/users/")
                    && lower_line.contains(&format!("/{alias_lower}")))
                || lower_line.contains(&format!("cd {alias_lower}"))
                || lower_line.contains("source_root")
                || lower_line.contains("repository")
                || lower_line.contains("workspace")
                || lower_line.contains("manifest");
            // A bare package name in a generated lockfile or an actor label is
            // not enough to claim an edge.  Require a URL/path/workspace
            // context, and do not scan generated lockfiles as prose evidence.
            if !repository_url
                && (!path_reference
                    || lower_path.ends_with(".lock")
                    || lower_path.contains("project.fractal"))
            {
                continue;
            }
            let confidence = if repository_url || path_reference {
                project_file::project_catalog::CatalogConfidence::High
            } else {
                project_file::project_catalog::CatalogConfidence::Medium
            };
            references.push(ExplicitProjectReference {
                target_project_key: target_project_key.clone(),
                target_alias: alias.clone(),
                evidence_path: file.path.clone(),
                evidence_hash: file.sha256.clone(),
                confidence,
                rationale: if repository_url {
                    format!("explicit repository URL reference to project alias `{alias}`")
                } else {
                    format!("explicit workspace/source path reference to project alias `{alias}`")
                },
            });
        }
    }
    references.sort_by(|left, right| {
        (
            &left.target_project_key,
            &left.evidence_path,
            &left.target_alias,
            &left.evidence_hash,
        )
            .cmp(&(
                &right.target_project_key,
                &right.evidence_path,
                &right.target_alias,
                &right.evidence_hash,
            ))
    });
    references.dedup_by(|left, right| {
        left.target_project_key == right.target_project_key
            && left.evidence_path == right.evidence_path
            && left.target_alias == right.target_alias
    });
    references
}

fn contains_project_alias(contents: &str, alias: &str) -> bool {
    let contents = contents.to_ascii_lowercase();
    let alias = alias.to_ascii_lowercase();
    let mut offset = 0;
    while let Some(found) = contents[offset..].find(&alias) {
        let start = offset + found;
        let end = start + alias.len();
        let before = contents[..start].chars().next_back();
        let after = contents[end..].chars().next();
        let is_word = |character: Option<char>| {
            character.is_some_and(|character| {
                character.is_ascii_alphanumeric() || character == '_' || character == '-'
            })
        };
        if !is_word(before) && !is_word(after) {
            return true;
        }
        offset = end;
        if offset >= contents.len() {
            break;
        }
    }
    false
}

fn stable_link_key(target_project_key: &str) -> String {
    let digest = Sha256::digest(target_project_key.as_bytes());
    let suffix = digest
        .iter()
        .take(8)
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("project-ref-{suffix}")
}

fn catalog_from_audit_report(
    inventory: &master_graph::RepositoryInventory,
    record: &master_graph::InventoryRecord,
    report: &project_audit::CatalogShardReport,
) -> Result<project_file::project_catalog::CatalogV1> {
    let now = project_file::project_timestamp();
    let workspace = report.workspace.clone();
    let mut evidence_counts = BTreeMap::new();
    evidence_counts.insert("files".to_owned(), report.inventory.files.len() as u64);
    evidence_counts.insert(
        "manifests".to_owned(),
        report.inventory.manifests.len() as u64,
    );
    evidence_counts.insert(
        "architecture_docs".to_owned(),
        report.inventory.architecture_docs.len() as u64,
    );
    let observed_commit = report.git.commit.clone();
    let is_git_repository = report.git.commit.is_some() || report.git.dirty_fingerprint.is_some();
    let evidence_for =
        |path: &str, hash: &str, kind| project_file::project_catalog::CatalogEvidence {
            path: path.to_owned(),
            sha256: if hash.starts_with("sha256:") {
                hash.to_owned()
            } else {
                format!("sha256:{hash}")
            },
            kind,
            observed_commit: observed_commit.clone(),
            spans: None,
            note: None,
            extra: BTreeMap::new(),
        };

    let fallback_evidence = report
        .inventory
        .manifests
        .first()
        .map(|item| {
            (
                item.path.as_str(),
                item.sha256.as_str(),
                project_file::project_catalog::CatalogEvidenceKind::Manifest,
            )
        })
        .or_else(|| {
            report.inventory.files.first().map(|item| {
                (
                    item.path.as_str(),
                    item.sha256.as_str(),
                    project_file::project_catalog::CatalogEvidenceKind::Source,
                )
            })
        });
    let fallback_evidence_list = || {
        fallback_evidence
            .map(|(path, hash, kind)| vec![evidence_for(path, hash, kind)])
            .unwrap_or_default()
    };

    let mut tests: Vec<_> = report
        .native_tests
        .iter()
        .enumerate()
        .take(project_file::project_catalog::MAX_TESTS)
        .map(|(index, test)| {
            let classification = match test.status {
                project_audit::project_catalog_contract::NativeCommandStatus::Passed => {
                    project_file::project_catalog::CatalogTestClassification::Pass
                }
                project_audit::project_catalog_contract::NativeCommandStatus::Failed => {
                    project_file::project_catalog::CatalogTestClassification::Fail
                }
                project_audit::project_catalog_contract::NativeCommandStatus::TimedOut => {
                    project_file::project_catalog::CatalogTestClassification::Timeout
                }
                project_audit::project_catalog_contract::NativeCommandStatus::MissingTool => {
                    project_file::project_catalog::CatalogTestClassification::MissingTool
                }
                project_audit::project_catalog_contract::NativeCommandStatus::Rejected => {
                    project_file::project_catalog::CatalogTestClassification::Skipped
                }
            };
            let log_sha256 = (!test.output.is_empty()).then(|| {
                let mut hasher = Sha256::new();
                hasher.update(test.output.as_bytes());
                let digest = hasher.finalize();
                format!(
                    "sha256:{}",
                    digest
                        .iter()
                        .map(|byte| format!("{byte:02x}"))
                        .collect::<String>()
                )
            });
            project_file::project_catalog::CatalogTest {
                key: format!("native-test-{index}"),
                command: test.command.join(" "),
                classification,
                exit_code: test.exit_code.map(i64::from),
                duration_ms: Some(test.duration_ms.min(u64::MAX as u128) as u64),
                log_sha256,
                log_excerpt: (!test.output.is_empty())
                    .then(|| test.output.chars().take(1024).collect()),
                evidence: fallback_evidence_list(),
                extra: BTreeMap::new(),
            }
        })
        .collect();
    if tests.is_empty() {
        let candidates = record
            .extra
            .get("candidate_native_test_commands")
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|entry| entry.get("command").and_then(serde_json::Value::as_str))
            .take(project_file::project_catalog::MAX_TESTS)
            .collect::<Vec<_>>();
        if candidates.is_empty() {
            tests.push(project_file::project_catalog::CatalogTest {
                key: "native-test-not-detected".to_owned(),
                command: "no allowlisted native test detected".to_owned(),
                classification: project_file::project_catalog::CatalogTestClassification::NotRun,
                exit_code: None,
                duration_ms: None,
                log_sha256: None,
                log_excerpt: None,
                evidence: fallback_evidence_list(),
                extra: BTreeMap::new(),
            });
        } else {
            tests.extend(candidates.into_iter().enumerate().map(|(index, command)| {
                project_file::project_catalog::CatalogTest {
                    key: format!("candidate-test-{index}"),
                    command: command.to_owned(),
                    classification:
                        project_file::project_catalog::CatalogTestClassification::NotRun,
                    exit_code: None,
                    duration_ms: None,
                    log_sha256: None,
                    log_excerpt: None,
                    evidence: fallback_evidence_list(),
                    extra: BTreeMap::new(),
                }
            }));
        }
    }
    let passing_test_keys: Vec<String> = tests
        .iter()
        .filter(|test| {
            test.classification == project_file::project_catalog::CatalogTestClassification::Pass
        })
        .map(|test| test.key.clone())
        .collect();
    let claim_status = if report.status
        == project_audit::project_catalog_contract::AuditStatus::Pass
        && !passing_test_keys.is_empty()
    {
        project_file::project_catalog::CatalogStatus::Verified
    } else {
        project_file::project_catalog::CatalogStatus::ImplementedUnverified
    };
    let mut components: Vec<_> = report
        .extraction
        .components
        .iter()
        .take(project_file::project_catalog::MAX_COMPONENTS)
        .map(|signal| project_file::project_catalog::CatalogComponent {
            key: project_file::project_catalog::component_key_from(&signal.name),
            name: signal.name.clone(),
            kind: project_file::project_catalog::CatalogComponentKind::Module,
            paths: vec![signal.evidence_path.clone()],
            description: None,
            status: claim_status,
            evidence: vec![evidence_for(
                &signal.evidence_path,
                &signal.evidence_hash,
                project_file::project_catalog::CatalogEvidenceKind::Source,
            )],
            extra: BTreeMap::new(),
        })
        .collect();
    components.sort_by(|left, right| left.key.cmp(&right.key));
    components.dedup_by(|left, right| left.key == right.key);

    if components.is_empty() {
        if let Some((path, hash, kind)) = fallback_evidence {
            let name = Path::new(path)
                .file_stem()
                .and_then(|value| value.to_str())
                .unwrap_or("repository-root")
                .to_owned();
            components.push(project_file::project_catalog::CatalogComponent {
                key: "repository-root".to_owned(),
                name,
                kind: project_file::project_catalog::CatalogComponentKind::Module,
                paths: vec![path.to_owned()],
                description: Some("Repository-level implementation evidence".to_owned()),
                status: claim_status,
                evidence: vec![evidence_for(path, hash, kind)],
                extra: BTreeMap::new(),
            });
        }
    }

    let source_component_key = components.first().map(|component| component.key.clone());
    let mut dependencies = Vec::new();
    for dependency in report
        .extraction
        .dependencies
        .iter()
        .take(project_file::project_catalog::MAX_COMPONENTS.saturating_sub(components.len()))
    {
        let dependency_key = project_file::project_catalog::component_key_from(&format!(
            "dependency-{}",
            dependency.name
        ));
        if components
            .iter()
            .any(|component| component.key == dependency_key)
        {
            continue;
        }
        components.push(project_file::project_catalog::CatalogComponent {
            key: dependency_key.clone(),
            name: dependency.name.clone(),
            kind: project_file::project_catalog::CatalogComponentKind::Library,
            paths: vec![dependency.evidence_path.clone()],
            description: Some(format!("Declared {} dependency", dependency.kind)),
            status: project_file::project_catalog::CatalogStatus::ImplementedUnverified,
            evidence: vec![evidence_for(
                &dependency.evidence_path,
                &dependency.evidence_hash,
                project_file::project_catalog::CatalogEvidenceKind::Manifest,
            )],
            extra: BTreeMap::new(),
        });
        if let Some(from_component) = &source_component_key {
            dependencies.push(project_file::project_catalog::CatalogDependency {
                from_component: from_component.clone(),
                to_component: dependency_key,
                kind: project_file::project_catalog::CatalogDependencyKind::Build,
                evidence: vec![evidence_for(
                    &dependency.evidence_path,
                    &dependency.evidence_hash,
                    project_file::project_catalog::CatalogEvidenceKind::Manifest,
                )],
                extra: BTreeMap::new(),
            });
        }
    }
    components.sort_by(|left, right| left.key.cmp(&right.key));
    dependencies.sort_by(|left, right| {
        (&left.from_component, &left.to_component, left.kind).cmp(&(
            &right.from_component,
            &right.to_component,
            right.kind,
        ))
    });

    let component_keys: BTreeSet<String> = components
        .iter()
        .map(|component| component.key.clone())
        .collect();
    let all_test_keys: Vec<String> = tests.iter().map(|test| test.key.clone()).collect();
    let mut capabilities: Vec<_> = report
        .extraction
        .implemented_features
        .iter()
        .take(project_file::project_catalog::MAX_CAPABILITIES)
        .map(|signal| project_file::project_catalog::CatalogCapability {
            key: project_file::project_catalog::component_key_from(&signal.name),
            title: signal.name.clone(),
            description: Some(signal.kind.clone()),
            status: claim_status,
            evidence: vec![evidence_for(
                &signal.evidence_path,
                &signal.evidence_hash,
                project_file::project_catalog::CatalogEvidenceKind::Document,
            )],
            test_keys: if claim_status == project_file::project_catalog::CatalogStatus::Verified {
                passing_test_keys.clone()
            } else {
                all_test_keys.clone()
            },
            component_keys: Vec::new(),
            extra: BTreeMap::new(),
        })
        .collect();
    if capabilities.is_empty() {
        capabilities.extend(
            components
                .iter()
                .take(project_file::project_catalog::MAX_CAPABILITIES)
                .map(
                    |component| project_file::project_catalog::CatalogCapability {
                        key: project_file::project_catalog::component_key_from(&format!(
                            "implemented-{}",
                            component.key
                        )),
                        title: format!("Implemented {}", component.name),
                        description: Some(
                            "Capability inferred from implemented repository component".to_owned(),
                        ),
                        status: component.status,
                        evidence: component.evidence.clone(),
                        test_keys: if component.status
                            == project_file::project_catalog::CatalogStatus::Verified
                        {
                            passing_test_keys.clone()
                        } else {
                            all_test_keys.clone()
                        },
                        component_keys: vec![component.key.clone()],
                        extra: BTreeMap::new(),
                    },
                ),
        );
    }
    capabilities.sort_by(|left, right| left.key.cmp(&right.key));
    capabilities.dedup_by(|left, right| left.key == right.key);

    let mut decisions: Vec<_> = report
        .extraction
        .decisions
        .iter()
        .take(project_file::project_catalog::MAX_DECISIONS)
        .map(|signal| project_file::project_catalog::CatalogDecision {
            key: project_file::project_catalog::component_key_from(&signal.name),
            title: signal.name.clone(),
            summary: Some(signal.kind.clone()),
            status: project_file::project_catalog::CatalogDecisionStatus::Adopted,
            evidence: vec![evidence_for(
                &signal.evidence_path,
                &signal.evidence_hash,
                project_file::project_catalog::CatalogEvidenceKind::Document,
            )],
            extra: BTreeMap::new(),
        })
        .collect();
    decisions.sort_by(|left, right| left.key.cmp(&right.key));
    decisions.dedup_by(|left, right| left.key == right.key);

    // Resolve legacy relationship candidates to the canonical project key at
    // production time.  `to.alias` is intentionally not used for an exact
    // inventory match: aliases are display labels and may be duplicated, while
    // the stable project key is the namespace used by the composer.
    let mut known_project_aliases: BTreeMap<String, String> = BTreeMap::new();
    for candidate in &inventory.records {
        if !candidate.exists
            || !candidate
                .project_fractal
                .as_ref()
                .is_some_and(|project| project.available)
        {
            continue;
        }
        let target_key = project_file::project_catalog::project_key(&candidate.canonical_workspace);
        known_project_aliases.insert(target_key.clone(), target_key.clone());
        for label in &candidate.labels {
            known_project_aliases
                .entry(label.clone())
                .or_insert_with(|| target_key.clone());
        }
    }
    let mut cross_graph_links: Vec<_> = report
        .extraction
        .relationships
        .iter()
        .filter_map(|relationship| {
            let target_project_key = known_project_aliases.get(&relationship.target)?;
            if target_project_key == &project_file::project_catalog::project_key(&workspace) {
                return None;
            }
            Some((target_project_key.clone(), relationship))
        })
        .take(project_file::project_catalog::MAX_LINKS)
        .enumerate()
        .map(|(index, (target_project_key, relationship))| {
            let link_type = match relationship.relationship {
                project_audit::project_catalog_contract::RelationshipKind::DependsOn => {
                    project_file::project_catalog::CatalogLinkType::DependsOn
                }
                project_audit::project_catalog_contract::RelationshipKind::Implements => {
                    project_file::project_catalog::CatalogLinkType::UsesComponent
                }
                project_audit::project_catalog_contract::RelationshipKind::Tests => {
                    project_file::project_catalog::CatalogLinkType::RelatedTo
                }
                project_audit::project_catalog_contract::RelationshipKind::Documents => {
                    project_file::project_catalog::CatalogLinkType::RelatedTo
                }
                project_audit::project_catalog_contract::RelationshipKind::Configures => {
                    project_file::project_catalog::CatalogLinkType::RelatedTo
                }
                project_audit::project_catalog_contract::RelationshipKind::Invokes => {
                    project_file::project_catalog::CatalogLinkType::RelatedTo
                }
            };
            project_file::project_catalog::CatalogCrossGraphLink {
                key: format!("audit-link-{index}"),
                link_type,
                from: project_file::project_catalog::CatalogLinkFrom {
                    component_key: {
                        let key =
                            project_file::project_catalog::component_key_from(&relationship.source);
                        component_keys.contains(&key).then_some(key)
                    },
                    extra: BTreeMap::new(),
                },
                to: project_file::project_catalog::CatalogLinkTo {
                    project_key: Some(target_project_key),
                    alias: None,
                    component_key: None,
                    extra: BTreeMap::new(),
                },
                confidence: match relationship.confidence {
                    project_audit::project_catalog_contract::Confidence::High => {
                        project_file::project_catalog::CatalogConfidence::High
                    }
                    project_audit::project_catalog_contract::Confidence::Medium => {
                        project_file::project_catalog::CatalogConfidence::Medium
                    }
                    project_audit::project_catalog_contract::Confidence::Low => {
                        project_file::project_catalog::CatalogConfidence::Low
                    }
                },
                rationale: Some(format!(
                    "{} relationship extracted from {}",
                    relationship.source, relationship.evidence_path
                )),
                evidence: vec![evidence_for(
                    &relationship.evidence_path,
                    &relationship.evidence_hash,
                    project_file::project_catalog::CatalogEvidenceKind::Source,
                )],
                extra: BTreeMap::new(),
            }
        })
        .collect();

    // Add only explicit project references, not every local import/test
    // relationship.  Grouping evidence by target gives one deterministic edge
    // per target and keeps each claim within the catalog evidence bound.
    let mut explicit_by_target: BTreeMap<String, Vec<ExplicitProjectReference>> = BTreeMap::new();
    for reference in explicit_project_references(inventory, record, report) {
        explicit_by_target
            .entry(reference.target_project_key.clone())
            .or_default()
            .push(reference);
    }
    for (target_project_key, references) in explicit_by_target {
        if cross_graph_links.len() >= project_file::project_catalog::MAX_LINKS {
            break;
        }
        let mut evidence = Vec::new();
        let mut confidence = project_file::project_catalog::CatalogConfidence::Low;
        let mut rationale = String::new();
        for reference in references
            .iter()
            .take(project_file::project_catalog::MAX_EVIDENCE_PER_CLAIM)
        {
            evidence.push(evidence_for(
                &reference.evidence_path,
                &reference.evidence_hash,
                project_file::project_catalog::CatalogEvidenceKind::Source,
            ));
            if reference.confidence == project_file::project_catalog::CatalogConfidence::High {
                confidence = project_file::project_catalog::CatalogConfidence::High;
            } else if confidence == project_file::project_catalog::CatalogConfidence::Low {
                confidence = reference.confidence;
            }
            if rationale.is_empty() {
                rationale = reference.rationale.clone();
            }
        }
        if evidence.is_empty() {
            continue;
        }
        cross_graph_links.push(project_file::project_catalog::CatalogCrossGraphLink {
            key: stable_link_key(&target_project_key),
            link_type: project_file::project_catalog::CatalogLinkType::RelatedTo,
            from: project_file::project_catalog::CatalogLinkFrom {
                component_key: None,
                extra: BTreeMap::new(),
            },
            to: project_file::project_catalog::CatalogLinkTo {
                project_key: Some(target_project_key),
                alias: None,
                component_key: None,
                extra: BTreeMap::new(),
            },
            confidence,
            rationale: Some(rationale),
            evidence,
            extra: BTreeMap::new(),
        });
    }
    cross_graph_links.sort_by(|left, right| left.key.cmp(&right.key));
    cross_graph_links.dedup_by(|left, right| left.key == right.key);
    let mut catalog = project_file::project_catalog::CatalogV1 {
        schema: project_file::project_catalog::CATALOG_SCHEMA.to_owned(),
        project_key: project_file::project_catalog::project_key(&workspace),
        generated_at: now.clone(),
        catalog_hash: String::new(),
        source: project_file::project_catalog::CatalogSource {
            canonical_workspace: workspace.clone(),
            workspace_fingerprint: project_file::project_catalog::workspace_fingerprint(&workspace),
            registry_numbers: record.registry_numbers.clone(),
            labels: record.labels.clone(),
            git: project_file::project_catalog::CatalogGit {
                is_git_repository,
                commit: is_git_repository
                    .then(|| report.git.commit.clone())
                    .flatten(),
                dirty: is_git_repository.then_some(report.git.dirty),
                dirty_fingerprint: is_git_repository
                    .then(|| report.git.dirty_fingerprint.clone())
                    .flatten(),
                unavailable_reason: None,
                remotes: Vec::new(),
                extra: BTreeMap::new(),
            },
            extra: BTreeMap::new(),
        },
        audit: project_file::project_catalog::CatalogAudit {
            auditor: "fractal graph audit".to_owned(),
            cli_version: Some(env!("CARGO_PKG_VERSION").to_owned()),
            inventory_hash: inventory.inventory_hash.clone(),
            started_at: now.clone(),
            finished_at: now,
            bounds: project_file::project_catalog::CatalogBounds {
                max_catalog_bytes: None,
                max_evidence_per_claim: Some(16),
                max_log_excerpt_chars: Some(4096),
                max_string_chars: Some(4096),
                test_timeout_ms: Some(120_000),
                extra: BTreeMap::new(),
            },
            truncated: false,
            evidence_counts: Some(evidence_counts),
            extra: BTreeMap::new(),
        },
        capabilities,
        components,
        dependencies,
        tests,
        decisions,
        cross_graph_links,
        diagnostics: report
            .warnings
            .iter()
            .take(project_file::project_catalog::MAX_DIAGNOSTICS)
            .map(|warning| project_file::project_catalog::CatalogDiagnostic {
                code: project_file::project_catalog::CatalogDiagnosticCode::TestUnavailable,
                severity: project_file::project_catalog::CatalogDiagnosticSeverity::Warning,
                message: warning.clone(),
                context: Some(warning.clone()),
                extra: BTreeMap::new(),
            })
            .collect(),
        extra: BTreeMap::new(),
    };
    project_file::project_catalog::normalize(&mut catalog)
        .map_err(|error| anyhow::anyhow!("normalize generated catalog: {error}"))?;
    if report.git.dirty || report.git.commit.is_none() {
        catalog.source.git.dirty_fingerprint = Some(
            project_file::project_catalog::compute_dirty_fingerprint(&catalog)
                .map_err(|error| anyhow::anyhow!("compute dirty fingerprint: {error}"))?,
        );
        project_file::project_catalog::normalize(&mut catalog)
            .map_err(|error| anyhow::anyhow!("normalize generated catalog: {error}"))?;
    }
    Ok(catalog)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn dispatches_version() {
        let cli = Cli::try_parse_from(["fractal", "version"]).unwrap();
        assert!(run(cli).is_ok());
    }

    #[test]
    fn dispatches_efficiency_status_with_defaults() {
        let cli = Cli::try_parse_from(["fractal", "efficiency"]).unwrap();
        assert!(run(cli).is_ok());
    }

    #[test]
    fn gate_preview_guidance_prints_token_and_exact_rerun_flags() {
        let token = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let approval = gate_preview_guidance("approval", token);
        assert!(approval.contains(&format!("Preview token: {token}")));
        assert!(approval.contains(&format!("--yes --expected-content-hash {token}")));
        assert!(approval.contains("exact approval"));

        let revocation = gate_preview_guidance("revocation", token);
        assert!(revocation.contains(&format!("Preview token: {token}")));
        assert!(revocation.contains(&format!("--yes --expected-content-hash {token}")));
        assert!(revocation.contains("exact approval, project snapshot, and ledger"));
        assert!(revocation.contains("old approval evidence drifts or is deleted"));
    }

    #[test]
    fn gate_apply_requires_preview_token() {
        let cli = Cli::try_parse_from([
            "fractal",
            "gate",
            "record",
            "--node",
            "secure",
            "--gate",
            "security_review",
            "--evidence",
            "review.txt",
            "--reviewer-id",
            "reviewer",
            "--role",
            "security_reviewer",
            "--attestation",
            "approve:graph:secure:security_review",
            "--yes",
        ])
        .unwrap();
        let error = run(cli).expect_err("--yes without a preview token must fail");
        assert!(format!("{error:#}").contains("--yes requires --expected-content-hash"));
    }

    #[test]
    fn dispatches_graph_compose_validate_only_from_frozen_inventory() {
        let root = temp_root("compose");
        let inventory = write_empty_inventory(&root);
        let cli = Cli::try_parse_from([
            "fractal",
            "graph",
            "compose",
            "--inventory",
            inventory.to_str().unwrap(),
            "--json",
            "--validate-only",
        ])
        .unwrap();
        assert!(run(cli).is_ok());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn dispatches_graph_audit_empty_frozen_inventory_to_report() {
        let root = temp_root("audit");
        let inventory = write_empty_inventory(&root);
        let report = root.join("audit-report.json");
        let cli = Cli::try_parse_from([
            "fractal",
            "graph",
            "audit",
            "--inventory",
            inventory.to_str().unwrap(),
            "--shard",
            "0/1",
            "--report",
            report.to_str().unwrap(),
        ])
        .unwrap();
        assert!(run(cli).is_ok());
        let report_json: serde_json::Value =
            serde_json::from_slice(&fs::read(&report).unwrap()).unwrap();
        assert_eq!(
            report_json
                .get("schema")
                .and_then(serde_json::Value::as_str),
            Some("fractal.graph_audit_report.v1")
        );
        assert_eq!(
            report_json
                .get("selected_projects")
                .and_then(serde_json::Value::as_u64),
            Some(0)
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn project_alias_matching_respects_namespace_boundaries() {
        assert!(contains_project_alias("cd ~/fractal-cli\n", "fractal-cli"));
        assert!(contains_project_alias(
            "repository = \"https://github.com/fractalsociety/fractal-cli.git\"",
            "fractal-cli"
        ));
        assert!(!contains_project_alias("fractal-cli-tools", "fractal-cli"));
        assert_eq!(stable_link_key("fractal-cli-bbbfd315b970").len(), 28);
    }

    #[test]
    fn explicit_project_reference_uses_canonical_target_key() {
        let root = temp_root("explicit-project-ref");
        let origin = root.join("origin");
        let target = root.join("fractal-cli");
        fs::create_dir_all(&origin).unwrap();
        fs::create_dir_all(&target).unwrap();
        fs::write(
            origin.join("Cargo.toml"),
            "repository = \"https://github.com/fractalsociety/fractal-cli.git\"\n",
        )
        .unwrap();
        let origin = fs::canonicalize(origin).unwrap();
        let target = fs::canonicalize(target).unwrap();
        let origin_str = origin.to_string_lossy().into_owned();
        let target_str = target.to_string_lossy().into_owned();
        let target_key = project_file::project_catalog::project_key(&target_str);
        let inventory = master_graph::RepositoryInventory {
            schema: "fractal.repository_inventory.v1".to_owned(),
            inventory_hash:
                "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned(),
            records: vec![
                master_graph::InventoryRecord {
                    canonical_workspace: origin_str.clone(),
                    exists: true,
                    labels: vec!["origin".to_owned()],
                    registry_numbers: vec![1],
                    unavailable_reason: None,
                    git: None,
                    project_fractal: Some(master_graph::InventoryProjectFractal {
                        available: true,
                        relative_path: Some(".fractal/project.fractal".to_owned()),
                        size_bytes: None,
                        unavailable_reason: None,
                    }),
                    extra: BTreeMap::new(),
                },
                master_graph::InventoryRecord {
                    canonical_workspace: target_str.clone(),
                    exists: true,
                    labels: vec!["fractal-cli".to_owned()],
                    registry_numbers: vec![2],
                    unavailable_reason: None,
                    git: None,
                    project_fractal: Some(master_graph::InventoryProjectFractal {
                        available: true,
                        relative_path: Some(".fractal/project.fractal".to_owned()),
                        size_bytes: None,
                        unavailable_reason: None,
                    }),
                    extra: BTreeMap::new(),
                },
            ],
            extra: BTreeMap::new(),
        };
        let report = project_audit::CatalogShardReport {
            workspace: origin_str.clone(),
            inventory: project_audit::project_catalog_contract::RepositoryInventory {
                files: vec![project_audit::project_catalog_contract::FileEvidence {
                    path: "Cargo.toml".to_owned(),
                    sha256: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
                        .to_owned(),
                    bytes: 80,
                }],
                ..Default::default()
            },
            ..Default::default()
        };
        let record = &inventory.records[0];
        let links = explicit_project_references(&inventory, record, &report);
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].target_project_key, target_key);
        assert_eq!(links[0].evidence_path, "Cargo.toml");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn dispatches_graph_master_to_read_only_board_entry_point() {
        let cli = Cli::try_parse_from([
            "fractal",
            "graph",
            "master",
            "--inventory",
            "/tmp/inventory.json",
            "--port",
            "8123",
            "--no-open",
        ])
        .unwrap();
        match cli.command {
            Some(Command::Graph(crate::cli::GraphArgs {
                command: GraphCommand::Master(args),
            })) => {
                assert_eq!(args.inventory, Path::new("/tmp/inventory.json"));
                assert_eq!(args.port, 8123);
                assert!(args.no_open);
            }
            other => panic!("expected graph master command, got {other:?}"),
        }
    }

    #[test]
    fn graph_audit_refuses_non_frozen_inventory() {
        let root = temp_root("bad-inventory");
        let inventory = root.join("inventory.json");
        fs::write(&inventory, br#"{"schema":"not-frozen","inventory_hash":"sha256:0000000000000000000000000000000000000000000000000000000000000000","records":[]}"#).unwrap();
        let report = root.join("audit-report.json");
        let cli = Cli::try_parse_from([
            "fractal",
            "graph",
            "audit",
            "--inventory",
            inventory.to_str().unwrap(),
            "--shard",
            "0/1",
            "--report",
            report.to_str().unwrap(),
        ])
        .unwrap();
        let error = run(cli).unwrap_err();
        assert!(format!("{error:#}").contains("frozen fractal.repository_inventory.v1"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn prd_plan_path_validation_rejects_absolute_and_traversal_paths() {
        assert!(validate_prd_relative_path(Path::new("/tmp/plan.md")).is_err());
        assert!(validate_prd_relative_path(Path::new("docs/../plan.md")).is_err());
        assert!(validate_prd_relative_path(Path::new("./docs/plan.md")).is_ok());
    }

    #[test]
    fn prd_plan_parent_hash_validation_requires_canonical_sha256() {
        assert!(validate_prd_parent_hash("sha256:").is_err());
        assert!(validate_prd_parent_hash(&format!("sha256:{}", "a".repeat(64))).is_ok());
        assert!(validate_prd_parent_hash(&format!("sha256:{}", "A".repeat(64))).is_err());
        assert!(validate_prd_parent_hash(&"0".repeat(64)).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn prd_plan_path_resolution_rejects_symlink_components() {
        use std::os::unix::fs::symlink;

        let root = temp_root("prd-symlink");
        fs::write(root.join("outside.md"), "outside").unwrap();
        symlink(root.join("outside.md"), root.join("plan.md")).unwrap();
        assert!(resolve_prd_path(&root, Path::new("plan.md")).is_err());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn graph_plan_prd_preview_is_read_only() -> Result<()> {
        let _env_lock = graph_store::ENV_LOCK
            .lock()
            .map_err(|_| anyhow::anyhow!("graph-store test environment lock poisoned"))?;
        let _home = graph_store::TestHome::new("prd-plan-preview")?;
        let root = temp_root("prd-plan-preview");
        let (parent, parent_hash) = seed_prd_plan_project(&root)?;
        fs::create_dir_all(root.join("docs"))?;
        fs::write(root.join("docs/plan.md"), sample_prd_markdown())?;
        let before = fs::read(project_file::path(&root))?;

        let args = prd_plan_args(&root, &parent_hash, false);
        run_graph_plan_prd(&args)?;

        assert_eq!(fs::read(project_file::path(&root))?, before);
        assert_eq!(project_file::load(&root)?.graph_hash, parent_hash);
        let expected_child = prd_graph::compile_prd(
            &sample_prd_markdown(),
            "docs/plan.md",
            "INT-002",
            "INT-003",
            Some(&parent_hash),
        )?
        .graph_hash;
        assert!(graph_store::load_graph(&expected_child).is_err());
        assert_eq!(graph_store::load_graph(&parent_hash)?, parent);

        let _ = fs::remove_dir_all(root);
        Ok(())
    }

    #[test]
    fn graph_plan_prd_apply_preserves_parent_and_persists_child() -> Result<()> {
        let _env_lock = graph_store::ENV_LOCK
            .lock()
            .map_err(|_| anyhow::anyhow!("graph-store test environment lock poisoned"))?;
        let _home = graph_store::TestHome::new("prd-plan-apply")?;
        let root = temp_root("prd-plan-apply");
        let (parent, parent_hash) = seed_prd_plan_project(&root)?;
        fs::create_dir_all(root.join("docs"))?;
        fs::write(root.join("docs/plan.md"), sample_prd_markdown())?;
        let expected_child = prd_graph::compile_prd(
            &sample_prd_markdown(),
            "docs/plan.md",
            "INT-002",
            "INT-003",
            Some(&parent_hash),
        )?;
        let child_hash = expected_child.graph_hash.clone();

        let args = prd_plan_args(&root, &parent_hash, true);
        run_graph_plan_prd(&args)?;

        assert_eq!(project_file::load(&root)?.graph_hash, child_hash);
        assert_eq!(graph_store::load_graph(&parent_hash)?, parent);
        assert_eq!(graph_store::load_graph(&child_hash)?, expected_child.graph);

        let _ = fs::remove_dir_all(root);
        Ok(())
    }

    #[test]
    fn graph_plan_prd_rejects_parent_hash_drift_before_compile() -> Result<()> {
        let _env_lock = graph_store::ENV_LOCK
            .lock()
            .map_err(|_| anyhow::anyhow!("graph-store test environment lock poisoned"))?;
        let _home = graph_store::TestHome::new("prd-plan-parent-drift")?;
        let root = temp_root("prd-plan-parent-drift");
        let (_parent, parent_hash) = seed_prd_plan_project(&root)?;
        fs::create_dir_all(root.join("docs"))?;
        fs::write(root.join("docs/plan.md"), sample_prd_markdown())?;
        let before = fs::read(project_file::path(&root))?;
        let wrong_hash = format!("sha256:{}", "f".repeat(64));
        let args = prd_plan_args(&root, &wrong_hash, true);
        let error = run_graph_plan_prd(&args).expect_err("parent hash drift must fail");
        assert!(format!("{error:#}").contains("current project graph hash mismatch"));
        assert_eq!(fs::read(project_file::path(&root))?, before);
        assert_eq!(project_file::load(&root)?.graph_hash, parent_hash);

        let _ = fs::remove_dir_all(root);
        Ok(())
    }

    fn sample_prd_markdown() -> String {
        concat!(
            "1. [x] **INT-001 [SEQ] — Foundation**\n",
            "   - **Owner:** Lead\n",
            "   - **Depends on:** none\n",
            "   - Establish the foundation.\n",
            "   - **Acceptance:** Foundation is complete.\n",
            "2. [ ] **INT-002 [PAR-A] — Adapter**\n",
            "   - **Owner:** Worker\n",
            "   - **Depends on:** INT-001\n",
            "   - Build the adapter.\n",
            "   - **Acceptance:** Adapter is complete.\n",
            "3. [ ] **INT-003 [SEQ] — Compose**\n",
            "   - **Owner:** Lead\n",
            "   - **Depends on:** INT-002\n",
            "   - Compose the adapter.\n",
            "   - **Acceptance:** Composition is complete.\n",
        )
        .to_owned()
    }

    fn seed_prd_plan_project(root: &Path) -> Result<(serde_json::Value, String)> {
        let mut parent = json!({
            "schema": "fractal.execution_graph.v1",
            "graph_id": "prd-plan-parent",
            "nodes": [{
                "id": "parent-node",
                "capability": "code.generate",
                "instruction": "Parent fixture"
            }],
            "edges": []
        });
        let parent_hash = fractal_contracts::canonical_sha256(&parent)
            .map_err(|error| anyhow::anyhow!("hash PRD parent fixture: {error}"))?;
        parent["graph_hash"] = json!(parent_hash);
        graph_store::commit_graph(&parent)?;
        project_file::persist(root, &parent, "PRD plan test")?;
        Ok((parent, parent_hash))
    }

    fn prd_plan_args(
        root: &Path,
        expected_parent_hash: &str,
        yes: bool,
    ) -> crate::cli::GraphPlanPrdArgs {
        crate::cli::GraphPlanPrdArgs {
            repo: root.to_owned(),
            prd: Path::new("docs/plan.md").to_owned(),
            from: "INT-002".to_owned(),
            through: "INT-003".to_owned(),
            expected_parent_hash: expected_parent_hash.to_owned(),
            yes,
            json: false,
        }
    }

    fn temp_root(name: &str) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!(
            "fractal-cli-main-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn write_empty_inventory(root: &Path) -> std::path::PathBuf {
        let path = root.join("inventory.json");
        fs::write(&path, br#"{"schema":"fractal.repository_inventory.v1","inventory_hash":"sha256:0000000000000000000000000000000000000000000000000000000000000000","records":[]}"#).unwrap();
        path
    }

    #[test]
    fn rejects_contradictory_efficiency_configuration() {
        let cli = Cli::try_parse_from([
            "fractal",
            "efficiency",
            "--mode",
            "observe",
            "--approve-intervention",
            "merge",
        ])
        .unwrap();
        let error = run(cli).unwrap_err();
        assert!(error.to_string().contains("contradictory"));

        let unsafe_run = Cli::try_parse_from([
            "fractal",
            "run",
            "--work",
            "work-7",
            "--allow-high-impact",
            "cancel",
        ])
        .unwrap();
        assert!(run(unsafe_run).is_err());
    }
}
