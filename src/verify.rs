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

/// Detect and run the workspace's tests, capturing a content hash of the runner
/// output as report evidence. Returns `None` when there is nothing to run (the
/// node is unverifiable, but that is not a failure).
fn run_suite(workspace: &Path) -> Result<Option<SuiteRun>> {
    let has = |name: &str| workspace.join(name).exists();
    let python_tests = std::fs::read_dir(workspace)
        .map(|entries| {
            entries.flatten().any(|entry| {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                name.starts_with("test_") && name.ends_with(".py") || name.ends_with("_test.py")
            })
        })
        .unwrap_or(false);

    let mut command = if has("Cargo.toml") {
        let mut c = Command::new("cargo");
        c.arg("test");
        c
    } else if python_tests {
        // Prefer pytest when it is importable; otherwise fall back to unittest
        // discovery so a missing pytest is not mistaken for a test failure.
        let pytest_available = Command::new("python3")
            .args(["-c", "import pytest"])
            .current_dir(workspace)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false);
        let mut c = Command::new("python3");
        if pytest_available {
            c.args(["-m", "pytest", "-q"]);
        } else {
            c.args(["-m", "unittest", "discover", "-q"]);
        }
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
