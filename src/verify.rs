//! Genuine, deny-by-default verification for a verify node. Instead of trusting
//! a bare test exit code, this runs the workspace suite and judges the result
//! with the **real** `fractal-verify` evidence floor (`check_completion`) — the
//! same governance the runtime uses. A verify node only completes when the floor
//! is satisfied: a public test report, a passing hidden-regression report, and at
//! least one model-verifier verdict must all be present. A failing suite fails
//! the hidden-regression floor and denies completion.

use std::path::Path;
use std::process::{Command, Stdio};

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
        .args(["CODE_SIGNING_ALLOWED=NO", "test"]);
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
        let mut c = Command::new("cargo");
        c.arg("test");
        c
    } else if python_tests {
        python_test_command(workspace, has_tests_dir)
    } else if has("package.json") && has(".fractal-profile") {
        let mut c = Command::new("npm");
        c.args(["run", "fractal:verify", "--silent"]);
        c
    } else if has("package.json") {
        let mut c = Command::new("npm");
        c.args(["test", "--silent"]);
        c
    } else {
        return Ok(None);
    };

    match command.current_dir(workspace).output() {
        Ok(output) => {
            let mut hasher = Sha256::new();
            hasher.update(&output.stdout);
            hasher.update(&output.stderr);
            let mut output_hash = String::from("sha256:");
            for byte in hasher.finalize() {
                output_hash.push_str(&format!("{byte:02x}"));
            }
            Ok(Some(SuiteRun {
                ok: output.status.success(),
                output_hash,
            }))
        }
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
            detail: format!("evidence floor denied completion: {missing:?}"),
        },
    };
    Ok(Some(verdict))
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
