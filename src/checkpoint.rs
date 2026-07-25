//! Durable run checkpoints so an interrupted build can be stopped and resumed.
//!
//! The mid-run supervisor persists progress after every wave: which graph is
//! current (it may have evolved) and which tasks are done. If the process is
//! killed, relaunching `fractal` in the same folder offers to resume — it reloads
//! the committed graph, pre-seeds the completed tasks (so they are NOT re-run),
//! and continues from the remaining frontier. On successful completion the
//! checkpoint is cleared so it is not offered again.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// A persisted snapshot of an in-flight run.
#[derive(Serialize, Deserialize, Clone)]
pub(crate) struct RunCheckpoint {
    pub key: String,
    pub graph_id: String,
    pub request: String,
    pub workspace: String,
    /// The latest (possibly evolved) committed graph hash.
    pub current_graph_hash: String,
    pub completed: Vec<String>,
    pub total: usize,
    pub done: bool,
    pub updated_at_ms: u64,
}

fn runs_dir() -> PathBuf {
    let root = match std::env::var_os("FRACTAL_HOME") {
        Some(home) => PathBuf::from(home),
        None => match std::env::var_os("HOME") {
            Some(home) => PathBuf::from(home).join(".fractal"),
            None => PathBuf::from(".fractal"),
        },
    };
    root.join("runs")
}

/// Stable per-(workspace, initial graph) key, fixed for the life of a run even as
/// the graph evolves into child hashes.
pub(crate) fn key_for(workspace: &Path, graph_id: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(workspace.to_string_lossy().as_bytes());
    hasher.update([0u8]);
    hasher.update(graph_id.as_bytes());
    hasher
        .finalize()
        .iter()
        .take(12)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn path(key: &str) -> PathBuf {
    runs_dir().join(format!("{key}.json"))
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn save(cp: &RunCheckpoint) {
    let dir = runs_dir();
    let _ = std::fs::create_dir_all(&dir);
    if let Ok(text) = serde_json::to_string_pretty(cp) {
        let _ = std::fs::write(path(&cp.key), text);
    }
}

/// Delete a checkpoint (on success, or when the user declines to resume it).
pub(crate) fn discard(key: &str) {
    let _ = std::fs::remove_file(path(key));
}

/// The most recent resumable (not-done, not-fully-complete, graph still present)
/// checkpoint for `workspace`, if any.
pub(crate) fn find_resumable(workspace: &Path) -> Option<RunCheckpoint> {
    let ws = workspace.to_string_lossy();
    let mut best: Option<RunCheckpoint> = None;
    for entry in std::fs::read_dir(runs_dir()).ok()?.flatten() {
        let Ok(text) = std::fs::read_to_string(entry.path()) else {
            continue;
        };
        let Ok(cp) = serde_json::from_str::<RunCheckpoint>(&text) else {
            continue;
        };
        if cp.done
            || cp.workspace != ws
            || cp.completed.is_empty()
            || cp.completed.len() >= cp.total
            || crate::graph_store::load_graph(&cp.current_graph_hash).is_err()
        {
            continue;
        }
        if best
            .as_ref()
            .is_none_or(|current| cp.updated_at_ms > current.updated_at_ms)
        {
            best = Some(cp);
        }
    }
    best
}

/// Every resumable (not-done, not-fully-complete, graph-present) checkpoint, in
/// any workspace — used to backfill the project registry from prior runs.
pub(crate) fn list_resumable() -> Vec<RunCheckpoint> {
    let mut out = Vec::new();
    let Ok(dir) = std::fs::read_dir(runs_dir()) else {
        return out;
    };
    for entry in dir.flatten() {
        let Ok(text) = std::fs::read_to_string(entry.path()) else {
            continue;
        };
        let Ok(cp) = serde_json::from_str::<RunCheckpoint>(&text) else {
            continue;
        };
        if cp.done
            || cp.completed.is_empty()
            || cp.completed.len() >= cp.total
            || crate::graph_store::load_graph(&cp.current_graph_hash).is_err()
        {
            continue;
        }
        out.push(cp);
    }
    out
}

/// A live recorder that persists run progress each wave. Owned by `run_end_to_end`
/// for the duration of a run; disabled when there is no request to key on.
pub(crate) struct Recorder {
    key: String,
    graph_id: String,
    request: String,
    workspace: String,
    enabled: bool,
}

impl Recorder {
    pub(crate) fn new(workspace: &Path, graph_id: &str, request: &str) -> Self {
        Recorder {
            key: key_for(workspace, graph_id),
            graph_id: graph_id.to_owned(),
            request: request.to_owned(),
            workspace: workspace.to_string_lossy().into_owned(),
            enabled: !request.trim().is_empty(),
        }
    }

    /// Persist the current progress. Cheap enough to call after every wave.
    pub(crate) fn record(
        &self,
        current_graph_hash: &str,
        completed: &BTreeSet<String>,
        total: usize,
    ) {
        if !self.enabled {
            return;
        }
        save(&RunCheckpoint {
            key: self.key.clone(),
            graph_id: self.graph_id.clone(),
            request: self.request.clone(),
            workspace: self.workspace.clone(),
            current_graph_hash: current_graph_hash.to_owned(),
            completed: completed.iter().cloned().collect(),
            total,
            done: false,
            updated_at_ms: now_ms(),
        });
    }

    /// Clear the checkpoint — the run finished, there is nothing to resume.
    pub(crate) fn finish(&self) {
        if self.enabled {
            discard(&self.key);
        }
    }
}
