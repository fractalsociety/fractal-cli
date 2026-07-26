//! P5.G3 driver: anchor one developmental step on a scale ledger using the real
//! `fractal-chain` lineage code, and report whether it is genuinely anchored +
//! tamper-evident. Reads a step JSON on stdin:
//! ```json
//! {"scale":"graph","subject":"fg_…#implement","operation":"repair",
//!  "step_id":"repair-precedence","motivating":"<64-hex>","produced":"<64-hex>"}
//! ```
//! Emits `{"anchored": true, "step_commitment": "sha256:…", "head": "sha256:…"}`.

use std::io::Read;

use ed25519_dalek::SigningKey;
use fractal_chain::{
    anchor_step, step_is_anchored, DevelopmentalOp, DevelopmentalStep, Hash256, ScaleLedger,
};
use serde_json::{json, Value};

fn parse_hash(value: &Value, field: &str) -> Result<Hash256, String> {
    let text = value.as_str().ok_or_else(|| format!("{field} must be a string"))?;
    let text = text.strip_prefix("sha256:").unwrap_or(text);
    if text.len() != 64 || !text.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!("{field} must be 64 hex chars"));
    }
    let mut out = [0u8; 32];
    for (i, chunk) in text.as_bytes().chunks(2).enumerate() {
        out[i] = u8::from_str_radix(std::str::from_utf8(chunk).unwrap(), 16)
            .map_err(|_| format!("{field} invalid hex"))?;
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
    std::io::stdin().read_to_string(&mut raw).map_err(|e| e.to_string())?;
    let input: Value = serde_json::from_str(&raw).map_err(|e| format!("invalid JSON: {e}"))?;

    let operation = match input["operation"].as_str() {
        Some("grow") => DevelopmentalOp::Grow,
        Some("repair") => DevelopmentalOp::Repair,
        Some("differentiate") => DevelopmentalOp::Differentiate,
        _ => return Err("operation must be grow|repair|differentiate".into()),
    };
    let scale = input["scale"].as_str().ok_or("scale required")?;
    let scale_level = fractal_chain::ScaleLevel::parse(scale)
        .ok_or_else(|| format!("unknown scale: {scale}"))?;
    let step = DevelopmentalStep {
        scale: scale_level,
        subject: input["subject"].as_str().ok_or("subject required")?.to_owned(),
        operation,
        step_id: input["step_id"].as_str().ok_or("step_id required")?.to_owned(),
        motivating_outcome: parse_hash(&input["motivating"], "motivating")?,
        produced_outcome: parse_hash(&input["produced"], "produced")?,
    };

    // Deterministic per-scale signing key for the demo ledger.
    let mut seed = [0u8; 32];
    seed[..scale.len().min(32)].copy_from_slice(&scale.as_bytes()[..scale.len().min(32)]);
    let mut ledger = ScaleLedger::new(scale, SigningKey::from_bytes(&seed));
    anchor_step(&mut ledger, &step, 1);
    ledger.verify().map_err(|e| format!("ledger did not verify: {e}"))?;
    let anchored = step_is_anchored(&ledger, &step);

    Ok(json!({
        "anchored": anchored,
        "step_commitment": hex(&step.commitment()),
        "head": hex(&ledger.head()),
    }))
}

fn main() {
    match run() {
        Ok(value) => println!("{value}"),
        Err(error) => {
            eprintln!("{}", json!({ "error": error }));
            std::process::exit(1);
        }
    }
}
