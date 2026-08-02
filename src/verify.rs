//! Independent, deny-by-default verification for graph verification nodes.
//!
//! A public test process is one piece of evidence only.  Protected regression
//! checkers are configured by the operator outside the worktree and execute in
//! disposable copies.  No verifier result is synthesized from another run;
//! unavailable evidence remains unavailable and therefore cannot satisfy a
//! completion floor.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Read;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Result};
use serde_json::Value;

use fractal_verify::{
    check_completion, CompletionDecision, Evidence, EvidenceKind, EvidenceKindTag,
    EvidenceRequirement, ModelVerifierVerdict, RegressionReport, TestReport, VerdictLabel,
};

use crate::evidence_manifest::{
    argv_identity, persist_manifest, safe_relative_ref, source_hashes, EvidenceManifest,
    VerifierRun,
};

const MAX_OUTPUT_BYTES: usize = 256 * 1024;
const MAX_EXCERPT_CHARS: usize = 2_000;

fn first_xcode_project(workspace: &Path) -> Option<PathBuf> {
    fs::read_dir(workspace)
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .find(|path| path.extension().is_some_and(|ext| ext == "xcodeproj"))
}

/// Build the real Xcode test command for a generated native app. If the agents
/// wrote the XcodeGen spec but have not generated the project yet, generate it
/// before verification; a failed generation leaves the workspace unverifiable.
fn xcode_test_command(workspace: &Path) -> Option<Command> {
    let mut project = first_xcode_project(workspace);
    if project.is_none() && workspace.join("project.yml").is_file() {
        let generated = Command::new("xcodegen")
            .arg("generate")
            .current_dir(workspace)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false);
        if generated {
            project = first_xcode_project(workspace);
        }
    }
    let project = project?;
    let scheme = project.file_stem()?.to_string_lossy().into_owned();
    let simulator = std::env::var("FRACTAL_IOS_SIMULATOR")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "iPhone 17 Pro".to_owned());
    let mut command = Command::new("xcodebuild");
    command
        .arg("-project")
        .arg(project)
        .args(["-scheme", &scheme])
        .arg("-destination")
        .arg(format!("platform=iOS Simulator,name={simulator}"))
        .args([
            "-destination-timeout",
            "60",
            "-test-timeouts-enabled",
            "YES",
            "-default-test-execution-time-allowance",
            "60",
            "-maximum-test-execution-time-allowance",
            "120",
            "CODE_SIGNING_ALLOWED=NO",
            "test",
        ]);
    Some(command)
}

/// A completion decision from the evidence floor, with a human-readable reason.
pub(crate) struct FloorVerdict {
    pub(crate) complete: bool,
    pub(crate) detail: String,
    /// True when a required process or suite could not be run.  This is kept
    /// separate from a failing process so callers can report honest
    /// unverifiable outcomes without treating them as success.
    #[allow(dead_code)]
    pub(crate) unavailable: bool,
    /// Relative content-addressed manifest path, when persistence succeeded.
    pub(crate) manifest_ref: Option<String>,
}

/// Result of a bounded verifier process.  `ok` is based only on the process
/// exit status; no report is inferred for a missing process.
#[derive(Clone, Debug)]
struct ProcessRun {
    ok: bool,
    exit_code: Option<i32>,
    output_hash: String,
    output_excerpt: String,
    timed_out: bool,
    output_truncated: bool,
    duration_ms: u64,
}

type SuiteRun = ProcessRun;

fn verify_timeout_ms() -> u64 {
    std::env::var("FRACTAL_VERIFY_TIMEOUT_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(300_000)
}

fn read_bounded<R: Read>(mut stream: R) -> (Vec<u8>, bool) {
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 8192];
    let mut truncated = false;
    loop {
        match stream.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => {
                if bytes.len() < MAX_OUTPUT_BYTES {
                    let remaining = MAX_OUTPUT_BYTES - bytes.len();
                    bytes.extend_from_slice(&buffer[..count.min(remaining)]);
                }
                if bytes.len() >= MAX_OUTPUT_BYTES && count > 0 {
                    truncated = true;
                }
            }
            Err(_) => break,
        }
    }
    (bytes, truncated)
}

fn run_bounded_with_env(
    command: &mut Command,
    timeout_ms: u64,
    network_denied: bool,
) -> Result<ProcessRun> {
    // Verifiers receive a minimal, non-secret environment.  Command-specific
    // behavior is supplied through explicit argv; no shell expansion occurs.
    let ci = command
        .get_envs()
        .find(|(key, _)| *key == "CI")
        .and_then(|(_, value)| value.map(|value| value.to_string_lossy().into_owned()));
    let node_options = command
        .get_envs()
        .find(|(key, _)| *key == "NODE_OPTIONS")
        .and_then(|(_, value)| value.map(|value| value.to_string_lossy().into_owned()));
    command
        .env_clear()
        .envs(sanitized_verifier_environment(network_denied));
    if let Some(ci) = ci {
        command.env("CI", ci);
    }
    if let Some(node_options) = node_options {
        command.env("NODE_OPTIONS", node_options);
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    command.process_group(0);
    let started = Instant::now();
    let mut child = command.spawn()?;
    let worker = crate::run_control::WorkerGuard::register(child.id());
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let stdout_reader = std::thread::spawn(move || {
        stdout
            .map(read_bounded)
            .unwrap_or_else(|| (Vec::new(), false))
    });
    let stderr_reader = std::thread::spawn(move || {
        stderr
            .map(read_bounded)
            .unwrap_or_else(|| (Vec::new(), false))
    });
    let deadline = Instant::now() + Duration::from_millis(timeout_ms.max(1));
    let (status, timed_out) = loop {
        if let Some(status) = child.try_wait()? {
            break (Some(status), false);
        }
        if Instant::now() >= deadline {
            crate::run_control::terminate_worker(child.id());
            let status = child.wait().ok();
            break (status, true);
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    drop(worker);
    let (stdout, stdout_truncated) = stdout_reader.join().unwrap_or_default();
    let (stderr, stderr_truncated) = stderr_reader.join().unwrap_or_default();
    let mut output = Vec::with_capacity(stdout.len() + stderr.len());
    output.extend_from_slice(&stdout);
    output.extend_from_slice(&stderr);
    let output_hash = crate::evidence_manifest::sha256_bytes(&output);
    let output_text = String::from_utf8_lossy(&output);
    let output_excerpt = output_text
        .chars()
        .rev()
        .take(MAX_EXCERPT_CHARS)
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    let exit_code = status.and_then(|status| status.code());
    Ok(ProcessRun {
        ok: !timed_out && status.is_some_and(|status| status.success()),
        exit_code,
        output_hash,
        output_excerpt,
        timed_out,
        output_truncated: stdout_truncated || stderr_truncated,
        duration_ms: started.elapsed().as_millis() as u64,
    })
}

/// Compatibility helper used by focused tests.  The invocation still gets
/// sanitized environment and bounded output.
#[allow(dead_code)]
fn run_bounded(command: &mut Command, timeout_ms: u64) -> Result<SuiteRun> {
    run_bounded_with_env(command, timeout_ms, true)
}

fn sanitized_verifier_environment(network_denied: bool) -> BTreeMap<String, String> {
    let mut output = BTreeMap::new();
    for name in [
        "PATH", "HOME", "TMPDIR", "LANG", "LC_ALL", "LC_CTYPE", "TERM", "USER",
    ] {
        if let Ok(value) = std::env::var(name) {
            output.insert(name.to_owned(), value);
        }
    }
    output.insert("CI".to_owned(), "true".to_owned());
    output.insert("GIT_TERMINAL_PROMPT".to_owned(), "0".to_owned());
    output.insert("PIP_NO_INDEX".to_owned(), "1".to_owned());
    output.insert("NO_NETWORK".to_owned(), "1".to_owned());
    output.insert("FRACTAL_OFFLINE".to_owned(), "1".to_owned());
    if network_denied {
        output.insert("NO_PROXY".to_owned(), "*".to_owned());
        output.insert("HTTP_PROXY".to_owned(), "".to_owned());
        output.insert("HTTPS_PROXY".to_owned(), "".to_owned());
        output.insert("ALL_PROXY".to_owned(), "".to_owned());
    }
    output
}

fn node_supports_webstorage_opt_out() -> bool {
    Command::new("node")
        .arg("--version")
        .env_clear()
        .envs(sanitized_verifier_environment(true))
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .and_then(|version| {
            version
                .trim()
                .trim_start_matches('v')
                .split('.')
                .next()
                .and_then(|major| major.parse::<u32>().ok())
        })
        .is_some_and(|major| major >= 25)
}

fn npm_test_command(workspace: &Path) -> Command {
    let package = fs::read_to_string(workspace.join("package.json"))
        .ok()
        .and_then(|contents| serde_json::from_str::<Value>(&contents).ok());
    let test_script = package
        .as_ref()
        .and_then(|value| value.pointer("/scripts/test"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let mut command = Command::new("npm");
    command.env("CI", "true");
    command.args(["test", "--silent"]);
    if test_script.contains("vitest") {
        command.args(["--", "--run"]);
        if workspace.join("e2e").is_dir() {
            command.args(["--exclude", "e2e/**"]);
        }
    }
    if node_supports_webstorage_opt_out() {
        command.env("NODE_OPTIONS", "--no-experimental-webstorage");
    }
    command
}

fn python_test_command(workspace: &Path, has_tests_dir: bool) -> Command {
    for venv in [".venv", "venv"] {
        let python = workspace.join(venv).join("bin").join("python");
        if python.exists() {
            let mut command = Command::new(python);
            command.args(["-m", "pytest", "-q"]);
            return command;
        }
    }
    let system_pytest = Command::new("python3")
        .args(["-c", "import pytest"])
        .current_dir(workspace)
        .env_clear()
        .envs(sanitized_verifier_environment(true))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false);
    if system_pytest {
        let mut command = Command::new("python3");
        command.args(["-m", "pytest", "-q"]);
        return command;
    }
    let mut command = Command::new("python3");
    if has_tests_dir {
        let directory = if workspace.join("tests").is_dir() {
            "tests"
        } else {
            "test"
        };
        command.args([
            "-m", "unittest", "discover", "-q", "-s", directory, "-t", ".",
        ]);
    } else {
        command.args(["-m", "unittest", "discover", "-q"]);
    }
    command
}

/// Detect and run a genuine public/native suite.  A missing runner is
/// represented as `None`, and the caller emits an unavailable verifier row.
fn run_suite(workspace: &Path) -> Result<Option<SuiteRun>> {
    let has = |name: &str| workspace.join(name).exists();
    let is_test_file = |name: &str| {
        (name.starts_with("test_") && name.ends_with(".py")) || name.ends_with("_test.py")
    };
    let dir_has_tests = |dir: &str| {
        fs::read_dir(workspace.join(dir))
            .map(|entries| {
                entries
                    .flatten()
                    .any(|entry| is_test_file(&entry.file_name().to_string_lossy()))
            })
            .unwrap_or(false)
    };
    let has_tests_dir = dir_has_tests("tests") || dir_has_tests("test");
    let python_tests = dir_has_tests(".") || has_tests_dir;
    let mut command = if has("project.yml") || first_xcode_project(workspace).is_some() {
        xcode_test_command(workspace)
    } else if has("Cargo.toml") {
        let mut command = Command::new("cargo");
        command.arg("test");
        Some(command)
    } else if python_tests {
        Some(python_test_command(workspace, has_tests_dir))
    } else if has("package.json") && has(".fractal-profile") {
        let mut command = Command::new("npm");
        command.args(["run", "fractal:verify", "--silent"]);
        Some(command)
    } else if has("package.json") {
        Some(npm_test_command(workspace))
    } else {
        None
    };
    let Some(ref mut command) = command else {
        return Ok(None);
    };
    match run_bounded_with_env(command.current_dir(workspace), verify_timeout_ms(), true) {
        Ok(run) => Ok(Some(run)),
        Err(_) => Ok(None),
    }
}

#[derive(Clone, Debug)]
struct VerifierConfig {
    id: String,
    kind: String,
    argv: Vec<String>,
    protected: bool,
}

fn required_ids(policy: Option<&crate::policy_executor::EffectivePolicy>) -> Vec<String> {
    let mut ids = policy
        .map(|policy| policy.verifier_ids.clone())
        .unwrap_or_default();
    ids.sort();
    ids.dedup();
    ids.retain(|id| !id.trim().is_empty());
    ids
}

/// Registry loading is intentionally operator-only.  The registry path and
/// checker argv never enter worker prompts, graph JSON, git history, or the
/// manifest (only a safe argv identity/hash does).
fn verifier_registry() -> BTreeMap<String, Value> {
    let path = std::env::var_os("FRACTAL_VERIFIER_REGISTRY")
        .or_else(|| std::env::var_os("FRACTAL_HIDDEN_VERIFIER_REGISTRY"));
    let Some(path) = path else {
        return BTreeMap::new();
    };
    let Ok(bytes) = fs::read(path) else {
        return BTreeMap::new();
    };
    let Ok(value) = serde_json::from_slice::<Value>(&bytes) else {
        return BTreeMap::new();
    };
    let object = value
        .get("verifiers")
        .and_then(Value::as_object)
        .or_else(|| value.as_object());
    object
        .into_iter()
        .flatten()
        .filter(|(key, _)| *key != "verifiers")
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

fn env_key_for(prefix: &str, id: &str) -> String {
    let normalized = id
        .chars()
        .map(|value| {
            if value.is_ascii_alphanumeric() {
                value.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect::<String>();
    format!("{prefix}{normalized}")
}

fn config_from_value(id: &str, value: &Value) -> Option<VerifierConfig> {
    // Environment-based registries commonly carry a JSON argv string so the
    // operator can keep shell parsing disabled.  Decode that representation
    // before treating a value as a checker path.
    if let Value::String(raw) = value {
        if let Ok(decoded) = serde_json::from_str::<Value>(raw) {
            if decoded.is_array() || decoded.is_object() {
                return config_from_value(id, &decoded);
            }
        }
    }
    let object = value.as_object();
    let kind = object
        .and_then(|object| object.get("kind").or_else(|| object.get("type")))
        .and_then(Value::as_str)
        .unwrap_or("regression")
        .to_ascii_lowercase();
    let protected = object
        .and_then(|object| object.get("protected"))
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let argv_value = object.and_then(|object| object.get("argv").or_else(|| object.get("command")));
    let mut argv = argv_value
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .filter(|argv| !argv.is_empty())
        .or_else(|| {
            object
                .and_then(|object| object.get("path"))
                .and_then(Value::as_str)
                .map(|path| vec![path.to_owned()])
        })?;
    if argv.iter().any(|arg| arg.contains('\0')) {
        return None;
    }
    if argv.len() == 1 {
        let path = Path::new(&argv[0]);
        if path.extension().is_some_and(|extension| extension == "py") {
            argv.insert(0, "python3".to_owned());
        }
    }
    Some(VerifierConfig {
        id: id.to_owned(),
        kind,
        argv,
        protected,
    })
}

fn configured_verifiers(ids: &[String]) -> Vec<VerifierConfig> {
    let registry = verifier_registry();
    ids.iter()
        .filter_map(|id| {
            registry
                .get(id)
                .and_then(|value| config_from_value(id, value))
                .or_else(|| {
                    let direct = [
                        env_key_for("FRACTAL_VERIFIER_", id),
                        env_key_for("FRACTAL_HIDDEN_VERIFIER_", id),
                    ]
                    .into_iter()
                    .find_map(|key| {
                        std::env::var_os(format!("{key}_ARGV"))
                            .or_else(|| std::env::var_os(format!("{key}_PATH")))
                    });
                    direct.and_then(|path| {
                        config_from_value(id, &Value::String(path.to_string_lossy().into_owned()))
                    })
                })
                .or_else(|| {
                    if id.eq_ignore_ascii_case("independent") {
                        std::env::var_os("FRACTAL_HIDDEN_CHECKER")
                            .or_else(|| std::env::var_os("FRACTAL_INDEPENDENT_CHECKER"))
                            .and_then(|path| {
                                config_from_value(
                                    id,
                                    &Value::String(path.to_string_lossy().into_owned()),
                                )
                            })
                    } else {
                        None
                    }
                })
        })
        .collect()
}

fn criterion_ids(graph: Option<&Value>) -> Vec<String> {
    let mut ids = BTreeSet::new();
    let locations = [
        graph.and_then(|graph| graph.get("acceptance_criteria")),
        graph.and_then(|graph| graph.pointer("/prd/acceptance_criteria")),
        graph.and_then(|graph| graph.pointer("/metadata/acceptance_criteria")),
        graph.and_then(|graph| graph.get("acceptance")),
    ];
    for location in locations.into_iter().flatten() {
        if let Some(entries) = location.as_array() {
            for entry in entries {
                let id = entry
                    .as_str()
                    .or_else(|| entry.get("id").and_then(Value::as_str))
                    .or_else(|| entry.get("criterion_id").and_then(Value::as_str));
                if let Some(id) = id.filter(|id| !id.trim().is_empty()) {
                    let id = id.trim();
                    let lower = id.to_ascii_lowercase();
                    if id.starts_with('/')
                        || id.starts_with('~')
                        || id.contains("..")
                        || id.contains('\\')
                        || ["prompt", "secret", "token", "cot", "chain-of-thought"]
                            .iter()
                            .any(|needle| lower.contains(needle))
                    {
                        ids.insert(format!(
                            "criterion:{}",
                            crate::evidence_manifest::sha256_bytes(id.as_bytes())
                                .strip_prefix("sha256:")
                                .unwrap_or_default()
                                .chars()
                                .take(16)
                                .collect::<String>()
                        ));
                    } else {
                        ids.insert(id.chars().take(120).collect::<String>());
                    }
                }
            }
        }
    }
    ids.into_iter().collect()
}

fn attempt_number(workspace: &Path, node: &str) -> u64 {
    crate::project_file::load(workspace)
        .ok()
        .and_then(|document| {
            document
                .learning
                .nodes
                .get(node)
                .map(|record| record.attempt_count as u64)
        })
        .filter(|attempt| *attempt > 0)
        .unwrap_or(1)
}

fn graph_hash(graph: Option<&Value>, workspace: &Path) -> Option<String> {
    graph
        .and_then(|graph| graph.get("graph_hash"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| {
            crate::project_file::load(workspace)
                .ok()
                .map(|document| document.graph_hash)
        })
}

fn status_for(run: Option<&ProcessRun>) -> String {
    match run {
        Some(run) if run.timed_out || run.output_truncated || !run.ok => "fail".to_owned(),
        Some(_) => "pass".to_owned(),
        None => "unavailable".to_owned(),
    }
}

fn unavailable_run(id: &str, kind: &str, protected: bool) -> VerifierRun {
    let (argv_identity, argv_hash) = argv_identity(&["<unavailable>".to_owned()], protected);
    VerifierRun {
        id: id.to_owned(),
        kind: kind.to_owned(),
        argv_identity,
        argv_hash,
        exit_code: None,
        duration_ms: None,
        output_hash: None,
        status: "unavailable".to_owned(),
        protected,
        artifact_refs: Vec::new(),
    }
}

fn copy_workspace(source: &Path, destination: &Path) -> Result<()> {
    fn copy_entry(source: &Path, destination: &Path) -> Result<()> {
        let metadata = fs::symlink_metadata(source)?;
        if metadata.file_type().is_symlink() {
            bail!("symlink in verifier source is not safe to copy");
        }
        if metadata.is_dir() {
            fs::create_dir_all(destination)?;
            let mut entries = fs::read_dir(source)?.collect::<std::result::Result<Vec<_>, _>>()?;
            entries.sort_by_key(|entry| entry.file_name());
            for entry in entries {
                if entry.file_name() == ".git" {
                    continue;
                }
                copy_entry(&entry.path(), &destination.join(entry.file_name()))?;
            }
        } else if metadata.is_file() {
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(source, destination)?;
        }
        Ok(())
    }
    copy_entry(source, destination)
}

fn checker_argv(config: &VerifierConfig, workspace: &Path, node: &str) -> Vec<String> {
    let workspace_text = workspace.to_string_lossy();
    let mut argv = config
        .argv
        .iter()
        .map(|arg| {
            arg.replace("{workspace}", &workspace_text)
                .replace("{node}", node)
        })
        .collect::<Vec<_>>();
    if !config.argv.iter().any(|arg| arg.contains("{workspace}")) {
        argv.extend(["--workspace".to_owned(), workspace_text.into_owned()]);
    }
    argv
}

fn run_external_verifier(
    source: &Path,
    config: &VerifierConfig,
    node: &str,
) -> Result<(ProcessRun, bool, Vec<String>, String)> {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let copy = std::env::temp_dir().join(format!("fractal-check-{stamp}-{}", std::process::id()));
    fs::create_dir_all(&copy)?;
    let result = (|| -> Result<(ProcessRun, bool, Vec<String>, String)> {
        copy_workspace(source, &copy)?;
        let before = crate::policy_executor::snapshot_workspace(&copy)?;
        let argv = checker_argv(config, &copy, node);
        let Some((program, args)) = argv.split_first() else {
            bail!("empty verifier argv");
        };
        let mut command = Command::new(program);
        command.args(args).current_dir(&copy);
        let run = run_bounded_with_env(&mut command, verify_timeout_ms(), true)?;
        let after = crate::policy_executor::snapshot_workspace(&copy)?;
        let mutated = before.digest != after.digest;
        let (safe_argv, argv_hash) = argv_identity(&argv, config.protected);
        Ok((run, mutated, safe_argv, argv_hash))
    })();
    let _ = fs::remove_dir_all(&copy);
    result
}

fn requirement_for(
    policy: Option<&crate::policy_executor::EffectivePolicy>,
    configs: &[VerifierConfig],
) -> EvidenceRequirement {
    let requested = policy
        .map(|policy| {
            policy
                .evidence_requirements
                .iter()
                .map(|value| value.to_ascii_lowercase())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let has_requested = |need: &str| {
        requested.iter().any(|value| {
            value == need
                || value.contains(need)
                || (need == "test" && value.contains("public"))
                || (need == "regression" && value.contains("hidden"))
        })
    };
    let required = required_ids(policy);
    let hidden_configured = configs
        .iter()
        .any(|config| config.protected && config.kind != "model");
    let hidden_required = has_requested("regression")
        || hidden_configured
        // A missing required checker is still denied by `missing_required_ids`;
        // require the typed hidden floor only once a protected regression
        // process is actually configured.
        || (configs.is_empty() && !required.is_empty() && !has_requested("model"));
    let model_required =
        has_requested("model") || configs.iter().any(|config| config.kind == "model");
    let mut required_kinds = Vec::new();
    if policy.is_none()
        || has_requested("test")
        || has_requested("command")
        || has_requested("stdout")
    {
        required_kinds.push(EvidenceKindTag::TestReport);
    }
    if hidden_required {
        required_kinds.push(EvidenceKindTag::RegressionReport);
    }
    if model_required {
        required_kinds.push(EvidenceKindTag::ModelVerifierVerdict);
    }
    required_kinds.sort_by_key(|kind| format!("{kind:?}"));
    required_kinds.dedup();
    EvidenceRequirement {
        required_kinds,
        min_verifiers: if model_required {
            configs
                .iter()
                .filter(|config| config.kind == "model")
                .count() as u32
        } else {
            0
        },
        require_hidden_regression: hidden_required,
        required_child_root: None,
    }
}

fn evidence_for_run(run: &VerifierRun, node: &str) -> Result<Option<Evidence>> {
    let status = run.status.as_str();
    if status == "unavailable" {
        return Ok(None);
    }
    let evidence = match run.kind.as_str() {
        "public" | "tests" | "native" => EvidenceKind::TestReport(TestReport {
            passed: u32::from(status == "pass"),
            failed: u32::from(status != "pass"),
            total: 1,
            report_hash: run.output_hash.clone().unwrap_or_default(),
        }),
        "model" | "model_verifier" => EvidenceKind::ModelVerifierVerdict(ModelVerifierVerdict {
            verifier_id: run.id.clone(),
            verdict: if status == "pass" {
                VerdictLabel::Pass
            } else {
                VerdictLabel::Fail
            },
            confidence_bp: if status == "pass" { 10_000 } else { 0 },
        }),
        _ => EvidenceKind::RegressionReport(RegressionReport {
            hidden_suite_id: run.id.clone(),
            passed: u32::from(status == "pass"),
            failed: u32::from(status != "pass"),
        }),
    };
    Evidence::new(evidence, node)
        .map(Some)
        .map_err(|error| anyhow::anyhow!("evidence hash failed: {error}"))
}

fn missing_required_ids(required: &[String], runs: &[VerifierRun]) -> Vec<String> {
    required
        .iter()
        .filter(|id| {
            !runs
                .iter()
                .any(|run| run.id == **id && run.status == "pass")
        })
        .cloned()
        .collect()
}

fn verifier_command_key(identity: &[String]) -> Vec<String> {
    let mut key = Vec::with_capacity(identity.len());
    let mut skip_next = false;
    for value in identity {
        if skip_next {
            skip_next = false;
            continue;
        }
        if value == "--workspace" || value == "--worktree" || value == "--cwd" {
            skip_next = true;
            continue;
        }
        key.push(value.clone());
    }
    key
}

fn evaluate_workspace_inner(
    workspace: &Path,
    node: &str,
    _agent: &str,
    policy: Option<&crate::policy_executor::EffectivePolicy>,
    graph: Option<&Value>,
    enforcement_report_hash: Option<String>,
) -> Result<FloorVerdict> {
    let required = required_ids(policy);
    let configs = configured_verifiers(&required);
    let suite = run_suite(workspace)?;
    let mut manifest = EvidenceManifest::new(node);
    manifest.policy_hash = policy.map(|policy| policy.policy_hash.clone());
    manifest.attempt = attempt_number(workspace, node);
    manifest.source = source_hashes(workspace, graph_hash(graph, workspace).as_deref());
    manifest.criterion_ids = criterion_ids(graph);
    manifest.artifact_refs = crate::project_file::load(workspace)
        .ok()
        .and_then(|document| {
            document
                .learning
                .nodes
                .get(node)
                .map(|record| record.artifacts_produced.clone())
        })
        .unwrap_or_default()
        .into_iter()
        .filter(|reference| reference.starts_with("artifact:") || safe_relative_ref(reference))
        .collect();
    let policy_pass = enforcement_report_hash.is_some();
    manifest.enforcement_report_hash = enforcement_report_hash;

    let mut public_run = suite.as_ref().map(|run| {
        let command = public_command_identity(workspace);
        let (safe, hash) = argv_identity(&command, false);
        VerifierRun {
            id: "public".to_owned(),
            kind: "public".to_owned(),
            argv_identity: safe,
            argv_hash: hash,
            exit_code: run.exit_code,
            duration_ms: Some(run.duration_ms),
            output_hash: Some(run.output_hash.clone()),
            status: status_for(Some(run)),
            protected: false,
            artifact_refs: Vec::new(),
        }
    });
    if public_run.is_none() {
        public_run = Some(unavailable_run("public", "public", false));
    }
    let mut runs = vec![public_run.expect("public run")];
    let public_command_key = verifier_command_key(&runs[0].argv_identity);
    let mut unavailable = suite.is_none();
    let mut duplicate = false;

    for id in &required {
        let Some(config) = configs.iter().find(|config| &config.id == id) else {
            runs.push(unavailable_run(id, "regression", true));
            unavailable = true;
            continue;
        };
        let result = run_external_verifier(workspace, config, node);
        match result {
            Ok((run, mutated, safe_argv, argv_hash)) => {
                let command_key = verifier_command_key(&safe_argv);
                let same_as_public = command_key == public_command_key;
                let duplicate_of_existing = runs
                    .iter()
                    .any(|existing| verifier_command_key(&existing.argv_identity) == command_key);
                duplicate |= same_as_public || duplicate_of_existing;
                let status = if mutated || same_as_public || duplicate_of_existing {
                    "fail".to_owned()
                } else {
                    status_for(Some(&run))
                };
                if status == "unavailable" {
                    unavailable = true;
                }
                if run.timed_out {
                    unavailable = true;
                }
                runs.push(VerifierRun {
                    id: id.clone(),
                    kind: config.kind.clone(),
                    argv_identity: safe_argv,
                    argv_hash,
                    exit_code: run.exit_code,
                    duration_ms: Some(run.duration_ms),
                    output_hash: Some(run.output_hash),
                    status,
                    protected: config.protected,
                    artifact_refs: Vec::new(),
                });
            }
            Err(_) => {
                runs.push(unavailable_run(id, &config.kind, config.protected));
                unavailable = true;
            }
        }
    }
    // Registry entries may be model verifiers not referenced by the contract;
    // they are intentionally not run.  Required IDs are the policy authority.
    manifest.verifier_runs = runs.clone();
    let mut evidence = Vec::new();
    for run in &runs {
        if let Some(record) = evidence_for_run(run, node)? {
            evidence.push(record);
        }
    }
    let requirement = requirement_for(policy, &configs);
    let missing_ids = missing_required_ids(&required, &runs);
    let decision = check_completion(&requirement, &evidence);
    let suite_failed = suite
        .as_ref()
        .is_some_and(|run| !run.ok || run.timed_out || run.output_truncated);
    let complete = matches!(decision, CompletionDecision::Complete)
        && missing_ids.is_empty()
        && !suite_failed
        && !duplicate
        && policy_pass;
    manifest.outcome = if complete {
        "pass".to_owned()
    } else if unavailable || !missing_ids.is_empty() {
        "unavailable".to_owned()
    } else {
        "fail".to_owned()
    };
    let persisted = persist_manifest(workspace, manifest)?;
    let manifest_ref =
        safe_relative_ref(&persisted.relative_path).then_some(persisted.relative_path);
    let detail = if complete {
        format!(
            "independent evidence floor satisfied (manifest {})",
            persisted.hash
        )
    } else {
        let mut reasons = Vec::new();
        if suite.is_none() {
            reasons.push("public test suite unavailable".to_owned());
        } else if suite_failed {
            let excerpt = suite
                .as_ref()
                .map(|run| run.output_excerpt.trim())
                .filter(|value| !value.is_empty())
                .unwrap_or("public test process failed");
            reasons.push(format!("public test process failed: {excerpt}"));
        }
        if !missing_ids.is_empty() {
            reasons.push(format!(
                "required verifier unavailable or failed: {missing_ids:?}"
            ));
        }
        if duplicate {
            reasons.push("duplicate verifier invocation rejected".to_owned());
        }
        if !policy_pass {
            reasons.push("policy enforcement report unavailable".to_owned());
        }
        if let CompletionDecision::Incomplete { missing } = decision {
            reasons.push(format!("evidence floor denied completion: {missing:?}"));
        }
        if reasons.is_empty() {
            reasons.push("verification unavailable".to_owned());
        }
        format!("{} (manifest {})", reasons.join("; "), persisted.hash)
    };
    Ok(FloorVerdict {
        complete,
        detail,
        unavailable: !complete && unavailable,
        manifest_ref,
    })
}

fn public_command_identity(workspace: &Path) -> Vec<String> {
    if workspace.join("Cargo.toml").is_file() {
        return vec!["cargo".to_owned(), "test".to_owned()];
    }
    if workspace.join("package.json").is_file() {
        return vec!["npm".to_owned(), "test".to_owned(), "--silent".to_owned()];
    }
    if workspace.join("project.yml").is_file() || first_xcode_project(workspace).is_some() {
        return vec!["xcodebuild".to_owned(), "test".to_owned()];
    }
    vec!["<unavailable>".to_owned()]
}

/// Evaluate with the immutable policy/graph context.  The policy report hash
/// is supplied by the execution layer after its own preflight/postflight
/// checks; a missing report is therefore a hard denial rather than a default.
pub(crate) fn evaluate_workspace_with_policy(
    workspace: &Path,
    node: &str,
    agent: &str,
    policy: Option<&crate::policy_executor::EffectivePolicy>,
    graph: Option<&Value>,
    enforcement_report_hash: Option<String>,
) -> Result<FloorVerdict> {
    let _ = agent; // agent identity is not verifier evidence and is not persisted.
    evaluate_workspace_inner(
        workspace,
        node,
        agent,
        policy,
        graph,
        enforcement_report_hash,
    )
}

/// Backward-compatible entry point.  It remains fail-closed: a single public
/// run cannot be mapped to hidden regression or model-verifier evidence.
#[allow(dead_code)]
pub(crate) fn evaluate_workspace(
    workspace: &Path,
    node: &str,
    agent: &str,
) -> Result<Option<FloorVerdict>> {
    Ok(Some(evaluate_workspace_inner(
        workspace, node, agent, None, None, None,
    )?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy_executor::{EffectiveLimits, EffectiveNetwork, EffectivePolicy};
    use std::sync::atomic::{AtomicU64, Ordering};

    fn temp_workspace() -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "fractal-verify-independent-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn policy_with_verifier(id: &str) -> EffectivePolicy {
        EffectivePolicy {
            schema: "fractal.node_policy_contract.v1".to_owned(),
            policy_hash: "sha256:policy".to_owned(),
            provenance: "test".to_owned(),
            capability: "project.tests.execute".to_owned(),
            sandbox_profile: "read-only".to_owned(),
            allowed_writes: Vec::new(),
            allowed_commands: vec!["cargo test".to_owned()],
            network: EffectiveNetwork {
                default: "deny".to_owned(),
                allowed_destinations: Vec::new(),
            },
            limits: EffectiveLimits {
                max_steps: 4,
                max_minutes: 5,
                max_attempts: 1,
                max_files_changed: 4,
                max_diff_lines: 100,
                max_input_tokens: 1,
                max_output_tokens: 1,
                max_cost_usd: 0,
            },
            verifier_ids: vec![id.to_owned()],
            evidence_requirements: vec!["public_tests".to_owned(), "regression".to_owned()],
            external_side_effects: false,
        }
    }

    #[cfg(unix)]
    #[test]
    fn verifier_processes_have_a_hard_timeout() {
        let mut command = Command::new("sh");
        command.args(["-c", "sleep 2"]);
        let run = run_bounded(&mut command, 50).unwrap();
        assert!(!run.ok);
        assert!(run.timed_out);
    }

    #[test]
    fn no_suite_is_explicitly_unverifiable() {
        let root = temp_workspace();
        let verdict = evaluate_workspace(&root, "verify", "agent")
            .unwrap()
            .unwrap();
        assert!(!verdict.complete);
        assert!(verdict.unavailable);
        assert!(verdict.manifest_ref.is_some());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn xcode_project_selects_native_simulator_test_runner() {
        let root = temp_workspace();
        fs::create_dir_all(root.join("ExpenseTracker.xcodeproj")).unwrap();
        let command = xcode_test_command(&root).expect("xcode project detected");
        let args = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert!(args
            .windows(2)
            .any(|pair| pair == ["-scheme", "ExpenseTracker"]));
        assert!(args
            .iter()
            .any(|arg| arg.starts_with("platform=iOS Simulator,name=")));
        assert_eq!(args.last().map(String::as_str), Some("test"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn vitest_is_forced_to_run_once_and_excludes_playwright_directory() {
        let root = temp_workspace();
        fs::create_dir_all(root.join("e2e")).unwrap();
        fs::write(
            root.join("package.json"),
            r#"{"scripts":{"test":"vitest"}}"#,
        )
        .unwrap();
        let command = npm_test_command(&root);
        let args = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(
            args,
            ["test", "--silent", "--", "--run", "--exclude", "e2e/**"]
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn protected_checker_runs_on_copy_and_mutation_is_failure() {
        let root = temp_workspace();
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname='x'\nversion='0.1.0'\nedition='2021'\n",
        )
        .unwrap();
        let script = root.join("checker.sh");
        fs::write(&script, "#!/bin/sh\necho changed > touched.txt\nexit 0\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let config = VerifierConfig {
            id: "hidden".to_owned(),
            kind: "regression".to_owned(),
            argv: vec![script.to_string_lossy().into_owned()],
            protected: true,
        };
        let (_, mutated, _, _) = run_external_verifier(&root, &config, "verify").unwrap();
        assert!(mutated);
        assert!(!root.join("touched.txt").exists());
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn public_and_distinct_hidden_verifier_can_pass() {
        let root = temp_workspace();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname='independent-fixture'\nversion='0.1.0'\nedition='2021'\n",
        )
        .unwrap();
        fs::write(root.join("src/lib.rs"), "#[test] fn public() {}\n").unwrap();
        let operator = temp_workspace();
        let checker = operator.join("hidden.sh");
        fs::write(&checker, "#!/bin/sh\nexit 0\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&checker, fs::Permissions::from_mode(0o700)).unwrap();
        let registry = operator.join("registry.json");
        fs::write(
            &registry,
            serde_json::json!({
                "verifiers": {
                    "hidden": {"kind":"regression", "path":checker, "protected":true}
                }
            })
            .to_string(),
        )
        .unwrap();
        std::env::set_var("FRACTAL_VERIFIER_REGISTRY", &registry);
        let policy = policy_with_verifier("hidden");
        let graph = serde_json::json!({
            "graph_hash":"sha256:graph",
            "acceptance_criteria":[{"id":"AC-1"}]
        });
        let verdict = evaluate_workspace_with_policy(
            &root,
            "verify",
            "codex-luna",
            Some(&policy),
            Some(&graph),
            Some("sha256:enforcement".to_owned()),
        )
        .unwrap();
        assert!(verdict.complete, "{}", verdict.detail);
        assert!(verdict.manifest_ref.is_some());
        std::env::remove_var("FRACTAL_VERIFIER_REGISTRY");
        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_dir_all(operator);
    }
}
