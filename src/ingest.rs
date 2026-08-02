//! Normalized multimodal ingress shared by every voice backend.
//!
//! Transcripts are data, never shell source. They enter through stdin, normalize
//! to `fractal.input.v1`, pass a conservative risk gate, and only then reach the
//! existing intent → graph → governed-execution pipeline.

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::{self, BufRead, IsTerminal, Read, Write};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::cli::{IngestArgs, InputFormat, VoiceArgs};

const INPUT_SCHEMA: &str = "fractal.input.v1";
const MAX_INPUT_BYTES: usize = 64 * 1024;
const DEFAULT_COMMAND_MODE: &str = "fractal-command";
const DICTATION_MODE: &str = "dictation";

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct InputEvent {
    pub(crate) schema: String,
    pub(crate) source: String,
    pub(crate) modality: String,
    pub(crate) content: String,
    #[serde(default = "default_command_mode")]
    pub(crate) mode: String,
    #[serde(default)]
    pub(crate) timestamp: String,
    #[serde(default)]
    pub(crate) context: BTreeMap<String, Value>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub(crate) enum Risk {
    ReadOnly,
    ReversibleWrite,
    Destructive,
    ExternalSideEffect,
}

impl std::fmt::Display for Risk {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::ReadOnly => "READ_ONLY",
            Self::ReversibleWrite => "REVERSIBLE_WRITE",
            Self::Destructive => "DESTRUCTIVE",
            Self::ExternalSideEffect => "EXTERNAL_SIDE_EFFECT",
        })
    }
}

fn default_command_mode() -> String {
    DEFAULT_COMMAND_MODE.to_owned()
}

/// Read and process one stdin event. Non-read-only voice events fail closed
/// unless the caller explicitly requested a typed `/dev/tty` confirmation.
pub(crate) fn run(
    args: &IngestArgs,
    fractalwork_override: Option<&Path>,
    coordinate: bool,
) -> Result<()> {
    let controls = crate::cli::EfficiencyOpts {
        efficiency_mode: args.efficiency.efficiency_mode,
        approve_intervention: args.efficiency.approve_intervention.clone(),
        override_intervention: args.efficiency.override_intervention.clone(),
        allow_high_impact: args.efficiency.allow_high_impact.clone(),
    };
    let efficiency = crate::efficiency_config::resolve(&controls)?;
    let event = read_event(args)?;
    if args.managed_project && !managed_project_source_allowed(&event.source) {
        bail!("managed project ingest is reserved for the native Fractal Voice app");
    }
    process_event(
        event,
        EventOptions {
            preview: args.preview,
            confirm: args.confirm,
            amend: args.amend,
            managed_project: args.managed_project,
            project_name: args.project_name.as_deref(),
            repo: args.repo.as_deref(),
            port: args.port,
            fractalwork_override,
            coordinate,
            efficiency: Some(&efficiency),
        },
    )
}

pub(crate) fn run_voice_transcript(
    transcript: &str,
    dictate: bool,
    args: &VoiceArgs,
    fractalwork_override: Option<&Path>,
    coordinate: bool,
) -> Result<()> {
    let mut context = BTreeMap::new();
    context.insert("engine".to_owned(), Value::String("moonshine".to_owned()));
    context.insert(
        "model".to_owned(),
        Value::String("moonshine-v2-medium-streaming".to_owned()),
    );
    let mut event = InputEvent {
        schema: INPUT_SCHEMA.to_owned(),
        source: "moonshine.v2.medium".to_owned(),
        modality: "voice".to_owned(),
        content: transcript.to_owned(),
        mode: if dictate {
            DICTATION_MODE
        } else {
            DEFAULT_COMMAND_MODE
        }
        .to_owned(),
        timestamp: now_rfc3339(),
        context,
    };
    normalize_and_validate(&mut event)?;
    process_event(
        event,
        EventOptions {
            preview: args.preview,
            confirm: args.confirm,
            amend: false,
            managed_project: false,
            project_name: None,
            repo: args.repo.as_deref(),
            port: args.port,
            fractalwork_override,
            coordinate,
            efficiency: None,
        },
    )
}

struct EventOptions<'a> {
    preview: bool,
    confirm: bool,
    amend: bool,
    managed_project: bool,
    project_name: Option<&'a str>,
    repo: Option<&'a Path>,
    port: u16,
    fractalwork_override: Option<&'a Path>,
    coordinate: bool,
    efficiency: Option<&'a crate::efficiency_config::EfficiencyConfig>,
}

fn process_event(event: InputEvent, options: EventOptions<'_>) -> Result<()> {
    let risk = classify_risk(&event.content);

    if options.preview {
        println!("{}", serde_json::to_string_pretty(&event)?);
        println!("risk: {risk}");
        return Ok(());
    }

    if event.mode == DICTATION_MODE {
        print!("{}", event.content);
        return Ok(());
    }
    if event.mode != DEFAULT_COMMAND_MODE {
        bail!(
            "unsupported input mode {:?}; expected {DEFAULT_COMMAND_MODE:?} or {DICTATION_MODE:?}",
            event.mode
        );
    }

    // Explicit amendment transport is fail-closed. It either queues work on
    // the active graph or returns an ordinary visible error; it can never reach
    // the generic build path or its /dev/tty confirmation prompt.
    if options.amend {
        if matches!(risk, Risk::Destructive | Risk::ExternalSideEffect) {
            bail!(
                "{risk} graph amendment was not queued; use a dedicated governed command for high-risk effects; no build was started"
            );
        }
        println!(
            "Normalized {} {} input · explicit graph amendment",
            event.source, event.modality
        );
        return match (options.repo, parse_graph_amendment(&event.content)) {
            (Some(workspace), Some((task_ref, instruction))) => {
                crate::run_control::queue_workspace_branch_amendment(
                    workspace,
                    &task_ref,
                    &instruction,
                )
            }
            (None, Some((task_ref, instruction))) => {
                crate::run_control::queue_active_amendment(&task_ref, &instruction)
            }
            (Some(workspace), None) => {
                crate::run_control::queue_workspace_project_amendment(workspace, &event.content)
            }
            (None, None) => crate::run_control::queue_active_project_amendment(&event.content),
        };
    }

    // Visibility changes must use the dedicated two-step command. Do not let a
    // voice agent accidentally queue one as an ordinary graph amendment and
    // then report the word "accepted" as if GitHub had changed.
    if let Some(target) = parse_visibility_intent(&event.content) {
        bail!(
            "project visibility was not changed; use `fractal visibility --project 'EXACT_PROJECT_NAME' --{target}`, read the warning, and repeat it with `--yes` only after explicit confirmation"
        );
    }

    // Mid-build graph amendments are intercepted before ordinary intent
    // execution so a spoken addition never starts a second project.
    if let Some((task_ref, instruction)) = parse_graph_amendment(&event.content) {
        println!(
            "Normalized {} {} input · graph amendment",
            event.source, event.modality
        );
        return crate::run_control::queue_active_amendment(&task_ref, &instruction);
    }

    // Control phrases are intercepted before ordinary intent execution. This is
    // deliberately narrow: a stop/status request must not become a new build.
    if let Some(control) = parse_run_control(&event.content) {
        println!(
            "Normalized {} {} input · run control",
            event.source, event.modality
        );
        return match control {
            VoiceRunControl::StopCurrent => crate::run_control::stop(&crate::cli::StopArgs {
                project: None,
                all: false,
            }),
            VoiceRunControl::StopAll => crate::run_control::stop(&crate::cli::StopArgs {
                project: None,
                all: true,
            }),
            VoiceRunControl::StopProject(project) => {
                crate::run_control::stop(&crate::cli::StopArgs {
                    project: Some(project),
                    all: false,
                })
            }
            VoiceRunControl::StatusRunning => {
                crate::run_control::status(&crate::cli::StatusArgs { running: true })
            }
        };
    }

    // "resume project N" is a control command that continues an already-approved
    // project — route it directly, bypassing the build write-confirmation gate so
    // it works hands-free by voice.
    if let Some(number) = crate::projects::parse_resume_command(&event.content) {
        println!(
            "Normalized {} {} input · resume command",
            event.source, event.modality
        );
        return crate::interactive::resume_project(
            number,
            options.fractalwork_override,
            options.port,
            options.coordinate,
        )
        .map(|_| ());
    }

    let managed_workspace = if options.managed_project {
        if !managed_project_risk_allowed(risk) {
            bail!(
                "{risk} voice input cannot run automatically from the Fractal Voice app; \
                 review and run it from a terminal"
            );
        }
        crate::project_sync::ensure_new_project_name_available(
            options.project_name.unwrap_or(&event.content),
        )?;
        Some(crate::interactive::prepare_managed_voice_workspace(
            options.project_name.unwrap_or(&event.content),
            &event.content,
        )?)
    } else {
        None
    };

    println!(
        "Normalized {} {} input · risk {risk}",
        event.source, event.modality
    );
    if risk != Risk::ReadOnly {
        println!(
            "Interpreted instruction:\n  {}",
            event.content.replace('\n', "\n  ")
        );
        io::stdout().flush().ok();
        if managed_workspace.is_some() && risk == Risk::ReversibleWrite {
            println!(
                "Managed project approval: reversible build scoped to {}",
                managed_workspace
                    .as_deref()
                    .expect("managed workspace exists")
                    .display()
            );
        } else if !options.confirm {
            bail!(
                "{risk} voice input was not executed; review it and rerun manually with --confirm for typed confirmation"
            );
        } else {
            confirm_on_tty(&event, risk)?;
        }
    }

    crate::interactive::execute_ingested(
        &event.content,
        managed_workspace.as_deref().or(options.repo),
        options.fractalwork_override,
        options.coordinate,
        options.port,
        options.efficiency,
    )
    .map(|_| ())
}

fn managed_project_source_allowed(source: &str) -> bool {
    source == "fractal-mac-app"
}

fn managed_project_risk_allowed(risk: Risk) -> bool {
    matches!(risk, Risk::ReadOnly | Risk::ReversibleWrite)
}

#[derive(Debug, Eq, PartialEq)]
enum VoiceRunControl {
    StopCurrent,
    StopAll,
    StopProject(String),
    StatusRunning,
}

fn parse_graph_amendment(input: &str) -> Option<(String, String)> {
    let lower = input.to_ascii_lowercase();
    if !lower.contains("add") || !lower.contains("branch") {
        return None;
    }
    let task_start = lower.find("task")? + "task".len();
    let after_task = input.get(task_start..)?.trim_start();
    let task_ref = after_task
        .split_whitespace()
        .next()?
        .trim_matches(|character: char| !character.is_ascii_digit() && character != '.')
        .to_owned();
    let (wave, position) = task_ref.split_once('.')?;
    if wave.is_empty()
        || position.is_empty()
        || !wave.bytes().all(|byte| byte.is_ascii_digit())
        || !position.bytes().all(|byte| byte.is_ascii_digit())
        || position == "0"
    {
        return None;
    }
    let task_ref_offset = after_task.find(&task_ref)? + task_ref.len();
    let remainder = after_task.get(task_ref_offset..)?.trim();
    let remainder_lower = remainder.to_ascii_lowercase();
    let branch_offset = remainder_lower.find("branch")? + "branch".len();
    let mut instruction = remainder.get(branch_offset..)?.trim();
    for prefix in [
        "and this branch will add",
        "this branch will add",
        "that will add",
        "which will add",
        "will add",
        "to add",
        "add",
    ] {
        if instruction.to_ascii_lowercase().starts_with(prefix) {
            instruction = instruction.get(prefix.len()..)?.trim();
            break;
        }
    }
    let instruction = instruction
        .trim_matches(|character: char| {
            character.is_ascii_punctuation() || character.is_whitespace()
        })
        .to_owned();
    (!instruction.is_empty()).then_some((task_ref, instruction))
}

fn parse_visibility_intent(input: &str) -> Option<&'static str> {
    let lower = input.trim().to_ascii_lowercase();
    let target = if lower.contains("public") {
        "public"
    } else if lower.contains("private") {
        "private"
    } else {
        return None;
    };
    let action = ["make ", "set ", "change ", "toggle ", "switch "]
        .iter()
        .any(|word| lower.starts_with(word) || lower.contains(&format!(" {word}")));
    let subject = ["project", "repository", "repo", "graph", "visibility"]
        .iter()
        .any(|word| lower.contains(word));
    (action && subject).then_some(target)
}

fn parse_run_control(input: &str) -> Option<VoiceRunControl> {
    let cleaned = input
        .trim()
        .trim_matches(|character: char| character.is_ascii_punctuation())
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let lower = cleaned.to_ascii_lowercase();
    let starts_stop = ["stop ", "pause ", "halt ", "cancel ", "abort "]
        .iter()
        .any(|prefix| lower.starts_with(prefix));
    if starts_stop
        && lower.contains("all")
        && ["fractal", "build", "project", "process"]
            .iter()
            .any(|word| lower.contains(word))
    {
        return Some(VoiceRunControl::StopAll);
    }
    if starts_stop {
        for marker in [
            "stop project ",
            "pause project ",
            "halt project ",
            "cancel project ",
            "abort project ",
        ] {
            if let Some(project) = lower.strip_prefix(marker) {
                let project = project.trim();
                if !project.is_empty()
                    && !matches!(project, "build" | "current" | "the current build")
                {
                    return Some(VoiceRunControl::StopProject(project.to_owned()));
                }
            }
        }
        if ["fractal", "build", "project", "current"]
            .iter()
            .any(|word| lower.contains(word))
        {
            return Some(VoiceRunControl::StopCurrent);
        }
    }
    let asks_status = ["status", "show ", "list ", "check "]
        .iter()
        .any(|prefix| lower.starts_with(prefix));
    if asks_status
        && lower.contains("running")
        && ["fractal", "build", "project"]
            .iter()
            .any(|word| lower.contains(word))
    {
        return Some(VoiceRunControl::StatusRunning);
    }
    None
}

fn read_event(args: &IngestArgs) -> Result<InputEvent> {
    if !args.stdin && atty_like_stdin() {
        bail!("ingest reads stdin; pipe a transcript or pass --stdin explicitly");
    }
    let mut input = String::new();
    io::stdin()
        .take((MAX_INPUT_BYTES + 1) as u64)
        .read_to_string(&mut input)
        .context("read input event from stdin")?;
    if input.len() > MAX_INPUT_BYTES {
        bail!("input exceeds the {MAX_INPUT_BYTES}-byte limit");
    }

    let format = if args.json {
        InputFormat::Json
    } else {
        args.format
    };
    let mut event = match format {
        InputFormat::Text => InputEvent {
            schema: INPUT_SCHEMA.to_owned(),
            source: args.source.clone(),
            modality: if args.source.eq_ignore_ascii_case("superwhisper") {
                "voice".to_owned()
            } else {
                "text".to_owned()
            },
            content: input,
            mode: args.mode.clone(),
            timestamp: now_rfc3339(),
            context: BTreeMap::new(),
        },
        InputFormat::Json => {
            serde_json::from_str::<InputEvent>(&input).context("parse fractal.input.v1 JSON")?
        }
    };
    normalize_and_validate(&mut event)?;
    Ok(event)
}

fn normalize_and_validate(event: &mut InputEvent) -> Result<()> {
    if event.schema != INPUT_SCHEMA {
        bail!(
            "unsupported input schema {:?}; expected {INPUT_SCHEMA:?}",
            event.schema
        );
    }
    event.source = event.source.trim().to_owned();
    event.modality = event.modality.trim().to_ascii_lowercase();
    event.mode = event.mode.trim().to_ascii_lowercase();
    event.content = event.content.trim().to_owned();
    if event.timestamp.trim().is_empty() {
        event.timestamp = now_rfc3339();
    }
    if event.source.is_empty()
        || event.source.len() > 64
        || !event
            .source
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | ':' | '-'))
    {
        bail!("source must be 1-64 characters from [A-Za-z0-9._:-]");
    }
    if !matches!(event.modality.as_str(), "voice" | "text") {
        bail!("modality must be \"voice\" or \"text\"");
    }
    if !matches!(event.mode.as_str(), DEFAULT_COMMAND_MODE | DICTATION_MODE) {
        bail!("mode must be {DEFAULT_COMMAND_MODE:?} or {DICTATION_MODE:?}");
    }
    if !looks_like_rfc3339(&event.timestamp) {
        bail!("timestamp must be an RFC 3339 date-time");
    }
    if event.content.is_empty() {
        bail!("input content is empty");
    }
    if event.content.len() > MAX_INPUT_BYTES {
        bail!("input content exceeds the {MAX_INPUT_BYTES}-byte limit");
    }
    Ok(())
}

/// Conservative deterministic classifier. Specific dangerous classes win over
/// generic read verbs ("show and then delete" is destructive, never read-only).
pub(crate) fn classify_risk(content: &str) -> Risk {
    let text = content.to_ascii_lowercase();
    let always_external = [
        "send ",
        "reply to ",
        "publish ",
        "post ",
        "deploy ",
        "push ",
        "submit ",
        "upload ",
        "open a pull request",
        "create a pull request",
        "call the api",
        "call an api",
    ];
    if always_external.iter().any(|term| text.contains(term)) {
        return Risk::ExternalSideEffect;
    }
    let economic_external = ["purchase ", "buy ", "pay ", "transfer "];
    if economic_external.iter().any(|term| text.contains(term))
        && !is_bounded_wallet_simulation(&text)
    {
        return Risk::ExternalSideEffect;
    }
    let destructive = [
        "delete ",
        "remove ",
        "wipe ",
        "erase ",
        "destroy ",
        "drop database",
        "truncate ",
        "reset --hard",
        "force push",
        "uninstall ",
        "revoke ",
        "kill all",
    ];
    if destructive.iter().any(|term| text.contains(term)) {
        return Risk::Destructive;
    }
    let writes = [
        "create ",
        "write ",
        "edit ",
        "modify ",
        "update ",
        "fix ",
        "repair ",
        "build ",
        "implement ",
        "install ",
        "start ",
        "run ",
        "commit ",
        "branch ",
        "rename ",
        "move ",
        "change ",
    ];
    if writes.iter().any(|term| text.contains(term)) {
        return Risk::ReversibleWrite;
    }
    let read_prefixes = [
        "show ",
        "list ",
        "check ",
        "inspect ",
        "read ",
        "review ",
        "summarize ",
        "search ",
        "find ",
        "explain ",
        "report ",
        "navigate ",
        "what ",
        "where ",
        "how ",
        "status",
    ];
    if read_prefixes
        .iter()
        .any(|prefix| text.trim_start().starts_with(prefix))
    {
        Risk::ReadOnly
    } else {
        // Unknown voice intent is never assumed harmless.
        Risk::ReversibleWrite
    }
}

/// Allows economic verbs to describe a local/internal simulation without
/// granting authority for live settlement. The exemption is deliberately
/// narrow: it needs both an explicit simulation marker and build/design intent,
/// and any phrase suggesting real signing, broadcasting, funds, or a live chain
/// restores the external-side-effect classification.
fn is_bounded_wallet_simulation(text: &str) -> bool {
    let simulation_markers = [
        "simulated wallet",
        "simulation-only wallet",
        "wallet simulation",
        "synthetic economy",
        "simulated economy",
        "mock wallet",
        "mock blockchain",
        "local simulation",
    ];
    let build_intent = [
        "build ",
        "create ",
        "design ",
        "implement ",
        "prototype ",
        "specify ",
        "simulate ",
    ];
    let live_effect_markers = [
        "send funds",
        "move funds",
        "transfer tokens",
        "transfer funds",
        "transfer assets",
        "transfer real",
        "pay real",
        "buy real",
        "purchase real",
        "sign transaction",
        "sign and broadcast",
        "broadcast transaction",
        "execute transfer",
        "execute payment",
        "on-chain transfer",
        "onchain transfer",
        "deploy contract",
        "connect my wallet",
        "connect to my wallet",
    ];

    simulation_markers.iter().any(|term| text.contains(term))
        && build_intent.iter().any(|term| text.contains(term))
        && !live_effect_markers.iter().any(|term| text.contains(term))
}

fn looks_like_rfc3339(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() >= 20
        && bytes.get(4) == Some(&b'-')
        && bytes.get(7) == Some(&b'-')
        && matches!(bytes.get(10), Some(b'T' | b't' | b' '))
        && bytes.get(13) == Some(&b':')
        && bytes.get(16) == Some(&b':')
        && (value.ends_with('Z')
            || value.ends_with('z')
            || value
                .get(19..)
                .is_some_and(|suffix| suffix.contains('+') || suffix.contains('-')))
}

fn event_fingerprint(event: &InputEvent) -> Result<String> {
    let bytes = serde_json::to_vec(event)?;
    let digest = Sha256::digest(bytes);
    Ok(digest[..6]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

fn confirm_on_tty(event: &InputEvent, risk: Risk) -> Result<()> {
    let fingerprint = event_fingerprint(event)?;
    let expected = format!("CONFIRM {fingerprint}");
    let mut tty = OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
        .context(
            "typed confirmation requires an interactive terminal; voice confirmation is refused",
        )?;
    writeln!(
        tty,
        "{risk} input requires typed confirmation. Type exactly: {expected}"
    )?;
    write!(tty, "> ")?;
    tty.flush()?;
    let mut answer = String::new();
    io::BufReader::new(tty)
        .read_line(&mut answer)
        .context("read typed confirmation")?;
    if answer.len() > 128 {
        bail!("confirmation was too long; nothing was executed");
    }
    if answer.trim() != expected {
        bail!("confirmation did not match; nothing was executed");
    }
    Ok(())
}

fn atty_like_stdin() -> bool {
    io::stdin().is_terminal()
}

fn now_rfc3339() -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let days = seconds.div_euclid(86_400);
    let day_seconds = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let hour = day_seconds / 3_600;
    let minute = day_seconds % 3_600 / 60;
    let second = day_seconds % 60;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

// Howard Hinnant's civil-from-days algorithm; `days` is relative to 1970-01-01.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(content: &str) -> InputEvent {
        InputEvent {
            schema: INPUT_SCHEMA.to_owned(),
            source: "superwhisper".to_owned(),
            modality: "voice".to_owned(),
            content: content.to_owned(),
            mode: DEFAULT_COMMAND_MODE.to_owned(),
            timestamp: String::new(),
            context: BTreeMap::new(),
        }
    }

    #[test]
    fn normalizes_a_json_voice_event() {
        let mut value = event("  Show experiment status.  ");
        normalize_and_validate(&mut value).unwrap();
        assert_eq!(value.content, "Show experiment status.");
        assert!(value.timestamp.ends_with('Z'));
    }

    #[test]
    fn rejects_unknown_schema_and_empty_content() {
        let mut wrong = event("hello");
        wrong.schema = "fractal.input.v2".to_owned();
        assert!(normalize_and_validate(&mut wrong).is_err());
        let mut empty = event("   ");
        assert!(normalize_and_validate(&mut empty).is_err());
        let mut bad_mode = event("hello");
        bad_mode.mode = "unsafe-bypass".to_owned();
        assert!(normalize_and_validate(&mut bad_mode).is_err());
        let mut bad_timestamp = event("hello");
        bad_timestamp.timestamp = "yesterday".to_owned();
        assert!(normalize_and_validate(&mut bad_timestamp).is_err());
    }

    #[test]
    fn risk_order_fails_closed() {
        assert_eq!(classify_risk("Show experiment status"), Risk::ReadOnly);
        assert_eq!(
            classify_risk("Create a new benchmark branch"),
            Risk::ReversibleWrite
        );
        assert_eq!(
            classify_risk("Show status and delete all failed runs"),
            Risk::Destructive
        );
        assert_eq!(
            classify_risk("Summarize this and send an email"),
            Risk::ExternalSideEffect
        );
        assert_eq!(classify_risk("Review email regressions"), Risk::ReadOnly);
        assert_eq!(classify_risk("Do the thing"), Risk::ReversibleWrite);
    }

    #[test]
    fn simulated_internal_wallet_build_is_reversible() {
        assert_eq!(
            classify_risk(
                "Build a synthetic economy with simulated wallets for my internal blockchain. \
                 Agents purchase simulated inference and transfer simulated credits. \
                 Do not sign or broadcast real transactions."
            ),
            Risk::ReversibleWrite
        );
        assert_eq!(
            classify_risk(
                "Design a local simulation where mock wallets buy tool services and pay \
                 other test agents."
            ),
            Risk::ReversibleWrite
        );
        assert_eq!(
            classify_risk(
                "Build simulated wallets for my internal blockchain where agents purchase \
                 simulated services and transfer simulated credits. No real funds, signing, \
                 broadcasting, mainnet activity, custody, or irreversible transfers."
            ),
            Risk::ReversibleWrite
        );
    }

    #[test]
    fn wallet_simulation_exemption_fails_closed_for_live_effects() {
        assert_eq!(
            classify_risk(
                "Build simulated wallets, then sign and broadcast a transfer on mainnet."
            ),
            Risk::ExternalSideEffect
        );
        assert_eq!(
            classify_risk(
                "Design a synthetic economy and send an email when agents purchase credits."
            ),
            Risk::ExternalSideEffect
        );
        assert_eq!(
            classify_risk("Transfer tokens on my internal blockchain."),
            Risk::ExternalSideEffect
        );
    }

    #[test]
    fn native_companion_boundary_is_source_and_risk_scoped() {
        assert!(managed_project_source_allowed("fractal-mac-app"));
        assert!(!managed_project_source_allowed("terminal"));
        assert!(managed_project_risk_allowed(Risk::ReversibleWrite));
        assert!(!managed_project_risk_allowed(Risk::Destructive));
        assert!(!managed_project_risk_allowed(Risk::ExternalSideEffect));
    }

    #[test]
    fn recognizes_only_explicit_run_control_phrases() {
        assert_eq!(
            parse_run_control("Stop all Fractal builds."),
            Some(VoiceRunControl::StopAll)
        );
        assert_eq!(
            parse_run_control("halt the current build"),
            Some(VoiceRunControl::StopCurrent)
        );
        assert_eq!(
            parse_run_control("stop project expense tracker"),
            Some(VoiceRunControl::StopProject("expense tracker".to_owned()))
        );
        assert_eq!(
            parse_run_control("pause project Racket"),
            Some(VoiceRunControl::StopProject("racket".to_owned()))
        );
        assert_eq!(
            parse_run_control("show running Fractal builds"),
            Some(VoiceRunControl::StatusRunning)
        );
        assert_eq!(parse_run_control("build a stop watch"), None);
    }

    #[test]
    fn recognizes_mid_build_task_branch_commands() {
        assert_eq!(
            parse_graph_amendment(
                "Add to task 0.1 another branch and this branch will add CSV export features."
            ),
            Some(("0.1".to_owned(), "CSV export features".to_owned()))
        );
        assert_eq!(parse_graph_amendment("build a branching puzzle"), None);
    }

    #[test]
    fn visibility_requests_cannot_be_accepted_as_build_intent() {
        assert_eq!(
            parse_visibility_intent("Make project coffee-2 public"),
            Some("public")
        );
        assert_eq!(
            parse_visibility_intent("change the repository visibility to private"),
            Some("private")
        );
        assert_eq!(
            parse_visibility_intent("Build a public transit timetable"),
            None
        );
    }

    #[test]
    fn timestamp_conversion_has_known_epoch() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(20_000), (2024, 10, 4));
    }

    #[test]
    fn fingerprint_is_stable() {
        let value = event("show status");
        assert_eq!(
            event_fingerprint(&value).unwrap(),
            event_fingerprint(&value).unwrap()
        );
    }
}
