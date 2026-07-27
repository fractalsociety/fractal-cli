mod amendments;
mod auth;
mod board;
mod bridge;
mod chain;
mod checkpoint;
mod cli;
mod compile;
mod contribute;
mod coordinate;
mod dataevol;
mod decompose;
mod evolve;
mod execute;
mod graph_store;
mod handoff;
mod harness;
mod harness_evolution;
mod ingest;
mod intent;
mod interactive;
mod mobile;
mod orchestrate;
mod pipeline;
mod project_file;
mod project_sync;
mod projects;
mod rlvr;
mod router;
mod run;
mod run_control;
mod safety;
mod social;
mod supervise;
mod ui;
mod verify;
mod voice;
mod work_builder;

use std::process::ExitCode;

use anyhow::Result;
use clap::Parser;

use crate::cli::{BridgeCommand, Cli, Command, GraphCommand};
use crate::work_builder::IntentClassification;

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error:#}");
            ExitCode::FAILURE
        }
    }
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
        },
        (None, Some(Command::Run(args))) if args.local => run_local(&args),
        (None, Some(Command::Run(args))) => match args.graph.as_deref() {
            Some(graph_hash) => run::run_graph(
                graph_hash,
                args.db.as_deref(),
                args.squad_bin.as_deref(),
                args.watch,
                args.dry_run,
            ),
            None => {
                pipeline::print_run_stub(args.work.as_deref());
                Ok(())
            }
        },
        (None, Some(Command::Evolve(args))) => evolve::run_evolve(&args),
        (None, Some(Command::Node(args))) => {
            pipeline::print_node_stub(&args.id, args.show, args.retry, args.cancel);
            Ok(())
        }
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
                    let status = match checkpoint::find_resumable(std::path::Path::new(
                        &project.workspace,
                    )) {
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
/// serving a live board that turns green as nodes complete.
fn run_local(args: &crate::cli::RunArgs) -> Result<()> {
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
        anyhow::bail!("no agents (claude/codex/cursor) on PATH");
    }

    let port = crate::cli::DEFAULT_GRAPH_PORT;
    let board_url = format!("http://127.0.0.1:{port}");
    if let Err(error) = board::serve_graph_file(&graph_file, port, None, args.dry_run) {
        eprintln!("(board unavailable: {error:#})");
    }
    std::thread::sleep(std::time::Duration::from_millis(1500));

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dispatches_version() {
        let cli = Cli::try_parse_from(["fractal", "version"]).unwrap();
        assert!(run(cli).is_ok());
    }
}
