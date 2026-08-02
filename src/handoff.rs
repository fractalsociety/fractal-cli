//! Bridge-free handoff from sandboxed desktop clients to Fractal Voice.
//!
//! The caller sends the build request over stdin. Fractal writes an owner-only,
//! short-lived file in the cross-application temporary directory and asks
//! LaunchServices to open it with the native app. If a sandbox prevents that
//! launch, the running app discovers the private queued file itself. The request
//! never becomes shell syntax or a URL query value.

use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::cli::HandoffArgs;

const SCHEMA: &str = "fractal.external_build.v1";
const APP_BUNDLE_ID: &str = "com.fractalsociety.voice";
const INSTALLED_APP_PATH: &str = "/Applications/Fractal Voice.app";
const MAX_REQUEST_BYTES: usize = 32 * 1024;
const MAX_PROJECT_NAME_CHARS: usize = 80;
const RESULT_SCHEMA: &str = "fractal.external_build_result.v1";
const MAX_RESULT_BYTES: u64 = 16 * 1024;
const RESULT_WAIT_ATTEMPTS: usize = 80;
const QUEUED_RESULT_WAIT_ATTEMPTS: usize = 24;
const RESULT_WAIT_INTERVAL: Duration = Duration::from_millis(250);

#[derive(Debug, Serialize)]
struct ExternalBuildHandoff<'a> {
    schema: &'static str,
    request: &'a str,
    project_name: &'a str,
    created_at_ms: u128,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExternalBuildResult {
    schema: String,
    status: String,
    #[serde(default)]
    message: String,
    #[serde(default)]
    project_name: String,
}

pub(crate) fn run(args: &HandoffArgs) -> Result<()> {
    let project_name = args.project_name.trim();
    validate_project_name(project_name)?;

    let mut request_bytes = Vec::new();
    io::stdin()
        .take((MAX_REQUEST_BYTES + 1) as u64)
        .read_to_end(&mut request_bytes)
        .context("read external build request from stdin")?;
    if request_bytes.len() > MAX_REQUEST_BYTES {
        bail!("build request exceeds {MAX_REQUEST_BYTES} bytes");
    }
    let request = std::str::from_utf8(&request_bytes)
        .context("build request must be UTF-8")?
        .trim();
    if request.is_empty() {
        bail!("build request is empty; pass it through stdin");
    }

    let path = write_handoff(request, project_name)?;
    let result_path = path.with_extension("result");
    let launched = launch_fractal_voice(&path);
    let wait_attempts = if launched {
        RESULT_WAIT_ATTEMPTS
    } else {
        // A sandbox can reject LaunchServices even while Fractal Voice is
        // already running and watching /tmp. Give that queued receiver a
        // bounded chance to return a duplicate-name result before we report
        // the request as queued.
        QUEUED_RESULT_WAIT_ATTEMPTS
    };
    match wait_for_result(&result_path, wait_attempts)? {
        Some(result) if result.status == "project_name_taken" => {
            let name = if result.project_name.trim().is_empty() {
                project_name
            } else {
                result.project_name.trim()
            };
            let detail = project_name_taken_message(name, &result.message);
            bail!("{detail}");
        }
        Some(result) if result.status == "failed" => {
            let detail = if result.message.trim().is_empty() {
                "Fractal Voice could not start the external build."
            } else {
                result.message.as_str()
            };
            bail!("{detail}");
        }
        Some(result) if result.status == "accepted" => {
            println!(
                "Sent “{project_name}” to Fractal Voice. The managed execution graph is starting."
            );
        }
        Some(result) => {
            fs::remove_file(&result_path).ok();
            bail!(
                "Fractal Voice returned an unsupported external-build result status `{}`",
                result.status
            );
        }
        None if launched => println!(
            "Sent “{project_name}” to Fractal Voice. The managed execution graph is starting."
        ),
        None => println!(
            "Queued “{project_name}” for Fractal Voice. Keep the app running; it will pick up the secure request automatically."
        ),
    }
    Ok(())
}

fn project_name_taken_message(project_name: &str, app_message: &str) -> String {
    if app_message.trim().is_empty() {
        format!(
            "Project name “{project_name}” is already taken. Retry with a different project name."
        )
    } else {
        app_message.trim().to_owned()
    }
}

fn wait_for_result(path: &Path, attempts: usize) -> Result<Option<ExternalBuildResult>> {
    for _ in 0..attempts {
        match read_result(path) {
            Ok(Some(result)) if result.schema != RESULT_SCHEMA => {
                fs::remove_file(path).ok();
                bail!("Fractal Voice returned an unsupported external-build result schema");
            }
            Ok(Some(result)) if result.status == "started" => {
                std::thread::sleep(RESULT_WAIT_INTERVAL);
            }
            Ok(Some(result)) => {
                fs::remove_file(path).ok();
                return Ok(Some(result));
            }
            Ok(None) => std::thread::sleep(RESULT_WAIT_INTERVAL),
            Err(error) => {
                fs::remove_file(path).ok();
                return Err(error);
            }
        }
    }
    Ok(None)
}

fn read_result(path: &Path) -> Result<Option<ExternalBuildResult>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("inspect Fractal Voice external-build result"),
    };
    if !metadata.file_type().is_file()
        || metadata.uid() != unsafe { libc::getuid() }
        || metadata.mode() & 0o777 != 0o600
        || metadata.len() == 0
        || metadata.len() > MAX_RESULT_BYTES
    {
        bail!("Fractal Voice external-build result is not a private regular file");
    }
    let bytes = fs::read(path).context("read Fractal Voice external-build result")?;
    let result =
        serde_json::from_slice(&bytes).context("decode Fractal Voice external-build result")?;
    Ok(Some(result))
}

fn launch_fractal_voice(path: &Path) -> bool {
    let installed_app = Path::new(INSTALLED_APP_PATH);
    if installed_app.is_dir() && run_open(&["-a", INSTALLED_APP_PATH], path) {
        return true;
    }
    run_open(&["-b", APP_BUNDLE_ID], path)
}

fn run_open(arguments: &[&str], path: &Path) -> bool {
    Command::new("/usr/bin/open")
        .args(arguments)
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn validate_project_name(project_name: &str) -> Result<()> {
    let count = project_name.chars().count();
    if count == 0 || count > MAX_PROJECT_NAME_CHARS {
        bail!("project name must be 1-{MAX_PROJECT_NAME_CHARS} characters");
    }
    if project_name.chars().any(char::is_control) {
        bail!("project name cannot contain control characters");
    }
    Ok(())
}

fn write_handoff(request: &str, project_name: &str) -> Result<PathBuf> {
    let created_at_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before Unix epoch")?
        .as_millis();
    let envelope = ExternalBuildHandoff {
        schema: SCHEMA,
        request,
        project_name,
        created_at_ms,
    };
    let bytes = serde_json::to_vec(&envelope).context("encode Fractal build handoff")?;
    let mut seed = Sha256::new();
    seed.update(&bytes);
    seed.update(std::process::id().to_le_bytes());
    seed.update(created_at_ms.to_le_bytes());
    let nonce: String = seed
        .finalize()
        .iter()
        .take(12)
        .map(|byte| format!("{byte:02x}"))
        .collect();
    // Use the system cross-application temporary directory rather than
    // `$TMPDIR`, which may point inside the caller's App Sandbox container.
    let path = PathBuf::from("/tmp").join(format!(
        "fractal-build-{}-{nonce}.fractalbuild",
        std::process::id()
    ));
    write_owner_only(&path, &bytes)?;
    Ok(path)
}

fn write_owner_only(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("create secure handoff {}", path.display()))?;
    file.write_all(bytes)
        .with_context(|| format!("write secure handoff {}", path.display()))?;
    file.sync_all()
        .with_context(|| format!("flush secure handoff {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn handoff_file_is_private_and_contains_the_named_request() {
        let path = write_handoff("Build a tiny Hello World app.", "Hello World").unwrap();
        let metadata = fs::metadata(&path).unwrap();
        let value: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();

        assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
        assert_eq!(value["schema"], SCHEMA);
        assert_eq!(value["request"], "Build a tiny Hello World app.");
        assert_eq!(value["project_name"], "Hello World");
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn handoff_rejects_unsafe_project_names() {
        assert!(validate_project_name("").is_err());
        assert!(validate_project_name("bad\nname").is_err());
        assert!(validate_project_name(&"x".repeat(81)).is_err());
        assert!(validate_project_name("Hello World").is_ok());
    }

    #[test]
    fn installed_receiver_is_tried_before_bundle_discovery() {
        assert_eq!(INSTALLED_APP_PATH, "/Applications/Fractal Voice.app");
        assert_eq!(APP_BUNDLE_ID, "com.fractalsociety.voice");
    }

    #[test]
    fn result_channel_accepts_only_private_terminal_result() {
        let path = std::env::temp_dir().join(format!(
            "fractal-build-result-test-{}-{}.result",
            std::process::id(),
            UNIX_EPOCH.elapsed().unwrap().as_nanos()
        ));
        let bytes = serde_json::json!({
            "schema": RESULT_SCHEMA,
            "status": "project_name_taken",
            "project_name": "Hello World",
            "message": "Project name “Hello World” is already taken. Retry with a different project name."
        });
        write_owner_only(&path, &serde_json::to_vec(&bytes).unwrap()).unwrap();
        let result = read_result(&path).unwrap().unwrap();
        assert_eq!(result.schema, RESULT_SCHEMA);
        assert_eq!(result.status, "project_name_taken");
        assert_eq!(result.project_name, "Hello World");
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn duplicate_project_result_tells_text_callers_to_retry() {
        assert_eq!(
            project_name_taken_message("link", ""),
            "Project name “link” is already taken. Retry with a different project name."
        );
        assert_eq!(
            project_name_taken_message("link", "Retry with another name."),
            "Retry with another name."
        );
    }
}
