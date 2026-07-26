//! P5.5 driver: turn a run's evidence + outcome fields + consent into a
//! replayable evidence root and a consent-gated sanitized export, using the real
//! `fractal-chain` primitives (no reimplementation, so nothing can drift).
//!
//! Reads one JSON object on stdin:
//! ```json
//! {
//!   "entries": [{"label": "node:analyze", "digest": "<64-hex>"}, ...],
//!   "fields":  [{"key": "summary", "value": "<64-hex>", "sensitivity": "public"|"private"}, ...],
//!   "consent": {"granted": true, "scope": "dataevol:promotion"}
//! }
//! ```
//! and writes the export as JSON on stdout, or an `{"error": "..."}` object with
//! a non-zero exit on failure (fail-closed).

use std::io::Read;

use fractal_chain::{
    replayable_evidence_root, sanitized_export, verify_replay, Consent, EvidenceEntry, Hash256,
    OutcomeField, Sensitivity,
};
use serde_json::{json, Value};

fn parse_hash(value: &Value, field: &str) -> Result<Hash256, String> {
    let text = value
        .as_str()
        .ok_or_else(|| format!("{field} must be a hex string"))?;
    let text = text.strip_prefix("sha256:").unwrap_or(text);
    if text.len() != 64 || !text.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!("{field} must be 64 hex chars"));
    }
    let mut out = [0u8; 32];
    for (index, chunk) in text.as_bytes().chunks(2).enumerate() {
        let byte = u8::from_str_radix(std::str::from_utf8(chunk).unwrap(), 16)
            .map_err(|_| format!("{field} has invalid hex"))?;
        out[index] = byte;
    }
    Ok(out)
}

fn hex(hash: &Hash256) -> String {
    let mut s = String::from("sha256:");
    for byte in hash {
        s.push_str(&format!("{byte:02x}"));
    }
    s
}

fn run() -> Result<Value, String> {
    let mut raw = String::new();
    std::io::stdin()
        .read_to_string(&mut raw)
        .map_err(|e| format!("read stdin: {e}"))?;
    let input: Value = serde_json::from_str(&raw).map_err(|e| format!("invalid JSON: {e}"))?;

    let mut entries = Vec::new();
    for (index, entry) in input["entries"]
        .as_array()
        .ok_or("entries must be an array")?
        .iter()
        .enumerate()
    {
        let label = entry["label"]
            .as_str()
            .ok_or_else(|| format!("entries[{index}].label missing"))?;
        let digest = parse_hash(&entry["digest"], &format!("entries[{index}].digest"))?;
        entries.push(EvidenceEntry::new(label, digest));
    }

    let mut fields = Vec::new();
    for (index, field) in input["fields"]
        .as_array()
        .ok_or("fields must be an array")?
        .iter()
        .enumerate()
    {
        let key = field["key"]
            .as_str()
            .ok_or_else(|| format!("fields[{index}].key missing"))?;
        let value_digest = parse_hash(&field["value"], &format!("fields[{index}].value"))?;
        let sensitivity = match field["sensitivity"].as_str() {
            Some("public") => Sensitivity::Public,
            Some("private") => Sensitivity::Private,
            _ => return Err(format!("fields[{index}].sensitivity must be public|private")),
        };
        fields.push(OutcomeField {
            key: key.to_owned(),
            value_digest,
            sensitivity,
        });
    }

    let consent = Consent {
        granted: input["consent"]["granted"].as_bool().unwrap_or(false),
        scope: input["consent"]["scope"].as_str().unwrap_or("").to_owned(),
    };

    // Replayable evidence root: recompute and confirm it replays.
    let evidence_root = replayable_evidence_root(&entries);
    let replay_ok = verify_replay(&entries, evidence_root);

    // Consent-gated, sanitized export (fail-closed inside the crate).
    let export = sanitized_export(&entries, &fields, &consent)
        .map_err(|e| format!("export denied: {e}"))?;

    Ok(json!({
        "replay_ok": replay_ok,
        "evidence_root": hex(&evidence_root),
        "export": {
            "evidenceRoot": hex(&export.evidence_root),
            "publicFields": export.public_fields.iter().map(|(k, d)| json!([k, hex(d)])).collect::<Vec<_>>(),
            "redactedCount": export.redacted_count,
            "consentScope": export.consent_scope,
            "exportCommitment": hex(&export.export_commitment),
        }
    }))
}

fn main() {
    match run() {
        Ok(value) => {
            println!("{value}");
        }
        Err(error) => {
            eprintln!("{}", json!({ "error": error }));
            std::process::exit(1);
        }
    }
}
