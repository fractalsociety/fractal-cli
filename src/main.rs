mod board;
mod cli;
mod compile;
mod evolve;
mod execute;
mod graph_store;
mod harness;
mod intent;
mod interactive;
mod pipeline;
mod run;
mod safety;
mod ui;
mod work_builder;

use std::process::ExitCode;

use anyhow::Result;
use clap::Parser;

use crate::cli::{Cli, Command, GraphCommand};
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
        request,
        command,
    } = cli;
    match (request, command) {
        (None, None) => interactive::run(fractalwork.as_deref()),
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
        (None, Some(Command::Graph(args))) => match args.command {
            GraphCommand::Open => board::open(),
            GraphCommand::Board(args) => board::serve_graph(
                &args.graph_hash,
                args.port,
                args.exec_graph_dir.as_deref(),
                args.no_open,
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
        if no_open {
            println!(
                "Skipping viewer (--no-open). Open it with:\n  fractal graph board {graph_hash} --port {port}"
            );
        } else if let Err(error) = board::serve_graph(&graph_hash, port, None, false) {
            eprintln!("warning: could not auto-open the execution-graph viewer: {error:#}");
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
    let outcome = execute::run_multi_agent(&graph, &workspace, &agents, Some(&board_url))?;
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
