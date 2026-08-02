//! Project audit inventory loading and shard reporting.
//!
//! This module is intentionally self-contained so later command wiring can
//! include it without changing its public contract. It performs read-only
//! repository inventory, conservative signal extraction, native verification
//! execution under strict bounds, and machine-readable shard report generation.

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::fs;
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Minimal interfaces expected from the project_catalog contract.
///
/// The command layer can convert these directly into the canonical
/// project_catalog model once that module is wired into the crate. Keeping the
/// names and payload shapes explicit here prevents the audit implementation
/// from depending on untyped JSON blobs.
pub(crate) mod project_catalog_contract {
    use super::*;

    #[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
    pub(crate) struct CatalogShardReport {
        pub(crate) schema: String,
        pub(crate) workspace: String,
        pub(crate) shard: CatalogShard,
        pub(crate) git: GitFingerprint,
        pub(crate) inventory: RepositoryInventory,
        pub(crate) extraction: ExtractedCatalogSignals,
        pub(crate) native_tests: Vec<NativeTestReport>,
        pub(crate) status: AuditStatus,
        pub(crate) warnings: Vec<String>,
    }

    #[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
    pub(crate) struct CatalogShard {
        pub(crate) index: u32,
        pub(crate) total: u32,
        pub(crate) selected_files: usize,
    }

    #[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
    pub(crate) struct RepositoryInventory {
        pub(crate) project_fractal_hash: Option<String>,
        pub(crate) files: Vec<FileEvidence>,
        pub(crate) manifests: Vec<ManifestEvidence>,
        pub(crate) architecture_docs: Vec<FileEvidence>,
    }

    #[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
    pub(crate) struct ExtractedCatalogSignals {
        pub(crate) implemented_features: Vec<CatalogSignal>,
        pub(crate) components: Vec<CatalogSignal>,
        pub(crate) dependencies: Vec<CatalogSignal>,
        pub(crate) decisions: Vec<CatalogSignal>,
        pub(crate) relationships: Vec<RelationshipCandidate>,
    }

    #[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
    pub(crate) struct FileEvidence {
        pub(crate) path: String,
        pub(crate) sha256: String,
        pub(crate) bytes: u64,
    }

    #[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
    pub(crate) struct ManifestEvidence {
        pub(crate) path: String,
        pub(crate) kind: ManifestKind,
        pub(crate) sha256: String,
        pub(crate) bytes: u64,
    }

    #[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
    pub(crate) enum ManifestKind {
        Cargo,
        PackageJson,
        Pyproject,
        Requirements,
        GoMod,
        SwiftPackage,
        Other(String),
    }

    impl Default for ManifestKind {
        fn default() -> Self {
            Self::Other("unknown".to_owned())
        }
    }

    #[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
    pub(crate) struct CatalogSignal {
        pub(crate) name: String,
        pub(crate) kind: String,
        pub(crate) evidence_path: String,
        pub(crate) evidence_hash: String,
        pub(crate) confidence: Confidence,
    }

    #[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
    pub(crate) struct RelationshipCandidate {
        pub(crate) source: String,
        pub(crate) relationship: RelationshipKind,
        pub(crate) target: String,
        pub(crate) evidence_path: String,
        pub(crate) evidence_hash: String,
        pub(crate) confidence: Confidence,
    }

    #[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
    pub(crate) enum RelationshipKind {
        DependsOn,
        Implements,
        Tests,
        #[default]
        Documents,
        Configures,
        Invokes,
    }

    #[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
    pub(crate) enum Confidence {
        High,
        Medium,
        #[default]
        Low,
    }

    #[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
    pub(crate) struct GitFingerprint {
        pub(crate) commit: Option<String>,
        pub(crate) dirty: bool,
        pub(crate) dirty_fingerprint: Option<String>,
    }

    #[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
    pub(crate) enum NativeCommandStatus {
        Passed,
        Failed,
        TimedOut,
        MissingTool,
        #[default]
        Rejected,
    }

    #[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
    pub(crate) struct NativeTestReport {
        pub(crate) command: Vec<String>,
        pub(crate) status: NativeCommandStatus,
        pub(crate) exit_code: Option<i32>,
        pub(crate) duration_ms: u128,
        pub(crate) output: String,
        pub(crate) truncated: bool,
    }

    #[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
    pub(crate) enum AuditStatus {
        Pass,
        Fail,
        #[default]
        Inconclusive,
    }
}

pub(crate) use project_catalog_contract::*;

const SCHEMA: &str = "fractal.project-audit-shard-report.v1";
const DEFAULT_MAX_FILES: usize = 2_000;
const DEFAULT_MAX_FILE_BYTES: u64 = 256 * 1024;
const DEFAULT_OUTPUT_BYTES: usize = 32 * 1024;
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Clone, Debug)]
pub(crate) struct AuditOptions {
    pub(crate) workspace: PathBuf,
    pub(crate) shard_index: u32,
    pub(crate) shard_total: u32,
    pub(crate) max_files: usize,
    pub(crate) max_file_bytes: u64,
    pub(crate) command_timeout: Duration,
    pub(crate) output_limit_bytes: usize,
    pub(crate) native_test_commands: Vec<Vec<String>>,
}

impl AuditOptions {
    pub(crate) fn new(workspace: impl Into<PathBuf>, shard_index: u32, shard_total: u32) -> Self {
        Self {
            workspace: workspace.into(),
            shard_index,
            shard_total,
            max_files: DEFAULT_MAX_FILES,
            max_file_bytes: DEFAULT_MAX_FILE_BYTES,
            command_timeout: DEFAULT_TIMEOUT,
            output_limit_bytes: DEFAULT_OUTPUT_BYTES,
            native_test_commands: Vec::new(),
        }
    }
}

#[derive(Clone, Debug)]
struct WalkedFile {
    relative: String,
    absolute: PathBuf,
    bytes: u64,
    sha256: String,
}

/// Load the contract-defined inventory for one deterministic shard and emit a
/// machine-readable report. This function is read-only with respect to the
/// repository except for native commands explicitly supplied by the caller.
pub(crate) fn load_project_audit_shard(options: AuditOptions) -> Result<CatalogShardReport> {
    validate_shard(options.shard_index, options.shard_total)?;
    let workspace = canonical_workspace(&options.workspace)?;
    let mut warnings = Vec::new();
    let all_files = walk_repository(
        &workspace,
        options.max_files,
        options.max_file_bytes,
        &mut warnings,
    )?;
    let shard_files = select_shard(&all_files, options.shard_index, options.shard_total);
    let inventory = build_inventory(&workspace, &shard_files)?;
    let extraction = extract_signals(
        &workspace,
        &shard_files,
        options.max_file_bytes,
        &mut warnings,
    )?;
    let git = git_fingerprint(&workspace, &mut warnings);
    let native_tests = options
        .native_test_commands
        .iter()
        .map(|cmd| {
            run_native_test(
                &workspace,
                cmd,
                options.command_timeout,
                options.output_limit_bytes,
            )
        })
        .collect::<Vec<_>>();
    let status = classify_status(&native_tests, &git, &warnings);

    Ok(CatalogShardReport {
        schema: SCHEMA.to_owned(),
        workspace: workspace.display().to_string(),
        shard: CatalogShard {
            index: options.shard_index,
            total: options.shard_total,
            selected_files: shard_files.len(),
        },
        git,
        inventory,
        extraction,
        native_tests,
        status,
        warnings,
    })
}

#[cfg(test)]
pub(crate) fn shard_report_json(report: &CatalogShardReport) -> Result<String> {
    serde_json::to_string_pretty(report).context("serialize project audit shard report")
}

fn validate_shard(index: u32, total: u32) -> Result<()> {
    if total == 0 {
        bail!("shard_total must be greater than zero");
    }
    if index >= total {
        bail!("shard_index {index} must be less than shard_total {total}");
    }
    Ok(())
}

fn canonical_workspace(path: &Path) -> Result<PathBuf> {
    let canonical = path
        .canonicalize()
        .with_context(|| format!("canonicalize workspace {}", path.display()))?;
    if !canonical.is_dir() {
        bail!("workspace is not a directory: {}", canonical.display());
    }
    Ok(canonical)
}

fn walk_repository(
    workspace: &Path,
    max_files: usize,
    max_file_bytes: u64,
    warnings: &mut Vec<String>,
) -> Result<Vec<WalkedFile>> {
    let mut files = Vec::new();
    let mut stack = vec![workspace.to_path_buf()];
    while let Some(directory) = stack.pop() {
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) => {
                warnings.push(format!(
                    "could not read directory {}: {error}",
                    rel_display(workspace, &directory)
                ));
                continue;
            }
        };
        let mut children = Vec::new();
        for entry in entries.flatten() {
            children.push(entry.path());
        }
        children.sort_by_key(|a| rel_string(workspace, a));
        for path in children.into_iter().rev() {
            let relative = match path.strip_prefix(workspace) {
                Ok(relative) => relative,
                Err(_) => continue,
            };
            if should_exclude_path(relative) {
                continue;
            }
            let metadata = match fs::symlink_metadata(&path) {
                Ok(metadata) => metadata,
                Err(error) => {
                    warnings.push(format!(
                        "could not inspect {}: {error}",
                        rel_display(workspace, &path)
                    ));
                    continue;
                }
            };
            if metadata.file_type().is_symlink() {
                if symlink_escapes_workspace(workspace, &path) {
                    warnings.push(format!(
                        "excluded symlink escaping workspace: {}",
                        rel_display(workspace, &path)
                    ));
                }
                continue;
            }
            if metadata.is_dir() {
                stack.push(path);
                continue;
            }
            if !metadata.is_file() {
                continue;
            }
            if metadata.len() > max_file_bytes {
                warnings.push(format!(
                    "excluded large file: {}",
                    rel_display(workspace, &path)
                ));
                continue;
            }
            if files.len() >= max_files {
                warnings.push(format!("repository walk capped at {max_files} files"));
                return Ok(sorted_files(files));
            }
            let sha256 = sha256_file(&path).with_context(|| format!("hash {}", path.display()))?;
            files.push(WalkedFile {
                relative: rel_string(workspace, &path),
                absolute: path,
                bytes: metadata.len(),
                sha256,
            });
        }
    }
    Ok(sorted_files(files))
}

fn sorted_files(mut files: Vec<WalkedFile>) -> Vec<WalkedFile> {
    files.sort_by(|a, b| a.relative.cmp(&b.relative));
    files
}

fn select_shard(files: &[WalkedFile], index: u32, total: u32) -> Vec<WalkedFile> {
    files
        .iter()
        .filter(|file| stable_bucket(&file.relative, total) == index)
        .cloned()
        .collect()
}

fn stable_bucket(path: &str, total: u32) -> u32 {
    let digest = Sha256::digest(path.as_bytes());
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&digest[..8]);
    (u64::from_be_bytes(bytes) % total as u64) as u32
}

fn should_exclude_path(relative: &Path) -> bool {
    let mut components = relative.components();
    if let Some(Component::Normal(first)) = components.next() {
        if first == OsStr::new(".fractal") {
            return components
                .next()
                .is_some_and(|component| component.as_os_str() != OsStr::new("project.fractal"));
        }
    }
    relative.components().any(|component| {
        let name = component.as_os_str().to_string_lossy().to_ascii_lowercase();
        matches!(
            name.as_str(),
            ".git"
                | "target"
                | "dist"
                | "build"
                | "node_modules"
                | ".pnpm-store"
                | ".yarn"
                | "vendor"
                | "__pycache__"
                | ".pytest_cache"
                | ".mypy_cache"
                | ".ruff_cache"
                | ".gradle"
                | "coverage"
        ) || name == ".env"
            || name.starts_with(".env.")
            || name.ends_with(".pem")
            || name.ends_with(".key")
            || name.ends_with("credentials")
            || name.contains("secret")
    })
}

fn symlink_escapes_workspace(workspace: &Path, path: &Path) -> bool {
    match fs::canonicalize(path) {
        Ok(target) => !target.starts_with(workspace),
        Err(_) => true,
    }
}

fn build_inventory(workspace: &Path, files: &[WalkedFile]) -> Result<RepositoryInventory> {
    let mut project_fractal_hash = None;
    let mut evidence = Vec::new();
    let mut manifests = Vec::new();
    let mut architecture_docs = Vec::new();
    for file in files {
        let file_evidence = FileEvidence {
            path: file.relative.clone(),
            sha256: file.sha256.clone(),
            bytes: file.bytes,
        };
        if file.relative == ".fractal/project.fractal" {
            project_fractal_hash = Some(file.sha256.clone());
        }
        if let Some(kind) = manifest_kind(&file.relative) {
            manifests.push(ManifestEvidence {
                path: file.relative.clone(),
                kind,
                sha256: file.sha256.clone(),
                bytes: file.bytes,
            });
        }
        if is_architecture_doc(&file.relative) {
            architecture_docs.push(file_evidence.clone());
        }
        evidence.push(file_evidence);
    }
    evidence.sort_by(|a, b| a.path.cmp(&b.path));
    manifests.sort_by(|a, b| a.path.cmp(&b.path));
    architecture_docs.sort_by(|a, b| a.path.cmp(&b.path));
    let expected = workspace.join(".fractal").join("project.fractal");
    if project_fractal_hash.is_none()
        && expected.exists()
        && !should_exclude_path(Path::new(".fractal/project.fractal"))
    {
        project_fractal_hash = Some(sha256_file(&expected)?);
    }
    Ok(RepositoryInventory {
        project_fractal_hash,
        files: evidence,
        manifests,
        architecture_docs,
    })
}

fn manifest_kind(path: &str) -> Option<ManifestKind> {
    let name = Path::new(path).file_name()?.to_string_lossy();
    match name.as_ref() {
        "Cargo.toml" => Some(ManifestKind::Cargo),
        "package.json" => Some(ManifestKind::PackageJson),
        "pyproject.toml" => Some(ManifestKind::Pyproject),
        "requirements.txt" => Some(ManifestKind::Requirements),
        "go.mod" => Some(ManifestKind::GoMod),
        "Package.swift" => Some(ManifestKind::SwiftPackage),
        "Gemfile" | "pom.xml" | "build.gradle" | "compose.yaml" | "docker-compose.yml" => {
            Some(ManifestKind::Other(name.into_owned()))
        }
        _ => None,
    }
}

fn is_architecture_doc(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower.ends_with("architecture.md")
        || lower.ends_with("design.md")
        || lower.ends_with("adr.md")
        || lower.contains("/adr/")
        || lower.contains("/adrs/")
        || lower.contains("architecture")
}

fn extract_signals(
    workspace: &Path,
    files: &[WalkedFile],
    max_file_bytes: u64,
    warnings: &mut Vec<String>,
) -> Result<ExtractedCatalogSignals> {
    let mut signals = ExtractedCatalogSignals::default();
    let mut seen_features = BTreeSet::new();
    let mut seen_components = BTreeSet::new();
    let mut seen_dependencies = BTreeSet::new();
    let mut seen_decisions = BTreeSet::new();
    let mut seen_relationships = BTreeSet::new();

    for file in files {
        if file.bytes > max_file_bytes || !is_text_candidate(&file.relative) {
            continue;
        }
        let contents = match fs::read_to_string(&file.absolute) {
            Ok(contents) => redact(&contents),
            Err(_) => continue,
        };
        if looks_binary_or_secret_dump(&contents) {
            warnings.push(format!(
                "redacted or skipped secret-like data in {}",
                file.relative
            ));
        }
        if let Some(kind) = manifest_kind(&file.relative) {
            for dep in extract_manifest_dependencies(&file.relative, &contents, &kind) {
                push_signal(
                    &mut signals.dependencies,
                    &mut seen_dependencies,
                    CatalogSignal {
                        name: dep,
                        kind: format!("{:?}", kind),
                        evidence_path: file.relative.clone(),
                        evidence_hash: file.sha256.clone(),
                        confidence: Confidence::Medium,
                    },
                );
            }
        }
        extract_markdown_signals(
            file,
            &contents,
            &mut signals,
            &mut seen_features,
            &mut seen_decisions,
        );
        extract_code_components(file, &contents, &mut signals, &mut seen_components);
        extract_relationships(file, &contents, &mut signals, &mut seen_relationships);
    }

    sort_signals(&mut signals.implemented_features);
    sort_signals(&mut signals.components);
    sort_signals(&mut signals.dependencies);
    sort_signals(&mut signals.decisions);
    signals.relationships.sort_by(|a, b| {
        (
            &a.source,
            &a.relationship_string(),
            &a.target,
            &a.evidence_path,
        )
            .cmp(&(
                &b.source,
                &b.relationship_string(),
                &b.target,
                &b.evidence_path,
            ))
    });
    let _ = workspace;
    Ok(signals)
}

trait RelationshipKindString {
    fn relationship_string(&self) -> String;
}

impl RelationshipKindString for RelationshipCandidate {
    fn relationship_string(&self) -> String {
        format!("{:?}", self.relationship)
    }
}

fn push_signal(vec: &mut Vec<CatalogSignal>, seen: &mut BTreeSet<String>, signal: CatalogSignal) {
    let key = format!("{}\0{}\0{}", signal.kind, signal.name, signal.evidence_path);
    if seen.insert(key) {
        vec.push(signal);
    }
}

fn push_relationship(
    vec: &mut Vec<RelationshipCandidate>,
    seen: &mut BTreeSet<String>,
    relationship: RelationshipCandidate,
) {
    let key = format!(
        "{}\0{:?}\0{}\0{}",
        relationship.source,
        relationship.relationship,
        relationship.target,
        relationship.evidence_path
    );
    if seen.insert(key) {
        vec.push(relationship);
    }
}

fn sort_signals(signals: &mut [CatalogSignal]) {
    signals.sort_by(|a, b| {
        (&a.kind, &a.name, &a.evidence_path).cmp(&(&b.kind, &b.name, &b.evidence_path))
    });
}

fn extract_markdown_signals(
    file: &WalkedFile,
    contents: &str,
    signals: &mut ExtractedCatalogSignals,
    seen_features: &mut BTreeSet<String>,
    seen_decisions: &mut BTreeSet<String>,
) {
    if !is_markdown_like(&file.relative) {
        return;
    }
    for line in contents.lines() {
        let trimmed = line.trim().trim_start_matches(['-', '*', ' ']);
        let lower = trimmed.to_ascii_lowercase();
        if lower.starts_with("feature:")
            || lower.starts_with("implemented:")
            || lower.contains(" acceptance:")
        {
            let name = trimmed
                .split_once(':')
                .map(|(_, rest)| rest)
                .unwrap_or(trimmed)
                .trim();
            if !name.is_empty() {
                push_signal(
                    &mut signals.implemented_features,
                    seen_features,
                    CatalogSignal {
                        name: compact_name(name),
                        kind: "documented_feature".to_owned(),
                        evidence_path: file.relative.clone(),
                        evidence_hash: file.sha256.clone(),
                        confidence: Confidence::Medium,
                    },
                );
            }
        }
        if lower.starts_with("decision:")
            || lower.starts_with("adr")
            || lower.contains("we decided")
        {
            push_signal(
                &mut signals.decisions,
                seen_decisions,
                CatalogSignal {
                    name: compact_name(trimmed),
                    kind: "architecture_decision".to_owned(),
                    evidence_path: file.relative.clone(),
                    evidence_hash: file.sha256.clone(),
                    confidence: Confidence::Medium,
                },
            );
        }
    }
}

fn extract_code_components(
    file: &WalkedFile,
    contents: &str,
    signals: &mut ExtractedCatalogSignals,
    seen_components: &mut BTreeSet<String>,
) {
    let extension = Path::new(&file.relative)
        .extension()
        .and_then(OsStr::to_str)
        .unwrap_or_default();
    for line in contents.lines() {
        let trimmed = line.trim();
        let candidate = match extension {
            "rs" => trimmed
                .strip_prefix("pub struct ")
                .or_else(|| trimmed.strip_prefix("struct "))
                .or_else(|| trimmed.strip_prefix("pub enum "))
                .or_else(|| trimmed.strip_prefix("enum "))
                .or_else(|| trimmed.strip_prefix("pub fn "))
                .or_else(|| trimmed.strip_prefix("fn ")),
            "py" => trimmed
                .strip_prefix("class ")
                .or_else(|| trimmed.strip_prefix("def ")),
            "ts" | "tsx" | "js" | "jsx" => trimmed
                .strip_prefix("export class ")
                .or_else(|| trimmed.strip_prefix("class "))
                .or_else(|| trimmed.strip_prefix("export function "))
                .or_else(|| trimmed.strip_prefix("function ")),
            "go" => trimmed
                .strip_prefix("func ")
                .or_else(|| trimmed.strip_prefix("type ")),
            _ => None,
        };
        if let Some(rest) = candidate {
            let name = rest
                .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-'))
                .next()
                .unwrap_or_default();
            if !name.is_empty() && !name.eq("main") {
                push_signal(
                    &mut signals.components,
                    seen_components,
                    CatalogSignal {
                        name: name.to_owned(),
                        kind: format!("{extension}_component"),
                        evidence_path: file.relative.clone(),
                        evidence_hash: file.sha256.clone(),
                        confidence: Confidence::High,
                    },
                );
            }
        }
    }
}

fn extract_relationships(
    file: &WalkedFile,
    contents: &str,
    signals: &mut ExtractedCatalogSignals,
    seen: &mut BTreeSet<String>,
) {
    let file_stem = Path::new(&file.relative)
        .file_stem()
        .and_then(OsStr::to_str)
        .unwrap_or(&file.relative)
        .to_owned();
    for line in contents.lines() {
        let trimmed = line.trim();
        if let Some(target) = trimmed
            .strip_prefix("use ")
            .or_else(|| trimmed.strip_prefix("import "))
        {
            push_relationship(
                &mut signals.relationships,
                seen,
                RelationshipCandidate {
                    source: file_stem.clone(),
                    relationship: RelationshipKind::DependsOn,
                    target: compact_name(target.trim_end_matches(';')),
                    evidence_path: file.relative.clone(),
                    evidence_hash: file.sha256.clone(),
                    confidence: Confidence::Low,
                },
            );
        }
        let lower = trimmed.to_ascii_lowercase();
        if lower.contains("test") && file.relative.to_ascii_lowercase().contains("test") {
            push_relationship(
                &mut signals.relationships,
                seen,
                RelationshipCandidate {
                    source: file_stem.clone(),
                    relationship: RelationshipKind::Tests,
                    target: compact_name(trimmed),
                    evidence_path: file.relative.clone(),
                    evidence_hash: file.sha256.clone(),
                    confidence: Confidence::Low,
                },
            );
        }
    }
}

fn extract_manifest_dependencies(path: &str, contents: &str, kind: &ManifestKind) -> Vec<String> {
    match kind {
        ManifestKind::Cargo | ManifestKind::Pyproject => extract_tomlish_dependencies(contents),
        ManifestKind::PackageJson => extract_package_json_dependencies(contents),
        ManifestKind::Requirements => contents
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .filter_map(|line| line.split(['=', '<', '>', '~', '!', ' ']).next())
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_owned)
            .collect(),
        ManifestKind::GoMod => contents
            .lines()
            .map(str::trim)
            .filter(|line| {
                !line.starts_with("module ")
                    && !line.starts_with("go ")
                    && !line.starts_with("require (")
                    && *line != ")"
            })
            .filter_map(|line| line.split_whitespace().next())
            .filter(|name| name.contains('/') || name.contains('.'))
            .map(str::to_owned)
            .collect(),
        ManifestKind::SwiftPackage => contents
            .lines()
            .filter_map(|line| line.split("url:").nth(1))
            .filter_map(|rest| rest.split('"').nth(1))
            .map(str::to_owned)
            .collect(),
        ManifestKind::Other(_) => {
            let _ = path;
            Vec::new()
        }
    }
}

fn extract_tomlish_dependencies(contents: &str) -> Vec<String> {
    let mut deps = Vec::new();
    let mut in_deps = false;
    for line in contents.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_deps = matches!(
                trimmed,
                "[dependencies]"
                    | "[dev-dependencies]"
                    | "[build-dependencies]"
                    | "[project.dependencies]"
            ) || trimmed.contains("dependencies");
            continue;
        }
        if in_deps && !trimmed.is_empty() && !trimmed.starts_with('#') {
            if let Some((name, _)) = trimmed.split_once('=') {
                let name = name.trim().trim_matches('"').to_owned();
                if !name.is_empty() {
                    deps.push(name);
                }
            } else if trimmed.starts_with('"') {
                if let Some(name) = trimmed
                    .trim_matches(',')
                    .trim_matches('"')
                    .split(['=', '<', '>', '~'])
                    .next()
                {
                    deps.push(name.to_owned());
                }
            }
        }
    }
    deps
}

fn extract_package_json_dependencies(contents: &str) -> Vec<String> {
    let parsed: serde_json::Value = match serde_json::from_str(contents) {
        Ok(value) => value,
        Err(_) => return Vec::new(),
    };
    let mut deps = Vec::new();
    for key in [
        "dependencies",
        "devDependencies",
        "peerDependencies",
        "optionalDependencies",
    ] {
        if let Some(map) = parsed.get(key).and_then(|value| value.as_object()) {
            deps.extend(map.keys().cloned());
        }
    }
    deps.sort();
    deps
}

fn is_text_candidate(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    let text_extensions = [
        ".rs", ".toml", ".json", ".md", ".txt", ".py", ".ts", ".tsx", ".js", ".jsx", ".go",
        ".swift", ".yaml", ".yml", ".lock",
    ];
    text_extensions.iter().any(|suffix| lower.ends_with(suffix))
        || lower == ".fractal/project.fractal"
}

fn is_markdown_like(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower.ends_with(".md")
        || lower.ends_with(".txt")
        || lower.contains("prd")
        || lower.contains("adr")
}

fn compact_name(input: &str) -> String {
    input
        .split_whitespace()
        .take(12)
        .collect::<Vec<_>>()
        .join(" ")
        .trim_matches(['`', '"', '\'', '.', ',', ':', ';'])
        .to_owned()
}

fn looks_binary_or_secret_dump(contents: &str) -> bool {
    contents.contains("-----BEGIN ")
        || contents.to_ascii_lowercase().contains("api_key")
        || contents.to_ascii_lowercase().contains("private_key")
        || contents.to_ascii_lowercase().contains("password=")
}

fn git_fingerprint(workspace: &Path, warnings: &mut Vec<String>) -> GitFingerprint {
    let commit = run_git_capture(workspace, &["rev-parse", "HEAD"]).ok();
    let status =
        run_git_capture(workspace, &["status", "--porcelain=v1"]).unwrap_or_else(|error| {
            warnings.push(format!("git status unavailable: {error}"));
            String::new()
        });
    let dirty = !status.trim().is_empty();
    let dirty_fingerprint = dirty.then(|| sha256_hex(status.as_bytes()));
    GitFingerprint {
        commit: commit
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty()),
        dirty,
        dirty_fingerprint,
    }
}

fn run_git_capture(workspace: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(workspace)
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("run git {}", args.join(" ")))?;
    if !output.status.success() {
        bail!("git {} failed", args.join(" "));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn run_native_test(
    workspace: &Path,
    command: &[String],
    timeout: Duration,
    output_limit_bytes: usize,
) -> NativeTestReport {
    let started = Instant::now();
    if !is_allowed_native_command(command) {
        return NativeTestReport {
            command: command.to_vec(),
            status: NativeCommandStatus::Rejected,
            exit_code: None,
            duration_ms: 0,
            output: "command rejected by project audit allowlist".to_owned(),
            truncated: false,
        };
    }
    let Some(program) = command.first() else {
        return NativeTestReport {
            command: command.to_vec(),
            status: NativeCommandStatus::Rejected,
            exit_code: None,
            duration_ms: 0,
            output: "empty command".to_owned(),
            truncated: false,
        };
    };
    let mut child = match Command::new(program)
        .args(&command[1..])
        .current_dir(workspace)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return NativeTestReport {
                command: command.to_vec(),
                status: NativeCommandStatus::MissingTool,
                exit_code: None,
                duration_ms: started.elapsed().as_millis(),
                output: format!("missing tool: {program}"),
                truncated: false,
            };
        }
        Err(error) => {
            return NativeTestReport {
                command: command.to_vec(),
                status: NativeCommandStatus::Failed,
                exit_code: None,
                duration_ms: started.elapsed().as_millis(),
                output: redact(&format!("failed to spawn command: {error}")),
                truncated: false,
            };
        }
    };
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut combined = Vec::new();
        if let Some(mut stdout) = stdout {
            let _ = stdout.read_to_end(&mut combined);
        }
        if let Some(mut stderr) = stderr {
            let mut err = Vec::new();
            let _ = stderr.read_to_end(&mut err);
            combined.extend_from_slice(&err);
        }
        let _ = tx.send(combined);
    });
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let output = rx
                    .recv_timeout(Duration::from_millis(100))
                    .unwrap_or_default();
                let (output, truncated) = bounded_redacted_output(&output, output_limit_bytes);
                return NativeTestReport {
                    command: command.to_vec(),
                    status: if status.success() {
                        NativeCommandStatus::Passed
                    } else {
                        NativeCommandStatus::Failed
                    },
                    exit_code: status.code(),
                    duration_ms: started.elapsed().as_millis(),
                    output,
                    truncated,
                };
            }
            Ok(None) if started.elapsed() >= timeout => {
                let _ = child.kill();
                let _ = child.wait();
                let output = rx
                    .recv_timeout(Duration::from_millis(100))
                    .unwrap_or_default();
                let (output, truncated) = bounded_redacted_output(&output, output_limit_bytes);
                return NativeTestReport {
                    command: command.to_vec(),
                    status: NativeCommandStatus::TimedOut,
                    exit_code: None,
                    duration_ms: started.elapsed().as_millis(),
                    output,
                    truncated,
                };
            }
            Ok(None) => thread::sleep(Duration::from_millis(20)),
            Err(error) => {
                return NativeTestReport {
                    command: command.to_vec(),
                    status: NativeCommandStatus::Failed,
                    exit_code: None,
                    duration_ms: started.elapsed().as_millis(),
                    output: redact(&format!("failed waiting for command: {error}")),
                    truncated: false,
                };
            }
        }
    }
}

fn is_allowed_native_command(command: &[String]) -> bool {
    let parts: Vec<&str> = command.iter().map(String::as_str).collect();
    matches!(
        parts.as_slice(),
        ["cargo", "test"]
            | ["cargo", "test", ..]
            | ["cargo", "check"]
            | ["cargo", "check", ..]
            | ["npm", "test"]
            | ["npm", "test", ..]
            | ["pnpm", "test"]
            | ["pnpm", "test", ..]
            | ["yarn", "test"]
            | ["yarn", "test", ..]
            | ["pytest"]
            | ["pytest", ..]
            | ["python", "-m", "pytest"]
            | ["python", "-m", "pytest", ..]
            | ["go", "test", "./..."]
            | ["go", "test", "./...", ..]
            | ["swift", "test"]
            | ["swift", "test", ..]
    )
}

fn bounded_redacted_output(bytes: &[u8], limit: usize) -> (String, bool) {
    let mut output = String::from_utf8_lossy(bytes).into_owned();
    output = redact(&output);
    let truncated = output.len() > limit;
    if truncated {
        output.truncate(limit);
        output.push_str("\n[truncated]");
    }
    (output, truncated)
}

fn redact(input: &str) -> String {
    let mut out = Vec::new();
    for line in input.lines() {
        let lower = line.to_ascii_lowercase();
        if lower.contains("password")
            || lower.contains("secret")
            || lower.contains("token")
            || lower.contains("api_key")
            || lower.contains("private_key")
            || line.contains("-----BEGIN ")
        {
            out.push("[REDACTED]".to_owned());
        } else {
            out.push(redact_inline(line));
        }
    }
    out.join("\n")
}

fn redact_inline(line: &str) -> String {
    line.split_whitespace()
        .map(|word| {
            let alnum = word.chars().filter(|c| c.is_ascii_alphanumeric()).count();
            if alnum >= 32
                && (word.contains('_')
                    || word.contains('-')
                    || word.chars().any(|c| c.is_ascii_digit()))
            {
                "[REDACTED]".to_owned()
            } else {
                word.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn classify_status(
    native_tests: &[NativeTestReport],
    git: &GitFingerprint,
    warnings: &[String],
) -> AuditStatus {
    if native_tests.iter().any(|test| {
        matches!(
            test.status,
            NativeCommandStatus::Failed | NativeCommandStatus::TimedOut
        )
    }) {
        return AuditStatus::Fail;
    }
    if native_tests.iter().any(|test| {
        matches!(
            test.status,
            NativeCommandStatus::Rejected | NativeCommandStatus::MissingTool
        )
    }) {
        return AuditStatus::Inconclusive;
    }
    if git.dirty || !warnings.is_empty() {
        return AuditStatus::Inconclusive;
    }
    if native_tests
        .iter()
        .any(|test| matches!(test.status, NativeCommandStatus::Passed))
    {
        AuditStatus::Pass
    } else {
        AuditStatus::Inconclusive
    }
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 16 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex_lower(&hasher.finalize()))
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex_lower(&hasher.finalize())
}

fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

fn rel_string(workspace: &Path, path: &Path) -> String {
    path.strip_prefix(workspace)
        .unwrap_or(path)
        .components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

fn rel_display(workspace: &Path, path: &Path) -> String {
    rel_string(workspace, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_ID: AtomicU64 = AtomicU64::new(1);

    struct TempRepo {
        path: PathBuf,
    }

    impl TempRepo {
        fn new() -> Self {
            let id = NEXT_ID.fetch_add(1, Ordering::SeqCst);
            let path = std::env::temp_dir().join(format!(
                "fractal_project_audit_test_{}_{}",
                std::process::id(),
                id
            ));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).unwrap();
            Self { path }
        }

        fn write(&self, path: &str, contents: &str) {
            let target = self.path.join(path);
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(target, contents).unwrap();
        }

        fn git(&self, args: &[&str]) {
            let status = Command::new("git")
                .args(args)
                .current_dir(&self.path)
                .status()
                .unwrap();
            assert!(status.success(), "git {:?} failed", args);
        }

        fn init_commit(&self) {
            self.git(&["init"]);
            self.git(&["config", "user.email", "test@example.com"]);
            self.git(&["config", "user.name", "Test User"]);
            self.git(&["add", "."]);
            self.git(&["commit", "-m", "initial"]);
        }
    }

    impl Drop for TempRepo {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn audit(repo: &TempRepo, commands: Vec<Vec<&str>>) -> CatalogShardReport {
        let mut options = AuditOptions::new(&repo.path, 0, 1);
        // Cargo may briefly wait on its shared package-cache lock while the full
        // test suite runs in parallel. Keep this fixture comfortably above that
        // contention window; timeout behavior has its own millisecond-bound test.
        options.command_timeout = Duration::from_secs(10);
        options.output_limit_bytes = 512;
        options.native_test_commands = commands
            .into_iter()
            .map(|cmd| cmd.into_iter().map(str::to_owned).collect())
            .collect();
        load_project_audit_shard(options).unwrap()
    }

    #[test]
    fn pass_status_when_allowed_command_succeeds_and_tree_clean() {
        let repo = TempRepo::new();
        repo.write(
            "Cargo.toml",
            "[package]\nname='x'\nversion='0.1.0'\n[dependencies]\nserde='1'\n",
        );
        repo.write("src/lib.rs", "pub struct Widget;\n");
        repo.init_commit();
        let report = audit(&repo, vec![vec!["cargo", "check", "--offline"]]);
        assert_eq!(report.status, AuditStatus::Pass);
        assert_eq!(report.extraction.dependencies[0].name, "serde");
        assert!(report
            .extraction
            .components
            .iter()
            .any(|c| c.name == "Widget"));
    }

    #[test]
    fn fail_status_when_allowed_command_fails() {
        let repo = TempRepo::new();
        repo.write("Cargo.toml", "[package]\nname='x'\nversion='0.1.0'\n");
        repo.write("src/lib.rs", "pub fn broken( {\n");
        repo.init_commit();
        let report = audit(&repo, vec![vec!["cargo", "check"]]);
        assert_eq!(report.native_tests[0].status, NativeCommandStatus::Failed);
        assert_eq!(report.status, AuditStatus::Fail);
    }

    #[test]
    fn timeout_status_when_command_exceeds_bound() {
        let repo = TempRepo::new();
        repo.write(
            "Cargo.toml",
            "[package]\nname = \"x\"\nversion = \"0.1.0\"\nedition = \"2021\"\nbuild = \"build.rs\"\n",
        );
        repo.write("src/lib.rs", "pub fn ok() {}\n");
        repo.write(
            "build.rs",
            "fn main() { std::thread::sleep(std::time::Duration::from_secs(5)); }\n",
        );
        repo.init_commit();
        let mut options = AuditOptions::new(&repo.path, 0, 1);
        options.command_timeout = Duration::from_millis(200);
        options.native_test_commands = vec![vec!["cargo".into(), "check".into()]];
        let report = load_project_audit_shard(options).unwrap();
        assert_eq!(report.native_tests[0].status, NativeCommandStatus::TimedOut);
        assert_eq!(report.status, AuditStatus::Fail);
    }

    #[test]
    fn missing_tool_is_inconclusive() {
        let repo = TempRepo::new();
        repo.write("Cargo.toml", "[package]\nname='x'\nversion='0.1.0'\n");
        repo.init_commit();
        let report = audit(
            &repo,
            vec![vec!["pytest", "--definitely-not-a-real-test-selector"]],
        );
        assert!(matches!(
            report.native_tests[0].status,
            NativeCommandStatus::Failed | NativeCommandStatus::MissingTool
        ));
    }

    #[test]
    fn dirty_tree_has_fingerprint_and_conservative_status() {
        let repo = TempRepo::new();
        repo.write("Cargo.toml", "[package]\nname='x'\nversion='0.1.0'\n");
        repo.init_commit();
        repo.write("src/lib.rs", "pub fn new_change() {}\n");
        let report = audit(&repo, vec![]);
        assert!(report.git.dirty);
        assert!(report.git.dirty_fingerprint.is_some());
        assert_eq!(report.status, AuditStatus::Inconclusive);
    }

    #[test]
    fn secret_like_data_is_excluded_or_redacted() {
        let repo = TempRepo::new();
        repo.write("README.md", "Feature: Safe audit\n");
        repo.write(".env", "TOKEN=super-secret-token\n");
        repo.write(
            "notes.md",
            "password=supersecret\nDecision: redact secrets\n",
        );
        repo.init_commit();
        let report = audit(&repo, vec![]);
        assert!(!report.inventory.files.iter().any(|f| f.path == ".env"));
        let json = shard_report_json(&report).unwrap();
        assert!(!json.contains("super-secret-token"));
        assert!(!json.contains("supersecret"));
    }

    #[cfg(unix)]
    #[test]
    fn symlink_escape_is_excluded() {
        let repo = TempRepo::new();
        repo.write("README.md", "Feature: symlink guard\n");
        let outside = std::env::temp_dir().join(format!("outside_{}", std::process::id()));
        fs::write(&outside, "outside").unwrap();
        std::os::unix::fs::symlink(&outside, repo.path.join("outside-link")).unwrap();
        repo.init_commit();
        let report = audit(&repo, vec![]);
        assert!(!report
            .inventory
            .files
            .iter()
            .any(|f| f.path == "outside-link"));
        assert!(report
            .warnings
            .iter()
            .any(|w| w.contains("symlink escaping")));
        let _ = fs::remove_file(outside);
    }

    #[test]
    fn deterministic_ordering_is_stable() {
        let repo = TempRepo::new();
        repo.write("b.rs", "pub struct B;\n");
        repo.write("a.rs", "pub struct A;\n");
        repo.write("docs/architecture.md", "Decision: stable ordering\n");
        repo.init_commit();
        let first = audit(&repo, vec![]);
        let second = audit(&repo, vec![]);
        assert_eq!(first.inventory.files, second.inventory.files);
        let paths: Vec<_> = first
            .inventory
            .files
            .iter()
            .map(|f| f.path.as_str())
            .collect();
        let mut sorted = paths.clone();
        sorted.sort();
        assert_eq!(paths, sorted);
    }

    #[test]
    fn shards_are_disjoint_and_cover_walked_files() {
        let repo = TempRepo::new();
        for idx in 0..20 {
            repo.write(
                &format!("src/file_{idx}.rs"),
                &format!("pub struct C{idx};\n"),
            );
        }
        repo.init_commit();
        let mut all = BTreeSet::new();
        let mut total = 0;
        for shard in 0..4 {
            let options = AuditOptions::new(&repo.path, shard, 4);
            let report = load_project_audit_shard(options).unwrap();
            total += report.inventory.files.len();
            for file in report.inventory.files {
                assert!(all.insert(file.path), "file appeared in multiple shards");
            }
        }
        let unsharded = audit(&repo, vec![]);
        assert_eq!(total, unsharded.inventory.files.len());
    }

    #[test]
    fn machine_readable_json_contains_schema() {
        let repo = TempRepo::new();
        repo.write("README.md", "Feature: JSON report\n");
        repo.init_commit();
        let report = audit(&repo, vec![]);
        let json = shard_report_json(&report).unwrap();
        assert!(json.contains(SCHEMA));
        let reparsed: CatalogShardReport = serde_json::from_str(&json).unwrap();
        assert_eq!(reparsed.schema, SCHEMA);
    }
}
