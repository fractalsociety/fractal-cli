//! Compact, deterministic evidence manifests for verification runs.
//!
//! A manifest is the only verifier output that is persisted in the project
//! worktree.  It contains hashes and bounded identities, never raw output,
//! prompts, environment values, or absolute paths.  The bytes are canonical
//! `fractal-cjson-v1` and the file name is the SHA-256 of those bytes, making
//! manifests content addressed and naturally deduplicated.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::process::Command;

use anyhow::{bail, Context, Result};
use fractal_contracts::canonical_json;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

pub(crate) const MANIFEST_SCHEMA: &str = "fractal.evidence_manifest.v1";
pub(crate) const MAX_MANIFEST_BYTES: usize = 64 * 1024;
const MAX_STRING_CHARS: usize = 512;
const MAX_RUNS: usize = 64;
const MAX_CRITERIA: usize = 128;
const MAX_ARTIFACT_REFS: usize = 128;

/// Hashes describing the source that was evaluated.  Missing git metadata is
/// represented as `None`; it is never replaced with a synthetic success hash.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct SourceHashes {
    pub(crate) graph: Option<String>,
    pub(crate) commit: Option<String>,
    pub(crate) diff: Option<String>,
}

/// A single public, protected, or model verifier invocation.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct VerifierRun {
    pub(crate) id: String,
    pub(crate) kind: String,
    /// Safe command identity only.  Secret arguments and absolute paths are
    /// replaced before this field is persisted.
    pub(crate) argv_identity: Vec<String>,
    pub(crate) argv_hash: String,
    pub(crate) exit_code: Option<i32>,
    pub(crate) duration_ms: Option<u64>,
    pub(crate) output_hash: Option<String>,
    /// `pass`, `fail`, or `unavailable`; no implicit value is inferred.
    pub(crate) status: String,
    pub(crate) protected: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) artifact_refs: Vec<String>,
}

/// Persisted verification evidence.  Keep this structure intentionally boring:
/// changing a field changes the content address and therefore cannot mutate an
/// execution graph hash.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct EvidenceManifest {
    pub(crate) schema: String,
    pub(crate) policy_hash: Option<String>,
    pub(crate) node: String,
    pub(crate) attempt: u64,
    pub(crate) source: SourceHashes,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) criterion_ids: Vec<String>,
    pub(crate) verifier_runs: Vec<VerifierRun>,
    pub(crate) outcome: String,
    #[serde(default)]
    pub(crate) artifact_refs: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) enforcement_report_hash: Option<String>,
}

/// The relative path and digest returned after an atomic manifest write.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PersistedManifest {
    pub(crate) hash: String,
    pub(crate) relative_path: String,
    pub(crate) bytes: usize,
}

impl EvidenceManifest {
    pub(crate) fn new(node: impl Into<String>) -> Self {
        Self {
            schema: MANIFEST_SCHEMA.to_owned(),
            policy_hash: None,
            node: bounded(node.into()),
            attempt: 1,
            source: SourceHashes::default(),
            criterion_ids: Vec::new(),
            verifier_runs: Vec::new(),
            outcome: "unavailable".to_owned(),
            artifact_refs: Vec::new(),
            enforcement_report_hash: None,
        }
    }

    /// Normalize ordering and bounds before canonical serialization.
    pub(crate) fn normalize(&mut self) {
        self.schema = MANIFEST_SCHEMA.to_owned();
        self.node = safe_identity(std::mem::take(&mut self.node));
        self.outcome = normalize_status(&self.outcome);
        self.policy_hash = self.policy_hash.take().map(bounded_hash);
        self.enforcement_report_hash = self.enforcement_report_hash.take().map(bounded_hash);
        self.criterion_ids = bounded_unique(std::mem::take(&mut self.criterion_ids), MAX_CRITERIA);
        self.artifact_refs =
            bounded_unique(std::mem::take(&mut self.artifact_refs), MAX_ARTIFACT_REFS);
        self.source.graph = self.source.graph.take().map(bounded_hash);
        self.source.commit = self.source.commit.take().map(bounded_hash);
        self.source.diff = self.source.diff.take().map(bounded_hash);
        for run in &mut self.verifier_runs {
            run.id = safe_identity(std::mem::take(&mut run.id));
            run.kind = bounded(std::mem::take(&mut run.kind));
            run.status = normalize_status(&run.status);
            run.argv_identity = run.argv_identity.drain(..).map(bounded).collect();
            run.argv_hash = bounded_hash(std::mem::take(&mut run.argv_hash));
            run.output_hash = run.output_hash.take().map(bounded_hash);
            run.artifact_refs =
                bounded_unique(std::mem::take(&mut run.artifact_refs), MAX_ARTIFACT_REFS);
        }
        self.verifier_runs.sort_by(|left, right| {
            (&left.id, &left.kind, &left.argv_hash).cmp(&(&right.id, &right.kind, &right.argv_hash))
        });
        self.verifier_runs.truncate(MAX_RUNS);
    }
}

/// Persist a manifest under `.fractal/evidence`, atomically and without
/// changing `.fractal/project.fractal` or its graph hash.
pub(crate) fn persist_manifest(
    workspace: &Path,
    mut manifest: EvidenceManifest,
) -> Result<PersistedManifest> {
    manifest.normalize();
    let mut value = serde_json::to_value(&manifest).context("encode evidence manifest")?;
    let mut bytes = canonical_json(&value).context("canonicalize evidence manifest")?;
    // The regular bounds above make this path uncommon.  If a caller supplied
    // very long argv identities, trim deterministically until the hard limit
    // is met rather than writing an unbounded controller artifact.
    if bytes.len() > MAX_MANIFEST_BYTES {
        trim_to_limit(&mut manifest);
        value = serde_json::to_value(&manifest).context("encode bounded evidence manifest")?;
        bytes = canonical_json(&value).context("canonicalize bounded evidence manifest")?;
    }
    if bytes.len() > MAX_MANIFEST_BYTES {
        bail!("evidence manifest exceeds {MAX_MANIFEST_BYTES} bytes after bounding");
    }

    let hash = sha256_bytes(&bytes);
    let digest = hash.strip_prefix("sha256:").unwrap_or(&hash);
    let relative_path = format!(".fractal/evidence/{digest}.json");
    let directory = workspace.join(".fractal").join("evidence");
    fs::create_dir_all(&directory).with_context(|| format!("create {relative_path}"))?;
    let destination = directory.join(format!("{digest}.json"));

    // Existing bytes are authoritative for a content address.  A mismatch is
    // a corruption signal, not an invitation to overwrite another manifest.
    if destination.is_file() {
        let existing = fs::read(&destination).with_context(|| format!("read {relative_path}"))?;
        if existing != bytes {
            bail!("content-addressed evidence manifest collision at {relative_path}");
        }
        return Ok(PersistedManifest {
            hash,
            relative_path,
            bytes: existing.len(),
        });
    }

    let temp_name = format!(".{digest}.{}.tmp", std::process::id());
    let temp = directory.join(temp_name);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)
        .with_context(|| format!("create temporary manifest for {relative_path}"))?;
    let write_result = (|| -> Result<()> {
        file.write_all(&bytes).context("write evidence manifest")?;
        file.sync_all().context("sync evidence manifest")?;
        fs::rename(&temp, &destination).context("atomically install evidence manifest")?;
        Ok(())
    })();
    if write_result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    write_result?;
    Ok(PersistedManifest {
        hash,
        relative_path,
        bytes: bytes.len(),
    })
}

/// Rebind an already persisted manifest to the final policy-enforcement
/// report.  This creates a new content address (the previous preflight
/// manifest remains immutable) and lets execution evidence point at the exact
/// report returned by postflight.
pub(crate) fn rebind_enforcement_report(
    workspace: &Path,
    relative_path: &str,
    report_hash: &str,
) -> Result<Option<String>> {
    if !safe_relative_ref(relative_path) {
        return Ok(None);
    }
    let bytes = fs::read(workspace.join(relative_path))
        .with_context(|| format!("read evidence manifest {relative_path}"))?;
    let mut manifest: EvidenceManifest =
        serde_json::from_slice(&bytes).context("decode evidence manifest for policy binding")?;
    manifest.enforcement_report_hash = Some(report_hash.to_owned());
    let persisted = persist_manifest(workspace, manifest)?;
    Ok(Some(persisted.relative_path))
}

/// Hash the canonical identity of an argv list.  This is used for duplicate
/// verifier detection and deliberately does not expose the raw command.
pub(crate) fn argv_identity(argv: &[String], protected: bool) -> (Vec<String>, String) {
    let mut safe = Vec::with_capacity(argv.len());
    let mut redact_next = false;
    for (index, argument) in argv.iter().enumerate() {
        let lower = argument.to_ascii_lowercase();
        if redact_next {
            safe.push("<redacted>".to_owned());
            redact_next = false;
            continue;
        }
        if lower == "--workspace" || lower == "--worktree" || lower == "--cwd" {
            safe.push(bounded(argument.clone()));
            redact_next = true;
            continue;
        }
        if lower.contains("/fractal-check-") || lower.contains("\\fractal-check-") {
            safe.push("<workspace>".to_owned());
            continue;
        }
        if protected && index > 0 && !argument.starts_with('-') {
            // Preserve only an equality-safe digest so two distinct
            // operator-owned checkers do not collapse into a false duplicate;
            // the path/content itself remains sealed from the manifest.
            safe.push(format!("<protected:{}>", short_hash(argument)));
            continue;
        }
        if is_secret_key(&lower) || is_secret_value(argument) {
            safe.push("<redacted>".to_owned());
            redact_next = lower.ends_with("key")
                || lower.ends_with("token")
                || lower.ends_with("secret")
                || lower.ends_with("password")
                || lower.ends_with("authorization");
            continue;
        }
        if argument.starts_with('/') || argument.starts_with('~') || argument.contains("\\") {
            safe.push(
                Path::new(argument)
                    .file_name()
                    .and_then(|name| name.to_str())
                    .map(|name| format!("<path>/{name}"))
                    .unwrap_or_else(|| "<path>".to_owned()),
            );
        } else {
            safe.push(bounded(argument.clone()));
        }
    }
    let value = Value::Array(safe.iter().cloned().map(Value::String).collect());
    let bytes = canonical_json(&value).unwrap_or_default();
    (safe, sha256_bytes(&bytes))
}

fn short_hash(value: &str) -> String {
    sha256_bytes(value.as_bytes())
        .strip_prefix("sha256:")
        .unwrap_or_default()
        .chars()
        .take(16)
        .collect()
}

/// Hash source provenance without persisting command output.  Git failures are
/// represented as unavailable hashes and never treated as a pass.
pub(crate) fn source_hashes(workspace: &Path, graph_hash: Option<&str>) -> SourceHashes {
    let commit =
        git_output(workspace, &["rev-parse", "HEAD"]).map(|value| sha256_bytes(value.as_bytes()));
    let diff = git_bytes(workspace, &["diff", "--no-ext-diff", "--binary"])
        .map(|value| sha256_bytes(&value));
    SourceHashes {
        graph: graph_hash
            .filter(|value| !value.trim().is_empty())
            .map(|value| bounded_hash(value.to_owned())),
        commit,
        diff,
    }
}

fn git_output(workspace: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .args(["-C", workspace.to_str()?])
        .args(args)
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn git_bytes(workspace: &Path, args: &[&str]) -> Option<Vec<u8>> {
    let output = Command::new("git")
        .args(["-C", workspace.to_str()?])
        .args(args)
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .output()
        .ok()?;
    output.status.success().then_some(output.stdout)
}

fn trim_to_limit(manifest: &mut EvidenceManifest) {
    for run in &mut manifest.verifier_runs {
        run.argv_identity.truncate(8);
        run.argv_identity.iter_mut().for_each(|arg| {
            if arg.chars().count() > 96 {
                *arg = arg.chars().take(96).collect();
            }
        });
        run.artifact_refs.truncate(8);
    }
    manifest.verifier_runs.truncate(16);
    manifest.criterion_ids.truncate(32);
    manifest.artifact_refs.truncate(32);
    manifest.node = manifest.node.chars().take(120).collect();
}

fn bounded(value: String) -> String {
    value.chars().take(MAX_STRING_CHARS).collect()
}

fn bounded_hash(value: String) -> String {
    bounded(value)
}

fn safe_identity(value: String) -> String {
    let lower = value.to_ascii_lowercase();
    if value.starts_with('/')
        || value.starts_with('~')
        || value.contains("..")
        || value.contains('\\')
        || [
            "prompt",
            "secret",
            "token",
            "password",
            "cot",
            "chain-of-thought",
        ]
        .iter()
        .any(|needle| lower.contains(needle))
    {
        format!("id:{}", short_hash(&value))
    } else {
        bounded(value)
    }
}

fn bounded_unique(mut values: Vec<String>, limit: usize) -> Vec<String> {
    values.retain(|value| !value.trim().is_empty());
    values = values.into_iter().map(bounded).collect();
    values.sort();
    values.dedup();
    values.truncate(limit);
    values
}

fn normalize_status(value: &str) -> String {
    match value {
        "pass" | "fail" | "unavailable" => value.to_owned(),
        _ => "unavailable".to_owned(),
    }
}

fn is_secret_key(value: &str) -> bool {
    let value = value.trim_start_matches('-');
    [
        "api_key",
        "apikey",
        "token",
        "secret",
        "password",
        "authorization",
        "prompt",
        "cot",
    ]
    .iter()
    .any(|key| value == *key || value.ends_with(&format!("_{key}")))
}

fn is_secret_value(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    lower.contains("api_key=")
        || lower.contains("token=")
        || lower.contains("secret=")
        || lower.contains("password=")
        || lower.contains("authorization=")
        || lower.contains("chain-of-thought")
        || lower.contains("<prompt>")
}

/// Validate a relative manifest reference before handing it to learning or
/// failure-graph projections.
pub(crate) fn safe_relative_ref(reference: &str) -> bool {
    let path = Path::new(reference);
    !reference.is_empty()
        && reference.len() <= MAX_STRING_CHARS
        && !path.is_absolute()
        && !reference.starts_with('~')
        && !reference.contains("..")
        && !reference.contains('\\')
        && reference.starts_with(".fractal/evidence/")
        && reference.ends_with(".json")
}

pub(crate) fn sha256_bytes(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut output = String::from("sha256:");
    for byte in digest {
        output.push_str(&format!("{byte:02x}"));
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn workspace() -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "fractal-evidence-manifest-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn deterministic_bounded_redacted_and_content_addressed() {
        let root = workspace();
        let mut manifest = EvidenceManifest::new("verify");
        manifest.policy_hash = Some("sha256:policy".to_owned());
        manifest.verifier_runs.push(VerifierRun {
            id: "hidden".to_owned(),
            kind: "regression".to_owned(),
            argv_identity: vec![
                "/private/checker.py".to_owned(),
                "--api_key=secret".to_owned(),
                "--workspace".to_owned(),
                "/tmp/workspace".to_owned(),
            ],
            argv_hash: String::new(),
            exit_code: Some(0),
            duration_ms: Some(2),
            output_hash: Some("sha256:out".to_owned()),
            status: "pass".to_owned(),
            protected: true,
            artifact_refs: Vec::new(),
        });
        let (identity, hash) = argv_identity(&manifest.verifier_runs[0].argv_identity, true);
        manifest.verifier_runs[0].argv_identity = identity;
        manifest.verifier_runs[0].argv_hash = hash;
        let first = persist_manifest(&root, manifest.clone()).unwrap();
        let second = persist_manifest(&root, manifest).unwrap();
        assert_eq!(first.hash, second.hash);
        assert_eq!(first.relative_path, second.relative_path);
        assert!(first.bytes <= MAX_MANIFEST_BYTES);
        let bytes = fs::read(root.join(&first.relative_path)).unwrap();
        assert_eq!(sha256_bytes(&bytes), first.hash);
        let text = String::from_utf8(bytes).unwrap();
        assert!(!text.contains("secret"));
        assert!(!text.contains("/private"));
        assert!(!text.contains("/tmp/workspace"));
        assert!(safe_relative_ref(&first.relative_path));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn argv_identity_is_stable_and_redacts_secrets() {
        let (first, hash) = argv_identity(
            &[
                "python3".to_owned(),
                "/tmp/checker.py".to_owned(),
                "--token".to_owned(),
                "secret".to_owned(),
            ],
            false,
        );
        let (second, hash_again) = argv_identity(
            &[
                "python3".to_owned(),
                "/another/checker.py".to_owned(),
                "--token".to_owned(),
                "other".to_owned(),
            ],
            false,
        );
        assert_eq!(hash, hash_again);
        assert_eq!(first, second);
        assert!(first.iter().all(|value| !value.contains("secret")));
    }
}
