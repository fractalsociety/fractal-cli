//! Standardized, portable per-project execution graph.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct FractalProject {
    pub(crate) schema: String,
    pub(crate) project: ProjectIdentity,
    pub(crate) graph_hash: String,
    pub(crate) graph: Value,
    pub(crate) updated_at: String,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct ProjectIdentity {
    pub(crate) slug: String,
    pub(crate) title: String,
    pub(crate) visibility: String,
}

pub(crate) fn path(workspace: &Path) -> PathBuf {
    workspace.join(".fractal").join("project.fractal")
}

pub(crate) fn persist(workspace: &Path, graph: &Value, title: &str) -> Result<PathBuf> {
    let graph_hash = graph
        .get("graph_hash")
        .and_then(Value::as_str)
        .context("execution graph is missing graph_hash")?;
    if graph.get("schema").and_then(Value::as_str) != Some("fractal.execution_graph.v1") {
        bail!("only fractal.execution_graph.v1 can be stored in a project.fractal file");
    }
    crate::graph_store::verify_graph_document(graph)
        .context("refuse to persist an execution graph with an invalid hash")?;
    reject_secret_fields(graph)?;
    let slug = slug_for(workspace);
    let document = FractalProject {
        schema: "fractal.project.v1".to_owned(),
        project: ProjectIdentity {
            slug,
            title: clean_title(title, workspace),
            visibility: "private".to_owned(),
        },
        graph_hash: graph_hash.to_owned(),
        graph: graph.clone(),
        updated_at: timestamp(),
    };
    let destination = path(workspace);
    let directory = destination.parent().expect("project file has parent");
    fs::create_dir_all(directory).with_context(|| format!("create {}", directory.display()))?;
    let bytes = serde_json::to_vec_pretty(&document)?;
    atomic_write(&destination, &bytes)?;
    Ok(destination)
}

pub(crate) fn load(workspace: &Path) -> Result<FractalProject> {
    let path = path(workspace);
    let document: FractalProject = serde_json::from_slice(
        &fs::read(&path).with_context(|| format!("read {}", path.display()))?,
    )
    .with_context(|| format!("decode {}", path.display()))?;
    validate(&document)?;
    Ok(document)
}

fn validate(document: &FractalProject) -> Result<()> {
    if document.schema != "fractal.project.v1"
        || document.graph_hash.is_empty()
        || document.project.slug.is_empty()
        || document.graph.get("graph_hash").and_then(Value::as_str)
            != Some(document.graph_hash.as_str())
    {
        bail!("invalid fractal.project.v1 document");
    }
    crate::graph_store::verify_graph_document(&document.graph)
        .context("embedded execution graph hash is invalid")?;
    reject_secret_fields(&document.graph)?;
    Ok(())
}

fn reject_secret_fields(value: &Value) -> Result<()> {
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
                    bail!("execution graph contains forbidden credential field `{key}`");
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

fn slug_for(workspace: &Path) -> String {
    let raw = workspace
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "project".to_owned());
    let mut slug = String::new();
    let mut separator = false;
    for character in raw.chars().flat_map(char::to_lowercase) {
        if character.is_ascii_alphanumeric() {
            slug.push(character);
            separator = false;
        } else if !separator && !slug.is_empty() {
            slug.push('-');
            separator = true;
        }
    }
    while slug.ends_with('-') {
        slug.pop();
    }
    if slug.is_empty() {
        "project".to_owned()
    } else {
        slug
    }
}

fn clean_title(title: &str, workspace: &Path) -> String {
    let title = title.split_whitespace().collect::<Vec<_>>().join(" ");
    if title.is_empty() {
        slug_for(workspace)
    } else {
        title.chars().take(240).collect()
    }
}

fn timestamp() -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs());
    rfc3339_utc(seconds)
}

fn rfc3339_utc(seconds: u64) -> String {
    let days = (seconds / 86_400) as i64;
    let seconds_of_day = seconds % 86_400;
    // Howard Hinnant's civil-from-days algorithm, with Unix epoch adjustment.
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    let hour = seconds_of_day / 3_600;
    let minute = seconds_of_day % 3_600 / 60;
    let second = seconds_of_day % 60;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

fn atomic_write(destination: &Path, bytes: &[u8]) -> Result<()> {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    let temporary = destination.with_extension(format!("tmp-{}-{nonce}", std::process::id()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    fs::rename(&temporary, destination)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn temp_workspace() -> PathBuf {
        std::env::temp_dir().join(format!(
            "My Expense App {}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[test]
    fn persists_portable_standard_document() -> Result<()> {
        let workspace = temp_workspace();
        fs::create_dir_all(&workspace)?;
        let mut graph = json!({
            "schema": "fractal.execution_graph.v1",
            "nodes": [],
            "edges": []
        });
        graph["graph_hash"] = Value::String(
            fractal_contracts::canonical_sha256(&graph)
                .map_err(|error| anyhow::anyhow!("hash fixture: {error}"))?,
        );
        let stored = persist(&workspace, &graph, "Build an expense tracker")?;
        assert_eq!(stored, workspace.join(".fractal/project.fractal"));
        let document = load(&workspace)?;
        assert_eq!(document.schema, "fractal.project.v1");
        assert!(document.project.slug.starts_with("my-expense-app-"));
        assert_eq!(document.graph, graph);
        let encoded = fs::read_to_string(stored)?;
        assert!(!encoded.contains(workspace.to_string_lossy().as_ref()));
        fs::remove_dir_all(workspace)?;
        Ok(())
    }

    #[test]
    fn renders_unix_epoch_as_rfc3339() {
        assert_eq!(rfc3339_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339_utc(1_722_470_400), "2024-08-01T00:00:00Z");
    }

    #[test]
    fn refuses_credential_fields() -> Result<()> {
        let workspace = temp_workspace();
        fs::create_dir_all(&workspace)?;
        let mut graph = json!({
            "schema": "fractal.execution_graph.v1",
            "nodes": [],
            "edges": [],
            "access_token": "must-not-leak"
        });
        graph["graph_hash"] = Value::String(
            fractal_contracts::canonical_sha256(&graph)
                .map_err(|error| anyhow::anyhow!("hash fixture: {error}"))?,
        );
        let error = persist(&workspace, &graph, "unsafe")
            .expect_err("credential-shaped fields must be refused");
        assert!(error.to_string().contains("forbidden credential field"));
        fs::remove_dir_all(workspace)?;
        Ok(())
    }
}
