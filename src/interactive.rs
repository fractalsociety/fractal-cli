//! `fractal` with no arguments: an interactive session, like `claude` or
//! `codex`. It asks to trust the current folder, then reads natural-language
//! requests and turns each one into a committed execution graph on a live board.

use std::collections::BTreeSet;
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

    // Stop/restart support: if a build in this folder was interrupted, offer to
    // resume it — reloading its graph and skipping the tasks already completed.
    if !agents.is_empty() {
        if let Some(cp) = crate::checkpoint::find_resumable(&workspace) {
            let short: String = cp.request.chars().take(88).collect();
            let answer = ask(&format!(
                "↻ An earlier build here was interrupted:\n    \"{short}\"\n    {}/{} tasks done. Resume it? [Y/n]: ",
                cp.completed.len(),
                cp.total
            ))?;
            if matches!(answer.to_ascii_lowercase().as_str(), "" | "y" | "yes") {
                println!(
                    "↻ Resuming — {} of {} tasks already done, continuing the rest…\n",
                    cp.completed.len(),
                    cp.total
                );
                let completed: BTreeSet<String> = cp.completed.iter().cloned().collect();
                let classification = intent::fractalwork_dir(fractalwork_override)
                    .and_then(|dir| intent::classify(&cp.request, &dir));
                let task_group = task_group_for(&classification, &cp.request);
                drive_committed_graph(
                    &cp.current_graph_hash,
                    &cp.request,
                    &workspace,
                    &agents,
                    backend,
                    DEFAULT_GRAPH_PORT,
                    &task_group,
                    &completed,
                    Some(&completed),
                    false,
                );
            } else {
                crate::checkpoint::discard(&cp.key);
                println!("  Discarded the interrupted run — starting fresh.\n");
            }
        }
    }

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
    // On exit, GRPO-train an adapter from the session's accumulated verifiable
    // rewards (skipped when fractal-rlvr is absent or there is too little data).
    match crate::rlvr::train() {
        Ok(Some(report)) => {
            println!("\n⛭ GRPO-trained an adapter from accumulated verifiable rewards:");
            println!("  {report}");
        }
        Ok(None) => {}
        Err(error) => eprintln!("  (rlvr training skipped: {error:#})"),
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
    println!("  The lead writes the PRD, architecture, acceptance criteria, and task DAG.");
    println!("  Fractal validates and compiles it, workers build it, and the lead closes it.");
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
        println!("No build agents (claude/codex/cursor/hermes) found on PATH — preview only.\n");
        return Ok(Vec::new());
    }

    println!("Build your agent team — add as many agents as you want.");
    println!("The FIRST one you add is the lead (plans + closes out); the rest are workers.");
    println!("Type a number to add an agent; press Enter on an empty line (or type `done`) when finished.");
    println!("Type `0` (or `none`) for preview only — compile graphs, run no workers.\n");

    let mut roster: Vec<String> = Vec::new();
    loop {
        for (index, agent) in available.iter().enumerate() {
            let role = if roster.first().map(String::as_str) == Some(agent.as_str()) {
                "  ✓ lead"
            } else if roster.contains(agent) {
                "  ✓ worker"
            } else {
                ""
            };
            println!("  {}) {agent}{role}", index + 1);
        }
        let prompt = if roster.is_empty() {
            "add agent > ".to_owned()
        } else {
            format!(
                "add another ({} selected) — or Enter to start > ",
                roster.len()
            )
        };
        let choice = ask(&prompt)?;
        let choice = choice.trim();

        if choice.is_empty() || choice.eq_ignore_ascii_case("done") {
            break;
        }
        if choice == "0" || choice.eq_ignore_ascii_case("none") {
            println!("\nPreview only — requests compile a graph + open its board, no workers.\n");
            return Ok(Vec::new());
        }
        let Ok(number) = choice.parse::<usize>() else {
            println!(
                "  (enter a number 1..{}, `done` to finish, or `0` for none)",
                available.len()
            );
            continue;
        };
        if number < 1 || number > available.len() {
            println!("  (enter a number 1..{})", available.len());
            continue;
        }
        let agent = available[number - 1].clone();
        if roster.contains(&agent) {
            println!("  ({agent} is already on the team)");
            continue;
        }
        let is_lead = roster.is_empty();
        if let Some(model) = pick_model(&agent)? {
            std::env::set_var(model_env_key(&agent), &model);
            println!(
                "  → added {agent} on model '{model}'{}",
                if is_lead { " (lead)" } else { " (worker)" }
            );
        } else {
            println!(
                "  → added {agent}{}",
                if is_lead { " (lead)" } else { " (worker)" }
            );
        }
        roster.push(agent);
    }

    if roster.is_empty() {
        println!("\nPreview only — no agents selected.\n");
    } else if roster.len() == 1 {
        println!("\nAgent: {} (lead). Type a request to build.\n", roster[0]);
    } else {
        println!(
            "\nTeam: {} (lead){}. Each checks out nodes from the graph until it is done.\n",
            roster[0],
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
    let menu: &[&str] = match agent {
        "claude" => &["fable", "opus", "sonnet"],
        // hermes is powered by an OpenRouter model; offer free nemotron variants.
        "hermes" => &[
            "nvidia/nemotron-nano-9b-v2:free",
            "nvidia/nemotron-3-super-120b-a12b:free",
            "nvidia/nemotron-3-ultra-550b-a55b:free",
        ],
        _ => &[],
    };
    if !menu.is_empty() {
        println!("Model for {agent}:");
        for (index, model) in menu.iter().enumerate() {
            println!("  {}) {model}", index + 1);
        }
        println!("  {}) default / other (type a model id)", menu.len() + 1);
        let choice = ask("> ")?;
        if let Ok(number) = choice.parse::<usize>() {
            if number >= 1 && number <= menu.len() {
                return Ok(Some(menu[number - 1].to_owned()));
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
/// Echo the key committed-graph lines from a submit plan to the console.
fn print_committed_plan_lines(text: &str) {
    for line in text.lines() {
        if line.starts_with("Harness:")
            || line.starts_with("Graph id:")
            || line.starts_with("Graph hash:")
            || line.starts_with("Nodes:")
            || line.starts_with("Committed:")
        {
            println!("  {line}");
        }
    }
}

/// Drive one already-gated normalized input without opening the interactive
/// wizard. Voice automation may only operate in a workspace the user previously
/// trusted from the interactive CLI; a background service can never grant trust.
pub(crate) fn execute_ingested(
    request: &str,
    workspace_override: Option<&Path>,
    fractalwork_override: Option<&Path>,
    coordinate_flag: bool,
    port: u16,
) -> Result<Option<crate::execute::RunOutcome>> {
    // Voice/typed control command: "resume project 3" continues that numbered
    // project regardless of the current folder, instead of starting a build.
    if let Some(number) = crate::projects::parse_resume_command(request) {
        return resume_project(number, fractalwork_override, port, coordinate_flag);
    }

    let workspace = match workspace_override {
        Some(path) => path
            .canonicalize()
            .with_context(|| format!("cannot resolve ingest workspace {}", path.display()))?,
        None => std::env::current_dir().context("cannot resolve ingest workspace")?,
    };
    let trust_store = trust_store_path();
    if !trust_contains(&trust_store, &workspace) {
        anyhow::bail!(
            "voice ingest cannot execute in untrusted workspace {}; run `fractal` there once and approve trust",
            workspace.display()
        );
    }
    let agents = execute::detect_agents();
    if agents.is_empty() {
        anyhow::bail!("no build agents (claude/codex/cursor/hermes) found on PATH");
    }
    let backend = Backend::resolve(coordinate_flag);

    // Auto-resume an interrupted build in this project instead of re-planning from
    // scratch. Non-interactive callers (voice ingest, `fractal ios`) run headless,
    // so continuing the existing graph — rather than decomposing a fresh, different
    // one and throwing away completed work — is the right default. A build that
    // finished cleanly has no checkpoint, so this only fires on genuine leftovers.
    if let Some(cp) = crate::checkpoint::find_resumable(&workspace) {
        println!(
            "↻ Resuming the interrupted build in {} — {}/{} tasks already done, continuing the rest…\n",
            workspace.display(),
            cp.completed.len(),
            cp.total
        );
        let completed: BTreeSet<String> = cp.completed.iter().cloned().collect();
        let classification = intent::fractalwork_dir(fractalwork_override)
            .and_then(|dir| intent::classify(&cp.request, &dir));
        let task_group = task_group_for(&classification, &cp.request);
        return Ok(drive_committed_graph(
            &cp.current_graph_hash,
            &cp.request,
            &workspace,
            &agents,
            backend,
            port,
            &task_group,
            &completed,
            Some(&completed),
            false,
        ));
    }

    execute_request(
        request,
        &workspace,
        fractalwork_override,
        port,
        &agents,
        backend,
    )
}

/// Resume a project by its stable number — used by `fractal resume <N>` and by the
/// voice/typed "resume project N" command. Continues from the saved checkpoint.
pub(crate) fn resume_project(
    number: u32,
    fractalwork_override: Option<&Path>,
    port: u16,
    coordinate_flag: bool,
) -> Result<Option<crate::execute::RunOutcome>> {
    let Some(project) = crate::projects::by_number(number) else {
        anyhow::bail!("no project #{number} — run `fractal projects` to see the list");
    };
    let workspace = PathBuf::from(&project.workspace);
    let Some(cp) = crate::checkpoint::find_resumable(&workspace) else {
        println!(
            "Project #{number} ({}) has nothing to resume — it is complete or hasn't started.",
            project.label
        );
        return Ok(None);
    };
    if !trust_contains(&trust_store_path(), &workspace) {
        anyhow::bail!(
            "project #{number} workspace {} is no longer trusted; run `fractal` there and approve trust",
            workspace.display()
        );
    }
    let agents = execute::detect_agents();
    if agents.is_empty() {
        anyhow::bail!("no build agents (claude/codex/cursor/hermes) found on PATH");
    }
    println!(
        "↻ Resuming project #{number} ({}) — {}/{} tasks already done, continuing the rest…\n",
        project.label,
        cp.completed.len(),
        cp.total
    );
    let completed: BTreeSet<String> = cp.completed.iter().cloned().collect();
    let classification = intent::fractalwork_dir(fractalwork_override)
        .and_then(|dir| intent::classify(&cp.request, &dir));
    let task_group = task_group_for(&classification, &cp.request);
    Ok(drive_committed_graph(
        &cp.current_graph_hash,
        &cp.request,
        &workspace,
        &agents,
        Backend::resolve(coordinate_flag),
        port,
        &task_group,
        &completed,
        Some(&completed),
        false,
    ))
}

fn execute_request(
    request: &str,
    workspace: &Path,
    fractalwork_override: Option<&Path>,
    port: u16,
    agents: &[String],
    backend: Backend,
) -> Result<Option<crate::execute::RunOutcome>> {
    let _run = crate::run_control::RunGuard::start_or_join(workspace, request, port)?;
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

    // Lead-planning path: every ordinary request becomes a structured PRD,
    // architecture, acceptance contract, and validated task DAG before workers
    // execute. Deterministic compilation remains the fail-soft fallback.
    let planning_browser_opened = if agents.is_empty() {
        false
    } else {
        publish_and_open_planning_preview(request, workspace, &agents[0])
    };
    let committed_hash = if agents.is_empty() {
        let plan = pipeline::render_submit_plan(
            request,
            Some(Mode::Build),
            None,
            Some(workspace),
            mapped,
        )?;
        print_committed_plan_lines(&plan.text);
        plan.committed_graph_hash
    } else {
        match crate::decompose::plan_and_commit(request, workspace, agents) {
            Ok(hash) => {
                println!("  Harness: harness.lead_planned_project.v1 (request → PRD → validated task DAG)");
                if let Ok(graph) = graph_store::load_graph(&hash) {
                    let count = |key| {
                        graph
                            .get(key)
                            .and_then(|v| v.as_array())
                            .map_or(0, Vec::len)
                    };
                    println!("  Graph hash: {hash}");
                    println!("  Nodes: {}  Edges: {}", count("nodes"), count("edges"));
                }
                Some(hash)
            }
            Err(error) => {
                eprintln!(
                    "  (lead planning failed: {error:#}; using the deterministic fallback harness)"
                );
                let plan = pipeline::render_submit_plan(
                    request,
                    Some(Mode::Build),
                    None,
                    Some(workspace),
                    mapped,
                )?;
                print_committed_plan_lines(&plan.text);
                plan.committed_graph_hash
            }
        }
    };

    let outcome = match committed_hash {
        Some(hash) => {
            let task_group = task_group_for(&classification, request);
            drive_committed_graph(
                &hash,
                request,
                workspace,
                agents,
                backend,
                port,
                &task_group,
                &BTreeSet::new(),
                None,
                planning_browser_opened,
            )
        }
        None => {
            println!("  (no graph committed)\n");
            None
        }
    };
    Ok(outcome)
}

/// Serve a committed graph's board and drive it to completion with the agent team,
/// applying router evolution. Shared by fresh runs and resume (which pre-seeds the
/// completed tasks so they are not re-run and shows them already green on the board).
#[allow(clippy::too_many_arguments)]
fn drive_committed_graph(
    hash: &str,
    request: &str,
    workspace: &Path,
    agents: &[String],
    backend: Backend,
    port: u16,
    task_group: &str,
    resume_completed: &BTreeSet<String>,
    board_preseed: Option<&BTreeSet<String>>,
    browser_already_open: bool,
) -> Option<crate::execute::RunOutcome> {
    let _run = match crate::run_control::RunGuard::start_or_join(workspace, request, port) {
        Ok(run) => run,
        Err(error) => {
            eprintln!("  run-control note: {error:#}");
            return None;
        }
    };
    let project_url = match graph_store::load_graph(hash)
        .and_then(|graph| crate::project_file::persist(workspace, &graph, request))
    {
        Ok(path) => {
            println!("  ◇ Project graph: {}", path.display());
            crate::project_sync::maybe_sync(workspace)
        }
        Err(error) => {
            eprintln!("  project graph note: {error:#}");
            None
        }
    };
    let number = crate::projects::register(workspace);
    println!(
        "  📁 Project #{number} · {} (say \"resume project {number}\" to continue later)",
        workspace
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default()
    );
    let (browser_target, is_cloud_target) = board::browser_target(project_url.as_deref(), port);
    println!(
        "  → opening the {} execution graph…",
        if is_cloud_target {
            "published Fractal Society"
        } else {
            "local"
        }
    );
    if let Err(error) = board::serve_graph(hash, port, None, is_cloud_target, board_preseed) {
        eprintln!("  (board unavailable: {error:#})");
    }
    if is_cloud_target && !browser_already_open {
        if let Err(error) = board::open_url(&browser_target) {
            eprintln!("  (Fractal Society page unavailable: {error:#}; opening local board)");
            let (local_board_url, _) = board::browser_target(None, port);
            if let Err(fallback_error) = board::open_url(&local_board_url) {
                eprintln!("  (local board unavailable: {fallback_error:#})");
            }
        }
    }
    if agents.is_empty() {
        println!(
            "  Graph is on the board. Building is off — enable it at launch to run workers.\n"
        );
        return None;
    }
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
    crate::run_control::set_graph(hash, &board_url);

    // Router evolution (closing the loop): before running, ask the accumulated
    // outcome memory which model is the cheapest *acceptable* one for this
    // task-kind, and pin it. Then record this run's outcome under the same key.
    let primary = agents[0].clone();
    let graph_value = graph_store::load_graph(hash).ok();
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
    let model0 = current_model(&primary);
    let group_id =
        crate::router::facts_for(task_group, &capabilities, &primary, &model0, &graph_id).group_id;
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
        task_group,
        &capabilities,
        &primary,
        &effective_model,
        &graph_id,
    );

    let spinner = crate::ui::Spinner::start("working");
    let outcome = crate::orchestrate::run_end_to_end(
        hash,
        workspace,
        agents,
        Some(&board_url),
        backend,
        &facts,
        request,
        resume_completed,
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
            if outcome.failed_node.is_none() {
                let _ = crate::project_sync::maybe_sync(workspace);
            }
            Some(outcome)
        }
        Err(error) => {
            eprintln!("  execution error: {error:#} · worked for {elapsed}\n");
            None
        }
    }
}

fn publish_and_open_planning_preview(request: &str, workspace: &Path, lead: &str) -> bool {
    let result = crate::decompose::commit_planning_preview(request, workspace, lead)
        .and_then(|hash| graph_store::load_graph(&hash))
        .and_then(|graph| crate::project_file::persist(workspace, &graph, request));
    let path = match result {
        Ok(path) => path,
        Err(error) => {
            eprintln!("  planning graph note: {error:#}");
            return false;
        }
    };
    println!("  ◇ Planning graph: {}", path.display());
    let Some(browser_url) = crate::project_sync::maybe_sync_planning(workspace) else {
        return false;
    };
    println!("  → opening the project now; it will refresh when planning completes…");
    match board::open_url(&browser_url) {
        Ok(()) => true,
        Err(error) => {
            eprintln!("  (could not open the planning graph: {error:#})");
            false
        }
    }
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
