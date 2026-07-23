//! `fractal` with no arguments: an interactive session, like `claude` or
//! `codex`. It asks to trust the current folder, then reads natural-language
//! requests and turns each one into a committed execution graph on a live board.

use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::cli::{Mode, DEFAULT_GRAPH_PORT};
use crate::orchestrate::Backend;
use crate::work_builder::IntentClassification;
use crate::{board, execute, graph_store, intent, pipeline};

/// Launch the interactive session in the current working directory.
pub(crate) fn run(fractalwork_override: Option<&Path>, coordinate_flag: bool) -> Result<()> {
    let workspace = std::env::current_dir().context("cannot resolve the current directory")?;
    banner(&workspace);

    let mut backend = Backend::resolve(coordinate_flag);

    if !ensure_trusted(&workspace)? {
        println!("Not trusted — exiting. Nothing was read or run.");
        return Ok(());
    }
    println!("✓ Trusted {}\n", workspace.display());

    // With no agent env set, guide the user through picking a team. When
    // $FRACTAL_WORKER / $FRACTAL_AGENTS are set, honor them without prompting.
    let agents: Vec<String> =
        if std::env::var("FRACTAL_WORKER").is_ok() || std::env::var("FRACTAL_AGENTS").is_ok() {
            let roster = if std::env::var("FRACTAL_WORKER").is_ok() {
                vec![execute::worker_label()]
            } else {
                execute::detect_agents()
            };
            match roster.len() {
                0 => println!("No worker (claude/codex/cursor) on PATH — preview only.\n"),
                1 => println!("Building with: {} (from environment).\n", roster[0]),
                _ => println!("Agent team (from environment): {}.\n", roster.join(", ")),
            }
            roster
        } else {
            setup_agents()?
        };
    println!("Backend: {} (toggle with /backend).", backend.label());
    println!("Type what you want built. Commands: /help, /trust, /backend, /exit.\n");

    let stdin = io::stdin();
    let mut lines = stdin.lock().lines();
    let mut request_index: u16 = 0;
    loop {
        crate::ui::print_prompt();
        let Some(line) = lines.next() else {
            println!();
            break; // Ctrl-D / EOF
        };
        let line = line.context("failed to read input")?;
        let request = line.trim();
        if request.is_empty() {
            continue;
        }
        match request {
            "/exit" | "/quit" => break,
            "/help" => print_help(),
            "/trust" => println!("Workspace {} is trusted.", workspace.display()),
            "/backend" => {
                backend = match backend {
                    Backend::InProcess => Backend::Coordinate,
                    Backend::Coordinate => Backend::InProcess,
                };
                println!("Backend is now {}.", backend.label());
            }
            other if other.starts_with('/') => {
                println!("Unknown command: {other}. Try /help.");
            }
            other => {
                let port = DEFAULT_GRAPH_PORT.saturating_add(request_index);
                request_index = request_index.wrapping_add(1);
                if let Err(error) = execute_request(
                    other,
                    &workspace,
                    fractalwork_override,
                    port,
                    &agents,
                    backend,
                ) {
                    eprintln!("error: {error:#}");
                }
            }
        }
    }
    println!("Goodbye.");
    Ok(())
}

fn banner(workspace: &Path) {
    println!("\n  ▟▛ Fractal — the self-evolving build pipeline");
    println!("  Turns a request into a verified, evidenced execution graph.\n");
    println!("  Workspace: {}", workspace.display());
}

fn print_help() {
    println!("  Type a request in plain language, e.g.:");
    println!("    build a CLI that reverses a string, with a passing test");
    println!("  Fractal classifies it, compiles a task-faithful execution graph,");
    println!("  commits it, and opens its live board.");
    println!("  /backend switches between the in-process and Coordinate executors.");
    println!("  Commands: /help  /trust  /backend  /exit");
}

/// Resolve, prompt for, and persist trust for `workspace`.
fn ensure_trusted(workspace: &Path) -> Result<bool> {
    let store = trust_store_path();
    if trust_contains(&store, workspace) {
        return Ok(true);
    }
    print!(
        "Do you trust the files in this folder? Fractal may read, build, and run code here. [y/N]: "
    );
    io::stdout().flush().ok();
    let mut answer = String::new();
    io::stdin()
        .read_line(&mut answer)
        .context("failed to read trust response")?;
    let trusted = matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes");
    if trusted {
        persist_trust(&store, workspace)?;
    }
    Ok(trusted)
}

/// Read one trimmed line from stdin after printing `prompt`.
fn ask(prompt: &str) -> Result<String> {
    print!("{prompt}");
    io::stdout().flush().ok();
    let mut answer = String::new();
    io::stdin()
        .read_line(&mut answer)
        .context("failed to read input")?;
    Ok(answer.trim().to_owned())
}

/// `$FRACTAL_<AGENT>_MODEL` key for pinning a model.
fn model_env_key(agent: &str) -> String {
    format!(
        "FRACTAL_{}_MODEL",
        agent.to_ascii_uppercase().replace('-', "_")
    )
}

/// The model currently pinned for `agent`, or `"default"`.
fn current_model(agent: &str) -> String {
    std::env::var(model_env_key(agent))
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "default".to_owned())
}

/// A stable task-kind label so repeat requests of the same kind share a
/// counterfactual group (and thus compare models). Prefers the classified
/// intent; falls back to a slug of the request.
fn task_group_for(classification: &Result<intent::TaskClassification>, request: &str) -> String {
    if let Ok(classification) = classification {
        if !classification.intent.trim().is_empty() {
            return format!("fractal-cli:{}", classification.intent.trim());
        }
    }
    let slug = request
        .split_whitespace()
        .take(6)
        .collect::<Vec<_>>()
        .join("-")
        .to_lowercase();
    format!("fractal-cli:{slug}")
}

/// Guided team setup: primary agent → its model → other worker agents. Returns
/// the roster (primary first); empty means preview-only. Pins the primary's
/// model via the environment so the worker command honors it.
fn setup_agents() -> Result<Vec<String>> {
    let available = execute::available_agents();
    if available.is_empty() {
        println!("No build agents (claude/codex/cursor) found on PATH — preview only.\n");
        return Ok(Vec::new());
    }

    println!("Pick your primary agent (does the lead work). It spends that agent's credits.");
    for (index, agent) in available.iter().enumerate() {
        println!("  {}) {agent}", index + 1);
    }
    println!("  0) none — preview only (compile graphs, run no workers)");
    let primary_index = ask("> ")?.parse::<usize>().unwrap_or(0);
    if primary_index == 0 || primary_index > available.len() {
        println!("\nPreview only — requests compile a graph + open its board, no workers.\n");
        return Ok(Vec::new());
    }
    let primary = available[primary_index - 1].clone();

    if let Some(model) = pick_model(&primary)? {
        std::env::set_var(model_env_key(&primary), &model);
        println!("  → {primary} will run on model '{model}'.");
    }

    let others: Vec<String> = available
        .iter()
        .enumerate()
        .filter(|(index, _)| *index != primary_index - 1)
        .map(|(_, agent)| agent.clone())
        .collect();
    let mut roster = vec![primary.clone()];
    if !others.is_empty() {
        println!("\nAdd other worker agents (comma-separated numbers, or Enter for none):");
        for (index, agent) in others.iter().enumerate() {
            println!("  {}) {agent}", index + 1);
        }
        let line = ask("> ")?;
        for token in line.split(',') {
            if let Ok(number) = token.trim().parse::<usize>() {
                if number >= 1 && number <= others.len() && !roster.contains(&others[number - 1]) {
                    roster.push(others[number - 1].clone());
                }
            }
        }
    }

    if roster.len() == 1 {
        println!("\nAgent: {primary} (lead). Type a request to build.\n");
    } else {
        println!(
            "\nTeam: {primary} (lead){}. Each checks out nodes from the graph until it is done.\n",
            roster[1..]
                .iter()
                .map(|agent| format!(", {agent}"))
                .collect::<String>()
        );
    }
    Ok(roster)
}

/// Pick a model for the primary agent. Known Claude aliases are offered as a
/// menu; other agents accept a free-form model id (Enter = the agent's default).
fn pick_model(agent: &str) -> Result<Option<String>> {
    if agent == "claude" {
        println!("Model for claude:");
        let options = ["fable", "opus", "sonnet"];
        for (index, model) in options.iter().enumerate() {
            println!("  {}) {model}", index + 1);
        }
        println!("  {}) default", options.len() + 1);
        let choice = ask("> ")?;
        if let Ok(number) = choice.parse::<usize>() {
            if number >= 1 && number <= options.len() {
                return Ok(Some(options[number - 1].to_owned()));
            }
            return Ok(None); // default / out of range
        }
        return Ok(if choice.is_empty() {
            None
        } else {
            Some(choice)
        });
    }
    let typed = ask(&format!("Model for {agent} (Enter for default): "))?;
    Ok(if typed.is_empty() { None } else { Some(typed) })
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

fn trust_store_path() -> PathBuf {
    fractal_home().join("trusted-folders.txt")
}

fn canonical(workspace: &Path) -> String {
    std::fs::canonicalize(workspace)
        .unwrap_or_else(|_| workspace.to_path_buf())
        .display()
        .to_string()
}

fn trust_contains(store: &Path, workspace: &Path) -> bool {
    let target = canonical(workspace);
    std::fs::read_to_string(store)
        .map(|text| text.lines().any(|line| line.trim() == target))
        .unwrap_or(false)
}

fn persist_trust(store: &Path, workspace: &Path) -> Result<()> {
    if let Some(parent) = store.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let mut existing = std::fs::read_to_string(store).unwrap_or_default();
    if !existing.ends_with('\n') && !existing.is_empty() {
        existing.push('\n');
    }
    existing.push_str(&canonical(workspace));
    existing.push('\n');
    std::fs::write(store, existing)
        .with_context(|| format!("failed to persist trust to {}", store.display()))
}

/// Compile + commit a graph for one request and open its board.
fn execute_request(
    request: &str,
    workspace: &Path,
    fractalwork_override: Option<&Path>,
    port: u16,
    agents: &[String],
    backend: Backend,
) -> Result<()> {
    println!("\n→ Understanding: {request}");
    let classification = intent::fractalwork_dir(fractalwork_override)
        .and_then(|directory| intent::classify(request, &directory));
    let mapped = classification.as_ref().ok().map(map_classification);
    if let Ok(classification) = &classification {
        println!(
            "  intent: {}  privacy: {}",
            classification.intent, classification.privacy
        );
    }

    let plan =
        pipeline::render_submit_plan(request, Some(Mode::Build), None, Some(workspace), mapped)?;
    for line in plan.text.lines() {
        if line.starts_with("Harness:")
            || line.starts_with("Graph id:")
            || line.starts_with("Graph hash:")
            || line.starts_with("Nodes:")
            || line.starts_with("Committed:")
        {
            println!("  {line}");
        }
    }

    match plan.committed_graph_hash {
        Some(hash) => {
            println!("  → opening the live board for this graph…");
            if let Err(error) = board::serve_graph(&hash, port, None, false) {
                eprintln!("  (board unavailable: {error:#})");
            }
            if agents.is_empty() {
                println!("  Graph is on the board. Building is off — enable it at launch to run workers.\n");
            } else {
                if agents.len() > 1 {
                    println!(
                        "  → executing with {} agents in {} (board turns green live)…",
                        agents.len(),
                        workspace.display()
                    );
                } else {
                    println!("  → executing in {}…", workspace.display());
                }
                let board_url = format!("http://127.0.0.1:{port}");

                // Router evolution (closing the loop): before running, ask the
                // accumulated outcome memory which model is the cheapest
                // *acceptable* one for this task-kind, and pin it. Then record this
                // run's outcome under the same key so selection keeps improving.
                let primary = agents[0].clone();
                let graph_value = graph_store::load_graph(&hash).ok();
                let graph_id = graph_value
                    .as_ref()
                    .and_then(|graph| graph.get("graph_id").and_then(|value| value.as_str()))
                    .unwrap_or("graph")
                    .to_owned();
                let capabilities = graph_value
                    .as_ref()
                    .and_then(|graph| graph.get("nodes").and_then(|value| value.as_array()))
                    .map(|nodes| {
                        nodes
                            .iter()
                            .filter_map(|node| {
                                node.get("capability")
                                    .and_then(|value| value.as_str())
                                    .map(str::to_owned)
                            })
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                let task_group = task_group_for(&classification, request);
                let model0 = current_model(&primary);
                let group_id = crate::router::facts_for(
                    &task_group,
                    &capabilities,
                    &primary,
                    &model0,
                    &graph_id,
                )
                .group_id;
                if let Some(rec) = crate::router::recommend(&group_id) {
                    if let Some((agent, model)) = rec.chosen_option_id.split_once(':') {
                        if agent == primary && model != model0 {
                            std::env::set_var(model_env_key(&primary), model);
                            println!(
                                "  ↻ router: pinned {primary} model '{model}' — cheapest acceptable \
                                 for this task-kind ({} sample(s), {} option(s) seen)",
                                rec.samples,
                                rec.observed.len()
                            );
                        }
                    }
                }
                let effective_model = current_model(&primary);
                let facts = crate::router::facts_for(
                    &task_group,
                    &capabilities,
                    &primary,
                    &effective_model,
                    &graph_id,
                );

                let spinner = crate::ui::Spinner::start("working");
                let outcome = crate::orchestrate::run_end_to_end(
                    &hash,
                    workspace,
                    agents,
                    Some(&board_url),
                    backend,
                    &facts,
                );
                let elapsed = crate::ui::format_elapsed(spinner.stop());
                match outcome {
                    Ok(outcome) => {
                        let mark = match outcome.verified {
                            Some(true) => "✓",
                            Some(false) => "✗",
                            None if outcome.built => "✓",
                            None => "·",
                        };
                        println!("  {mark} {} · worked for {elapsed}\n", outcome.detail);
                    }
                    Err(error) => {
                        eprintln!("  execution error: {error:#} · worked for {elapsed}\n")
                    }
                }
            }
        }
        None => println!("  (no graph committed)\n"),
    }
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
