//! One voice surface with a native Moonshine default and Superwhisper adapter.

use std::fs;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;

use crate::cli::{VoiceArgs, VoiceCommand, VoiceEngine};

const MOONSHINE_PACKAGE: &str = "moonshine-voice==0.0.73";
const MOONSHINE_BRIDGE: &str = include_str!("../scripts/moonshine_voice_bridge.py");

pub(crate) fn run(
    args: &VoiceArgs,
    dictate: bool,
    fractalwork_override: Option<&Path>,
    coordinate: bool,
) -> Result<()> {
    if let Some(command) = args.command {
        return match command {
            VoiceCommand::Setup => setup_moonshine(),
            VoiceCommand::Engines => show_engines(),
        };
    }
    match args.engine {
        VoiceEngine::Moonshine => {
            if args.mode_key.is_some() {
                bail!("--mode-key is only valid with --engine superwhisper");
            }
            run_moonshine(args, dictate, fractalwork_override, coordinate)
        }
        VoiceEngine::Superwhisper => launch_superwhisper(args, dictate),
    }
}

fn run_moonshine(
    args: &VoiceArgs,
    dictate: bool,
    fractalwork_override: Option<&Path>,
    coordinate: bool,
) -> Result<()> {
    let paths = voice_paths(&fractal_home()?);
    if args.dry_run {
        println!("engine: moonshine");
        println!("model: moonshine-v2-medium-streaming");
        println!("runtime: {}", paths.python.display());
        println!("model cache: {}", paths.model_cache.display());
        println!(
            "ready: {}",
            if moonshine_ready(&paths) { "yes" } else { "no" }
        );
        return Ok(());
    }
    if !moonshine_ready(&paths) {
        bail!("Moonshine is not set up; run `fractal voice setup` once");
    }
    if !args.app_control && !std::io::stdin().is_terminal() {
        bail!(
            "native microphone recording requires an interactive terminal \
             (or the Fractal Voice companion)"
        );
    }
    write_bridge(&paths)?;
    eprintln!("  🎙 Moonshine v2 Medium is listening locally.");
    eprintln!("  Speak your instruction, then press Enter to stop.\n");
    let output = Command::new(&paths.python)
        .arg(&paths.bridge)
        .arg("transcribe")
        .arg("--cache-root")
        .arg(&paths.model_cache)
        .stdin(Stdio::inherit())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .output()
        .context("launch Moonshine microphone transcriber")?;
    if !output.status.success() {
        bail!("Moonshine transcription failed ({})", output.status);
    }
    let response = last_json(&output.stdout).context("decode Moonshine transcript")?;
    let transcript = response
        .get("transcript")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .context("Moonshine did not detect any speech")?;
    println!("\nTranscript:\n  {transcript}\n");
    crate::ingest::run_voice_transcript(transcript, dictate, args, fractalwork_override, coordinate)
}

fn setup_moonshine() -> Result<()> {
    let paths = voice_paths(&fractal_home()?);
    fs::create_dir_all(&paths.root)
        .with_context(|| format!("create Moonshine runtime {}", paths.root.display()))?;
    fs::create_dir_all(&paths.model_cache).with_context(|| {
        format!(
            "create Moonshine model cache {}",
            paths.model_cache.display()
        )
    })?;
    if paths.python.is_file() && !python_is_compatible(&paths.python) {
        println!("Replacing an incompatible pre-Python-3.10 Moonshine environment…");
        fs::remove_dir_all(&paths.venv)
            .with_context(|| format!("replace Moonshine environment {}", paths.venv.display()))?;
    }
    if !paths.python.is_file() {
        let python = compatible_python()?;
        println!("Creating Fractal's isolated Moonshine environment…");
        let status = Command::new(&python)
            .args(["-m", "venv"])
            .arg(&paths.venv)
            .status()
            .with_context(|| format!("create Moonshine Python environment with {python}"))?;
        if !status.success() {
            bail!("{python} could not create the Moonshine environment ({status})");
        }
    }
    println!("Installing pinned native Moonshine runtime {MOONSHINE_PACKAGE}…");
    let status = Command::new(&paths.python)
        .args([
            "-m",
            "pip",
            "install",
            "--disable-pip-version-check",
            MOONSHINE_PACKAGE,
        ])
        .status()
        .context("install Moonshine Voice into its isolated environment")?;
    if !status.success() {
        bail!("Moonshine package installation failed ({status})");
    }
    write_bridge(&paths)?;
    println!("Downloading and verifying Moonshine v2 Medium Streaming…");
    let output = Command::new(&paths.python)
        .arg(&paths.bridge)
        .arg("setup")
        .arg("--cache-root")
        .arg(&paths.model_cache)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .output()
        .context("download Moonshine model")?;
    if !output.status.success() {
        bail!("Moonshine model setup failed ({})", output.status);
    }
    let receipt = last_json(&output.stdout).context("decode Moonshine setup receipt")?;
    fs::write(&paths.receipt, serde_json::to_vec_pretty(&receipt)?)
        .with_context(|| format!("write Moonshine receipt {}", paths.receipt.display()))?;
    println!("✓ Native voice ready: Moonshine v2 Medium Streaming");
    println!("  model cache: {}", paths.model_cache.display());
    println!("  `fractal voice` now records locally with Moonshine.");
    Ok(())
}

fn show_engines() -> Result<()> {
    let paths = voice_paths(&fractal_home()?);
    println!("Voice engines:");
    println!(
        "  moonshine     default · native/on-device · {}",
        if moonshine_ready(&paths) {
            "ready"
        } else {
            "not installed (run `fractal voice setup`)"
        }
    );
    println!("  superwhisper  optional compatibility backend · external app");
    Ok(())
}

fn launch_superwhisper(args: &VoiceArgs, dictate: bool) -> Result<()> {
    let env_key = if dictate {
        "FRACTAL_SUPERWHISPER_DICTATE_MODE_KEY"
    } else {
        "FRACTAL_SUPERWHISPER_MODE_KEY"
    };
    let mode_key = args
        .mode_key
        .clone()
        .or_else(|| std::env::var(env_key).ok())
        .filter(|value| !value.trim().is_empty());
    let links = superwhisper_links(mode_key.as_deref())?;
    if args.dry_run {
        for link in links {
            println!("open {link}");
        }
        return Ok(());
    }
    #[cfg(not(target_os = "macos"))]
    bail!("Superwhisper deep-link launching is supported on macOS only");
    #[cfg(target_os = "macos")]
    {
        for (index, link) in links.iter().enumerate() {
            let status = Command::new("open")
                .arg(link)
                .status()
                .with_context(|| format!("open Superwhisper deep link {link}"))?;
            if !status.success() {
                bail!("macOS open failed for {link} ({status})");
            }
            if index + 1 < links.len() {
                std::thread::sleep(Duration::from_millis(args.delay_ms));
            }
        }
    }
    Ok(())
}

fn superwhisper_links(mode_key: Option<&str>) -> Result<Vec<String>> {
    let mut links = Vec::new();
    if let Some(key) = mode_key {
        let key = key.trim();
        if key.is_empty()
            || key.len() > 128
            || !key.chars().all(|character| {
                character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-')
            })
        {
            bail!("Superwhisper mode key must use only [A-Za-z0-9._-]");
        }
        links.push(format!("superwhisper://mode?key={key}"));
    }
    links.push("superwhisper://record".to_owned());
    Ok(links)
}

#[derive(Debug)]
struct VoicePaths {
    root: PathBuf,
    venv: PathBuf,
    python: PathBuf,
    bridge: PathBuf,
    receipt: PathBuf,
    model_cache: PathBuf,
}

fn voice_paths(home: &Path) -> VoicePaths {
    let root = home.join("voice").join("moonshine");
    let venv = root.join("venv");
    VoicePaths {
        python: venv.join("bin").join("python"),
        bridge: root.join("fractal_moonshine_bridge.py"),
        receipt: root.join("setup.json"),
        model_cache: home.join("models").join("moonshine-v2-medium-streaming"),
        root,
        venv,
    }
}

fn fractal_home() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("FRACTAL_HOME").filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(path));
    }
    let home = std::env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .context("HOME is unavailable; set FRACTAL_HOME for Moonshine")?;
    Ok(PathBuf::from(home).join(".fractal"))
}

fn moonshine_ready(paths: &VoicePaths) -> bool {
    if !paths.python.is_file() || !python_is_compatible(&paths.python) {
        return false;
    }
    let Ok(receipt) = fs::read(&paths.receipt) else {
        return false;
    };
    let Ok(receipt) = serde_json::from_slice::<Value>(&receipt) else {
        return false;
    };
    receipt
        .get("model_path")
        .and_then(Value::as_str)
        .is_some_and(|path| Path::new(path).exists())
}

fn compatible_python() -> Result<String> {
    for candidate in ["python3.12", "python3.11", "python3.10", "python3"] {
        if python_is_compatible(Path::new(candidate)) {
            return Ok(candidate.to_owned());
        }
    }
    bail!(
        "Moonshine requires Python 3.10–3.12; install one with \
         `brew install python@3.12`, then rerun `fractal voice setup`"
    )
}

fn python_is_compatible(python: &Path) -> bool {
    Command::new(python)
        .args([
            "-c",
            "import sys; raise SystemExit(0 if (3, 10) <= sys.version_info[:2] <= (3, 12) else 1)",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn write_bridge(paths: &VoicePaths) -> Result<()> {
    fs::create_dir_all(&paths.root)?;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&paths.bridge)
        .with_context(|| format!("write Moonshine bridge {}", paths.bridge.display()))?;
    file.write_all(MOONSHINE_BRIDGE.as_bytes())?;
    Ok(())
}

fn last_json(bytes: &[u8]) -> Result<Value> {
    let text = std::str::from_utf8(bytes).context("Moonshine returned non-UTF-8 output")?;
    let line = text
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .context("Moonshine returned no result")?;
    serde_json::from_str(line).map_err(|error| anyhow!("invalid Moonshine result: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn superwhisper_mode_keys_are_safe_data() {
        assert_eq!(
            superwhisper_links(Some("fractal-command_1")).unwrap(),
            vec![
                "superwhisper://mode?key=fractal-command_1",
                "superwhisper://record"
            ]
        );
        assert!(superwhisper_links(Some("bad&record=true")).is_err());
    }

    #[test]
    fn moonshine_paths_are_owned_by_fractal_home() {
        let paths = voice_paths(Path::new("/tmp/fractal-home"));
        assert_eq!(
            paths.model_cache,
            Path::new("/tmp/fractal-home/models/moonshine-v2-medium-streaming")
        );
        assert_eq!(
            paths.python,
            Path::new("/tmp/fractal-home/voice/moonshine/venv/bin/python")
        );
    }

    #[test]
    fn parses_only_the_final_json_result() {
        let value = last_json(
            b"download progress\n{\"schema\":\"fractal.moonshine_transcript.v1\",\"transcript\":\"hello\"}\n",
        )
        .unwrap();
        assert_eq!(value["transcript"], "hello");
    }
}
