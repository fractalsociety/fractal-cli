use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;

const CLASSIFIER_SCRIPT: &str = "packages/agent-network-core/scripts/fractal-classify.mjs";

#[derive(Debug, Deserialize, Eq, PartialEq)]
pub(crate) struct TaskClassification {
    #[serde(rename = "intent")]
    pub(crate) intent: String,
    #[serde(rename = "privacy")]
    pub(crate) privacy: String,
    #[serde(rename = "difficulty")]
    pub(crate) difficulty: String,
    #[serde(rename = "verification")]
    pub(crate) verification: String,
    #[serde(rename = "external_calls")]
    pub(crate) external_calls: bool,
    #[serde(rename = "budget")]
    pub(crate) budget: u64,
    #[serde(rename = "tools")]
    pub(crate) tools: Vec<String>,
    #[serde(rename = "policy_hash")]
    pub(crate) policy_hash: String,
}

/// Resolve FractalWork from the CLI override, environment, or ~/fractalwork.
pub(crate) fn fractalwork_dir(cli_override: Option<&Path>) -> Result<PathBuf> {
    if let Some(path) = cli_override {
        return Ok(path.to_path_buf());
    }
    if let Some(path) = std::env::var_os("FRACTALWORK_DIR").filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(path));
    }
    let home = std::env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            anyhow!("cannot locate FractalWork: set --fractalwork <path> or FRACTALWORK_DIR")
        })?;
    Ok(PathBuf::from(home).join("fractalwork"))
}

/// Classify a request through FractalWork's TypeScript rules classifier.
pub(crate) fn classify(request: &str, fractalwork_dir: &Path) -> Result<TaskClassification> {
    let script = fractalwork_dir.join(CLASSIFIER_SCRIPT);
    if !script.is_file() {
        bail!(
            "FractalWork classifier script is missing at {}; stage or install FractalWork and pass --fractalwork <path>",
            script.display()
        );
    }

    let output = Command::new("node")
        .args(["--import", "tsx"])
        .arg(&script)
        .arg(request)
        .current_dir(fractalwork_dir)
        .output()
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                anyhow!(
                    "Node.js is unavailable; install Node.js and the FractalWork dependencies (including tsx)"
                )
            } else {
                anyhow!("failed to start the FractalWork classifier: {error}")
            }
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        bail!(
            "FractalWork classifier failed ({}): {}. Ensure Node.js is installed and run the FractalWork dependency install so tsx is available",
            output.status,
            if stderr.is_empty() {
                "no error output"
            } else {
                &stderr
            }
        );
    }

    let stdout = std::str::from_utf8(&output.stdout)
        .context("FractalWork classifier returned non-UTF-8 output")?;
    serde_json::from_str(stdout.trim())
        .context("FractalWork classifier returned invalid classification JSON")
}

/// Replace only the Intent stub in an existing submit plan.
pub(crate) fn render_submit_plan(
    stub_plan: &str,
    classification: Result<&TaskClassification, &str>,
) -> String {
    let mut lines = Vec::new();
    for line in stub_plan.lines() {
        if line.starts_with("1. Intent [STUB]") {
            match classification {
                Ok(classification) => {
                    lines.push("1. Intent".to_owned());
                    lines.push(format!("   intent: {}", classification.intent));
                    lines.push(format!("   privacy: {}", classification.privacy));
                    lines.push(format!("   difficulty: {}", classification.difficulty));
                    lines.push(format!("   verification: {}", classification.verification));
                    lines.push(format!("   tools: {}", classification.tools.join(", ")));
                }
                Err(reason) => {
                    lines.push(format!("{line} (classification unavailable: {reason})"));
                }
            }
        } else {
            lines.push(line.to_owned());
        }
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_classification_json_fixture() {
        let fixture = r#"{"intent":"code","privacy":"public","difficulty":"easy","verification":"none","external_calls":false,"budget":25,"tools":["repo-map","test-runner"],"policy_hash":"sha256:fixture"}"#;
        let classification: TaskClassification = serde_json::from_str(fixture).unwrap();

        assert_eq!(
            classification,
            TaskClassification {
                intent: "code".to_owned(),
                privacy: "public".to_owned(),
                difficulty: "easy".to_owned(),
                verification: "none".to_owned(),
                external_calls: false,
                budget: 25,
                tools: vec!["repo-map".to_owned(), "test-runner".to_owned()],
                policy_hash: "sha256:fixture".to_owned(),
            }
        );
    }

    #[test]
    fn renders_real_intent_block_from_injected_classification() {
        let classification = TaskClassification {
            intent: "code".to_owned(),
            privacy: "public".to_owned(),
            difficulty: "easy".to_owned(),
            verification: "none".to_owned(),
            external_calls: false,
            budget: 25,
            tools: vec!["repo-map".to_owned(), "test-runner".to_owned()],
            policy_hash: "sha256:fixture".to_owned(),
        };
        let stub = "Request: build it\nPipeline plan:\n1. Intent [STUB] TODO: classifier\n2. FractalWork [STUB] TODO: constructor";

        let plan = render_submit_plan(stub, Ok(&classification));

        assert!(plan.contains(
            "1. Intent\n   intent: code\n   privacy: public\n   difficulty: easy\n   verification: none\n   tools: repo-map, test-runner"
        ));
        assert!(plan.contains("2. FractalWork [STUB] TODO: constructor"));
        assert!(!plan.contains("1. Intent [STUB]"));
    }
}
