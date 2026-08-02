//! Typed `fractal.catalog.v1` envelope: models, normalization, hashing, and validation.
//!
//! Persistence goes through [`crate::project_file::replace_catalog`] only — this module
//! never writes project documents itself and never mutates graph/execution/learning.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

pub(crate) const CATALOG_SCHEMA: &str = "fractal.catalog.v1";
pub(crate) const DEFAULT_MAX_CATALOG_BYTES: usize = 262_144;
pub(crate) const MAX_EVIDENCE_PER_CLAIM: usize = 20;
pub(crate) const MAX_CAPABILITIES: usize = 200;
pub(crate) const MAX_COMPONENTS: usize = 200;
pub(crate) const MAX_DEPENDENCIES: usize = 500;
pub(crate) const MAX_TESTS: usize = 100;
pub(crate) const MAX_DECISIONS: usize = 100;
pub(crate) const MAX_LINKS: usize = 200;
pub(crate) const MAX_DIAGNOSTICS: usize = 200;
pub(crate) const MAX_LOG_EXCERPT_CHARS: usize = 1024;
pub(crate) const MAX_NOTE_CHARS: usize = 512;
pub(crate) const MAX_SPANS_PER_EVIDENCE: usize = 20;

const LOCAL_KEY_RE: &str = r"^[a-z0-9][a-z0-9-]{0,63}$";
const PROJECT_KEY_RE: &str = r"^[a-z0-9][a-z0-9-]{0,47}-[0-9a-f]{12}$";
const HASH256_RE: &str = r"^sha256:[0-9a-f]{64}$";
const GIT_COMMIT_RE: &str = r"^[0-9a-f]{40}$";
const BARE_HEX64_RE: &str = r"^[0-9a-f]{64}$";
const RFC3339_UTC_RE: &str = r"^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}(\.[0-9]+)?Z$";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct CatalogV1 {
    pub(crate) schema: String,
    pub(crate) project_key: String,
    pub(crate) generated_at: String,
    pub(crate) catalog_hash: String,
    pub(crate) source: CatalogSource,
    pub(crate) audit: CatalogAudit,
    pub(crate) capabilities: Vec<CatalogCapability>,
    pub(crate) components: Vec<CatalogComponent>,
    pub(crate) dependencies: Vec<CatalogDependency>,
    pub(crate) tests: Vec<CatalogTest>,
    pub(crate) decisions: Vec<CatalogDecision>,
    pub(crate) cross_graph_links: Vec<CatalogCrossGraphLink>,
    pub(crate) diagnostics: Vec<CatalogDiagnostic>,
    #[serde(default, flatten)]
    pub(crate) extra: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct CatalogSource {
    pub(crate) canonical_workspace: String,
    pub(crate) workspace_fingerprint: String,
    pub(crate) registry_numbers: Vec<u64>,
    pub(crate) labels: Vec<String>,
    pub(crate) git: CatalogGit,
    #[serde(default, flatten)]
    pub(crate) extra: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct CatalogGit {
    pub(crate) is_git_repository: bool,
    pub(crate) commit: Option<String>,
    pub(crate) dirty: Option<bool>,
    pub(crate) dirty_fingerprint: Option<String>,
    pub(crate) unavailable_reason: Option<String>,
    #[serde(default)]
    pub(crate) remotes: Vec<CatalogRemote>,
    #[serde(default, flatten)]
    pub(crate) extra: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct CatalogRemote {
    pub(crate) name: String,
    pub(crate) fingerprint_sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) sanitized_url: Option<String>,
    #[serde(default, flatten)]
    pub(crate) extra: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct CatalogAudit {
    pub(crate) auditor: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) cli_version: Option<String>,
    pub(crate) inventory_hash: String,
    pub(crate) started_at: String,
    pub(crate) finished_at: String,
    pub(crate) bounds: CatalogBounds,
    pub(crate) truncated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) evidence_counts: Option<BTreeMap<String, u64>>,
    #[serde(default, flatten)]
    pub(crate) extra: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct CatalogBounds {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) max_catalog_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) max_evidence_per_claim: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) max_log_excerpt_chars: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) max_string_chars: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) test_timeout_ms: Option<u64>,
    #[serde(default, flatten)]
    pub(crate) extra: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct CatalogCapability {
    pub(crate) key: String,
    pub(crate) title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) description: Option<String>,
    pub(crate) status: CatalogStatus,
    pub(crate) evidence: Vec<CatalogEvidence>,
    pub(crate) test_keys: Vec<String>,
    pub(crate) component_keys: Vec<String>,
    #[serde(default, flatten)]
    pub(crate) extra: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct CatalogComponent {
    pub(crate) key: String,
    pub(crate) name: String,
    pub(crate) kind: CatalogComponentKind,
    pub(crate) paths: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) description: Option<String>,
    pub(crate) status: CatalogStatus,
    pub(crate) evidence: Vec<CatalogEvidence>,
    #[serde(default, flatten)]
    pub(crate) extra: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct CatalogDependency {
    pub(crate) from_component: String,
    pub(crate) to_component: String,
    pub(crate) kind: CatalogDependencyKind,
    pub(crate) evidence: Vec<CatalogEvidence>,
    #[serde(default, flatten)]
    pub(crate) extra: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct CatalogTest {
    pub(crate) key: String,
    pub(crate) command: String,
    pub(crate) classification: CatalogTestClassification,
    pub(crate) exit_code: Option<i64>,
    pub(crate) duration_ms: Option<u64>,
    pub(crate) log_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) log_excerpt: Option<String>,
    pub(crate) evidence: Vec<CatalogEvidence>,
    #[serde(default, flatten)]
    pub(crate) extra: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct CatalogDecision {
    pub(crate) key: String,
    pub(crate) title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) summary: Option<String>,
    pub(crate) status: CatalogDecisionStatus,
    pub(crate) evidence: Vec<CatalogEvidence>,
    #[serde(default, flatten)]
    pub(crate) extra: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct CatalogCrossGraphLink {
    pub(crate) key: String,
    #[serde(rename = "type")]
    pub(crate) link_type: CatalogLinkType,
    pub(crate) from: CatalogLinkFrom,
    pub(crate) to: CatalogLinkTo,
    pub(crate) confidence: CatalogConfidence,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) rationale: Option<String>,
    pub(crate) evidence: Vec<CatalogEvidence>,
    #[serde(default, flatten)]
    pub(crate) extra: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct CatalogLinkFrom {
    pub(crate) component_key: Option<String>,
    #[serde(default, flatten)]
    pub(crate) extra: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct CatalogLinkTo {
    pub(crate) project_key: Option<String>,
    pub(crate) alias: Option<String>,
    pub(crate) component_key: Option<String>,
    #[serde(default, flatten)]
    pub(crate) extra: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct CatalogDiagnostic {
    pub(crate) code: CatalogDiagnosticCode,
    pub(crate) severity: CatalogDiagnosticSeverity,
    pub(crate) message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) context: Option<String>,
    #[serde(default, flatten)]
    pub(crate) extra: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct CatalogEvidence {
    pub(crate) path: String,
    pub(crate) sha256: String,
    pub(crate) kind: CatalogEvidenceKind,
    pub(crate) observed_commit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) spans: Option<Vec<[u64; 2]>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) note: Option<String>,
    #[serde(default, flatten)]
    pub(crate) extra: BTreeMap<String, Value>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CatalogStatus {
    Verified,
    ImplementedUnverified,
    Partial,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CatalogComponentKind {
    Binary,
    Library,
    Module,
    Service,
    App,
    Ui,
    Schema,
    Docs,
    Config,
    TestSuite,
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CatalogDependencyKind {
    Build,
    Runtime,
    Dev,
    Test,
    Data,
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CatalogTestClassification {
    Pass,
    Fail,
    Timeout,
    MissingTool,
    Skipped,
    NotRun,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CatalogDecisionStatus {
    Adopted,
    Proposed,
    Superseded,
    Rejected,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CatalogLinkType {
    DependsOn,
    UsesComponent,
    DerivedFrom,
    ForkedFrom,
    Supersedes,
    SharesComponent,
    RelatedTo,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CatalogConfidence {
    High,
    Medium,
    Low,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CatalogDiagnosticCode {
    CatalogBoundExceeded,
    ManifestUnreadable,
    RedactedContent,
    SymlinkEscapeSkipped,
    TestUnavailable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CatalogDiagnosticSeverity {
    Error,
    Warning,
    Info,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CatalogEvidenceKind {
    Source,
    Manifest,
    TestLog,
    Graph,
    Document,
}

/// Derive `sha256:` + hex of the UTF-8 workspace path bytes.
pub(crate) fn workspace_fingerprint(canonical_workspace: &str) -> String {
    sha256_prefixed(canonical_workspace.as_bytes())
}

/// Derive stable `project_key` from a canonical absolute workspace path.
pub(crate) fn project_key(canonical_workspace: &str) -> String {
    let segment = Path::new(canonical_workspace)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "project".to_owned());
    let slug = slugify(&segment, 48);
    let fingerprint = workspace_fingerprint(canonical_workspace);
    let digest = fingerprint.strip_prefix("sha256:").unwrap_or(&fingerprint);
    let suffix: String = digest.chars().take(12).collect();
    format!("{slug}-{suffix}")
}

/// Sanitize a structural identifier into a local key (64-char cap).
pub(crate) fn component_key_from(identifier: &str) -> String {
    slugify(identifier, 64)
}

/// Sort arrays and recompute `catalog_hash` for a deterministic envelope.
pub(crate) fn normalize(catalog: &mut CatalogV1) -> Result<(), String> {
    catalog.schema = CATALOG_SCHEMA.to_owned();
    catalog
        .capabilities
        .sort_by(|left, right| left.key.cmp(&right.key));
    catalog
        .components
        .sort_by(|left, right| left.key.cmp(&right.key));
    catalog.dependencies.sort_by(|left, right| {
        (
            left.from_component.as_str(),
            left.to_component.as_str(),
            left.kind,
        )
            .cmp(&(
                right.from_component.as_str(),
                right.to_component.as_str(),
                right.kind,
            ))
    });
    catalog
        .tests
        .sort_by(|left, right| left.key.cmp(&right.key));
    catalog
        .decisions
        .sort_by(|left, right| left.key.cmp(&right.key));
    catalog
        .cross_graph_links
        .sort_by(|left, right| left.key.cmp(&right.key));
    catalog.diagnostics.sort_by(|left, right| {
        (
            left.code,
            left.context.as_deref().unwrap_or(""),
            left.message.as_str(),
        )
            .cmp(&(
                right.code,
                right.context.as_deref().unwrap_or(""),
                right.message.as_str(),
            ))
    });

    for capability in &mut catalog.capabilities {
        normalize_evidence(&mut capability.evidence);
    }
    for component in &mut catalog.components {
        component.paths.sort();
        normalize_evidence(&mut component.evidence);
    }
    for dependency in &mut catalog.dependencies {
        normalize_evidence(&mut dependency.evidence);
    }
    for test in &mut catalog.tests {
        normalize_evidence(&mut test.evidence);
    }
    for decision in &mut catalog.decisions {
        normalize_evidence(&mut decision.evidence);
    }
    for link in &mut catalog.cross_graph_links {
        normalize_evidence(&mut link.evidence);
    }

    catalog.catalog_hash = compute_catalog_hash(catalog)?;
    Ok(())
}

/// Canonical hash of the envelope with `catalog_hash` removed.
pub(crate) fn compute_catalog_hash(catalog: &CatalogV1) -> Result<String, String> {
    let mut value = serde_json::to_value(catalog).map_err(|error| error.to_string())?;
    if let Some(object) = value.as_object_mut() {
        object.remove("catalog_hash");
    }
    fractal_contracts::canonical_sha256(&value).map_err(|error| error.to_string())
}

/// Dirty fingerprint over deduplicated sorted evidence `{path, sha256}` pairs.
pub(crate) fn compute_dirty_fingerprint(catalog: &CatalogV1) -> Result<String, String> {
    let mut pairs: BTreeSet<(String, String)> = BTreeSet::new();
    for evidence in collect_all_evidence(catalog) {
        pairs.insert((evidence.path.clone(), evidence.sha256.clone()));
    }
    let array: Vec<Value> = pairs
        .into_iter()
        .map(|(path, sha256)| {
            serde_json::json!({
                "path": path,
                "sha256": sha256,
            })
        })
        .collect();
    fractal_contracts::canonical_sha256(&Value::Array(array)).map_err(|error| error.to_string())
}

/// Validate a typed catalog envelope (schema, bounds, status, hash, secrets).
pub(crate) fn validate(catalog: &CatalogV1) -> Result<(), String> {
    if catalog.schema != CATALOG_SCHEMA {
        return Err(format!(
            "unsupported catalog schema `{}`; expected `{CATALOG_SCHEMA}`",
            catalog.schema
        ));
    }
    if !matches_pattern(&catalog.project_key, PROJECT_KEY_RE) {
        return Err(format!("invalid project_key `{}`", catalog.project_key));
    }
    if !matches_pattern(&catalog.generated_at, RFC3339_UTC_RE) {
        return Err(format!("invalid generated_at `{}`", catalog.generated_at));
    }
    if !matches_pattern(&catalog.catalog_hash, HASH256_RE) {
        return Err(format!("invalid catalog_hash `{}`", catalog.catalog_hash));
    }

    validate_source(catalog)?;
    validate_audit(&catalog.audit)?;

    if catalog.capabilities.len() > MAX_CAPABILITIES {
        return Err(format!(
            "capabilities exceed bound ({})",
            catalog.capabilities.len()
        ));
    }
    if catalog.components.len() > MAX_COMPONENTS {
        return Err(format!(
            "components exceed bound ({})",
            catalog.components.len()
        ));
    }
    if catalog.dependencies.len() > MAX_DEPENDENCIES {
        return Err(format!(
            "dependencies exceed bound ({})",
            catalog.dependencies.len()
        ));
    }
    if catalog.tests.len() > MAX_TESTS {
        return Err(format!("tests exceed bound ({})", catalog.tests.len()));
    }
    if catalog.decisions.len() > MAX_DECISIONS {
        return Err(format!(
            "decisions exceed bound ({})",
            catalog.decisions.len()
        ));
    }
    if catalog.cross_graph_links.len() > MAX_LINKS {
        return Err(format!(
            "cross_graph_links exceed bound ({})",
            catalog.cross_graph_links.len()
        ));
    }
    if catalog.diagnostics.len() > MAX_DIAGNOSTICS {
        return Err(format!(
            "diagnostics exceed bound ({})",
            catalog.diagnostics.len()
        ));
    }

    ensure_sorted_by_key(
        catalog.capabilities.iter().map(|item| item.key.as_str()),
        "capabilities",
    )?;
    ensure_sorted_by_key(
        catalog.components.iter().map(|item| item.key.as_str()),
        "components",
    )?;
    ensure_sorted_by_key(catalog.tests.iter().map(|item| item.key.as_str()), "tests")?;
    ensure_sorted_by_key(
        catalog.decisions.iter().map(|item| item.key.as_str()),
        "decisions",
    )?;
    ensure_sorted_by_key(
        catalog
            .cross_graph_links
            .iter()
            .map(|item| item.key.as_str()),
        "cross_graph_links",
    )?;
    ensure_dependencies_sorted(&catalog.dependencies)?;
    ensure_diagnostics_sorted(&catalog.diagnostics)?;

    let component_keys = unique_keys(
        catalog.components.iter().map(|item| item.key.as_str()),
        "component",
    )?;
    let capability_keys = unique_keys(
        catalog.capabilities.iter().map(|item| item.key.as_str()),
        "capability",
    )?;
    let _ = capability_keys;
    let test_keys = unique_keys(catalog.tests.iter().map(|item| item.key.as_str()), "test")?;
    let _ = unique_keys(
        catalog.decisions.iter().map(|item| item.key.as_str()),
        "decision",
    )?;
    let _ = unique_keys(
        catalog
            .cross_graph_links
            .iter()
            .map(|item| item.key.as_str()),
        "cross_graph_link",
    )?;

    let tests_by_key: BTreeMap<&str, &CatalogTest> = catalog
        .tests
        .iter()
        .map(|test| (test.key.as_str(), test))
        .collect();

    for component in &catalog.components {
        validate_local_key(&component.key, "component")?;
        if component.name.trim().is_empty() || component.name.chars().count() > 256 {
            return Err(format!("invalid component name for `{}`", component.key));
        }
        if component.paths.is_empty() || component.paths.len() > 32 {
            return Err(format!("invalid component paths for `{}`", component.key));
        }
        if !component
            .paths
            .windows(2)
            .all(|window| window[0] <= window[1])
        {
            return Err(format!(
                "component `{}` paths must be sorted ascending",
                component.key
            ));
        }
        for path in &component.paths {
            validate_rel_path(path)?;
        }
        validate_evidence_list(&component.evidence, "component")?;
        validate_component_status(component, &catalog.capabilities, &tests_by_key)?;
    }

    for capability in &catalog.capabilities {
        validate_local_key(&capability.key, "capability")?;
        if capability.title.trim().is_empty() || capability.title.chars().count() > 256 {
            return Err(format!("invalid capability title for `{}`", capability.key));
        }
        if let Some(description) = &capability.description {
            if description.chars().count() > 2048 {
                return Err(format!(
                    "capability `{}` description exceeds maxLength",
                    capability.key
                ));
            }
        }
        validate_evidence_list(&capability.evidence, "capability")?;
        for component_key in &capability.component_keys {
            if !component_keys.contains(component_key.as_str()) {
                return Err(format!(
                    "capability `{}` references missing component `{component_key}`",
                    capability.key
                ));
            }
        }
        for test_key in &capability.test_keys {
            if !test_keys.contains(test_key.as_str()) {
                return Err(format!(
                    "capability `{}` references missing test `{test_key}`",
                    capability.key
                ));
            }
        }
        validate_capability_status(capability, &tests_by_key)?;
    }

    let mut dependency_triples = BTreeSet::new();
    for dependency in &catalog.dependencies {
        if !component_keys.contains(dependency.from_component.as_str())
            || !component_keys.contains(dependency.to_component.as_str())
        {
            return Err(format!(
                "dependency references missing component `{}` -> `{}`",
                dependency.from_component, dependency.to_component
            ));
        }
        if !dependency_triples.insert((
            dependency.from_component.clone(),
            dependency.to_component.clone(),
            dependency.kind,
        )) {
            return Err("duplicate dependency triple".to_owned());
        }
        validate_evidence_list(&dependency.evidence, "dependency")?;
    }

    for test in &catalog.tests {
        validate_local_key(&test.key, "test")?;
        if test.command.trim().is_empty() || test.command.chars().count() > 512 {
            return Err(format!("invalid test command for `{}`", test.key));
        }
        if let Some(log_sha256) = &test.log_sha256 {
            if !matches_pattern(log_sha256, HASH256_RE) {
                return Err(format!("invalid test log_sha256 for `{}`", test.key));
            }
        }
        if let Some(excerpt) = &test.log_excerpt {
            if excerpt.chars().count() > MAX_LOG_EXCERPT_CHARS {
                return Err(format!("test `{}` log_excerpt exceeds bound", test.key));
            }
        }
        validate_evidence_list(&test.evidence, "test")?;
    }

    for decision in &catalog.decisions {
        validate_local_key(&decision.key, "decision")?;
        if decision.title.trim().is_empty() || decision.title.chars().count() > 256 {
            return Err(format!("invalid decision title for `{}`", decision.key));
        }
        validate_evidence_list(&decision.evidence, "decision")?;
    }

    for link in &catalog.cross_graph_links {
        validate_local_key(&link.key, "cross_graph_link")?;
        if let Some(component_key) = &link.from.component_key {
            if !component_keys.contains(component_key.as_str()) {
                return Err(format!(
                    "link `{}` from.component_key `{component_key}` is missing",
                    link.key
                ));
            }
        }
        let has_project = link
            .to
            .project_key
            .as_ref()
            .is_some_and(|value| !value.is_empty());
        let has_alias = link
            .to
            .alias
            .as_ref()
            .is_some_and(|value| !value.is_empty());
        if !has_project && !has_alias {
            return Err(format!(
                "link `{}` requires to.project_key or to.alias",
                link.key
            ));
        }
        if let Some(project_key) = &link.to.project_key {
            if !matches_pattern(project_key, PROJECT_KEY_RE) {
                return Err(format!(
                    "link `{}` has invalid to.project_key `{project_key}`",
                    link.key
                ));
            }
        }
        if let Some(component_key) = &link.to.component_key {
            validate_local_key(component_key, "link to.component_key")?;
        }
        validate_evidence_list(&link.evidence, "cross_graph_link")?;
    }

    for diagnostic in &catalog.diagnostics {
        if diagnostic.message.trim().is_empty() || diagnostic.message.chars().count() > 1024 {
            return Err("invalid diagnostic message".to_owned());
        }
        if let Some(context) = &diagnostic.context {
            if context.chars().count() > 1024 {
                return Err("diagnostic context exceeds maxLength".to_owned());
            }
        }
    }

    if catalog.audit.truncated
        && !catalog
            .diagnostics
            .iter()
            .any(|item| item.code == CatalogDiagnosticCode::CatalogBoundExceeded)
    {
        return Err(
            "audit.truncated is true without a catalog_bound_exceeded diagnostic".to_owned(),
        );
    }

    validate_dirty_fingerprint(catalog)?;

    let expected_hash = compute_catalog_hash(catalog)?;
    if catalog.catalog_hash != expected_hash {
        return Err(format!(
            "catalog_hash mismatch: claimed {}, computed {expected_hash}",
            catalog.catalog_hash
        ));
    }

    let encoded = serde_json::to_value(catalog).map_err(|error| error.to_string())?;
    reject_secret_fields(&encoded)?;

    let max_bytes = catalog
        .audit
        .bounds
        .max_catalog_bytes
        .unwrap_or(DEFAULT_MAX_CATALOG_BYTES as u64) as usize;
    let serialized = serde_json::to_vec(&encoded).map_err(|error| error.to_string())?;
    if serialized.len() > max_bytes {
        return Err(format!(
            "catalog exceeds max_catalog_bytes ({}/{})",
            serialized.len(),
            max_bytes
        ));
    }

    let derived_key = project_key(&catalog.source.canonical_workspace);
    if catalog.project_key != derived_key {
        return Err(format!(
            "catalog project_key `{}` does not match derived `{derived_key}`",
            catalog.project_key
        ));
    }
    let derived_fingerprint = workspace_fingerprint(&catalog.source.canonical_workspace);
    if catalog.source.workspace_fingerprint != derived_fingerprint {
        return Err("catalog source.workspace_fingerprint mismatch".to_owned());
    }

    Ok(())
}

/// Validate a raw JSON catalog value; unsupported schemas are rejected for writes.
pub(crate) fn validate_value(value: &Value) -> Result<CatalogV1, String> {
    let schema = value
        .get("schema")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if schema != CATALOG_SCHEMA {
        return Err(format!(
            "unsupported catalog schema `{schema}`; expected `{CATALOG_SCHEMA}`"
        ));
    }
    let catalog: CatalogV1 =
        serde_json::from_value(value.clone()).map_err(|error| error.to_string())?;
    validate(&catalog)?;
    Ok(catalog)
}

fn validate_source(catalog: &CatalogV1) -> Result<(), String> {
    let source = &catalog.source;
    if !source.canonical_workspace.starts_with('/')
        || source.canonical_workspace.is_empty()
        || source.canonical_workspace.chars().count() > 1024
        || source.canonical_workspace.contains('~')
    {
        return Err("canonical_workspace must be an absolute inventory path".to_owned());
    }
    if !matches_pattern(&source.workspace_fingerprint, HASH256_RE) {
        return Err("invalid workspace_fingerprint".to_owned());
    }
    if source.registry_numbers.len() > 32 {
        return Err("registry_numbers exceed bound".to_owned());
    }
    if source.labels.len() > 32 {
        return Err("labels exceed bound".to_owned());
    }
    for label in &source.labels {
        if label.trim().is_empty() || label.chars().count() > 256 {
            return Err("invalid source label".to_owned());
        }
    }
    let git = &source.git;
    if let Some(commit) = &git.commit {
        if !matches_pattern(commit, GIT_COMMIT_RE) {
            return Err(format!("invalid git commit `{commit}`"));
        }
    }
    if let Some(reason) = &git.unavailable_reason {
        if reason.chars().count() > 512 {
            return Err("unavailable_reason exceeds maxLength".to_owned());
        }
    }
    if git.remotes.len() > 8 {
        return Err("git remotes exceed bound".to_owned());
    }
    for remote in &git.remotes {
        if remote.name.trim().is_empty() || remote.name.chars().count() > 128 {
            return Err("invalid remote name".to_owned());
        }
        if !matches_pattern(&remote.fingerprint_sha256, BARE_HEX64_RE) {
            return Err("invalid remote fingerprint_sha256".to_owned());
        }
        if let Some(url) = &remote.sanitized_url {
            if url.chars().count() > 512 {
                return Err("sanitized_url exceeds maxLength".to_owned());
            }
            if looks_credentialed_url(url) {
                return Err("sanitized_url must not contain credentials".to_owned());
            }
        }
    }
    if !git.is_git_repository && (git.commit.is_some() || git.dirty.is_some()) {
        return Err("non-git source must leave commit/dirty null".to_owned());
    }
    Ok(())
}

fn validate_audit(audit: &CatalogAudit) -> Result<(), String> {
    if audit.auditor.trim().is_empty() || audit.auditor.chars().count() > 256 {
        return Err("invalid audit.auditor".to_owned());
    }
    if !matches_pattern(&audit.inventory_hash, HASH256_RE) {
        return Err("invalid audit.inventory_hash".to_owned());
    }
    if !matches_pattern(&audit.started_at, RFC3339_UTC_RE)
        || !matches_pattern(&audit.finished_at, RFC3339_UTC_RE)
    {
        return Err("invalid audit timestamps".to_owned());
    }
    if let Some(version) = &audit.cli_version {
        if version.chars().count() > 64 {
            return Err("cli_version exceeds maxLength".to_owned());
        }
    }
    Ok(())
}

fn validate_capability_status(
    capability: &CatalogCapability,
    tests_by_key: &BTreeMap<&str, &CatalogTest>,
) -> Result<(), String> {
    match capability.status {
        CatalogStatus::Verified => {
            if capability.evidence.is_empty() {
                return Err(format!(
                    "capability `{}` status verified requires evidence",
                    capability.key
                ));
            }
            let has_pass = capability.test_keys.iter().any(|key| {
                tests_by_key
                    .get(key.as_str())
                    .is_some_and(|test| test.classification == CatalogTestClassification::Pass)
            });
            if !has_pass {
                return Err(format!(
                    "capability `{}` status verified requires a passing test",
                    capability.key
                ));
            }
        }
        CatalogStatus::ImplementedUnverified => {
            if capability.evidence.is_empty() {
                return Err(format!(
                    "capability `{}` status implemented_unverified requires evidence",
                    capability.key
                ));
            }
        }
        CatalogStatus::Partial | CatalogStatus::Unknown => {}
    }
    Ok(())
}

fn validate_component_status(
    component: &CatalogComponent,
    capabilities: &[CatalogCapability],
    tests_by_key: &BTreeMap<&str, &CatalogTest>,
) -> Result<(), String> {
    match component.status {
        CatalogStatus::Verified => {
            if component.evidence.is_empty() {
                return Err(format!(
                    "component `{}` status verified requires evidence",
                    component.key
                ));
            }
            let exercised = capabilities.iter().any(|capability| {
                capability
                    .component_keys
                    .iter()
                    .any(|key| key == &component.key)
                    && capability.test_keys.iter().any(|test_key| {
                        tests_by_key.get(test_key.as_str()).is_some_and(|test| {
                            test.classification == CatalogTestClassification::Pass
                        })
                    })
            });
            if !exercised {
                return Err(format!(
                    "component `{}` status verified requires a linked passing test",
                    component.key
                ));
            }
        }
        CatalogStatus::ImplementedUnverified => {
            if component.evidence.is_empty() {
                return Err(format!(
                    "component `{}` status implemented_unverified requires evidence",
                    component.key
                ));
            }
        }
        CatalogStatus::Partial | CatalogStatus::Unknown => {}
    }
    Ok(())
}

fn validate_dirty_fingerprint(catalog: &CatalogV1) -> Result<(), String> {
    let git = &catalog.source.git;
    let evidence_exists = !collect_all_evidence(catalog).is_empty();
    let dirty = git.dirty == Some(true);
    let commit_missing = git.commit.is_none();

    if dirty || (commit_missing && evidence_exists) {
        let Some(fingerprint) = &git.dirty_fingerprint else {
            return Err(
                "dirty_fingerprint must be set when dirty or when commit is null with evidence"
                    .to_owned(),
            );
        };
        if !matches_pattern(fingerprint, HASH256_RE) {
            return Err("invalid dirty_fingerprint".to_owned());
        }
        let expected = compute_dirty_fingerprint(catalog)?;
        if fingerprint != &expected {
            return Err(format!(
                "dirty_fingerprint mismatch: claimed {fingerprint}, computed {expected}"
            ));
        }
    } else if git.dirty == Some(false) && git.commit.is_some() && git.dirty_fingerprint.is_some() {
        return Err(
            "dirty_fingerprint must be null when dirty is false and commit is present".to_owned(),
        );
    }
    Ok(())
}

fn validate_evidence_list(evidence: &[CatalogEvidence], context: &str) -> Result<(), String> {
    if evidence.len() > MAX_EVIDENCE_PER_CLAIM {
        return Err(format!("{context} evidence exceeds bound"));
    }
    if !evidence
        .windows(2)
        .all(|window| (&window[0].path, &window[0].sha256) <= (&window[1].path, &window[1].sha256))
    {
        return Err(format!(
            "{context} evidence must be sorted by (path, sha256)"
        ));
    }
    for item in evidence {
        validate_rel_path(&item.path)?;
        if !matches_pattern(&item.sha256, HASH256_RE) {
            return Err(format!("{context} evidence has invalid sha256"));
        }
        if let Some(commit) = &item.observed_commit {
            if !matches_pattern(commit, GIT_COMMIT_RE) {
                return Err(format!("{context} evidence has invalid observed_commit"));
            }
        }
        if let Some(spans) = &item.spans {
            if spans.len() > MAX_SPANS_PER_EVIDENCE {
                return Err(format!("{context} evidence spans exceed bound"));
            }
            for span in spans {
                if span[0] == 0 || span[1] == 0 || span[0] > span[1] {
                    return Err(format!("{context} evidence span is invalid"));
                }
            }
        }
        if let Some(note) = &item.note {
            if note.chars().count() > MAX_NOTE_CHARS {
                return Err(format!("{context} evidence note exceeds bound"));
            }
        }
    }
    Ok(())
}

fn validate_rel_path(path: &str) -> Result<(), String> {
    if path.is_empty() || path.chars().count() > 512 {
        return Err(format!("invalid evidence path `{path}`"));
    }
    let first = path.chars().next().unwrap_or('/');
    if matches!(first, '/' | '\\' | '~') {
        return Err(format!(
            "evidence path must be repository-relative: `{path}`"
        ));
    }
    if path.split('/').any(|segment| segment == "..") {
        return Err(format!("evidence path must not contain '..': `{path}`"));
    }
    Ok(())
}

fn validate_local_key(key: &str, kind: &str) -> Result<(), String> {
    if !matches_pattern(key, LOCAL_KEY_RE) {
        return Err(format!("invalid {kind} key `{key}`"));
    }
    Ok(())
}

fn normalize_evidence(evidence: &mut [CatalogEvidence]) {
    evidence.sort_by(|left, right| (&left.path, &left.sha256).cmp(&(&right.path, &right.sha256)));
}

fn collect_all_evidence(catalog: &CatalogV1) -> Vec<&CatalogEvidence> {
    let mut out = Vec::new();
    for capability in &catalog.capabilities {
        out.extend(capability.evidence.iter());
    }
    for component in &catalog.components {
        out.extend(component.evidence.iter());
    }
    for dependency in &catalog.dependencies {
        out.extend(dependency.evidence.iter());
    }
    for test in &catalog.tests {
        out.extend(test.evidence.iter());
    }
    for decision in &catalog.decisions {
        out.extend(decision.evidence.iter());
    }
    for link in &catalog.cross_graph_links {
        out.extend(link.evidence.iter());
    }
    out
}

fn ensure_sorted_by_key<'a>(
    keys: impl IntoIterator<Item = &'a str>,
    label: &str,
) -> Result<(), String> {
    let mut previous: Option<&str> = None;
    for key in keys {
        if let Some(prev) = previous {
            if prev > key {
                return Err(format!("{label} must be sorted by key"));
            }
        }
        previous = Some(key);
    }
    Ok(())
}

fn ensure_dependencies_sorted(dependencies: &[CatalogDependency]) -> Result<(), String> {
    let mut previous: Option<(&str, &str, CatalogDependencyKind)> = None;
    for dependency in dependencies {
        let current = (
            dependency.from_component.as_str(),
            dependency.to_component.as_str(),
            dependency.kind,
        );
        if let Some(prev) = previous {
            if prev > current {
                return Err("dependencies must be sorted by (from, to, kind)".to_owned());
            }
        }
        previous = Some(current);
    }
    Ok(())
}

fn ensure_diagnostics_sorted(diagnostics: &[CatalogDiagnostic]) -> Result<(), String> {
    let mut previous: Option<(CatalogDiagnosticCode, &str)> = None;
    for diagnostic in diagnostics {
        let current = (diagnostic.code, diagnostic.context.as_deref().unwrap_or(""));
        if let Some(prev) = previous {
            if prev > current {
                return Err("diagnostics must be sorted by (code, context)".to_owned());
            }
        }
        previous = Some(current);
    }
    Ok(())
}

fn unique_keys<'a>(
    keys: impl IntoIterator<Item = &'a str>,
    kind: &str,
) -> Result<BTreeSet<&'a str>, String> {
    let mut set = BTreeSet::new();
    for key in keys {
        if !set.insert(key) {
            return Err(format!("duplicate {kind} key `{key}`"));
        }
    }
    Ok(set)
}

fn slugify(raw: &str, max_len: usize) -> String {
    let lower = raw.to_ascii_lowercase();
    let mut out = String::new();
    let mut prev_dash = false;
    for ch in lower.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch);
            prev_dash = false;
        } else if !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }
    let trimmed = out.trim_matches('-').to_owned();
    let mut truncated: String = trimmed.chars().take(max_len).collect();
    while truncated.ends_with('-') {
        truncated.pop();
    }
    if truncated.is_empty() {
        "project".to_owned()
    } else {
        truncated
    }
}

fn sha256_prefixed(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(71);
    out.push_str("sha256:");
    for byte in digest {
        out.push(char::from(b"0123456789abcdef"[usize::from(byte >> 4)]));
        out.push(char::from(b"0123456789abcdef"[usize::from(byte & 0x0f)]));
    }
    out
}

fn matches_pattern(value: &str, pattern: &str) -> bool {
    match pattern {
        LOCAL_KEY_RE => is_local_key(value),
        PROJECT_KEY_RE => is_project_key(value),
        HASH256_RE => is_hash256(value),
        GIT_COMMIT_RE => is_git_commit(value),
        BARE_HEX64_RE => is_bare_hex64(value),
        RFC3339_UTC_RE => is_rfc3339_utc(value),
        _ => false,
    }
}

fn is_local_key(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.is_empty() || bytes.len() > 64 {
        return false;
    }
    let Some((first, rest)) = bytes.split_first() else {
        return false;
    };
    if !(first.is_ascii_lowercase() || first.is_ascii_digit()) {
        return false;
    }
    rest.iter()
        .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-')
}

fn is_project_key(value: &str) -> bool {
    let Some((slug, suffix)) = value.rsplit_once('-') else {
        return false;
    };
    if suffix.len() != 12
        || !suffix
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
    {
        return false;
    }
    let bytes = slug.as_bytes();
    if bytes.is_empty() || bytes.len() > 48 {
        return false;
    }
    let Some((first, rest)) = bytes.split_first() else {
        return false;
    };
    (first.is_ascii_lowercase() || first.is_ascii_digit())
        && rest
            .iter()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-')
}

fn is_hash256(value: &str) -> bool {
    let Some(hex) = value.strip_prefix("sha256:") else {
        return false;
    };
    is_bare_hex64(hex)
}

fn is_bare_hex64(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

fn is_git_commit(value: &str) -> bool {
    value.len() == 40
        && value
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

fn is_rfc3339_utc(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() < 20 || !bytes.ends_with(b"Z") {
        return false;
    }
    let core = &value[..19];
    let ok_core = core.as_bytes().get(4) == Some(&b'-')
        && core.as_bytes().get(7) == Some(&b'-')
        && core.as_bytes().get(10) == Some(&b'T')
        && core.as_bytes().get(13) == Some(&b':')
        && core.as_bytes().get(16) == Some(&b':')
        && core.as_bytes()[..4]
            .iter()
            .chain(&core.as_bytes()[5..7])
            .chain(&core.as_bytes()[8..10])
            .chain(&core.as_bytes()[11..13])
            .chain(&core.as_bytes()[14..16])
            .chain(&core.as_bytes()[17..19])
            .all(u8::is_ascii_digit);
    if !ok_core {
        return false;
    }
    if bytes.len() == 20 {
        return true;
    }
    if bytes.get(19) != Some(&b'.') {
        return false;
    }
    let frac = &value[20..value.len() - 1];
    !frac.is_empty() && frac.bytes().all(|byte| byte.is_ascii_digit())
}

fn looks_credentialed_url(url: &str) -> bool {
    if let Some(rest) = url.split_once("://").map(|(_, rest)| rest) {
        if let Some((userinfo, _)) = rest.split_once('@') {
            return userinfo.contains(':') || !userinfo.is_empty();
        }
    }
    false
}

fn reject_secret_fields(value: &Value) -> Result<(), String> {
    match value {
        Value::Object(object) => {
            for (key, child) in object {
                let normalized = key.to_ascii_lowercase().replace('-', "_");
                if matches!(
                    normalized.as_str(),
                    "access_token"
                        | "api_key"
                        | "authorization"
                        | "credentials"
                        | "password"
                        | "private_key"
                        | "refresh_token"
                        | "secret"
                        | "secrets"
                        | "token"
                ) {
                    return Err(format!(
                        "catalog contains forbidden credential field `{key}`"
                    ));
                }
                reject_secret_fields(child)?;
            }
        }
        Value::Array(array) => {
            for child in array {
                reject_secret_fields(child)?;
            }
        }
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample_evidence(path: &str) -> CatalogEvidence {
        CatalogEvidence {
            path: path.to_owned(),
            sha256: "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                .to_owned(),
            kind: CatalogEvidenceKind::Source,
            observed_commit: Some("56df19ed4dd0f19b56fc2c10faaa40278dc07936".to_owned()),
            spans: None,
            note: None,
            extra: BTreeMap::new(),
        }
    }

    fn base_catalog() -> CatalogV1 {
        let workspace = "/Users/jamesstar/fractal-cli";
        let mut catalog = CatalogV1 {
            schema: CATALOG_SCHEMA.to_owned(),
            project_key: project_key(workspace),
            generated_at: "2026-08-02T14:05:00Z".to_owned(),
            catalog_hash: String::new(),
            source: CatalogSource {
                canonical_workspace: workspace.to_owned(),
                workspace_fingerprint: workspace_fingerprint(workspace),
                registry_numbers: vec![18],
                labels: vec!["fractal-cli".to_owned()],
                git: CatalogGit {
                    is_git_repository: true,
                    commit: Some("56df19ed4dd0f19b56fc2c10faaa40278dc07936".to_owned()),
                    dirty: Some(false),
                    dirty_fingerprint: None,
                    unavailable_reason: None,
                    remotes: vec![CatalogRemote {
                        name: "origin".to_owned(),
                        fingerprint_sha256:
                            "f1057ab13991aac2f870f5e8a03e76b7f40c7e6ea63a5130d2296458d36164fa"
                                .to_owned(),
                        sanitized_url: Some(
                            "https://github.com/fractalsociety/fractal-cli.git".to_owned(),
                        ),
                        extra: BTreeMap::new(),
                    }],
                    extra: BTreeMap::new(),
                },
                extra: BTreeMap::new(),
            },
            audit: CatalogAudit {
                auditor: "fractal graph audit".to_owned(),
                cli_version: Some("0.9.4".to_owned()),
                inventory_hash:
                    "sha256:a0bbf8551226effda0186e95c0c2a0ae7efb5edc67d77b992f2b4ec5342b7baa"
                        .to_owned(),
                started_at: "2026-08-02T14:03:12Z".to_owned(),
                finished_at: "2026-08-02T14:05:00Z".to_owned(),
                bounds: CatalogBounds {
                    max_catalog_bytes: Some(DEFAULT_MAX_CATALOG_BYTES as u64),
                    max_evidence_per_claim: Some(20),
                    max_log_excerpt_chars: Some(1024),
                    max_string_chars: Some(2048),
                    test_timeout_ms: Some(600_000),
                    extra: BTreeMap::new(),
                },
                truncated: false,
                evidence_counts: None,
                extra: BTreeMap::new(),
            },
            capabilities: vec![CatalogCapability {
                key: "canonical-project-persistence".to_owned(),
                title: "Locked persistence".to_owned(),
                description: None,
                status: CatalogStatus::Verified,
                evidence: vec![sample_evidence("src/project_file.rs")],
                test_keys: vec!["cargo-test".to_owned()],
                component_keys: vec!["fractal-cli-bin".to_owned()],
                extra: BTreeMap::new(),
            }],
            components: vec![
                CatalogComponent {
                    key: "fractal-chain".to_owned(),
                    name: "fractal-chain".to_owned(),
                    kind: CatalogComponentKind::Library,
                    paths: vec!["crates/fractal-chain".to_owned()],
                    description: None,
                    status: CatalogStatus::ImplementedUnverified,
                    evidence: vec![sample_evidence("crates/fractal-chain/Cargo.toml")],
                    extra: BTreeMap::new(),
                },
                CatalogComponent {
                    key: "fractal-cli-bin".to_owned(),
                    name: "fractal-cli".to_owned(),
                    kind: CatalogComponentKind::Binary,
                    paths: vec!["src".to_owned()],
                    description: None,
                    status: CatalogStatus::Verified,
                    evidence: vec![sample_evidence("src/main.rs")],
                    extra: BTreeMap::new(),
                },
            ],
            dependencies: vec![CatalogDependency {
                from_component: "fractal-cli-bin".to_owned(),
                to_component: "fractal-chain".to_owned(),
                kind: CatalogDependencyKind::Build,
                evidence: vec![sample_evidence("Cargo.toml")],
                extra: BTreeMap::new(),
            }],
            tests: vec![CatalogTest {
                key: "cargo-test".to_owned(),
                command: "cargo test --no-fail-fast".to_owned(),
                classification: CatalogTestClassification::Pass,
                exit_code: Some(0),
                duration_ms: Some(1000),
                log_sha256: Some(
                    "sha256:1a46b67449e33a32d4f3335cc7072442d774a058db25255a3240579d45c9a0e1"
                        .to_owned(),
                ),
                log_excerpt: Some("test result: ok".to_owned()),
                evidence: vec![sample_evidence("Cargo.toml")],
                extra: BTreeMap::new(),
            }],
            decisions: vec![CatalogDecision {
                key: "additive-catalog-envelope".to_owned(),
                title: "Catalog lives under extra".to_owned(),
                summary: None,
                status: CatalogDecisionStatus::Adopted,
                evidence: vec![sample_evidence("AGENTS.md")],
                extra: BTreeMap::new(),
            }],
            cross_graph_links: vec![],
            diagnostics: vec![],
            extra: BTreeMap::new(),
        };
        normalize(&mut catalog).expect("normalize sample");
        catalog
    }

    #[test]
    fn validates_contract_example_and_derives_identity() {
        let example = include_str!("../schemas/fractal.catalog.v1.schema.json");
        let schema: Value = serde_json::from_str(example).unwrap();
        let value = schema["examples"][0].clone();
        let catalog = validate_value(&value).expect("contract example must validate");
        assert_eq!(
            catalog.project_key,
            project_key("/Users/jamesstar/fractal-cli")
        );
        assert_eq!(catalog.project_key, "fractal-cli-bbbfd315b970");
        assert_eq!(
            project_key("/Users/jamesstar/fractal-efficiency.yFzdFF"),
            "fractal-efficiency-yfzdff-fe96f21dda82"
        );
        assert_eq!(component_key_from("Cargo.toml"), "cargo-toml");
        assert_eq!(component_key_from("My Package!!"), "my-package");
    }

    #[test]
    fn valid_catalog_passes_validation() {
        let catalog = base_catalog();
        validate(&catalog).expect("valid catalog");
    }

    #[test]
    fn partial_status_is_accepted_without_passing_tests() {
        let mut catalog = base_catalog();
        catalog.capabilities[0].status = CatalogStatus::Partial;
        catalog.capabilities[0].test_keys.clear();
        catalog.components[1].status = CatalogStatus::Partial;
        normalize(&mut catalog).unwrap();
        validate(&catalog).expect("partial statuses are conservative and valid");
    }

    #[test]
    fn future_fields_are_preserved_through_round_trip() {
        let mut catalog = base_catalog();
        catalog
            .extra
            .insert("future_catalog_field".to_owned(), json!({"kept": true}));
        catalog.capabilities[0]
            .extra
            .insert("future_capability_note".to_owned(), json!("ok"));
        normalize(&mut catalog).unwrap();
        validate(&catalog).unwrap();
        let encoded = serde_json::to_value(&catalog).unwrap();
        let decoded: CatalogV1 = serde_json::from_value(encoded).unwrap();
        assert_eq!(
            decoded.extra.get("future_catalog_field"),
            Some(&json!({"kept": true}))
        );
        assert_eq!(
            decoded.capabilities[0].extra.get("future_capability_note"),
            Some(&json!("ok"))
        );
        validate(&decoded).unwrap();
    }

    #[test]
    fn invalid_catalog_hash_is_rejected() {
        let mut catalog = base_catalog();
        catalog.catalog_hash =
            "sha256:0000000000000000000000000000000000000000000000000000000000000000".to_owned();
        let error = validate(&catalog).expect_err("hash mismatch");
        assert!(error.contains("catalog_hash mismatch"));
    }

    #[test]
    fn secret_fields_are_rejected() {
        let mut catalog = base_catalog();
        catalog.source.git.remotes[0]
            .extra
            .insert("token".to_owned(), json!("ghp_secret"));
        normalize(&mut catalog).unwrap();
        let error = validate(&catalog).expect_err("secret key");
        assert!(error.contains("forbidden credential field"));
    }

    #[test]
    fn oversized_catalog_is_rejected() {
        let mut catalog = base_catalog();
        catalog.audit.bounds.max_catalog_bytes = Some(64);
        catalog.audit.truncated = true;
        catalog.diagnostics = vec![CatalogDiagnostic {
            code: CatalogDiagnosticCode::CatalogBoundExceeded,
            severity: CatalogDiagnosticSeverity::Warning,
            message: "truncated for test".to_owned(),
            context: Some("capabilities".to_owned()),
            extra: BTreeMap::new(),
        }];
        normalize(&mut catalog).unwrap();
        let error = validate(&catalog).expect_err("oversized");
        assert!(error.contains("max_catalog_bytes"));
    }

    #[test]
    fn dirty_fingerprint_is_required_and_checked() {
        let mut catalog = base_catalog();
        catalog.source.git.dirty = Some(true);
        catalog.source.git.dirty_fingerprint = None;
        normalize(&mut catalog).unwrap();
        let error = validate(&catalog).expect_err("dirty fingerprint required");
        assert!(error.contains("dirty_fingerprint"));

        catalog.source.git.dirty_fingerprint = Some(compute_dirty_fingerprint(&catalog).unwrap());
        normalize(&mut catalog).unwrap();
        validate(&catalog).expect("dirty catalog with matching fingerprint");
    }

    #[test]
    fn verified_without_passing_test_is_rejected() {
        let mut catalog = base_catalog();
        catalog.tests[0].classification = CatalogTestClassification::Fail;
        normalize(&mut catalog).unwrap();
        let error = validate(&catalog).expect_err("verified requires pass");
        assert!(error.contains("passing test"));
    }
}
