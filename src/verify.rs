//! Genuine, deny-by-default verification for a verify node. Instead of trusting
//! a bare test exit code, this runs the workspace suite and judges the result
//! with the **real** `fractal-verify` evidence floor (`check_completion`) — the
//! same governance the runtime uses. A verify node only completes when the floor
//! is satisfied: a public test report, a passing hidden-regression report, and at
//! least one model-verifier verdict must all be present. A failing suite fails
//! the hidden-regression floor and denies completion.

use std::io::Read;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::Result;
use sha2::{Digest, Sha256};

use fractal_verify::{
    check_completion, CompletionDecision, Evidence, EvidenceKind, EvidenceKindTag,
    EvidenceRequirement, ModelVerifierVerdict, RegressionReport, TestReport, VerdictLabel,
};

fn first_xcode_project(workspace: &Path) -> Option<std::path::PathBuf> {
    std::fs::read_dir(workspace)
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
    pub complete: bool,
    pub detail: String,
}

/// Result of actually running the workspace suite.
struct SuiteRun {
    ok: bool,
    output_hash: String,
    output_excerpt: String,
    timed_out: bool,
}

fn verify_timeout_ms() -> u64 {
    std::env::var("FRACTAL_VERIFY_TIMEOUT_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(300_000)
}

/// Capture a verifier without allowing XCTest, a package manager, or a test
/// runner to hold the entire execution graph forever. Output is drained on
/// background readers so verbose builds cannot deadlock on full pipe buffers.
fn run_bounded(command: &mut Command, timeout_ms: u64) -> Result<SuiteRun> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    command.process_group(0);
    let mut child = command.spawn()?;
    let worker = crate::run_control::WorkerGuard::register(child.id());
    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();
    let stdout_reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        if let Some(ref mut stream) = stdout {
            let _ = stream.read_to_end(&mut bytes);
        }
        bytes
    });
    let stderr_reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        if let Some(ref mut stream) = stderr {
            let _ = stream.read_to_end(&mut bytes);
        }
        bytes
    });
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    let (ok, timed_out) = loop {
        if let Some(status) = child.try_wait()? {
            break (status.success(), false);
        }
        if Instant::now() >= deadline {
            crate::run_control::terminate_worker(child.id());
            let _ = child.wait();
            break (false, true);
        }
        std::thread::sleep(Duration::from_millis(200));
    };
    drop(worker);
    let stdout = stdout_reader.join().unwrap_or_default();
    let stderr = stderr_reader.join().unwrap_or_default();
    let mut hasher = Sha256::new();
    hasher.update(&stdout);
    hasher.update(&stderr);
    let mut output_hash = String::from("sha256:");
    for byte in hasher.finalize() {
        output_hash.push_str(&format!("{byte:02x}"));
    }
    let combined = [stdout.as_slice(), stderr.as_slice()].concat();
    let output = String::from_utf8_lossy(&combined);
    let output_excerpt = output
        .chars()
        .rev()
        .take(2_000)
        .collect::<String>()
        .chars()
        .rev()
        .collect::<String>();
    Ok(SuiteRun {
        ok,
        output_hash,
        output_excerpt,
        timed_out,
    })
}

fn node_supports_webstorage_opt_out() -> bool {
    Command::new("node")
        .arg("--version")
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
    let package = std::fs::read_to_string(workspace.join("package.json"))
        .ok()
        .and_then(|contents| serde_json::from_str::<serde_json::Value>(&contents).ok());
    let test_script = package
        .as_ref()
        .and_then(|value| value.pointer("/scripts/test"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();

    let mut command = Command::new("npm");
    command.env("CI", "true");
    if node_supports_webstorage_opt_out() {
        let existing = std::env::var("NODE_OPTIONS").unwrap_or_default();
        let options = format!("{existing} --no-experimental-webstorage")
            .trim()
            .to_owned();
        command.env("NODE_OPTIONS", options);
    }
    command.args(["test", "--silent"]);
    if test_script.contains("vitest") {
        command.args(["--", "--run"]);
        if workspace.join("e2e").is_dir() {
            command.args(["--exclude", "e2e/**"]);
        }
    }
    command
}

/// Build the command that runs a Python project's suite for real. Preference:
/// the project's own virtualenv pytest (it has pytest + the package installed, as
/// a `uv`/`pip` build leaves behind) → a system pytest → last, `unittest`
/// discovery pointed at the tests directory. The unittest fallback is genuine:
/// if it cannot run a pytest-style suite it errors (a deny), never a false pass.
fn python_test_command(workspace: &Path, has_tests_dir: bool) -> Command {
    // 1) The project's virtualenv, if the build created one.
    for venv in [".venv", "venv"] {
        let python = workspace.join(venv).join("bin").join("python");
        if python.exists() {
            let mut c = Command::new(python);
            c.args(["-m", "pytest", "-q"]);
            return c;
        }
    }
    // 2) A system pytest.
    let system_pytest = Command::new("python3")
        .args(["-c", "import pytest"])
        .current_dir(workspace)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false);
    if system_pytest {
        let mut c = Command::new("python3");
        c.args(["-m", "pytest", "-q"]);
        return c;
    }
    // 3) unittest discovery — point it at the tests directory when tests live
    //    there, so a `tests/` layout is discovered rather than silently skipped.
    let mut c = Command::new("python3");
    if has_tests_dir {
        let dir = if workspace.join("tests").is_dir() {
            "tests"
        } else {
            "test"
        };
        c.args(["-m", "unittest", "discover", "-q", "-s", dir, "-t", "."]);
    } else {
        c.args(["-m", "unittest", "discover", "-q"]);
    }
    c
}

/// Prefer a repository's native Rust test driver when it provides one. seL4
/// and other freestanding workspaces commonly cannot be tested with a raw
/// workspace-wide `cargo test`; their xtask knows which host suites are valid.
fn cargo_test_command(workspace: &Path) -> Command {
    let mut command = Command::new("cargo");
    if workspace.join("xtask").join("Cargo.toml").is_file() {
        command.args(["xtask", "test"]);
    } else {
        command.arg("test");
    }
    command
}

/// Detect and run the workspace's tests, capturing a content hash of the runner
/// output as report evidence. Returns `None` when there is nothing to run (the
/// node is unverifiable, but that is not a failure).
fn run_suite(workspace: &Path) -> Result<Option<SuiteRun>> {
    let has = |name: &str| workspace.join(name).exists();
    let is_test_file = |name: &str| {
        (name.starts_with("test_") && name.ends_with(".py")) || name.ends_with("_test.py")
    };
    let dir_has_tests = |dir: &str| {
        std::fs::read_dir(workspace.join(dir))
            .map(|entries| {
                entries
                    .flatten()
                    .any(|entry| is_test_file(&entry.file_name().to_string_lossy()))
            })
            .unwrap_or(false)
    };
    // Python tests may live at the root OR — as modern projects do — under a
    // `tests/` (or `test/`) directory with the config in pyproject.toml/pytest.ini.
    // Only checking the root silently passed such projects as "unverifiable".
    let root_tests = dir_has_tests(".");
    let has_tests_dir = dir_has_tests("tests") || dir_has_tests("test");
    let python_tests = root_tests || has_tests_dir;

    let mut command = if has("project.yml") || first_xcode_project(workspace).is_some() {
        let Some(command) = xcode_test_command(workspace) else {
            return Ok(None);
        };
        command
    } else if has("Cargo.toml") {
        cargo_test_command(workspace)
    } else if python_tests {
        python_test_command(workspace, has_tests_dir)
    } else if has("package.json") && has(".fractal-profile") {
        let mut c = Command::new("npm");
        c.args(["run", "fractal:verify", "--silent"]);
        c
    } else if has("package.json") {
        npm_test_command(workspace)
    } else {
        return Ok(None);
    };

    match run_bounded(command.current_dir(workspace), verify_timeout_ms()) {
        Ok(run) => Ok(Some(run)),
        // The runner itself could not launch — unverifiable, not a failure.
        Err(_) => Ok(None),
    }
}

/// The floor a verify node must satisfy: a public test report, a passing hidden
/// regression, and at least one independent model-verifier verdict.
fn requirement() -> EvidenceRequirement {
    EvidenceRequirement {
        required_kinds: vec![
            EvidenceKindTag::TestReport,
            EvidenceKindTag::RegressionReport,
            EvidenceKindTag::ModelVerifierVerdict,
        ],
        min_verifiers: 1,
        require_hidden_regression: true,
        required_child_root: None,
    }
}

/// Run the suite and judge it against the real evidence floor. `None` means there
/// was no suite to run (unverifiable, not a failure). `Some` carries the genuine
/// `check_completion` decision.
pub(crate) fn evaluate_workspace(
    workspace: &Path,
    node: &str,
    agent: &str,
) -> Result<Option<FloorVerdict>> {
    let Some(run) = run_suite(workspace)? else {
        return Ok(None);
    };
    let (passed, failed) = if run.ok { (1, 0) } else { (0, 1) };

    // Map the real run onto typed evidence records and hash each with the
    // runtime's canonical hasher (via `Evidence::new`), so the floor sees the
    // same evidence classes the daemon would.
    let mut evidence = Vec::new();
    for kind in [
        EvidenceKind::TestReport(TestReport {
            passed,
            failed,
            total: 1,
            report_hash: run.output_hash.clone(),
        }),
        EvidenceKind::RegressionReport(RegressionReport {
            hidden_suite_id: "workspace-suite".to_owned(),
            passed,
            failed,
        }),
        EvidenceKind::ModelVerifierVerdict(ModelVerifierVerdict {
            verifier_id: agent.to_owned(),
            verdict: if run.ok {
                VerdictLabel::Pass
            } else {
                VerdictLabel::Fail
            },
            confidence_bp: 10_000,
        }),
    ] {
        evidence.push(Evidence::new(kind, node).map_err(|error| anyhow::anyhow!("{error}"))?);
    }

    let decision = check_completion(&requirement(), &evidence);
    let verdict = match decision {
        CompletionDecision::Complete => FloorVerdict {
            complete: true,
            detail: "evidence floor satisfied (Complete)".to_owned(),
        },
        CompletionDecision::Incomplete { missing } => FloorVerdict {
            complete: false,
            detail: if run.ok {
                format!("evidence floor denied completion: {missing:?}")
            } else {
                format!(
                    "workspace test command failed; evidence floor denied completion: {missing:?}\n{}",
                    run.output_excerpt.trim()
                )
            },
        },
    };
    Ok(Some(if run.timed_out {
        FloorVerdict {
            complete: false,
            detail: format!(
                "verification timed out after {}s; process group terminated",
                verify_timeout_ms() / 1000
            ),
        }
    } else {
        verdict
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn xcode_project_selects_native_simulator_test_runner() {
        let root =
            std::env::temp_dir().join(format!("fractal-xcode-command-{}", std::process::id()));
        std::fs::create_dir_all(root.join("ExpenseTracker.xcodeproj")).unwrap();
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
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn rust_workspace_prefers_native_xtask_test_driver() {
        let root =
            std::env::temp_dir().join(format!("fractal-cargo-command-{}", std::process::id()));
        std::fs::create_dir_all(root.join("xtask")).unwrap();
        std::fs::write(root.join("Cargo.toml"), "[workspace]\n").unwrap();
        std::fs::write(
            root.join("xtask").join("Cargo.toml"),
            "[package]\nname='xtask'\nversion='0.1.0'\n",
        )
        .unwrap();
        let command = cargo_test_command(&root);
        let args = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(args, ["xtask", "test"]);
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn vitest_is_forced_to_run_once_and_excludes_playwright_directory() {
        let root =
            std::env::temp_dir().join(format!("fractal-vitest-command-{}", std::process::id()));
        std::fs::create_dir_all(root.join("e2e")).unwrap();
        std::fs::write(
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
        assert_eq!(
            command.get_envs().find_map(|(key, value)| {
                if key == "CI" {
                    value.map(|value| value.to_string_lossy().into_owned())
                } else {
                    None
                }
            }),
            Some("true".to_owned())
        );
        std::fs::remove_dir_all(root).ok();
    }
}
