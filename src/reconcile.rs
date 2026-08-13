//! Read-only reconciliation of the six-repository master graph.
//!
//! This module deliberately operates on frozen JSON evidence. It never calls
//! the registry, sync, checkout, fetch, or any project-file writer. The only
//! write performed by the command wrapper is the output path explicitly chosen
//! by the caller.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

const SCHEMA: &str = "fractal.graph_reconcile.v1";
const EXPECTED: &[&str] = &[
    "fractalmaster",
    "fractal-cli",
    "fractalchain",
    "FractalRuntime",
    "Fractalwork",
    "fractalsociety-website",
];

#[derive(Clone, Debug)]
pub(crate) struct ReconcileOptions {
    pub(crate) inventory: PathBuf,
    pub(crate) audits: Vec<PathBuf>,
    pub(crate) baseline: Option<PathBuf>,
    pub(crate) output: Option<PathBuf>,
}

#[derive(Clone, Debug)]
struct Identity {
    repository: String,
    root: String,
    declared: Option<String>,
    remote: Option<String>,
    claims: Vec<String>,
}

#[derive(Clone, Debug)]
struct Evidence {
    identity: Identity,
    audit: Option<Value>,
    hashes: BTreeMap<String, String>,
    nodes: BTreeSet<String>,
    interfaces: BTreeSet<String>,
}

#[derive(Clone, Debug)]
struct Finding {
    repository: String,
    reason_code: String,
    expected: Option<String>,
    observed: Option<String>,
    message: String,
    command: String,
    preconditions: Vec<String>,
    safety: String,
    blocked: bool,
    unresolved: bool,
}

type SourceSets = (BTreeMap<String, String>, BTreeSet<String>, BTreeSet<String>);

pub(crate) fn run(options: &ReconcileOptions) -> Result<()> {
    let result = match reconcile(options) {
        Ok(result) => result,
        Err(error) => {
            let finding = finding(
                "master-graph",
                "schema_error",
                Some(SCHEMA),
                Some(&error.to_string()),
                "reconciliation evidence could not be decoded or validated",
            );
            let remediation_plan = vec![remediation(&finding)];
            let mut repositories: Vec<_> =
                EXPECTED.iter().map(|name| normalize_key(name)).collect();
            repositories.sort();
            json!({
                "schema": SCHEMA,
                "reconciled": false,
                "repositories": repositories,
                "hashes": {},
                "composed_view_hash": Value::Null,
                "remediation_plan": remediation_plan,
                "summary": {
                    "repository_count": 0,
                    "expected_count": EXPECTED.len(),
                    "finding_count": 1,
                    "blocked_count": 1,
                    "unresolved_count": 1
                }
            })
        }
    };
    let bytes = serde_json::to_vec_pretty(&result)?;
    if let Some(path) = &options.output {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            fs::create_dir_all(parent).with_context(|| {
                format!(
                    "create reconciliation output directory {}",
                    parent.display()
                )
            })?;
        }
        fs::write(path, &bytes)
            .with_context(|| format!("write reconciliation output {}", path.display()))?;
    } else {
        println!("{}", String::from_utf8_lossy(&bytes));
    }
    if result
        .get("reconciled")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        Ok(())
    } else {
        bail!("graph reconciliation has unresolved findings")
    }
}

fn reconcile(options: &ReconcileOptions) -> Result<Value> {
    let inventory_bytes = fs::read(&options.inventory)
        .with_context(|| format!("read frozen inventory {}", options.inventory.display()))?;
    let inventory_raw: Value =
        serde_json::from_slice(&inventory_bytes).context("decode frozen inventory JSON")?;
    let inventory = crate::master_graph::load_inventory(&options.inventory)
        .context("reconcile requires fractal.repository_inventory.v1 inventory")?;
    let inventory_hash = canonical_hash_without(&inventory_raw, &["inventory_hash"])?;
    let audit_values = load_audits(&options.audits)?;
    let mut findings = Vec::new();
    let mut by_repo: BTreeMap<String, Evidence> = BTreeMap::new();
    let mut identities: BTreeMap<String, Vec<String>> = BTreeMap::new();

    for record in &inventory.records {
        let identity = resolve_identity(
            &record.canonical_workspace,
            record.labels.iter().map(String::as_str),
            &record.extra,
            record.git.as_ref().and_then(|g| {
                g.remotes
                    .iter()
                    .filter_map(|r| r.sanitized_url.as_deref())
                    .next()
            }),
        );
        let key = identity.repository.clone();
        identities
            .entry(normalize_key(&key))
            .or_default()
            .push(record.canonical_workspace.clone());
        if key == "unexpected" {
            findings.push(finding(
                &key,
                "unexpected_repository",
                None,
                Some(&record.canonical_workspace),
                "inventory contains a repository outside the six-repository contract",
            ));
        }
        if !record.exists
            || record
                .project_fractal
                .as_ref()
                .is_some_and(|p| !p.available)
        {
            findings.push(finding(
                &key,
                "unavailable_evidence",
                Some("available"),
                record.unavailable_reason.as_deref(),
                "inventory evidence marks the repository or project graph unavailable",
            ));
        }
        let audit = find_audit(&identity, &audit_values);
        if audit.is_none() {
            findings.push(finding(
                &key,
                "missing_evidence",
                Some("current audit"),
                None,
                "no current project-audit evidence matches the inventory identity",
            ));
        }
        let (hashes, nodes, interfaces) = collect_hashes(&identity.root, audit.as_ref())?;
        let mut hashes = hashes;
        for field in ["audit", "project_graph", "nodes", "links", "interfaces"] {
            let claimed_key = format!("{field}_claimed");
            if let (Some(expected), Some(observed)) = (hashes.get(&claimed_key), hashes.get(field))
            {
                if expected != observed {
                    findings.push(finding(
                        &key,
                        &format!("{field}_hash_drift"),
                        Some(expected),
                        Some(observed),
                        "audit evidence hash does not match canonical source content",
                    ));
                }
            }
            hashes.remove(&claimed_key);
        }
        by_repo.insert(
            key,
            Evidence {
                identity,
                audit,
                hashes,
                nodes,
                interfaces,
            },
        );
    }

    for (normalized, roots) in &identities {
        if roots.len() > 1 {
            let repo = by_repo
                .values()
                .find(|e| normalize_key(&e.identity.repository) == *normalized)
                .map(|e| e.identity.repository.clone())
                .unwrap_or_else(|| normalized.clone());
            findings.push(finding(
                &repo,
                "duplicate_identity",
                Some(&roots[0]),
                Some(&roots.join(",")),
                "multiple inventory records resolve to one repository identity",
            ));
            let folded: BTreeSet<_> = roots.iter().map(|root| root.to_ascii_lowercase()).collect();
            if folded.len() == 1 {
                findings.push(finding(
                    &repo,
                    "case_collision",
                    Some(&roots[0]),
                    Some(&roots.join(",")),
                    "repository roots collide after case normalization",
                ));
            }
        }
    }
    for expected in EXPECTED {
        let matches: Vec<_> = by_repo
            .values()
            .filter(|e| normalize_key(&e.identity.repository) == normalize_key(expected))
            .collect();
        if matches.is_empty() {
            findings.push(finding(
                expected,
                "missing_evidence",
                Some("inventory and current audit"),
                None,
                "required repository is absent from frozen evidence",
            ));
        } else if matches.len() > 1 {
            findings.push(finding(
                expected,
                "ambiguous_alias",
                Some(expected),
                Some(
                    &matches
                        .iter()
                        .map(|e| e.identity.root.clone())
                        .collect::<Vec<_>>()
                        .join(","),
                ),
                "an alias resolves to more than one repository",
            ));
        }
    }
    for evidence in by_repo.values() {
        if evidence.identity.claims.len() > 1 {
            let mapped_claims: Vec<_> = evidence
                .identity
                .claims
                .iter()
                .filter_map(|claim| expected_name(claim).map(|expected| (claim, expected)))
                .collect();
            let claims: BTreeSet<_> = mapped_claims.iter().map(|(_, expected)| expected).collect();
            if claims.len() > 1 {
                findings.push(finding(
                    &evidence.identity.repository,
                    "identity_conflict",
                    Some(&evidence.identity.repository),
                    Some(&evidence.identity.claims.join(",")),
                    "canonical root, declared key, and remote identity disagree",
                ));
            } else if mapped_claims
                .iter()
                .map(|(claim, _)| claim.to_ascii_lowercase())
                .collect::<BTreeSet<_>>()
                .len()
                == 1
                && mapped_claims
                    .iter()
                    .map(|(claim, _)| claim.to_string())
                    .collect::<BTreeSet<_>>()
                    .len()
                    > 1
            {
                findings.push(finding(
                    &evidence.identity.repository,
                    "case_collision",
                    Some(&evidence.identity.repository),
                    Some(&evidence.identity.claims.join(",")),
                    "identity claims differ only by case",
                ));
            }
        }
    }

    let baseline = options
        .baseline
        .as_ref()
        .map(|path| read_json(path, "baseline reconciliation evidence"))
        .transpose()?;
    compare_one_baseline(
        &mut findings,
        "master-graph",
        "inventory",
        &inventory_hash,
        baseline.as_ref(),
    );
    let mut hashes = BTreeMap::new();
    hashes.insert(
        "inventory".to_owned(),
        Value::String(inventory_hash.clone()),
    );
    for (repo, evidence) in &by_repo {
        let repo_hashes = json!({
            "identity": identity_hash(&evidence.identity)?,
            "inventory": inventory_hash,
            "audit": evidence.hashes.get("audit"),
            "project_graph": evidence.hashes.get("project_graph"),
            "nodes": evidence.hashes.get("nodes"),
            "links": evidence.hashes.get("links"),
            "interfaces": evidence.hashes.get("interfaces"),
        });
        hashes.insert(repo.clone(), repo_hashes);
        compare_baseline(&mut findings, repo, &evidence.hashes, baseline.as_ref());
        scan_links(&mut findings, repo, evidence, &by_repo);
    }

    let composed_view_hash = match crate::master_graph::compose_inventory(
        &inventory,
        crate::master_graph::ComposeOptions {
            validate_only: false,
            cache: None,
        },
    ) {
        Ok(crate::master_graph::ComposeResult::View(view)) => Some(view.view_hash),
        Ok(crate::master_graph::ComposeResult::ValidateOnly(view)) => Some(view.view_hash),
        Err(error) => {
            findings.push(finding(
                "master-graph",
                "schema_error",
                Some("composable master graph"),
                Some(&error.to_string()),
                "master graph composition could not validate the frozen sources",
            ));
            None
        }
    };
    if let Some(observed) = &composed_view_hash {
        compare_one_baseline(
            &mut findings,
            "master-graph",
            "composed_view",
            observed,
            baseline.as_ref(),
        );
    }

    findings.sort_by(|a, b| {
        (
            a.repository.as_str(),
            a.reason_code.as_str(),
            a.message.as_str(),
        )
            .cmp(&(
                b.repository.as_str(),
                b.reason_code.as_str(),
                b.message.as_str(),
            ))
    });
    let remediation_plan: Vec<Value> = findings.iter().map(remediation).collect();
    let reconciled = findings.is_empty()
        && EXPECTED.iter().all(|name| {
            by_repo
                .keys()
                .any(|repo| normalize_key(repo) == normalize_key(name))
        });
    let mut repositories: Vec<_> = EXPECTED.iter().map(|name| normalize_key(name)).collect();
    repositories.sort();
    Ok(json!({
        "schema": SCHEMA,
        "reconciled": reconciled,
        "repositories": repositories,
        "hashes": hashes,
        "composed_view_hash": composed_view_hash,
        "remediation_plan": remediation_plan,
        "summary": {
            "repository_count": by_repo.len(),
            "expected_count": EXPECTED.len(),
            "finding_count": findings.len(),
            "blocked_count": findings.iter().filter(|f| f.blocked).count(),
            "unresolved_count": findings.iter().filter(|f| f.unresolved).count(),
        }
    }))
}

fn finding(
    repo: &str,
    reason: &str,
    expected: Option<&str>,
    observed: Option<&str>,
    message: &str,
) -> Finding {
    Finding {
        repository: repo.to_owned(),
        reason_code: reason.to_owned(),
        expected: expected.map(str::to_owned),
        observed: observed.map(str::to_owned),
        message: message.to_owned(),
        command: "fractal graph audit --inventory <frozen-inventory> --report <audit-report>"
            .to_owned(),
        preconditions: vec![
            "frozen inventory is unchanged".to_owned(),
            "current audit evidence is available".to_owned(),
        ],
        safety: if reason == "unsupported_remediation" {
            "unsupported"
        } else {
            "read_only"
        }
        .to_owned(),
        blocked: true,
        unresolved: true,
    }
}

fn remediation(finding: &Finding) -> Value {
    let seed = json!({"repository": finding.repository, "reason_code": finding.reason_code, "expected": finding.expected, "observed": finding.observed});
    let action_id =
        fractal_contracts::canonical_sha256(&seed).unwrap_or_else(|_| "sha256:invalid".to_owned());
    json!({
        "action_id": format!("reconcile:{action_id}"),
        "repository": finding.repository,
        "reason_code": finding.reason_code,
        "expected": finding.expected,
        "observed": finding.observed,
        "expected_identity_or_hash": finding.expected,
        "observed_identity_or_hash": finding.observed,
        "supported_command": finding.command,
        "supported_interface": "fractal.graph_reconcile.v1",
        "preconditions": finding.preconditions,
        "safety_classification": finding.safety,
        "blocked": finding.blocked,
        "unresolved": finding.unresolved,
        "message": finding.message,
    })
}

fn load_audits(paths: &[PathBuf]) -> Result<Vec<Value>> {
    let mut reports = Vec::new();
    for path in paths {
        let value = read_json(path, "current audit evidence")?;
        if let Some(items) = value.get("reports").and_then(Value::as_array) {
            reports.extend(items.iter().cloned());
        } else if let Some(items) = value.get("audits").and_then(Value::as_array) {
            reports.extend(items.iter().cloned());
        } else if let Some(items) = value.as_array() {
            reports.extend(items.iter().cloned());
        } else {
            reports.push(value);
        }
    }
    reports.sort_by_key(canonical_bytes);
    Ok(reports)
}

fn read_json(path: &Path, context: &str) -> Result<Value> {
    let bytes = fs::read(path).with_context(|| format!("read {context} {}", path.display()))?;
    serde_json::from_slice(&bytes).with_context(|| format!("decode {context} {}", path.display()))
}

fn resolve_identity<'a, I>(
    root: &str,
    labels: I,
    extra: &BTreeMap<String, Value>,
    remote: Option<&str>,
) -> Identity
where
    I: IntoIterator<Item = &'a str>,
{
    let normalized_root = normalize_root(root);
    let declared = [
        "repository_key",
        "declared_repository_key",
        "repository",
        "key",
        "name",
        "project_key",
    ]
    .iter()
    .find_map(|key| extra.get(*key).and_then(Value::as_str).map(str::to_owned));
    let normalized_remote = remote.map(normalize_remote).filter(|v| !v.is_empty());
    let mut claims = Vec::new();
    if let Some(declared) = &declared {
        claims.push(declared.clone());
    }
    if let Some(segment) = Path::new(&normalized_root)
        .file_name()
        .and_then(|s| s.to_str())
    {
        claims.push(segment.to_owned());
    }
    for label in labels {
        claims.push(label.to_owned());
    }
    if let Some(remote) = &normalized_remote {
        if let Some(segment) = remote.rsplit('/').next() {
            claims.push(segment.to_owned());
        }
    }
    let mapped: BTreeSet<_> = claims
        .iter()
        .filter_map(|claim| expected_name(claim))
        .collect();
    let repository = if mapped.len() == 1 {
        mapped.iter().next().unwrap().clone()
    } else {
        "unexpected".to_owned()
    };
    Identity {
        repository,
        root: normalized_root,
        declared,
        remote: normalized_remote,
        claims,
    }
}

fn expected_name(value: &str) -> Option<String> {
    let normalized = normalize_key(value);
    EXPECTED
        .iter()
        .find(|expected| {
            let expected = normalize_key(expected);
            expected == normalized || expected.replace('-', "") == normalized.replace('-', "")
        })
        .map(|v| (*v).to_owned())
}

fn normalize_key(value: &str) -> String {
    let mut result = String::new();
    let mut dash = false;
    for c in value.trim().chars().flat_map(char::to_lowercase) {
        if c.is_ascii_alphanumeric() {
            result.push(c);
            dash = false;
        } else if !dash && !result.is_empty() {
            result.push('-');
            dash = true;
        }
    }
    result.trim_end_matches('-').to_owned()
}

fn normalize_root(value: &str) -> String {
    let path = PathBuf::from(value);
    fs::canonicalize(&path)
        .unwrap_or_else(|_| {
            if path.is_absolute() {
                path
            } else {
                std::env::current_dir().unwrap_or_default().join(path)
            }
        })
        .to_string_lossy()
        .replace('\\', "/")
}

fn normalize_remote(value: &str) -> String {
    let mut remote = value.trim().to_ascii_lowercase();
    if let Some((host, path)) = remote.strip_prefix("git@").and_then(|v| v.split_once(':')) {
        remote = format!("{host}/{path}");
    }
    for prefix in ["https://", "http://", "ssh://", "git://"] {
        if let Some(stripped) = remote.strip_prefix(prefix) {
            remote = stripped.to_owned();
            break;
        }
    }
    if let Some((_, path)) = remote.split_once('@') {
        remote = path.to_owned();
    }
    while remote.ends_with('/') {
        remote.pop();
    }
    if remote.ends_with(".git") {
        remote.truncate(remote.len() - 4);
    }
    remote
}

fn find_audit(identity: &Identity, audits: &[Value]) -> Option<Value> {
    audits
        .iter()
        .find(|audit| {
            let schema = audit.get("schema").and_then(Value::as_str);
            if schema.is_some() && schema != Some("fractal.project-audit-shard-report.v1") {
                return false;
            }
            let root = audit
                .get("workspace")
                .or_else(|| audit.get("canonical_workspace"))
                .and_then(Value::as_str)
                .map(normalize_root);
            let key = ["repository_key", "repository", "project_key", "name"]
                .iter()
                .find_map(|k| audit.get(*k).and_then(Value::as_str));
            let remote = audit
                .get("remote")
                .and_then(Value::as_str)
                .map(normalize_remote);
            root.as_deref() == Some(identity.root.as_str())
                || key.and_then(expected_name).as_deref() == Some(identity.repository.as_str())
                || remote.as_deref() == identity.remote.as_deref()
        })
        .cloned()
}

fn collect_hashes(root: &str, audit: Option<&Value>) -> Result<SourceSets> {
    let mut hashes = BTreeMap::new();
    let mut nodes = BTreeSet::new();
    let mut interfaces = BTreeSet::new();
    let project = Path::new(root).join(".fractal/project.fractal");
    let document = fs::read(&project)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
    if let Some(document) = &document {
        if let Some(graph) = document.get("graph") {
            let mut graph_for_hash = graph.clone();
            if let Some(obj) = graph_for_hash.as_object_mut() {
                obj.remove("graph_hash");
            }
            hashes.insert("project_graph".to_owned(), hash_value(&graph_for_hash)?);
            if let Some(items) = graph.get("nodes").and_then(Value::as_array) {
                for item in items {
                    if let Some(id) = item
                        .get("id")
                        .or_else(|| item.get("key"))
                        .and_then(Value::as_str)
                    {
                        nodes.insert(id.to_owned());
                    }
                }
                hashes.insert("nodes".to_owned(), hash_sorted_array(items)?);
            }
        }
        if let Some(catalog) = document.get("catalog") {
            if let Some(items) = catalog.get("cross_graph_links").and_then(Value::as_array) {
                for item in items {
                    if let Some(id) = item.get("key").and_then(Value::as_str) {
                        interfaces.insert(id.to_owned());
                    }
                }
                hashes.insert("links".to_owned(), hash_sorted_array(items)?);
            }
            if let Some(items) = catalog.get("interfaces").and_then(Value::as_array) {
                for item in items {
                    if let Some(id) = item.get("key").and_then(Value::as_str) {
                        interfaces.insert(id.to_owned());
                    }
                }
            }
        }
    }
    if let Some(audit) = audit {
        let computed_audit = canonical_hash_without(
            audit,
            &["started_at", "finished_at", "generated_at", "updated_at"],
        )?;
        hashes.insert("audit".to_owned(), computed_audit.clone());
        for (field, key) in [
            ("audit_hash", "audit"),
            ("project_graph_hash", "project_graph"),
            ("node_hash", "nodes"),
            ("link_hash", "links"),
            ("interface_hash", "interfaces"),
        ] {
            if let Some(value) = audit.get(field).and_then(Value::as_str) {
                hashes.insert(format!("{key}_claimed"), value.to_owned());
            }
        }
        for field in ["nodes", "graph_nodes"] {
            if let Some(items) = audit.get(field).and_then(Value::as_array) {
                for item in items {
                    if let Some(id) = item
                        .get("id")
                        .or_else(|| item.get("key"))
                        .and_then(Value::as_str)
                    {
                        nodes.insert(id.to_owned());
                    }
                }
            }
        }
        for field in ["interfaces", "interface_keys"] {
            if let Some(items) = audit.get(field).and_then(Value::as_array) {
                for item in items {
                    if let Some(id) = item
                        .as_str()
                        .or_else(|| item.get("key").and_then(Value::as_str))
                    {
                        interfaces.insert(id.to_owned());
                    }
                }
            }
        }
    }
    Ok((hashes, nodes, interfaces))
}

fn compare_baseline(
    findings: &mut Vec<Finding>,
    repo: &str,
    hashes: &BTreeMap<String, String>,
    baseline: Option<&Value>,
) {
    for (field, observed) in hashes {
        compare_one_baseline(findings, repo, field, observed, baseline);
    }
}

fn compare_one_baseline(
    findings: &mut Vec<Finding>,
    repo: &str,
    field: &str,
    observed: &str,
    baseline: Option<&Value>,
) {
    let Some(baseline) = baseline else { return };
    let expected = baseline
        .get("hashes")
        .and_then(|v| v.get(repo))
        .and_then(|v| v.get(field))
        .and_then(Value::as_str)
        .or_else(|| {
            baseline
                .get("repositories")
                .and_then(|v| v.get(repo))
                .and_then(|v| v.get("hashes"))
                .and_then(|v| v.get(field))
                .and_then(Value::as_str)
        })
        .or_else(|| {
            baseline
                .get("hashes")
                .and_then(|v| v.get(field))
                .and_then(Value::as_str)
        });
    if let Some(expected) = expected.filter(|expected| *expected != observed) {
        findings.push(finding(
            repo,
            &format!("{field}_hash_drift"),
            Some(expected),
            Some(observed),
            "current canonical hash differs from baseline",
        ));
    }
}

fn scan_links(
    findings: &mut Vec<Finding>,
    repo: &str,
    evidence: &Evidence,
    all: &BTreeMap<String, Evidence>,
) {
    let Some(audit) = &evidence.audit else { return };
    let links = audit
        .get("links")
        .or_else(|| audit.get("cross_graph_links"))
        .or_else(|| audit.get("interfaces"))
        .and_then(Value::as_array);
    if let Some(remediations) = audit.get("remediations").and_then(Value::as_array) {
        for remediation in remediations {
            if remediation.get("supported").and_then(Value::as_bool) == Some(false) {
                findings.push(finding(
                    repo,
                    "unsupported_remediation",
                    Some("supported interface"),
                    remediation.get("command").and_then(Value::as_str),
                    "audit evidence requests a remediation outside supported interfaces",
                ));
            }
        }
    }
    let Some(links) = links else { return };
    for link in links {
        let target = link
            .get("target_repository")
            .or_else(|| link.get("target_repo"))
            .or_else(|| link.pointer("/to/repository"))
            .or_else(|| link.get("target"))
            .and_then(Value::as_str);
        if let Some(target) = target {
            let target_repo = expected_name(target).unwrap_or_else(|| normalize_key(target));
            if !all
                .keys()
                .any(|key| normalize_key(key) == normalize_key(&target_repo))
            {
                findings.push(finding(
                    repo,
                    "dangling_repository_link",
                    Some(&target_repo),
                    None,
                    "link target repository is not present in the six-repository inventory",
                ));
                continue;
            }
            if let Some(target_evidence) = all
                .values()
                .find(|e| normalize_key(&e.identity.repository) == normalize_key(&target_repo))
            {
                if let Some(node) = link
                    .get("target_node")
                    .or_else(|| link.pointer("/to/node_id"))
                    .or_else(|| link.get("node"))
                    .and_then(Value::as_str)
                {
                    if !target_evidence.nodes.contains(node) {
                        findings.push(finding(
                            repo,
                            "dangling_node_link",
                            Some(node),
                            None,
                            "link target node is absent from the target graph",
                        ));
                    }
                }
                if let Some(interface) = link
                    .get("target_interface")
                    .or_else(|| link.pointer("/to/interface"))
                    .and_then(Value::as_str)
                {
                    if !target_evidence.interfaces.contains(interface) {
                        findings.push(finding(
                            repo,
                            "dangling_interface_link",
                            Some(interface),
                            None,
                            "link target interface is absent from the target evidence",
                        ));
                    }
                }
            }
        }
        if let Some(claimed) = link
            .get("link_hash")
            .or_else(|| link.get("hash"))
            .and_then(Value::as_str)
        {
            let mut stable = link.clone();
            if let Some(obj) = stable.as_object_mut() {
                obj.remove("link_hash");
                obj.remove("hash");
            }
            if let Ok(observed) = hash_value(&stable) {
                if observed != claimed {
                    findings.push(finding(
                        repo,
                        "stale_link_hash",
                        Some(claimed),
                        Some(&observed),
                        "link hash does not match canonical link content",
                    ));
                }
            }
        }
    }
}

fn identity_hash(identity: &Identity) -> Result<String> {
    hash_value(
        &json!({"repository": identity.repository, "root": identity.root, "declared": identity.declared, "remote": identity.remote}),
    )
}

fn hash_sorted_array(items: &[Value]) -> Result<String> {
    let mut values: Vec<_> = items.iter().map(normalize_value).collect();
    values.sort_by_key(canonical_bytes);
    hash_value(&Value::Array(values))
}

fn canonical_hash_without(value: &Value, keys: &[&str]) -> Result<String> {
    let normalized = normalize_value_without(value, keys);
    hash_value(&normalized)
}

fn normalize_value_without(value: &Value, keys: &[&str]) -> Value {
    match value {
        Value::Object(object) => Value::Object(
            object
                .iter()
                .filter(|(key, _)| !keys.contains(&key.as_str()))
                .map(|(key, child)| (key.clone(), normalize_value_without(child, keys)))
                .collect(),
        ),
        Value::Array(array) => {
            let mut values: Vec<_> = array
                .iter()
                .map(|child| normalize_value_without(child, keys))
                .collect();
            values.sort_by_key(canonical_bytes);
            Value::Array(values)
        }
        other => other.clone(),
    }
}

fn hash_value(value: &Value) -> Result<String> {
    fractal_contracts::canonical_sha256(value)
        .map_err(|error| anyhow::anyhow!("canonical hash: {error}"))
}

fn normalize_value(value: &Value) -> Value {
    match value {
        Value::Object(object) => Value::Object(
            object
                .iter()
                .map(|(k, v)| (k.clone(), normalize_value(v)))
                .collect(),
        ),
        Value::Array(array) => {
            let mut values: Vec<_> = array.iter().map(normalize_value).collect();
            values.sort_by_key(canonical_bytes);
            Value::Array(values)
        }
        other => other.clone(),
    }
}

fn canonical_bytes(value: &Value) -> Vec<u8> {
    serde_json::to_vec(value).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_paths_and_remote_aliases() {
        assert_eq!(
            normalize_remote("git@github.com:FractalSociety/fractal-cli.git"),
            "github.com/fractalsociety/fractal-cli"
        );
        assert_eq!(normalize_key("FractalRuntime"), "fractalruntime");
        assert_eq!(
            expected_name("fractal-master"),
            Some("fractalmaster".to_owned())
        );
        assert_eq!(
            expected_name("Fractal Society Website"),
            Some("fractalsociety-website".to_owned())
        );
    }

    #[test]
    fn path_display_and_remote_aliases_converge_without_conflict() {
        let mut extra = BTreeMap::new();
        extra.insert(
            "repository_key".to_owned(),
            Value::String("fractal-cli".to_owned()),
        );
        let identity = resolve_identity(
            "/tmp/Fractal CLI",
            ["Fractal CLI"],
            &extra,
            Some("https://github.com/FractalSociety/fractal-cli.git"),
        );
        let mapped: BTreeSet<_> = identity
            .claims
            .iter()
            .filter_map(|claim| expected_name(claim))
            .collect();
        assert_eq!(identity.repository, "fractal-cli");
        assert_eq!(mapped.len(), 1);
    }

    #[test]
    fn nested_wall_clock_fields_are_excluded_from_hashes() {
        let a = json!({"audit":{"started_at":"one"},"items":[{"updated_at":"old","key":"a"}]});
        let b = json!({"audit":{"started_at":"two"},"items":[{"updated_at":"new","key":"a"}]});
        assert_eq!(
            canonical_hash_without(&a, &["started_at", "updated_at"]).unwrap(),
            canonical_hash_without(&b, &["started_at", "updated_at"]).unwrap()
        );
    }

    #[test]
    fn remediation_action_ids_are_stable() {
        let a = finding(
            "fractal-cli",
            "project_graph_hash_drift",
            Some("sha256:a"),
            Some("sha256:b"),
            "drift",
        );
        let b = finding(
            "fractal-cli",
            "project_graph_hash_drift",
            Some("sha256:a"),
            Some("sha256:b"),
            "drift",
        );
        assert_eq!(remediation(&a), remediation(&b));
    }

    #[test]
    fn array_order_is_not_hash_significant() {
        let a = json!({"items":[{"key":"b"},{"key":"a"}]});
        let b = json!({"items":[{"key":"a"},{"key":"b"}]});
        assert_eq!(
            hash_value(&normalize_value(&a)).unwrap(),
            hash_value(&normalize_value(&b)).unwrap()
        );
    }

    #[test]
    fn volatile_audit_times_are_excluded() {
        let a = json!({"finished_at":"one","value":1});
        let b = json!({"finished_at":"two","value":1});
        assert_eq!(
            canonical_hash_without(&a, &["finished_at"]).unwrap(),
            canonical_hash_without(&b, &["finished_at"]).unwrap()
        );
    }
}
