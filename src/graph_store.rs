//! Durable, content-addressed storage for compiled execution graphs.

use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;

#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// The result of durably committing one execution graph.
#[derive(Debug, Eq, PartialEq)]
pub(crate) struct CommitRecord {
    pub(crate) graph_hash: String,
    pub(crate) path: PathBuf,
    pub(crate) bytes: usize,
}

/// Return the content-addressed path for an execution-graph hash.
///
/// Commit and load operations additionally validate the environment and hash
/// before using this path.
pub(crate) fn graph_path(graph_hash: &str) -> PathBuf {
    let root = match env::var_os("FRACTAL_HOME") {
        Some(home) => PathBuf::from(home),
        None => match env::var_os("HOME") {
            Some(home) => PathBuf::from(home).join(".fractal"),
            None => PathBuf::from(".fractal"),
        },
    };
    root.join("graphs")
        .join(format!("{}.json", graph_hash.trim_start_matches("sha256:")))
}

/// Sidecar path holding the compile inputs (harness genome + work + target) for
/// a committed graph, so harness evolution can mutate the genome and recompile.
pub(crate) fn source_path(graph_hash: &str) -> PathBuf {
    let hash = graph_hash.trim_start_matches("sha256:");
    graph_path(graph_hash).with_file_name(format!("{hash}.source.json"))
}

/// Persist the compile inputs alongside a committed graph (best-effort).
pub(crate) fn persist_source(
    graph_hash: &str,
    harness: &Value,
    work: &Value,
    target_id: &str,
) -> Result<()> {
    let path = source_path(graph_hash);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).ok();
    }
    let document = serde_json::json!({
        "schema": "fractal.graph_source.v1",
        "harness": harness,
        "work": work,
        "target": target_id,
    });
    fs::write(&path, serde_json::to_vec_pretty(&document)?)
        .with_context(|| format!("write graph source {}", path.display()))?;
    Ok(())
}

/// Load the compile inputs `(harness, work, target_id)` for a committed graph.
pub(crate) fn load_source(graph_hash: &str) -> Option<(Value, Value, String)> {
    let text = fs::read_to_string(source_path(graph_hash)).ok()?;
    let document: Value = serde_json::from_str(&text).ok()?;
    let harness = document.get("harness")?.clone();
    let work = document.get("work")?.clone();
    let target = document
        .get("target")
        .and_then(Value::as_str)
        .unwrap_or("darwin-arm64")
        .to_owned();
    Some((harness, work, target))
}

/// Atomically persist a compiled graph after verifying its claimed hash.
pub(crate) fn commit_graph(graph: &Value) -> Result<CommitRecord> {
    let graph_hash = claimed_hash(graph)?;
    verify_graph_hash(graph, graph_hash)?;
    validate_graph_hash(graph_hash)?;
    let root = graph_root()?;
    ensure_store_root(&root)?;
    let path = graph_path(graph_hash);
    let bytes = serde_json::to_vec_pretty(graph).context("encode execution graph as JSON")?;
    atomic_write(&root, &path, &bytes)?;

    Ok(CommitRecord {
        graph_hash: graph_hash.to_owned(),
        path,
        bytes: bytes.len(),
    })
}

/// Load and hash-verify a previously committed execution graph.
pub(crate) fn load_graph(graph_hash: &str) -> Result<Value> {
    validate_graph_hash(graph_hash)?;
    graph_root()?;
    let path = graph_path(graph_hash);
    let bytes = fs::read(&path)
        .with_context(|| format!("execution graph {graph_hash} is not in the local store"))?;
    let graph: Value = serde_json::from_slice(&bytes)
        .with_context(|| format!("stored execution graph {} is invalid JSON", path.display()))?;
    let stored_hash = claimed_hash(&graph)
        .with_context(|| format!("stored execution graph {} is invalid", path.display()))?;
    if stored_hash != graph_hash {
        bail!(
            "stored execution graph hash field mismatch: requested {graph_hash}, found {stored_hash}"
        );
    }
    verify_graph_hash(&graph, graph_hash).with_context(|| {
        format!(
            "stored execution graph {} failed verification",
            path.display()
        )
    })?;
    Ok(graph)
}

/// Print a stored graph as JSON or as a concise human-readable summary.
pub(crate) fn show(graph_hash: &str, json: bool) -> Result<()> {
    let graph = load_graph(graph_hash)?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&graph).context("encode stored execution graph")?
        );
        return Ok(());
    }

    let graph_id = graph
        .get("graph_id")
        .and_then(Value::as_str)
        .context("stored execution graph is missing graph_id")?;
    let stored_hash = claimed_hash(&graph)?;
    let nodes = graph
        .get("nodes")
        .and_then(Value::as_array)
        .context("stored execution graph is missing nodes")?;
    let edges = graph
        .get("edges")
        .and_then(Value::as_array)
        .context("stored execution graph is missing edges")?;
    let node_ids = nodes
        .iter()
        .map(|node| {
            node.get("id")
                .and_then(Value::as_str)
                .context("stored execution graph contains a node without an id")
        })
        .collect::<Result<Vec<_>>>()?;

    println!("Graph id: {graph_id}");
    println!("Graph hash: {stored_hash}");
    println!("Nodes: {}  Edges: {}", nodes.len(), edges.len());
    println!("Node ids: {}", node_ids.join(", "));
    Ok(())
}

fn graph_root() -> Result<PathBuf> {
    if let Some(home) = env::var_os("FRACTAL_HOME") {
        if home.is_empty() {
            bail!("FRACTAL_HOME is set but empty");
        }
        return Ok(PathBuf::from(home).join("graphs"));
    }
    let home =
        env::var_os("HOME").context("cannot resolve graph store: set FRACTAL_HOME or HOME")?;
    if home.is_empty() {
        bail!("HOME is set but empty");
    }
    Ok(PathBuf::from(home).join(".fractal").join("graphs"))
}

fn ensure_store_root(root: &Path) -> Result<()> {
    fs::create_dir_all(root)
        .with_context(|| format!("create graph store directory {}", root.display()))?;
    #[cfg(unix)]
    fs::set_permissions(root, fs::Permissions::from_mode(0o700))
        .with_context(|| format!("set graph store permissions on {}", root.display()))?;
    Ok(())
}

fn claimed_hash(graph: &Value) -> Result<&str> {
    graph
        .get("graph_hash")
        .and_then(Value::as_str)
        .context("execution graph is missing string graph_hash")
}

fn validate_graph_hash(graph_hash: &str) -> Result<&str> {
    let digest = graph_hash
        .strip_prefix("sha256:")
        .context("execution graph hash must start with `sha256:`")?;
    if digest.len() != 64
        || !digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        bail!("execution graph hash must contain 64 lowercase hexadecimal characters");
    }
    Ok(digest)
}

fn verify_graph_hash(graph: &Value, claimed: &str) -> Result<()> {
    validate_graph_hash(claimed)?;
    let mut hash_input = graph
        .as_object()
        .cloned()
        .context("execution graph must be a JSON object")?;
    hash_input.remove("graph_hash");
    let computed = fractal_contracts::canonical_sha256(&Value::Object(hash_input))
        .map_err(|error| anyhow!("canonical execution graph hashing failed: {error}"))?;
    if computed != claimed {
        bail!("execution graph hash mismatch: claimed {claimed}, computed {computed}");
    }
    Ok(())
}

fn atomic_write(root: &Path, destination: &Path, bytes: &[u8]) -> Result<()> {
    let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let temp_path = root.join(format!(".graph-{}-{sequence}.tmp", process::id()));
    let write_result = write_temp_file(&temp_path, bytes);
    if let Err(error) = write_result {
        let _ = fs::remove_file(&temp_path);
        return Err(error);
    }
    if let Err(error) = fs::rename(&temp_path, destination) {
        let _ = fs::remove_file(&temp_path);
        return Err(error).with_context(|| {
            format!(
                "atomically commit execution graph to {}",
                destination.display()
            )
        });
    }
    File::open(root)
        .and_then(|directory| directory.sync_all())
        .with_context(|| format!("sync graph store directory {}", root.display()))?;
    Ok(())
}

fn write_temp_file(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options
        .open(path)
        .with_context(|| format!("create temporary graph file {}", path.display()))?;
    file.write_all(bytes)
        .with_context(|| format!("write temporary graph file {}", path.display()))?;
    file.sync_all()
        .with_context(|| format!("sync temporary graph file {}", path.display()))
}

#[cfg(test)]
pub(crate) static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
pub(crate) struct TestHome {
    pub(crate) path: PathBuf,
    previous: Option<std::ffi::OsString>,
}

#[cfg(test)]
impl TestHome {
    pub(crate) fn new(label: &str) -> Result<Self> {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .context("system clock before Unix epoch")?
            .as_nanos();
        let path = env::temp_dir().join(format!(
            "fractal-graph-store-{label}-{}-{nonce}",
            process::id()
        ));
        fs::create_dir_all(&path)?;
        let previous = env::var_os("FRACTAL_HOME");
        env::set_var("FRACTAL_HOME", &path);
        Ok(Self { path, previous })
    }
}

#[cfg(test)]
impl Drop for TestHome {
    fn drop(&mut self) {
        match &self.previous {
            Some(previous) => env::set_var("FRACTAL_HOME", previous),
            None => env::remove_var("FRACTAL_HOME"),
        }
        let _ = fs::remove_dir_all(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn valid_graph() -> Result<Value> {
        let mut graph = json!({
            "schema": "fractal.execution_graph.v1",
            "graph_id": "fg_test",
            "nodes": [{"id": "analyze"}, {"id": "verify"}],
            "edges": [{"from": "analyze", "to": "verify"}]
        });
        let hash = fractal_contracts::canonical_sha256(&graph)
            .map_err(|error| anyhow!("hash test graph: {error}"))?;
        graph["graph_hash"] = Value::String(hash);
        Ok(graph)
    }

    #[test]
    fn commit_then_load_round_trip() -> Result<()> {
        let _lock = ENV_LOCK
            .lock()
            .map_err(|_| anyhow!("environment lock poisoned"))?;
        let _home = TestHome::new("round-trip")?;
        let graph = valid_graph()?;
        let record = commit_graph(&graph)?;

        assert_eq!(record.graph_hash, graph["graph_hash"]);
        assert_eq!(load_graph(&record.graph_hash)?, graph);
        assert_eq!(record.path, graph_path(&record.graph_hash));
        assert!(record.bytes > 0);
        Ok(())
    }

    #[test]
    fn graph_path_uses_temp_fractal_home() -> Result<()> {
        let _lock = ENV_LOCK
            .lock()
            .map_err(|_| anyhow!("environment lock poisoned"))?;
        let home = TestHome::new("path-resolution")?;
        let hash = format!("sha256:{}", "a".repeat(64));

        assert_eq!(
            graph_path(&hash),
            home.path
                .join("graphs")
                .join(format!("{}.json", "a".repeat(64)))
        );
        Ok(())
    }

    #[test]
    fn rejects_tampered_claimed_hash() -> Result<()> {
        let _lock = ENV_LOCK
            .lock()
            .map_err(|_| anyhow!("environment lock poisoned"))?;
        let _home = TestHome::new("tampered")?;
        let mut graph = valid_graph()?;
        graph["graph_hash"] = Value::String(format!("sha256:{}", "0".repeat(64)));

        let error = commit_graph(&graph).expect_err("tampered graph must be rejected");
        assert!(error.to_string().contains("hash mismatch"));
        Ok(())
    }

    #[test]
    fn absent_hash_errors() -> Result<()> {
        let _lock = ENV_LOCK
            .lock()
            .map_err(|_| anyhow!("environment lock poisoned"))?;
        let _home = TestHome::new("absent")?;
        let hash = format!("sha256:{}", "1".repeat(64));

        let error = load_graph(&hash).expect_err("absent graph must error");
        assert!(error.to_string().contains("not in the local store"));
        Ok(())
    }

    #[test]
    fn successful_commit_leaves_no_temp_file() -> Result<()> {
        let _lock = ENV_LOCK
            .lock()
            .map_err(|_| anyhow!("environment lock poisoned"))?;
        let home = TestHome::new("atomic")?;
        commit_graph(&valid_graph()?)?;
        let entries =
            fs::read_dir(home.path.join("graphs"))?.collect::<std::io::Result<Vec<_>>>()?;

        assert!(entries
            .iter()
            .all(|entry| { entry.file_name().to_string_lossy().ends_with(".json") }));
        Ok(())
    }

    #[test]
    fn double_commit_is_idempotent() -> Result<()> {
        let _lock = ENV_LOCK
            .lock()
            .map_err(|_| anyhow!("environment lock poisoned"))?;
        let home = TestHome::new("idempotent")?;
        let graph = valid_graph()?;
        let first = commit_graph(&graph)?;
        let second = commit_graph(&graph)?;

        assert_eq!(first, second);
        let entries =
            fs::read_dir(home.path.join("graphs"))?.collect::<std::io::Result<Vec<_>>>()?;
        assert_eq!(entries.len(), 1);
        Ok(())
    }
}
