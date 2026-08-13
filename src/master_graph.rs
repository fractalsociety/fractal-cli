//! Deterministic, read-only master-graph composition over a frozen
//! `fractal.repository_inventory.v1` artifact.
//!
//! The composed `fractal.master_graph_view.v1` lives only in process memory.
//! This module never writes source repositories, never treats the fingerprint
//! cache as authoritative, and never routes through sync or mutation paths.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::Path;
use std::sync::{Mutex, OnceLock};
use std::time::SystemTime;

#[cfg(test)]
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

const VIEW_SCHEMA: &str = "fractal.master_graph_view.v1";
const INVENTORY_SCHEMA: &str = "fractal.repository_inventory.v1";
const CATALOG_SCHEMA: &str = "fractal.catalog.v1";
const PROJECT_RELATIVE: &str = ".fractal/project.fractal";

const MAX_VIEW_PROJECTS: usize = 2048;
const MAX_VIEW_NODES: usize = 65_536;
const MAX_VIEW_EDGES: usize = 65_536;
const MAX_VIEW_DIAGNOSTICS: usize = 16_384;
const MAX_CACHE_ENTRIES: usize = MAX_VIEW_PROJECTS;

const LINK_TYPES: &[&str] = &[
    "depends_on",
    "uses_component",
    "derived_from",
    "forked_from",
    "supersedes",
    "shares_component",
    "related_to",
];
const DEP_KINDS: &[&str] = &["build", "runtime", "dev", "test", "data", "other"];
const STATUSES: &[&str] = &["verified", "implemented_unverified", "partial", "unknown"];

/// Options for read-only composition.
#[derive(Clone, Debug, Default)]
pub(crate) struct ComposeOptions {
    /// When true, return [`ComposeResult::ValidateOnly`] after composition.
    pub(crate) validate_only: bool,
    /// Optional shared fingerprint cache. Cached entries are never authoritative.
    pub(crate) cache: Option<&'static Mutex<FingerprintCache>>,
}

/// Full view or validate-only projection.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub(crate) enum ComposeResult {
    View(MasterGraphView),
    ValidateOnly(ValidateOnlyOutput),
}

/// Validate-only projection: hashes, summary, diagnostics — no wall-clock fields.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub(crate) struct ValidateOnlyOutput {
    pub(crate) schema: String,
    pub(crate) inventory_hash: String,
    pub(crate) view_hash: String,
    pub(crate) summary: ViewSummary,
    pub(crate) diagnostics: Vec<ViewDiagnostic>,
    pub(crate) valid: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub(crate) struct MasterGraphView {
    pub(crate) schema: String,
    pub(crate) inventory_hash: String,
    pub(crate) summary: ViewSummary,
    pub(crate) projects: Vec<ProjectEntry>,
    pub(crate) nodes: Vec<ViewNode>,
    pub(crate) edges: Vec<ViewEdge>,
    pub(crate) diagnostics: Vec<ViewDiagnostic>,
    pub(crate) sources: Vec<SourceProvenance>,
    pub(crate) unavailable: Vec<UnavailableEntry>,
    pub(crate) view_hash: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub(crate) struct ViewSummary {
    pub(crate) projects_total: usize,
    pub(crate) available_inventory_count: usize,
    pub(crate) audited_available: usize,
    pub(crate) invalid_catalogs: usize,
    pub(crate) node_count: usize,
    pub(crate) edge_count: usize,
    pub(crate) links_resolved: usize,
    pub(crate) links_unresolved: usize,
    pub(crate) cycle_count: usize,
    pub(crate) diagnostic_counts: DiagnosticCounts,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub(crate) struct DiagnosticCounts {
    pub(crate) error: usize,
    pub(crate) warning: usize,
    pub(crate) info: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub(crate) struct ProjectEntry {
    pub(crate) project_key: String,
    pub(crate) canonical_workspace: String,
    pub(crate) workspace_fingerprint: String,
    pub(crate) labels: Vec<String>,
    pub(crate) registry_numbers: Vec<u64>,
    pub(crate) available: bool,
    pub(crate) catalog_state: String,
    pub(crate) graph_hash: Option<String>,
    pub(crate) catalog_hash: Option<String>,
    pub(crate) git: ProjectGitSummary,
    pub(crate) status_counts: StatusCounts,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub(crate) struct ProjectGitSummary {
    pub(crate) commit: Option<String>,
    pub(crate) dirty: Option<bool>,
    pub(crate) unavailable_reason: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub(crate) struct StatusCounts {
    pub(crate) verified: usize,
    pub(crate) implemented_unverified: usize,
    pub(crate) partial: usize,
    pub(crate) unknown: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub(crate) struct ViewNode {
    pub(crate) id: String,
    pub(crate) kind: String,
    pub(crate) project_key: String,
    pub(crate) key: String,
    pub(crate) title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) component_kind: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub(crate) struct ViewEdge {
    pub(crate) id: String,
    #[serde(rename = "type")]
    pub(crate) edge_type: String,
    pub(crate) origin_project_key: String,
    pub(crate) from: String,
    pub(crate) to: EdgeTarget,
    pub(crate) resolution: String,
    pub(crate) cycle_group: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) confidence: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub(crate) struct EdgeTarget {
    pub(crate) node_id: Option<String>,
    pub(crate) raw: Option<Value>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub(crate) struct ViewDiagnostic {
    pub(crate) code: String,
    pub(crate) severity: String,
    pub(crate) message: String,
    pub(crate) project_key: Option<String>,
    pub(crate) edge_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) context: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub(crate) struct SourceProvenance {
    pub(crate) project_key: String,
    pub(crate) canonical_workspace: String,
    pub(crate) relative_path: String,
    pub(crate) project_fractal_sha256: String,
    pub(crate) size_bytes: u64,
    pub(crate) graph_hash: Option<String>,
    pub(crate) catalog_hash: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub(crate) struct UnavailableEntry {
    pub(crate) canonical_workspace: String,
    pub(crate) reason: String,
    pub(crate) registry_numbers: Vec<u64>,
}

/// Frozen inventory artifact.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub(crate) struct RepositoryInventory {
    pub(crate) schema: String,
    pub(crate) inventory_hash: String,
    #[serde(default)]
    pub(crate) records: Vec<InventoryRecord>,
    #[serde(flatten)]
    pub(crate) extra: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub(crate) struct InventoryRecord {
    pub(crate) canonical_workspace: String,
    #[serde(default)]
    pub(crate) exists: bool,
    #[serde(default)]
    pub(crate) labels: Vec<String>,
    #[serde(default)]
    pub(crate) registry_numbers: Vec<u64>,
    #[serde(default)]
    pub(crate) unavailable_reason: Option<String>,
    #[serde(default)]
    pub(crate) git: Option<InventoryGit>,
    #[serde(default)]
    pub(crate) project_fractal: Option<InventoryProjectFractal>,
    #[serde(flatten)]
    pub(crate) extra: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub(crate) struct InventoryGit {
    #[serde(default)]
    pub(crate) is_git_repository: Option<bool>,
    #[serde(default)]
    pub(crate) head: Option<String>,
    #[serde(default)]
    pub(crate) dirty: Option<bool>,
    #[serde(default)]
    pub(crate) unavailable_reason: Option<String>,
    #[serde(default)]
    pub(crate) remotes: Vec<InventoryRemote>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub(crate) struct InventoryRemote {
    #[serde(default)]
    pub(crate) name: Option<String>,
    #[serde(default)]
    pub(crate) fingerprint_sha256: Option<String>,
    #[serde(default)]
    pub(crate) sanitized_url: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub(crate) struct InventoryProjectFractal {
    #[serde(default)]
    pub(crate) available: bool,
    #[serde(default)]
    pub(crate) relative_path: Option<String>,
    #[serde(default)]
    pub(crate) size_bytes: Option<u64>,
    #[serde(default)]
    pub(crate) unavailable_reason: Option<String>,
}

/// Fingerprint identifying a project.fractal snapshot. Cache entries keyed by
/// this are advisory only and must be re-checked against the filesystem.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct FileFingerprint {
    pub(crate) canonical_path: String,
    pub(crate) size: u64,
    pub(crate) mtime_nanos: u128,
    pub(crate) graph_hash: String,
    pub(crate) catalog_hash: String,
}

#[derive(Clone, Debug)]
struct CachedProjectSlice {
    fingerprint: FileFingerprint,
    catalog_state: String,
    catalog: Option<Value>,
    graph_hash: Option<String>,
    catalog_hash: Option<String>,
    bytes_sha256: String,
    size_bytes: u64,
    load_error: Option<String>,
}

/// Bounded in-memory fingerprint cache. Never authoritative.
#[derive(Debug, Default)]
pub(crate) struct FingerprintCache {
    entries: BTreeMap<String, CachedProjectSlice>,
    hits: u64,
    misses: u64,
    invalidations: u64,
}

impl FingerprintCache {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    #[cfg(test)]
    pub(crate) fn hits(&self) -> u64 {
        self.hits
    }

    #[cfg(test)]
    pub(crate) fn misses(&self) -> u64 {
        self.misses
    }

    #[cfg(test)]
    pub(crate) fn invalidations(&self) -> u64 {
        self.invalidations
    }

    fn get_if_fresh(&mut self, key: &str, current: &FileFingerprint) -> Option<CachedProjectSlice> {
        match self.entries.get(key) {
            Some(entry) if &entry.fingerprint == current => {
                self.hits += 1;
                Some(entry.clone())
            }
            Some(_) => {
                self.entries.remove(key);
                self.invalidations += 1;
                self.misses += 1;
                None
            }
            None => {
                self.misses += 1;
                None
            }
        }
    }

    fn insert(&mut self, key: String, slice: CachedProjectSlice) {
        if self.entries.len() >= MAX_CACHE_ENTRIES && !self.entries.contains_key(&key) {
            if let Some(oldest) = self.entries.keys().next().cloned() {
                self.entries.remove(&oldest);
            }
        }
        self.entries.insert(key, slice);
    }

    #[cfg(test)]
    pub(crate) fn clear(&mut self) {
        self.entries.clear();
    }
}

fn shared_cache() -> &'static Mutex<FingerprintCache> {
    static CACHE: OnceLock<Mutex<FingerprintCache>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(FingerprintCache::new()))
}

/// Load and lightly validate a frozen inventory artifact (read-only).
#[allow(dead_code)]
pub(crate) fn load_inventory(path: &Path) -> Result<RepositoryInventory> {
    let bytes = fs::read(path).with_context(|| format!("read inventory {}", path.display()))?;
    let value: Value = serde_json::from_slice(&bytes)
        .with_context(|| format!("decode inventory {}", path.display()))?;
    let schema = value.get("schema").and_then(Value::as_str).unwrap_or("");
    if schema != INVENTORY_SCHEMA {
        bail!(
            "inventory schema must be {INVENTORY_SCHEMA}, found `{}`",
            schema
        );
    }
    let inventory: RepositoryInventory = serde_json::from_value(value)
        .with_context(|| format!("parse inventory {}", path.display()))?;
    if !inventory.inventory_hash.starts_with("sha256:") || inventory.inventory_hash.len() != 71 {
        bail!("inventory_hash must be sha256:<64 hex>");
    }
    Ok(inventory)
}

/// Compose a master graph view from an inventory file path.
#[allow(dead_code)] // wired by later clap/board integration nodes
pub(crate) fn compose_path(
    inventory_path: &Path,
    options: ComposeOptions,
) -> Result<ComposeResult> {
    let inventory = load_inventory(inventory_path)?;
    compose_inventory(&inventory, options)
}

/// Compose from an already-loaded inventory (still read-only w.r.t. sources).
pub(crate) fn compose_inventory(
    inventory: &RepositoryInventory,
    options: ComposeOptions,
) -> Result<ComposeResult> {
    let view = compose_view(inventory, options.cache.or_else(|| Some(shared_cache())))?;
    if options.validate_only {
        let valid = view.summary.invalid_catalogs == 0
            && view.diagnostics.iter().all(|d| d.severity != "error");
        Ok(ComposeResult::ValidateOnly(ValidateOnlyOutput {
            schema: VIEW_SCHEMA.to_owned(),
            inventory_hash: view.inventory_hash.clone(),
            view_hash: view.view_hash.clone(),
            summary: view.summary.clone(),
            diagnostics: view.diagnostics.clone(),
            valid,
        }))
    } else {
        Ok(ComposeResult::View(view))
    }
}

fn compose_view(
    inventory: &RepositoryInventory,
    cache: Option<&'static Mutex<FingerprintCache>>,
) -> Result<MasterGraphView> {
    let mut diagnostics: Vec<ViewDiagnostic> = Vec::new();
    let mut projects: Vec<ProjectEntry> = Vec::new();
    let mut nodes: BTreeMap<String, ViewNode> = BTreeMap::new();
    let mut edges: BTreeMap<String, ViewEdge> = BTreeMap::new();
    let mut sources: Vec<SourceProvenance> = Vec::new();
    let mut unavailable: Vec<UnavailableEntry> = Vec::new();

    // Sort inventory records by canonical_workspace for deterministic processing.
    let mut records = inventory.records.clone();
    records.sort_by(|a, b| a.canonical_workspace.cmp(&b.canonical_workspace));

    // Detect project_key collisions across workspaces.
    let mut key_owners: BTreeMap<String, String> = BTreeMap::new();
    let mut excluded_workspaces: BTreeSet<String> = BTreeSet::new();

    for record in &records {
        if !record.exists {
            continue;
        }
        let key = derive_project_key(&record.canonical_workspace);
        if let Some(first) = key_owners.get(&key) {
            if first != &record.canonical_workspace {
                excluded_workspaces.insert(record.canonical_workspace.clone());
                diagnostics.push(diag(
                    "project_key_collision",
                    "error",
                    format!(
                        "workspace `{}` collides on project_key `{key}` with `{}`; excluded",
                        record.canonical_workspace, first
                    ),
                    Some(key.clone()),
                    None,
                    Some(record.canonical_workspace.clone()),
                ));
            }
        } else {
            key_owners.insert(key, record.canonical_workspace.clone());
        }
    }

    // Alias index for duplicate detection + resolution.
    let mut label_to_keys: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut remote_fp_to_keys: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut workspace_fp_to_keys: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut project_keys: BTreeSet<String> = BTreeSet::new();
    let mut catalogs: BTreeMap<String, Value> = BTreeMap::new();
    let mut component_keys_by_project: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();

    let mut available_count = 0usize;
    let mut audited_available = 0usize;
    let mut invalid_catalogs = 0usize;
    let mut truncated = false;

    for record in &records {
        let canonical = record.canonical_workspace.clone();
        if !record.exists {
            unavailable.push(UnavailableEntry {
                canonical_workspace: canonical.clone(),
                reason: record
                    .unavailable_reason
                    .clone()
                    .unwrap_or_else(|| "workspace_path_does_not_exist".to_owned()),
                registry_numbers: record.registry_numbers.clone(),
            });
            diagnostics.push(diag(
                "unavailable_workspace",
                "warning",
                "registry workspace does not exist and was recorded without fabricating content"
                    .to_owned(),
                None,
                None,
                Some(canonical),
            ));
            continue;
        }

        if excluded_workspaces.contains(&canonical) {
            continue;
        }

        if record
            .project_fractal
            .as_ref()
            .is_some_and(|project| !project.available)
            || !Path::new(&canonical).join(PROJECT_RELATIVE).exists()
        {
            let reason = record
                .project_fractal
                .as_ref()
                .and_then(|project| project.unavailable_reason.as_deref())
                .unwrap_or("project_fractal_missing")
                .to_owned();
            unavailable.push(UnavailableEntry {
                canonical_workspace: canonical.clone(),
                reason,
                registry_numbers: record.registry_numbers.clone(),
            });
            diagnostics.push(diag(
                "missing_project_fractal",
                "warning",
                "project.fractal is missing; workspace retained as explicitly unavailable"
                    .to_owned(),
                Some(derive_project_key(&canonical)),
                None,
                Some(canonical),
            ));
            continue;
        }

        available_count += 1;
        let project_key = derive_project_key(&canonical);
        let workspace_fingerprint = derive_workspace_fingerprint(&canonical);
        project_keys.insert(project_key.clone());

        for label in &record.labels {
            label_to_keys
                .entry(label.clone())
                .or_default()
                .insert(project_key.clone());
        }
        workspace_fp_to_keys
            .entry(workspace_fingerprint.clone())
            .or_default()
            .insert(project_key.clone());
        if let Some(git) = &record.git {
            for remote in &git.remotes {
                if let Some(fp) = &remote.fingerprint_sha256 {
                    remote_fp_to_keys
                        .entry(fp.clone())
                        .or_default()
                        .insert(project_key.clone());
                }
            }
        }

        let loaded = load_project_slice(&canonical, cache)?;
        let title = record
            .labels
            .first()
            .cloned()
            .unwrap_or_else(|| path_segment(&canonical));

        let mut catalog_state = loaded.catalog_state.clone();
        let mut status_counts = StatusCounts::default();
        let mut catalog_hash = loaded.catalog_hash.clone();
        let graph_hash = loaded.graph_hash.clone();

        if let Some(err) = &loaded.load_error {
            diagnostics.push(diag(
                "invalid_project_document",
                "error",
                format!("failed to load project document: {err}"),
                Some(project_key.clone()),
                None,
                Some(canonical.clone()),
            ));
            catalog_state = "invalid".to_owned();
            invalid_catalogs += 1;
        } else if catalog_state == "missing" {
            diagnostics.push(diag(
                "missing_catalog",
                "info",
                "project document has no catalog key".to_owned(),
                Some(project_key.clone()),
                None,
                None,
            ));
        } else if catalog_state == "unsupported_schema" {
            diagnostics.push(diag(
                "unsupported_catalog_schema",
                "error",
                "catalog schema is not fractal.catalog.v1; left opaque".to_owned(),
                Some(project_key.clone()),
                None,
                None,
            ));
            invalid_catalogs += 1;
        } else if catalog_state == "invalid" {
            diagnostics.push(diag(
                "invalid_catalog",
                "error",
                "catalog failed validation".to_owned(),
                Some(project_key.clone()),
                None,
                None,
            ));
            invalid_catalogs += 1;
        } else if catalog_state == "valid" {
            if let Some(catalog) = &loaded.catalog {
                match validate_catalog(catalog, &project_key, &canonical, &workspace_fingerprint) {
                    Ok(validated) => {
                        audited_available += 1;
                        catalog_hash = Some(validated.catalog_hash.clone());
                        status_counts = validated.status_counts.clone();
                        catalogs.insert(project_key.clone(), validated.value.clone());
                        component_keys_by_project
                            .insert(project_key.clone(), validated.component_keys.clone());
                        for d in validated.diagnostics {
                            diagnostics.push(d);
                        }
                    }
                    Err(message) => {
                        catalog_state = "invalid".to_owned();
                        invalid_catalogs += 1;
                        diagnostics.push(diag(
                            "invalid_catalog",
                            "error",
                            message,
                            Some(project_key.clone()),
                            None,
                            None,
                        ));
                    }
                }
            }
        }

        // Provenance for every successfully read byte stream.
        if loaded.size_bytes > 0 || loaded.bytes_sha256 != empty_sha256() {
            sources.push(SourceProvenance {
                project_key: project_key.clone(),
                canonical_workspace: canonical.clone(),
                relative_path: PROJECT_RELATIVE.to_owned(),
                project_fractal_sha256: loaded.bytes_sha256.clone(),
                size_bytes: loaded.size_bytes,
                graph_hash: graph_hash.clone(),
                catalog_hash: catalog_hash.clone(),
            });
        }

        let git = ProjectGitSummary {
            commit: record.git.as_ref().and_then(|g| g.head.clone()),
            dirty: record.git.as_ref().and_then(|g| g.dirty),
            unavailable_reason: record
                .git
                .as_ref()
                .and_then(|g| g.unavailable_reason.clone()),
        };

        if projects.len() < MAX_VIEW_PROJECTS {
            projects.push(ProjectEntry {
                project_key: project_key.clone(),
                canonical_workspace: canonical.clone(),
                workspace_fingerprint,
                labels: record.labels.clone(),
                registry_numbers: record.registry_numbers.clone(),
                available: true,
                catalog_state,
                graph_hash,
                catalog_hash,
                git,
                status_counts,
            });
        } else {
            truncated = true;
        }

        // Always emit the project node (even without a catalog).
        insert_node(
            &mut nodes,
            &mut diagnostics,
            ViewNode {
                id: format!("project:{project_key}"),
                kind: "project".to_owned(),
                project_key: project_key.clone(),
                key: project_key.clone(),
                title,
                status: None,
                component_kind: None,
            },
            &mut truncated,
        );
    }

    // duplicate_alias diagnostics
    for (label, keys) in &label_to_keys {
        if keys.len() > 1 {
            diagnostics.push(diag(
                "duplicate_alias",
                "warning",
                format!("label `{label}` maps to multiple project_keys"),
                None,
                None,
                Some(keys.iter().cloned().collect::<Vec<_>>().join(",")),
            ));
        }
    }

    // Build catalog-derived nodes and edges.
    for (project_key, catalog) in &catalogs {
        let components = catalog
            .get("components")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        for component in components {
            let key = component
                .get("key")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            if key.is_empty() {
                continue;
            }
            let title = component
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or(&key)
                .to_owned();
            let status = component
                .get("status")
                .and_then(Value::as_str)
                .map(str::to_owned);
            let component_kind = component
                .get("kind")
                .and_then(Value::as_str)
                .map(str::to_owned);
            insert_node(
                &mut nodes,
                &mut diagnostics,
                ViewNode {
                    id: format!("component:{project_key}/{key}"),
                    kind: "component".to_owned(),
                    project_key: project_key.clone(),
                    key,
                    title,
                    status,
                    component_kind,
                },
                &mut truncated,
            );
        }

        let capabilities = catalog
            .get("capabilities")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        for capability in capabilities {
            let key = capability
                .get("key")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            if key.is_empty() {
                continue;
            }
            let title = capability
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or(&key)
                .to_owned();
            let status = capability
                .get("status")
                .and_then(Value::as_str)
                .map(str::to_owned);
            insert_node(
                &mut nodes,
                &mut diagnostics,
                ViewNode {
                    id: format!("capability:{project_key}/{key}"),
                    kind: "capability".to_owned(),
                    project_key: project_key.clone(),
                    key,
                    title,
                    status,
                    component_kind: None,
                },
                &mut truncated,
            );
        }

        let dependencies = catalog
            .get("dependencies")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        for dep in dependencies {
            let from_c = dep
                .get("from_component")
                .and_then(Value::as_str)
                .unwrap_or("");
            let to_c = dep
                .get("to_component")
                .and_then(Value::as_str)
                .unwrap_or("");
            let kind = dep.get("kind").and_then(Value::as_str).unwrap_or("other");
            if from_c.is_empty() || to_c.is_empty() {
                continue;
            }
            let id = format!("dep:{project_key}/{from_c}->{to_c}:{kind}");
            insert_edge(
                &mut edges,
                ViewEdge {
                    id,
                    edge_type: "internal_dependency".to_owned(),
                    origin_project_key: project_key.clone(),
                    from: format!("component:{project_key}/{from_c}"),
                    to: EdgeTarget {
                        node_id: Some(format!("component:{project_key}/{to_c}")),
                        raw: None,
                    },
                    resolution: "resolved".to_owned(),
                    cycle_group: None,
                    confidence: None,
                },
                &mut truncated,
            );
        }

        let mut seen_link_keys: BTreeSet<String> = BTreeSet::new();
        let links = catalog
            .get("cross_graph_links")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        for link in links {
            let link_key = link
                .get("key")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            if link_key.is_empty() {
                continue;
            }
            if !seen_link_keys.insert(link_key.clone()) {
                diagnostics.push(diag(
                    "duplicate_link_key",
                    "warning",
                    format!("duplicate cross_graph_links key `{link_key}`"),
                    Some(project_key.clone()),
                    Some(format!("link:{project_key}/{link_key}")),
                    Some(format!("cross_graph_links[key={link_key}]")),
                ));
                continue;
            }

            let link_type = link
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("related_to")
                .to_owned();
            let confidence = link
                .get("confidence")
                .and_then(Value::as_str)
                .map(str::to_owned);
            let from_component = link.pointer("/from/component_key").and_then(Value::as_str);
            let from_id = match from_component {
                Some(c) if !c.is_empty() => format!("component:{project_key}/{c}"),
                _ => format!("project:{project_key}"),
            };

            let to_obj = link.get("to").cloned().unwrap_or(Value::Null);
            let to_project_key = to_obj.get("project_key").and_then(Value::as_str);
            let to_alias = to_obj.get("alias").and_then(Value::as_str);
            let to_component = to_obj
                .get("component_key")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty());

            let edge_id = format!("link:{project_key}/{link_key}");
            let raw_target = json!({
                "project_key": to_project_key,
                "alias": to_alias,
                "component_key": to_component,
            });

            let (resolution, target_project, target_node) = resolve_link_target(
                project_key,
                to_project_key,
                to_alias,
                to_component,
                &project_keys,
                &label_to_keys,
                &remote_fp_to_keys,
                &workspace_fp_to_keys,
                &component_keys_by_project,
                &mut diagnostics,
                &edge_id,
                &link_key,
            );

            let (node_id, raw) = match resolution.as_str() {
                "resolved" | "self" => (target_node, None),
                _ => (None, Some(raw_target)),
            };

            let _ = target_project; // retained for cycle detection via node_id
            insert_edge(
                &mut edges,
                ViewEdge {
                    id: edge_id,
                    edge_type: link_type,
                    origin_project_key: project_key.clone(),
                    from: from_id,
                    to: EdgeTarget { node_id, raw },
                    resolution,
                    cycle_group: None,
                    confidence,
                },
                &mut truncated,
            );
        }
    }

    // Cross-project cycle detection among resolved (non-self) link edges.
    let cycle_count = assign_cycle_groups(&mut edges, &mut diagnostics);

    if truncated {
        diagnostics.push(diag(
            "view_truncated",
            "warning",
            "master view exceeded configured bounds; excess material was dropped".to_owned(),
            None,
            None,
            None,
        ));
    }

    // Deterministic ordering.
    projects.sort_by(|a, b| a.project_key.cmp(&b.project_key));
    sources.sort_by(|a, b| a.project_key.cmp(&b.project_key));
    unavailable.sort_by(|a, b| a.canonical_workspace.cmp(&b.canonical_workspace));
    let mut nodes: Vec<ViewNode> = nodes.into_values().collect();
    nodes.sort_by(|a, b| a.id.cmp(&b.id));
    let mut edges: Vec<ViewEdge> = edges.into_values().collect();
    edges.sort_by(|a, b| a.id.cmp(&b.id));
    sort_diagnostics(&mut diagnostics);

    // Enforce max bounds after sort (stable drop from the end).
    if nodes.len() > MAX_VIEW_NODES {
        nodes.truncate(MAX_VIEW_NODES);
    }
    if edges.len() > MAX_VIEW_EDGES {
        edges.truncate(MAX_VIEW_EDGES);
    }
    if diagnostics.len() > MAX_VIEW_DIAGNOSTICS {
        diagnostics.truncate(MAX_VIEW_DIAGNOSTICS);
    }

    let links_resolved = edges
        .iter()
        .filter(|e| e.edge_type != "internal_dependency" && e.resolution == "resolved")
        .count();
    let links_unresolved = edges
        .iter()
        .filter(|e| {
            e.edge_type != "internal_dependency"
                && (e.resolution == "unresolved" || e.resolution == "ambiguous")
        })
        .count();

    let diagnostic_counts = count_diagnostics(&diagnostics);
    let projects_total = available_count + unavailable.len() + excluded_workspaces.len();

    let mut view = MasterGraphView {
        schema: VIEW_SCHEMA.to_owned(),
        inventory_hash: inventory.inventory_hash.clone(),
        summary: ViewSummary {
            projects_total,
            available_inventory_count: available_count,
            audited_available,
            invalid_catalogs,
            node_count: nodes.len(),
            edge_count: edges.len(),
            links_resolved,
            links_unresolved,
            cycle_count,
            diagnostic_counts,
        },
        projects,
        nodes,
        edges,
        diagnostics,
        sources,
        unavailable,
        view_hash: String::new(),
    };
    view.view_hash = compute_view_hash(&view)?;
    Ok(view)
}

#[allow(clippy::too_many_arguments)]
fn resolve_link_target(
    origin_project_key: &str,
    to_project_key: Option<&str>,
    to_alias: Option<&str>,
    to_component: Option<&str>,
    project_keys: &BTreeSet<String>,
    label_to_keys: &BTreeMap<String, BTreeSet<String>>,
    remote_fp_to_keys: &BTreeMap<String, BTreeSet<String>>,
    workspace_fp_to_keys: &BTreeMap<String, BTreeSet<String>>,
    component_keys_by_project: &BTreeMap<String, BTreeSet<String>>,
    diagnostics: &mut Vec<ViewDiagnostic>,
    edge_id: &str,
    link_key: &str,
) -> (String, Option<String>, Option<String>) {
    let context = format!("cross_graph_links[key={link_key}]");

    let matched: Vec<String> = if let Some(pk) = to_project_key.filter(|s| !s.is_empty()) {
        if project_keys.contains(pk) {
            vec![pk.to_owned()]
        } else {
            Vec::new()
        }
    } else if let Some(alias) = to_alias.filter(|s| !s.is_empty()) {
        let mut matches = BTreeSet::new();
        if project_keys.contains(alias) {
            matches.insert(alias.to_owned());
        }
        if let Some(keys) = label_to_keys.get(alias) {
            matches.extend(keys.iter().cloned());
        }
        if looks_like_bare_hex(alias) {
            if let Some(keys) = remote_fp_to_keys.get(alias) {
                matches.extend(keys.iter().cloned());
            }
        }
        if alias.starts_with("sha256:") {
            if let Some(keys) = workspace_fp_to_keys.get(alias) {
                matches.extend(keys.iter().cloned());
            }
        }
        matches.into_iter().collect()
    } else {
        Vec::new()
    };

    match matched.as_slice() {
        [] => {
            diagnostics.push(diag(
                "unresolved_link_target",
                "warning",
                match to_alias {
                    Some(alias) => format!(
                        "alias `{alias}` matched no project_key, label, remote fingerprint, or workspace fingerprint in the frozen inventory"
                    ),
                    None => format!(
                        "project_key `{}` not present in composed inventory",
                        to_project_key.unwrap_or("")
                    ),
                },
                Some(origin_project_key.to_owned()),
                Some(edge_id.to_owned()),
                Some(context),
            ));
            ("unresolved".to_owned(), None, None)
        }
        [one] if matched.len() == 1 => {
            let target_pk = one.clone();
            if target_pk == origin_project_key {
                diagnostics.push(diag(
                    "self_link",
                    "warning",
                    "cross_graph_link resolves to its own project".to_owned(),
                    Some(origin_project_key.to_owned()),
                    Some(edge_id.to_owned()),
                    Some(context.clone()),
                ));
                let node_id = match to_component {
                    Some(c)
                        if component_keys_by_project
                            .get(&target_pk)
                            .is_some_and(|set| set.contains(c)) =>
                    {
                        format!("component:{target_pk}/{c}")
                    }
                    Some(c) => {
                        diagnostics.push(diag(
                            "unresolved_link_component",
                            "warning",
                            format!(
                                "component_key `{c}` missing in target catalog; resolved to project node"
                            ),
                            Some(origin_project_key.to_owned()),
                            Some(edge_id.to_owned()),
                            Some(context.clone()),
                        ));
                        format!("project:{target_pk}")
                    }
                    None => format!("project:{target_pk}"),
                };
                ("self".to_owned(), Some(target_pk), Some(node_id))
            } else {
                let node_id = match to_component {
                    Some(c)
                        if component_keys_by_project
                            .get(&target_pk)
                            .is_some_and(|set| set.contains(c)) =>
                    {
                        format!("component:{target_pk}/{c}")
                    }
                    Some(c) => {
                        diagnostics.push(diag(
                            "unresolved_link_component",
                            "warning",
                            format!(
                                "component_key `{c}` missing in target catalog; resolved to project node"
                            ),
                            Some(origin_project_key.to_owned()),
                            Some(edge_id.to_owned()),
                            Some(context.clone()),
                        ));
                        format!("project:{target_pk}")
                    }
                    None => format!("project:{target_pk}"),
                };
                ("resolved".to_owned(), Some(target_pk), Some(node_id))
            }
        }
        _ => {
            diagnostics.push(diag(
                "ambiguous_alias",
                "warning",
                format!(
                    "alias `{}` matched multiple projects: {}",
                    to_alias.unwrap_or(""),
                    matched.join(",")
                ),
                Some(origin_project_key.to_owned()),
                Some(edge_id.to_owned()),
                Some(context),
            ));
            ("ambiguous".to_owned(), None, None)
        }
    }
}

fn assign_cycle_groups(
    edges: &mut BTreeMap<String, ViewEdge>,
    diagnostics: &mut Vec<ViewDiagnostic>,
) -> usize {
    // Build project-level adjacency from resolved cross links.
    let mut adjacency: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut link_edges: Vec<(String, String, String)> = Vec::new(); // id, from_pk, to_pk

    for edge in edges.values() {
        if edge.edge_type == "internal_dependency" || edge.resolution != "resolved" {
            continue;
        }
        let Some(to_id) = edge.to.node_id.as_deref() else {
            continue;
        };
        let Some(to_pk) = project_key_from_node_id(to_id) else {
            continue;
        };
        let from_pk = edge.origin_project_key.clone();
        if from_pk == to_pk {
            continue;
        }
        adjacency
            .entry(from_pk.clone())
            .or_default()
            .insert(to_pk.clone());
        link_edges.push((edge.id.clone(), from_pk, to_pk));
    }

    let sccs = strongly_connected_components(&adjacency);
    let mut cyclic_projects: BTreeSet<String> = BTreeSet::new();
    for scc in &sccs {
        if scc.len() > 1 {
            cyclic_projects.extend(scc.iter().cloned());
        } else if let Some(single) = scc.iter().next() {
            // Self-loop at project level (should already be "self", but guard).
            if adjacency
                .get(single)
                .is_some_and(|succ| succ.contains(single))
            {
                cyclic_projects.insert(single.clone());
            }
        }
    }

    let mut groups: BTreeMap<String, Vec<String>> = BTreeMap::new(); // representative -> edge ids
    for (edge_id, from_pk, to_pk) in &link_edges {
        if cyclic_projects.contains(from_pk) && cyclic_projects.contains(to_pk) {
            // Find SCC id = min project key in the component containing from_pk
            let scc = sccs
                .iter()
                .find(|c| c.contains(from_pk))
                .cloned()
                .unwrap_or_default();
            if scc.len() <= 1 {
                continue;
            }
            let rep = scc.iter().min().cloned().unwrap_or_default();
            groups.entry(rep).or_default().push(edge_id.clone());
        }
    }

    let mut ordered_groups: Vec<Vec<String>> = groups.into_values().collect();
    for g in &mut ordered_groups {
        g.sort();
    }
    ordered_groups.sort_by(|a, b| {
        let min_a = a.first().map(String::as_str).unwrap_or("");
        let min_b = b.first().map(String::as_str).unwrap_or("");
        min_a.cmp(min_b)
    });

    for (idx, group) in ordered_groups.iter().enumerate() {
        for edge_id in group {
            if let Some(edge) = edges.get_mut(edge_id) {
                edge.cycle_group = Some(idx);
                diagnostics.push(diag(
                    "cross_project_cycle",
                    "warning",
                    format!("edge participates in cross-project cycle group {idx}"),
                    Some(edge.origin_project_key.clone()),
                    Some(edge_id.clone()),
                    Some(format!("cycle_group={idx}")),
                ));
            }
        }
    }

    ordered_groups.len()
}

fn strongly_connected_components(
    adjacency: &BTreeMap<String, BTreeSet<String>>,
) -> Vec<BTreeSet<String>> {
    let mut nodes: BTreeSet<String> = BTreeSet::new();
    for (k, vs) in adjacency {
        nodes.insert(k.clone());
        nodes.extend(vs.iter().cloned());
    }
    let mut index = 0usize;
    let mut stack: Vec<String> = Vec::new();
    let mut on_stack: HashSet<String> = HashSet::new();
    let mut indices: HashMap<String, usize> = HashMap::new();
    let mut lowlink: HashMap<String, usize> = HashMap::new();
    let mut result: Vec<BTreeSet<String>> = Vec::new();

    #[allow(clippy::too_many_arguments)]
    fn strongconnect(
        v: &str,
        adjacency: &BTreeMap<String, BTreeSet<String>>,
        index: &mut usize,
        stack: &mut Vec<String>,
        on_stack: &mut HashSet<String>,
        indices: &mut HashMap<String, usize>,
        lowlink: &mut HashMap<String, usize>,
        result: &mut Vec<BTreeSet<String>>,
    ) {
        indices.insert(v.to_owned(), *index);
        lowlink.insert(v.to_owned(), *index);
        *index += 1;
        stack.push(v.to_owned());
        on_stack.insert(v.to_owned());

        if let Some(succ) = adjacency.get(v) {
            for w in succ {
                if !indices.contains_key(w) {
                    strongconnect(
                        w, adjacency, index, stack, on_stack, indices, lowlink, result,
                    );
                    let lw = *lowlink.get(w).unwrap_or(&0);
                    let lv = *lowlink.get(v).unwrap_or(&0);
                    lowlink.insert(v.to_owned(), lv.min(lw));
                } else if on_stack.contains(w) {
                    let iw = *indices.get(w).unwrap_or(&0);
                    let lv = *lowlink.get(v).unwrap_or(&0);
                    lowlink.insert(v.to_owned(), lv.min(iw));
                }
            }
        }

        if lowlink.get(v) == indices.get(v) {
            let mut comp = BTreeSet::new();
            while let Some(w) = stack.pop() {
                on_stack.remove(&w);
                comp.insert(w.clone());
                if w == v {
                    break;
                }
            }
            result.push(comp);
        }
    }

    for v in &nodes {
        if !indices.contains_key(v) {
            strongconnect(
                v,
                adjacency,
                &mut index,
                &mut stack,
                &mut on_stack,
                &mut indices,
                &mut lowlink,
                &mut result,
            );
        }
    }
    result
}

fn project_key_from_node_id(node_id: &str) -> Option<String> {
    if let Some(rest) = node_id.strip_prefix("project:") {
        return Some(rest.to_owned());
    }
    for prefix in ["component:", "capability:"] {
        if let Some(rest) = node_id.strip_prefix(prefix) {
            if let Some((pk, _)) = rest.split_once('/') {
                return Some(pk.to_owned());
            }
        }
    }
    None
}

struct ValidatedCatalog {
    value: Value,
    catalog_hash: String,
    component_keys: BTreeSet<String>,
    status_counts: StatusCounts,
    diagnostics: Vec<ViewDiagnostic>,
}

fn validate_catalog(
    catalog: &Value,
    expected_project_key: &str,
    expected_workspace: &str,
    expected_fingerprint: &str,
) -> Result<ValidatedCatalog, String> {
    let obj = catalog
        .as_object()
        .ok_or_else(|| "catalog must be an object".to_owned())?;
    let schema = obj.get("schema").and_then(Value::as_str).unwrap_or("");
    if schema != CATALOG_SCHEMA {
        return Err(format!("unsupported catalog schema `{schema}`"));
    }

    let project_key = obj.get("project_key").and_then(Value::as_str).unwrap_or("");
    if project_key != expected_project_key {
        return Err(format!(
            "catalog project_key `{project_key}` does not match derived `{expected_project_key}`"
        ));
    }

    let source = obj.get("source").and_then(Value::as_object);
    if let Some(source) = source {
        if source.get("canonical_workspace").and_then(Value::as_str) != Some(expected_workspace) {
            return Err("catalog source.canonical_workspace mismatch".to_owned());
        }
        if source.get("workspace_fingerprint").and_then(Value::as_str) != Some(expected_fingerprint)
        {
            return Err("catalog source.workspace_fingerprint mismatch".to_owned());
        }
    } else {
        return Err("catalog missing source".to_owned());
    }

    for required in [
        "generated_at",
        "audit",
        "capabilities",
        "components",
        "dependencies",
        "tests",
        "decisions",
        "cross_graph_links",
        "diagnostics",
        "catalog_hash",
    ] {
        if !obj.contains_key(required) {
            return Err(format!("catalog missing required field `{required}`"));
        }
    }

    let claimed = obj
        .get("catalog_hash")
        .and_then(Value::as_str)
        .unwrap_or("");
    let computed = compute_catalog_hash(catalog).map_err(|e| e.to_string())?;
    if claimed != computed {
        return Err(format!(
            "catalog_hash mismatch: claimed {claimed}, computed {computed}"
        ));
    }

    let mut diagnostics = Vec::new();
    let mut component_keys = BTreeSet::new();
    let mut status_counts = StatusCounts::default();

    let components = obj
        .get("components")
        .and_then(Value::as_array)
        .ok_or_else(|| "components must be an array".to_owned())?;
    let mut prev_key: Option<&str> = None;
    for component in components {
        let key = component
            .get("key")
            .and_then(Value::as_str)
            .ok_or_else(|| "component missing key".to_owned())?;
        if !is_local_key(key) {
            return Err(format!("invalid component key `{key}`"));
        }
        if !component_keys.insert(key.to_owned()) {
            return Err(format!("duplicate component key `{key}`"));
        }
        if let Some(prev) = prev_key {
            if prev > key {
                return Err("components must be sorted by key".to_owned());
            }
        }
        prev_key = Some(key);
        bump_status(
            &mut status_counts,
            component.get("status").and_then(Value::as_str),
        );
    }

    let capabilities = obj
        .get("capabilities")
        .and_then(Value::as_array)
        .ok_or_else(|| "capabilities must be an array".to_owned())?;
    let mut cap_keys = BTreeSet::new();
    prev_key = None;
    for capability in capabilities {
        let key = capability
            .get("key")
            .and_then(Value::as_str)
            .ok_or_else(|| "capability missing key".to_owned())?;
        if !is_local_key(key) {
            return Err(format!("invalid capability key `{key}`"));
        }
        if !cap_keys.insert(key.to_owned()) {
            return Err(format!("duplicate capability key `{key}`"));
        }
        if let Some(prev) = prev_key {
            if prev > key {
                return Err("capabilities must be sorted by key".to_owned());
            }
        }
        prev_key = Some(key);
        bump_status(
            &mut status_counts,
            capability.get("status").and_then(Value::as_str),
        );
        if let Some(refs) = capability.get("component_keys").and_then(Value::as_array) {
            for r in refs {
                let rk = r.as_str().unwrap_or("");
                if !component_keys.contains(rk) {
                    return Err(format!("capability references missing component `{rk}`"));
                }
            }
        }
    }

    let deps = obj
        .get("dependencies")
        .and_then(Value::as_array)
        .ok_or_else(|| "dependencies must be an array".to_owned())?;
    let mut dep_triples = BTreeSet::new();
    for dep in deps {
        let from_c = dep
            .get("from_component")
            .and_then(Value::as_str)
            .unwrap_or("");
        let to_c = dep
            .get("to_component")
            .and_then(Value::as_str)
            .unwrap_or("");
        let kind = dep.get("kind").and_then(Value::as_str).unwrap_or("");
        if !component_keys.contains(from_c) || !component_keys.contains(to_c) {
            return Err(format!(
                "dependency references missing component `{from_c}` -> `{to_c}`"
            ));
        }
        if !DEP_KINDS.contains(&kind) {
            return Err(format!("invalid dependency kind `{kind}`"));
        }
        if !dep_triples.insert((from_c.to_owned(), to_c.to_owned(), kind.to_owned())) {
            return Err("duplicate dependency triple".to_owned());
        }
    }

    let links = obj
        .get("cross_graph_links")
        .and_then(Value::as_array)
        .ok_or_else(|| "cross_graph_links must be an array".to_owned())?;
    let mut link_keys = BTreeSet::new();
    prev_key = None;
    for link in links {
        let key = link
            .get("key")
            .and_then(Value::as_str)
            .ok_or_else(|| "link missing key".to_owned())?;
        if !is_local_key(key) {
            return Err(format!("invalid link key `{key}`"));
        }
        if !link_keys.insert(key.to_owned()) {
            diagnostics.push(diag(
                "duplicate_link_key",
                "warning",
                format!("duplicate cross_graph_links key `{key}`"),
                Some(expected_project_key.to_owned()),
                Some(format!("link:{expected_project_key}/{key}")),
                Some(format!("cross_graph_links[key={key}]")),
            ));
        }
        if let Some(prev) = prev_key {
            if prev > key {
                return Err("cross_graph_links must be sorted by key".to_owned());
            }
        }
        prev_key = Some(key);
        let link_type = link.get("type").and_then(Value::as_str).unwrap_or("");
        if !LINK_TYPES.contains(&link_type) {
            return Err(format!("invalid link type `{link_type}`"));
        }
        let to = link.get("to").ok_or_else(|| "link missing to".to_owned())?;
        let pk = to.get("project_key").and_then(Value::as_str);
        let alias = to.get("alias").and_then(Value::as_str);
        if pk.filter(|s| !s.is_empty()).is_none() && alias.filter(|s| !s.is_empty()).is_none() {
            return Err(format!("link `{key}` requires to.project_key or to.alias"));
        }
    }

    // Secret-key scan over the catalog object.
    reject_secret_keys(catalog).map_err(|e| e.to_string())?;

    Ok(ValidatedCatalog {
        value: catalog.clone(),
        catalog_hash: computed,
        component_keys,
        status_counts,
        diagnostics,
    })
}

fn bump_status(counts: &mut StatusCounts, status: Option<&str>) {
    match status {
        Some("verified") => counts.verified += 1,
        Some("implemented_unverified") => counts.implemented_unverified += 1,
        Some("partial") => counts.partial += 1,
        Some("unknown") => counts.unknown += 1,
        Some(other) if STATUSES.contains(&other) => {}
        _ => counts.unknown += 1,
    }
}

fn load_project_slice(
    canonical_workspace: &str,
    cache: Option<&'static Mutex<FingerprintCache>>,
) -> Result<CachedProjectSlice> {
    let path = Path::new(canonical_workspace).join(PROJECT_RELATIVE);
    if !path.exists() {
        return Ok(CachedProjectSlice {
            fingerprint: FileFingerprint {
                canonical_path: path.to_string_lossy().into_owned(),
                size: 0,
                mtime_nanos: 0,
                graph_hash: String::new(),
                catalog_hash: String::new(),
            },
            catalog_state: "unavailable".to_owned(),
            catalog: None,
            graph_hash: None,
            catalog_hash: None,
            bytes_sha256: empty_sha256(),
            size_bytes: 0,
            load_error: Some("project.fractal missing".to_owned()),
        });
    }

    let meta = fs::metadata(&path).with_context(|| format!("stat {}", path.display()))?;
    let size = meta.len();
    let mtime_nanos = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0);

    // Provisional fingerprint without hashes — used only after load.
    let bytes = fs::read(&path).with_context(|| format!("read {}", path.display()))?;
    let bytes_sha256 = sha256_hex_prefixed(&bytes);

    // Attempt typed load (validates document; never writes).
    let load_result = crate::project_file::load(Path::new(canonical_workspace));
    let (graph_hash, catalog_value, load_error) = match load_result {
        Ok(doc) => {
            let catalog = doc.extra.get("catalog").cloned();
            (Some(doc.graph_hash), catalog, None)
        }
        Err(err) => {
            // Fall back to raw JSON parse for diagnostics / malformed sources.
            let raw: Result<Value, _> = serde_json::from_slice(&bytes);
            match raw {
                Ok(value) => {
                    let graph_hash = value
                        .get("graph_hash")
                        .and_then(Value::as_str)
                        .map(str::to_owned);
                    let catalog = value.get("catalog").cloned();
                    (graph_hash, catalog, Some(err.to_string()))
                }
                Err(parse_err) => (
                    None,
                    None,
                    Some(format!("load failed: {err}; parse failed: {parse_err}")),
                ),
            }
        }
    };

    let (catalog_state, catalog_hash, catalog) = match &catalog_value {
        None => ("missing".to_owned(), None, None),
        Some(c) => {
            let schema = c.get("schema").and_then(Value::as_str).unwrap_or("");
            if schema != CATALOG_SCHEMA {
                (
                    "unsupported_schema".to_owned(),
                    c.get("catalog_hash")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    None,
                )
            } else {
                let ch = c
                    .get("catalog_hash")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                ("valid".to_owned(), ch, Some(c.clone()))
            }
        }
    };

    // If typed load failed, surface as invalid regardless of catalog shape.
    let catalog_state =
        if load_error.is_some() && (catalog_state == "valid" || catalog_value.is_none()) {
            "invalid".to_owned()
        } else {
            catalog_state
        };

    let fingerprint = FileFingerprint {
        canonical_path: path.to_string_lossy().into_owned(),
        size,
        mtime_nanos,
        graph_hash: graph_hash.clone().unwrap_or_default(),
        catalog_hash: catalog_hash.clone().unwrap_or_default(),
    };

    if let Some(cache) = cache {
        if let Ok(mut guard) = cache.lock() {
            if let Some(hit) = guard.get_if_fresh(&fingerprint.canonical_path, &fingerprint) {
                // Re-verify bytes hash so cached data is never authoritative alone.
                if hit.bytes_sha256 == bytes_sha256 && hit.size_bytes == size {
                    return Ok(hit);
                }
                guard.invalidations += 1;
                guard.entries.remove(&fingerprint.canonical_path);
            }
            let slice = CachedProjectSlice {
                fingerprint: fingerprint.clone(),
                catalog_state: catalog_state.clone(),
                catalog: catalog.clone(),
                graph_hash: graph_hash.clone(),
                catalog_hash: catalog_hash.clone(),
                bytes_sha256: bytes_sha256.clone(),
                size_bytes: size,
                load_error: load_error.clone(),
            };
            guard.insert(fingerprint.canonical_path.clone(), slice.clone());
            return Ok(slice);
        }
    }

    Ok(CachedProjectSlice {
        fingerprint,
        catalog_state,
        catalog,
        graph_hash,
        catalog_hash,
        bytes_sha256,
        size_bytes: size,
        load_error,
    })
}

fn insert_node(
    nodes: &mut BTreeMap<String, ViewNode>,
    diagnostics: &mut Vec<ViewDiagnostic>,
    node: ViewNode,
    truncated: &mut bool,
) {
    if nodes.len() >= MAX_VIEW_NODES {
        *truncated = true;
        return;
    }
    if nodes.contains_key(&node.id) {
        diagnostics.push(diag(
            "component_key_collision",
            "warning",
            format!("duplicate node id `{}` dropped", node.id),
            Some(node.project_key.clone()),
            None,
            Some(node.id.clone()),
        ));
        return;
    }
    nodes.insert(node.id.clone(), node);
}

fn insert_edge(edges: &mut BTreeMap<String, ViewEdge>, edge: ViewEdge, truncated: &mut bool) {
    if edges.len() >= MAX_VIEW_EDGES {
        *truncated = true;
        return;
    }
    edges.entry(edge.id.clone()).or_insert(edge);
}

fn diag(
    code: &str,
    severity: &str,
    message: String,
    project_key: Option<String>,
    edge_id: Option<String>,
    context: Option<String>,
) -> ViewDiagnostic {
    ViewDiagnostic {
        code: code.to_owned(),
        severity: severity.to_owned(),
        message,
        project_key,
        edge_id,
        context,
    }
}

fn sort_diagnostics(diagnostics: &mut [ViewDiagnostic]) {
    diagnostics.sort_by(|a, b| {
        (
            a.code.as_str(),
            a.project_key.as_deref(),
            a.edge_id.as_deref(),
            a.context.as_deref(),
        )
            .cmp(&(
                b.code.as_str(),
                b.project_key.as_deref(),
                b.edge_id.as_deref(),
                b.context.as_deref(),
            ))
    });
}

fn count_diagnostics(diagnostics: &[ViewDiagnostic]) -> DiagnosticCounts {
    let mut counts = DiagnosticCounts::default();
    for d in diagnostics {
        match d.severity.as_str() {
            "error" => counts.error += 1,
            "warning" => counts.warning += 1,
            "info" => counts.info += 1,
            _ => {}
        }
    }
    counts
}

pub(crate) fn derive_workspace_fingerprint(canonical_workspace: &str) -> String {
    sha256_hex_prefixed(canonical_workspace.as_bytes())
}

pub(crate) fn derive_project_key(canonical_workspace: &str) -> String {
    let slug = slugify_segment(&path_segment(canonical_workspace), 48);
    let fp = derive_workspace_fingerprint(canonical_workspace);
    let digest = fp.strip_prefix("sha256:").unwrap_or(&fp);
    format!("{}-{}", slug, &digest[..12.min(digest.len())])
}

fn path_segment(canonical_workspace: &str) -> String {
    Path::new(canonical_workspace)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "project".to_owned())
}

fn slugify_segment(segment: &str, max_len: usize) -> String {
    let lower = segment.to_ascii_lowercase();
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

fn is_local_key(key: &str) -> bool {
    let bytes = key.as_bytes();
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
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
}

fn looks_like_bare_hex(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit())
}

fn sha256_hex_prefixed(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(71);
    out.push_str("sha256:");
    for byte in digest {
        out.push(HEX[(byte >> 4) as usize]);
        out.push(HEX[(byte & 0x0f) as usize]);
    }
    out
}

fn empty_sha256() -> String {
    sha256_hex_prefixed(b"")
}

const HEX: &[char] = &[
    '0', '1', '2', '3', '4', '5', '6', '7', '8', '9', 'a', 'b', 'c', 'd', 'e', 'f',
];

fn compute_catalog_hash(catalog: &Value) -> Result<String> {
    let mut value = catalog.clone();
    if let Some(obj) = value.as_object_mut() {
        obj.remove("catalog_hash");
    }
    fractal_contracts::canonical_sha256(&value).map_err(|e| anyhow!("catalog hash failed: {e}"))
}

fn compute_view_hash(view: &MasterGraphView) -> Result<String> {
    let mut value = serde_json::to_value(view).context("encode master view")?;
    if let Some(obj) = value.as_object_mut() {
        obj.remove("view_hash");
    }
    fractal_contracts::canonical_sha256(&value).map_err(|e| anyhow!("view hash failed: {e}"))
}

fn reject_secret_keys(value: &Value) -> Result<()> {
    match value {
        Value::Object(map) => {
            for (key, child) in map {
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
                    bail!("catalog contains forbidden credential field `{key}`");
                }
                reject_secret_keys(child)?;
            }
        }
        Value::Array(items) => {
            for child in items {
                reject_secret_keys(child)?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// Build N synthetic fixtures and compose them. Used as a bounded benchmark helper.
#[cfg(test)]
pub(crate) fn benchmark_compose_fixtures(count: usize) -> Result<BenchmarkReport> {
    let root = std::env::temp_dir().join(format!(
        "fractal-master-graph-bench-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    fs::create_dir_all(&root)?;
    let mut records = Vec::with_capacity(count);
    for i in 0..count {
        let workspace = root.join(format!("proj-{i:04}"));
        write_fixture_project(&workspace, &format!("proj-{i:04}"), None, &[])?;
        let canonical = workspace
            .canonicalize()
            .unwrap_or(workspace)
            .to_string_lossy()
            .into_owned();
        records.push(InventoryRecord {
            canonical_workspace: canonical,
            exists: true,
            labels: vec![format!("proj-{i:04}")],
            registry_numbers: vec![(i as u64) + 1],
            unavailable_reason: None,
            git: None,
            project_fractal: Some(InventoryProjectFractal {
                available: true,
                relative_path: Some(PROJECT_RELATIVE.to_owned()),
                size_bytes: None,
                unavailable_reason: None,
            }),
            extra: BTreeMap::new(),
        });
    }
    records.sort_by(|a, b| a.canonical_workspace.cmp(&b.canonical_workspace));
    let inventory = RepositoryInventory {
        schema: INVENTORY_SCHEMA.to_owned(),
        inventory_hash: sha256_hex_prefixed(b"benchmark-inventory"),
        records,
        extra: BTreeMap::new(),
    };
    let cache: &'static Mutex<FingerprintCache> =
        Box::leak(Box::new(Mutex::new(FingerprintCache::new())));

    let cold_started = Instant::now();
    let cold_view = compose_view(&inventory, Some(cache))?;
    let cold_elapsed = cold_started.elapsed();

    let warm_started = Instant::now();
    let warm_view = compose_view(&inventory, Some(cache))?;
    let warm_elapsed = warm_started.elapsed();
    let (warm_hits, warm_misses) = {
        let guard = cache
            .lock()
            .map_err(|_| anyhow!("benchmark cache poisoned"))?;
        (guard.hits(), guard.misses())
    };

    let mutated_workspace = Path::new(&inventory.records[0].canonical_workspace);
    std::thread::sleep(Duration::from_millis(20));
    write_fixture_project(
        mutated_workspace,
        "proj-0000",
        None,
        &[("changed", "changed")],
    )?;
    let invalidation_started = Instant::now();
    let invalidated_view = compose_view(&inventory, Some(cache))?;
    let invalidation_elapsed = invalidation_started.elapsed();
    let (final_hits, final_misses, final_invalidations) = {
        let guard = cache
            .lock()
            .map_err(|_| anyhow!("benchmark cache poisoned"))?;
        (guard.hits(), guard.misses(), guard.invalidations())
    };

    let _ = fs::remove_dir_all(&root);
    Ok(BenchmarkReport {
        fixture_count: count,
        node_count: cold_view.nodes.len(),
        edge_count: cold_view.edges.len(),
        cold_elapsed,
        warm_elapsed,
        invalidation_elapsed,
        warm_hits,
        warm_misses,
        invalidation_hits: final_hits.saturating_sub(warm_hits),
        reloaded_after_change: final_misses.saturating_sub(warm_misses),
        invalidations_after_change: final_invalidations,
        cold_view_hash: cold_view.view_hash,
        warm_view_hash: warm_view.view_hash,
        invalidated_view_hash: invalidated_view.view_hash,
    })
}

#[derive(Clone, Debug)]
#[cfg(test)]
pub(crate) struct BenchmarkReport {
    pub(crate) fixture_count: usize,
    pub(crate) node_count: usize,
    pub(crate) edge_count: usize,
    pub(crate) cold_elapsed: Duration,
    pub(crate) warm_elapsed: Duration,
    pub(crate) invalidation_elapsed: Duration,
    pub(crate) warm_hits: u64,
    pub(crate) warm_misses: u64,
    pub(crate) invalidation_hits: u64,
    pub(crate) reloaded_after_change: u64,
    pub(crate) invalidations_after_change: u64,
    pub(crate) cold_view_hash: String,
    pub(crate) warm_view_hash: String,
    pub(crate) invalidated_view_hash: String,
}

#[cfg(test)]
fn write_fixture_project(
    workspace: &Path,
    label: &str,
    links: Option<Value>,
    components: &[(&str, &str)],
) -> Result<()> {
    fs::create_dir_all(workspace.join(".fractal"))?;
    let canonical = fs::canonicalize(workspace)
        .with_context(|| format!("canonicalize {}", workspace.display()))?
        .to_string_lossy()
        .into_owned();

    let mut graph = json!({
        "schema": "fractal.execution_graph.v1",
        "nodes": [],
        "edges": []
    });
    let graph_hash =
        fractal_contracts::canonical_sha256(&graph).map_err(|e| anyhow!("hash graph: {e}"))?;
    graph
        .as_object_mut()
        .unwrap()
        .insert("graph_hash".to_owned(), Value::String(graph_hash.clone()));

    let project_key = derive_project_key(&canonical);
    let workspace_fingerprint = derive_workspace_fingerprint(&canonical);

    let mut component_values = Vec::new();
    if components.is_empty() {
        component_values.push(json!({
            "key": "main",
            "name": label,
            "kind": "binary",
            "paths": ["src"],
            "status": "implemented_unverified",
            "evidence": [{
                "path": "Cargo.toml",
                "sha256": "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "kind": "manifest",
                "observed_commit": null
            }]
        }));
    } else {
        for (key, name) in components {
            component_values.push(json!({
                "key": key,
                "name": name,
                "kind": "library",
                "paths": [format!("crates/{key}")],
                "status": "implemented_unverified",
                "evidence": [{
                    "path": format!("crates/{key}/Cargo.toml"),
                    "sha256": "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                    "kind": "manifest",
                    "observed_commit": null
                }]
            }));
        }
        component_values.sort_by(|a, b| {
            a.get("key")
                .and_then(Value::as_str)
                .cmp(&b.get("key").and_then(Value::as_str))
        });
    }

    let mut cross_graph_links = links.unwrap_or_else(|| Value::Array(vec![]));
    if let Some(arr) = cross_graph_links.as_array_mut() {
        arr.sort_by(|a, b| {
            a.get("key")
                .and_then(Value::as_str)
                .cmp(&b.get("key").and_then(Value::as_str))
        });
    }
    let mut catalog = json!({
        "schema": CATALOG_SCHEMA,
        "project_key": project_key,
        "generated_at": "2026-08-02T00:00:00Z",
        "source": {
            "canonical_workspace": canonical,
            "workspace_fingerprint": workspace_fingerprint,
            "registry_numbers": [1],
            "labels": [label],
            "git": {
                "is_git_repository": false,
                "commit": null,
                "dirty": null,
                "dirty_fingerprint": null,
                "unavailable_reason": "not_a_git_repository",
                "remotes": []
            }
        },
        "audit": {
            "auditor": "fractal graph audit",
            "inventory_hash": "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "started_at": "2026-08-02T00:00:00Z",
            "finished_at": "2026-08-02T00:00:01Z",
            "bounds": {
                "max_catalog_bytes": 262144,
                "max_evidence_per_claim": 20,
                "max_log_excerpt_chars": 1024,
                "max_string_chars": 2048,
                "test_timeout_ms": 600000
            },
            "truncated": false
        },
        "capabilities": [],
        "components": component_values,
        "dependencies": [],
        "tests": [],
        "decisions": [],
        "cross_graph_links": cross_graph_links,
        "diagnostics": []
    });
    catalog
        .as_object_mut()
        .expect("fixture catalog object")
        .insert("catalog_hash".to_owned(), Value::String(String::new()));
    let dirty_fingerprint = {
        let typed: crate::project_file::project_catalog::CatalogV1 =
            serde_json::from_value(catalog.clone())?;
        crate::project_file::project_catalog::compute_dirty_fingerprint(&typed)
            .map_err(|error| anyhow!("compute fixture dirty fingerprint: {error}"))?
    };
    catalog
        .pointer_mut("/source/git/dirty_fingerprint")
        .expect("fixture git object")
        .clone_from(&Value::String(dirty_fingerprint));
    let catalog_hash = compute_catalog_hash(&catalog)?;
    catalog
        .as_object_mut()
        .unwrap()
        .insert("catalog_hash".to_owned(), Value::String(catalog_hash));

    let document = json!({
        "schema": "fractal.project.v1",
        "project": {
            "slug": slugify_segment(label, 48),
            "title": label,
            "visibility": "private"
        },
        "graph_hash": graph_hash,
        "graph": graph,
        "learning": {
            "schema": "fractal.learning.v1",
            "nodes": {},
            "graph_edits": []
        },
        "updated_at": "2026-08-02T00:00:00Z",
        "catalog": catalog
    });

    fs::write(
        workspace.join(PROJECT_RELATIVE),
        serde_json::to_vec_pretty(&document)?,
    )?;
    Ok(())
}

#[cfg(test)]
fn write_inventory(path: &Path, mut records: Vec<InventoryRecord>) -> Result<RepositoryInventory> {
    records.sort_by(|a, b| a.canonical_workspace.cmp(&b.canonical_workspace));
    let mut inventory = RepositoryInventory {
        schema: INVENTORY_SCHEMA.to_owned(),
        inventory_hash: String::new(),
        records,
        extra: BTreeMap::new(),
    };
    let mut for_hash = serde_json::to_value(&inventory)?;
    if let Some(obj) = for_hash.as_object_mut() {
        obj.remove("inventory_hash");
    }
    inventory.inventory_hash = fractal_contracts::canonical_sha256(&for_hash)
        .map_err(|e| anyhow!("inventory hash: {e}"))?;
    fs::write(path, serde_json::to_vec_pretty(&inventory)?)?;
    Ok(inventory)
}

#[cfg(test)]
fn record_for(workspace: &Path, label: &str, number: u64, exists: bool) -> InventoryRecord {
    let canonical = if exists {
        workspace
            .canonicalize()
            .unwrap_or_else(|_| workspace.to_path_buf())
            .to_string_lossy()
            .into_owned()
    } else {
        workspace.to_string_lossy().into_owned()
    };
    InventoryRecord {
        canonical_workspace: canonical,
        exists,
        labels: vec![label.to_owned()],
        registry_numbers: vec![number],
        unavailable_reason: (!exists).then(|| "workspace_path_does_not_exist".to_owned()),
        git: Some(InventoryGit {
            is_git_repository: Some(false),
            head: None,
            dirty: None,
            unavailable_reason: Some("not_a_git_repository".to_owned()),
            remotes: vec![],
        }),
        project_fractal: Some(InventoryProjectFractal {
            available: exists,
            relative_path: Some(PROJECT_RELATIVE.to_owned()),
            size_bytes: None,
            unavailable_reason: (!exists).then(|| "missing".to_owned()),
        }),
        extra: BTreeMap::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(label: &str) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!(
            "fractal-mg-{}-{}-{}",
            label,
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn reordered_inventory_yields_identical_view_hash() {
        let root = temp_root("reorder");
        let a = root.join("alpha-app");
        let b = root.join("beta-app");
        write_fixture_project(&a, "alpha-app", None, &[]).unwrap();
        write_fixture_project(&b, "beta-app", None, &[]).unwrap();

        let rec_a = record_for(&a, "alpha-app", 1, true);
        let rec_b = record_for(&b, "beta-app", 2, true);

        let inv_path = root.join("inv.json");
        let inventory = write_inventory(&inv_path, vec![rec_a.clone(), rec_b.clone()]).unwrap();
        let view1 = compose_view(&inventory, None).unwrap();

        let mut reversed = inventory.clone();
        reversed.records.reverse();
        let view2 = compose_view(&reversed, None).unwrap();

        assert_eq!(view1.view_hash, view2.view_hash);
        assert_eq!(
            serde_json::to_value(&view1).unwrap(),
            serde_json::to_value(&view2).unwrap()
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn duplicate_slugs_remain_distinct_project_keys() {
        let root = temp_root("dup-slug");
        let a = root.join("My App");
        let b = root.join("my-app");
        fs::create_dir_all(&a).unwrap();
        fs::create_dir_all(&b).unwrap();
        write_fixture_project(&a, "My App", None, &[]).unwrap();
        write_fixture_project(&b, "my-app", None, &[]).unwrap();

        let inventory = write_inventory(
            &root.join("inv.json"),
            vec![
                record_for(&a, "My App", 1, true),
                record_for(&b, "my-app", 2, true),
            ],
        )
        .unwrap();
        let view = compose_view(&inventory, None).unwrap();
        let keys: BTreeSet<_> = view
            .projects
            .iter()
            .map(|p| p.project_key.clone())
            .collect();
        assert_eq!(
            keys.len(),
            2,
            "slug collision must be disambiguated by fingerprint"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn alias_resolution_and_ambiguous_alias() {
        let root = temp_root("alias");
        let a = root.join("proj-a");
        let b = root.join("proj-b");
        let shared_label = "shared-label";

        let b_canonical_early = {
            fs::create_dir_all(&b).unwrap();
            write_fixture_project(&b, "proj-b", None, &[("lib-b", "lib-b")]).unwrap();
            fs::canonicalize(&b).unwrap().to_string_lossy().into_owned()
        };
        let b_key = derive_project_key(&b_canonical_early);

        let links = json!([{
            "key": "uses-b",
            "type": "depends_on",
            "from": {"component_key": null},
            "to": {"project_key": null, "alias": "proj-b", "component_key": null},
            "confidence": "high",
            "evidence": []
        }]);
        write_fixture_project(&a, "proj-a", Some(links), &[]).unwrap();

        let mut rec_a = record_for(&a, "proj-a", 1, true);
        rec_a.labels = vec!["proj-a".to_owned(), shared_label.to_owned()];
        let mut rec_b = record_for(&b, "proj-b", 2, true);
        rec_b.labels = vec!["proj-b".to_owned(), shared_label.to_owned()];

        let inventory =
            write_inventory(&root.join("inv.json"), vec![rec_a.clone(), rec_b.clone()]).unwrap();
        let view = compose_view(&inventory, None).unwrap();

        let resolved = view
            .edges
            .iter()
            .find(|e| e.id.contains("uses-b"))
            .expect("link edge");
        assert_eq!(resolved.resolution, "resolved");
        assert!(resolved
            .to
            .node_id
            .as_deref()
            .unwrap_or("")
            .contains(&b_key));

        assert!(
            view.diagnostics.iter().any(|d| d.code == "duplicate_alias"),
            "shared label must produce duplicate_alias"
        );

        // Ambiguous: alias equals shared label
        let links_ambiguous = json!([{
            "key": "ambig",
            "type": "related_to",
            "from": {"component_key": null},
            "to": {"project_key": null, "alias": shared_label, "component_key": null},
            "confidence": "low",
            "evidence": []
        }]);
        write_fixture_project(&a, "proj-a", Some(links_ambiguous), &[]).unwrap();
        let inventory = write_inventory(&root.join("inv.json"), vec![rec_a, rec_b]).unwrap();
        let view = compose_view(&inventory, None).unwrap();
        let edge = view.edges.iter().find(|e| e.id.contains("ambig")).unwrap();
        assert_eq!(edge.resolution, "ambiguous");
        assert!(edge.to.node_id.is_none());
        assert!(view.diagnostics.iter().any(|d| d.code == "ambiguous_alias"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn unavailable_and_malformed_sources() {
        let root = temp_root("unavail");
        let good = root.join("good");
        let missing = root.join("missing-workspace");
        let malformed = root.join("malformed");
        write_fixture_project(&good, "good", None, &[]).unwrap();
        fs::create_dir_all(malformed.join(".fractal")).unwrap();
        fs::write(malformed.join(PROJECT_RELATIVE), b"{not-json").unwrap();

        let inventory = write_inventory(
            &root.join("inv.json"),
            vec![
                record_for(&good, "good", 1, true),
                record_for(&missing, "missing", 2, false),
                record_for(&malformed, "malformed", 3, true),
            ],
        )
        .unwrap();
        let view = compose_view(&inventory, None).unwrap();
        assert!(view
            .unavailable
            .iter()
            .any(|u| u.canonical_workspace.contains("missing-workspace")));
        assert!(view
            .diagnostics
            .iter()
            .any(|d| d.code == "unavailable_workspace"));
        assert!(view
            .diagnostics
            .iter()
            .any(|d| d.code == "invalid_project_document"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn missing_targets_self_links_and_cycles() {
        let root = temp_root("links");
        let a = root.join("cycle-a");
        let b = root.join("cycle-b");
        fs::create_dir_all(&a).unwrap();
        fs::create_dir_all(&b).unwrap();

        // Need B's key for A's link and vice versa — create B first.
        write_fixture_project(&b, "cycle-b", None, &[]).unwrap();
        let b_key = derive_project_key(&fs::canonicalize(&b).unwrap().to_string_lossy());
        write_fixture_project(&a, "cycle-a", None, &[]).unwrap();
        let a_key = derive_project_key(&fs::canonicalize(&a).unwrap().to_string_lossy());

        let a_links = json!([
            {
                "key": "to-b",
                "type": "depends_on",
                "from": {"component_key": null},
                "to": {"project_key": b_key, "alias": null, "component_key": null},
                "confidence": "high",
                "evidence": []
            },
            {
                "key": "missing-target",
                "type": "related_to",
                "from": {"component_key": null},
                "to": {"project_key": null, "alias": "no-such-project", "component_key": null},
                "confidence": "low",
                "evidence": []
            },
            {
                "key": "selfish",
                "type": "related_to",
                "from": {"component_key": null},
                "to": {"project_key": a_key, "alias": null, "component_key": null},
                "confidence": "medium",
                "evidence": []
            }
        ]);
        let b_links = json!([{
            "key": "to-a",
            "type": "depends_on",
            "from": {"component_key": null},
            "to": {"project_key": a_key, "alias": null, "component_key": null},
            "confidence": "high",
            "evidence": []
        }]);
        write_fixture_project(&a, "cycle-a", Some(a_links), &[]).unwrap();
        write_fixture_project(&b, "cycle-b", Some(b_links), &[]).unwrap();

        let inventory = write_inventory(
            &root.join("inv.json"),
            vec![
                record_for(&a, "cycle-a", 1, true),
                record_for(&b, "cycle-b", 2, true),
            ],
        )
        .unwrap();
        let view = compose_view(&inventory, None).unwrap();

        assert!(view.edges.iter().any(|e| e.resolution == "unresolved"));
        assert!(view
            .diagnostics
            .iter()
            .any(|d| d.code == "unresolved_link_target"));
        assert!(view.edges.iter().any(|e| e.resolution == "self"));
        assert!(view.diagnostics.iter().any(|d| d.code == "self_link"));
        assert!(view.summary.cycle_count >= 1);
        assert!(view
            .edges
            .iter()
            .any(|e| e.cycle_group.is_some() && e.resolution == "resolved"));
        assert!(view
            .diagnostics
            .iter()
            .any(|d| d.code == "cross_project_cycle"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn component_key_collision_diagnostic() {
        // Force a collision by inserting two nodes with the same id via duplicate
        // component keys that somehow pass — validate_catalog rejects duplicates,
        // so we exercise insert_node collision by composing a handcrafted catalog
        // that skips re-validation path: use two capability/component with same
        // namespaced id through a catalog that validates (impossible for true dups).
        // Instead verify project_key_collision exclusion path with identical workspace
        // fingerprints is unreachable; exercise insert_node directly.
        let mut nodes = BTreeMap::new();
        let mut diagnostics = Vec::new();
        let mut truncated = false;
        let node = ViewNode {
            id: "component:demo-aaaaaaaaaaaa/main".to_owned(),
            kind: "component".to_owned(),
            project_key: "demo-aaaaaaaaaaaa".to_owned(),
            key: "main".to_owned(),
            title: "main".to_owned(),
            status: None,
            component_kind: None,
        };
        insert_node(&mut nodes, &mut diagnostics, node.clone(), &mut truncated);
        insert_node(&mut nodes, &mut diagnostics, node, &mut truncated);
        assert_eq!(nodes.len(), 1);
        assert!(diagnostics
            .iter()
            .any(|d| d.code == "component_key_collision"));
    }

    #[test]
    fn cache_hits_and_invalidation_on_source_mutation() {
        let cache = shared_cache();
        {
            let mut g = cache.lock().unwrap();
            g.clear();
            g.hits = 0;
            g.misses = 0;
            g.invalidations = 0;
        }

        let root = temp_root("cache");
        let a = root.join("cached");
        write_fixture_project(&a, "cached", None, &[]).unwrap();
        let inventory = write_inventory(
            &root.join("inv.json"),
            vec![record_for(&a, "cached", 1, true)],
        )
        .unwrap();

        let view1 = compose_view(&inventory, Some(cache)).unwrap();
        let view2 = compose_view(&inventory, Some(cache)).unwrap();
        assert_eq!(view1.view_hash, view2.view_hash);
        {
            let g = cache.lock().unwrap();
            assert!(g.hits() >= 1, "second compose should hit cache");
        }

        // Mutate source bytes — cache must invalidate and provenance must change.
        let old_hash = view1.sources[0].project_fractal_sha256.clone();
        std::thread::sleep(Duration::from_millis(20));
        write_fixture_project(
            &a,
            "cached",
            Some(json!([])),
            &[("main", "cached"), ("extra", "extra")],
        )
        .unwrap();
        let view3 = compose_view(&inventory, Some(cache)).unwrap();
        assert_ne!(old_hash, view3.sources[0].project_fractal_sha256);
        {
            let g = cache.lock().unwrap();
            assert!(g.invalidations() >= 1 || g.misses() >= 2);
        }
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn source_mutation_changes_provenance_without_writing_inventory_sources() {
        let root = temp_root("mutate");
        let a = root.join("srcproj");
        write_fixture_project(&a, "srcproj", None, &[]).unwrap();
        let inv_path = root.join("inv.json");
        let inventory =
            write_inventory(&inv_path, vec![record_for(&a, "srcproj", 1, true)]).unwrap();
        let before = fs::read(&inv_path).unwrap();
        let view1 = compose_view(&inventory, None).unwrap();
        let hash1 = view1.sources[0].project_fractal_sha256.clone();

        // Touch only the project document; inventory artifact must remain byte-identical.
        write_fixture_project(&a, "srcproj", Some(json!([])), &[("main", "renamed")]).unwrap();
        let view2 = compose_view(&inventory, None).unwrap();
        assert_ne!(hash1, view2.sources[0].project_fractal_sha256);
        assert_eq!(before, fs::read(&inv_path).unwrap());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn validate_only_output_omits_full_graph_payload() {
        let root = temp_root("validate");
        let a = root.join("vonly");
        write_fixture_project(&a, "vonly", None, &[]).unwrap();
        let inventory = write_inventory(
            &root.join("inv.json"),
            vec![record_for(&a, "vonly", 1, true)],
        )
        .unwrap();
        let result = compose_inventory(
            &inventory,
            ComposeOptions {
                validate_only: true,
                cache: None,
            },
        )
        .unwrap();
        match result {
            ComposeResult::ValidateOnly(out) => {
                assert_eq!(out.schema, VIEW_SCHEMA);
                assert!(!out.view_hash.is_empty());
                assert_eq!(out.summary.available_inventory_count, 1);
            }
            ComposeResult::View(_) => panic!("expected validate-only"),
        }
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn benchmark_helper_builds_500_fixtures() {
        let report = benchmark_compose_fixtures(500).expect("benchmark");
        assert_eq!(report.fixture_count, 500);
        assert!(report.node_count >= 500);
        assert_eq!(report.cold_view_hash, report.warm_view_hash);
        assert_ne!(report.warm_view_hash, report.invalidated_view_hash);
        assert!(report.cold_elapsed < Duration::from_secs(2));
        assert!(report.warm_elapsed < Duration::from_millis(500));
        assert_eq!(report.warm_hits, 500);
        assert_eq!(report.warm_misses, 500);
        assert_eq!(report.invalidation_hits, 499);
        assert_eq!(report.reloaded_after_change, 1);
        assert_eq!(report.invalidations_after_change, 1);
        eprintln!(
            "master_graph 500-fixture benchmark: cold={:?} warm={:?} invalidation={:?} nodes={} edges={} warm_hits={} reloaded={} invalidations={}",
            report.cold_elapsed,
            report.warm_elapsed,
            report.invalidation_elapsed,
            report.node_count,
            report.edge_count,
            report.warm_hits,
            report.reloaded_after_change,
            report.invalidations_after_change,
        );
    }

    #[test]
    fn project_key_matches_contract_examples() {
        assert_eq!(
            derive_project_key("/workspace/fractal-cli"),
            "fractal-cli-3c8b9dde9efc"
        );
        assert_eq!(
            derive_project_key("/workspace/fractal-efficiency"),
            "fractal-efficiency-5793dcf94336"
        );
    }
}
