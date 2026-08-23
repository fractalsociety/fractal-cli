//! Capability settlement 19 acceptance harness (FRAC P1.1 / P1.2 / P1.4).
//!
//! Binary-only package: this integration crate stays std-only and validates the
//! frozen baseline contract plus docs. Heavy settlement / local-devnet suites live
//! in `src/chain_client19.rs` unit tests (compiled via `orchestrate::chain_client19`).

use std::fs;
use std::path::PathBuf;
use std::process::Command;

/// Every fixture this suite reads is resolved from the crate manifest, never
/// from an absolute path or `$HOME`, so the suite is byte-for-byte reproducible
/// on any checkout.
fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

const FIXTURE_DIR: &str = "tests/fixtures/capability_settlement19";
const OUTCOME_RECEIPT_VECTORS_FIXTURE: &str = "outcome_receipt_v1_vectors.json";
const OUTCOME_RECEIPT_VECTORS_SHA256: &str =
    "3515bd3e881dd82488c45ba9b77e73bda0e7425657fef47b69998b768dc19b3a";
const OUTCOME_MEMORY_FIXTURE: &str = "outcome-memory.jsonl";
const OUTCOME_MEMORY_SHA256: &str =
    "9c9ab40f41432fc5ba55db3b3c305167d59980d9cb7b5a16fb29c9ad15f28b05";
const OFFLINE_TRACE_FIXTURE: &str = "offline_verified_run_trace.json";
const OFFLINE_TRACE_SHA256: &str =
    "8c71dab46a93eae49871874f3a4c885d00e350abbc0512b36843b274f8b1d917";

fn fixture(name: &str) -> PathBuf {
    repo_root().join(FIXTURE_DIR).join(name)
}

fn sha256_hex(bytes: &[u8]) -> String {
    // Prefer system shasum so we stay dependency-free in this integration crate.
    let mut child = Command::new("shasum")
        .args(["-a", "256"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("spawn shasum");
    {
        use std::io::Write;
        child
            .stdin
            .as_mut()
            .expect("stdin")
            .write_all(bytes)
            .expect("write shasum stdin");
    }
    let out = child.wait_with_output().expect("shasum");
    assert!(out.status.success(), "shasum failed: {:?}", out);
    let text = String::from_utf8_lossy(&out.stdout);
    text.split_whitespace()
        .next()
        .expect("hash")
        .trim()
        .to_owned()
}

#[test]
fn docs_contract_exists() {
    let path = repo_root().join("docs/capability-settlement19.md");
    let body = fs::read_to_string(&path).expect("docs/capability-settlement19.md");
    assert!(body.contains("price_paid_frac"));
    assert!(body.contains("SettleOutcomeReceipt"));
    assert!(body.contains("8c71dab46a93eae49871874f3a4c885d00e350abbc0512b36843b274f8b1d917"));
    assert!(body.contains("3515bd3e881dd82488c45ba9b77e73bda0e7425657fef47b69998b768dc19b3a"));
}

#[test]
fn frozen_export_outcome_example_hash() {
    let path = repo_root().join("crates/fractal-chain/examples/export_outcome.rs");
    let bytes = fs::read(&path).expect("export_outcome.rs");
    assert_eq!(
        sha256_hex(&bytes),
        "c85f00b05be5e7d4a1c1644aff09d31baaf41f8cc1811aa7d502bf44d31fee4f"
    );
}

/// The cross-repository `outcome_receipt_v1` settlement vectors, vendored into
/// this repo and frozen by content hash. The upstream contract lives in a
/// separate repository, so the bytes — not a path to that checkout — are the
/// contract this crate is held to.
#[test]
fn frozen_outcome_receipt_vectors_hash() {
    let bytes = fs::read(fixture(OUTCOME_RECEIPT_VECTORS_FIXTURE)).expect("vendored vectors.json");
    assert_eq!(sha256_hex(&bytes), OUTCOME_RECEIPT_VECTORS_SHA256);
}

#[test]
fn frozen_outcome_memory_fixture_hash() {
    let bytes = fs::read(fixture(OUTCOME_MEMORY_FIXTURE)).expect("vendored outcome-memory.jsonl");
    assert_eq!(sha256_hex(&bytes), OUTCOME_MEMORY_SHA256);
}

/// The offline verified-run trace is frozen as owned bytes. `orchestrate.rs`
/// asserts its real producer emits exactly these bytes, so this hash is the
/// byte-identical-offline-behavior contract rather than a transcription.
#[test]
fn frozen_offline_verified_run_trace_hash() {
    let bytes = fs::read(fixture(OFFLINE_TRACE_FIXTURE)).expect("offline trace fixture");
    assert_eq!(sha256_hex(&bytes), OFFLINE_TRACE_SHA256);
    // Offline means offline: no settle mode, no economics leaked into the trace.
    let body = String::from_utf8(bytes).expect("offline trace is utf-8");
    assert!(body.contains("\"mode\": \"offline\""));
    assert!(body.contains("\"settle\": false"));
    assert!(!body.contains("price_paid_frac"));
}

/// Portability guard: no in-scope source or owned fixture may reach outside the
/// checkout via an absolute machine path or a hard-coded home directory.
#[test]
fn owned_sources_and_fixtures_have_no_machine_specific_paths() {
    // Needles are split so this test's own source does not match them.
    let needles = [
        concat!("/Us", "ers/"),
        concat!("/ho", "me/"),
        concat!("fractal-", "projects/"),
    ];
    let mut scanned = 0usize;
    for rel in [
        "src/chain_client19.rs",
        "src/orchestrate.rs",
        "src/dataevol.rs",
        "tests/capability_settlement19.rs",
    ] {
        let body = fs::read_to_string(repo_root().join(rel)).expect(rel);
        for needle in needles {
            assert!(
                !body.contains(needle),
                "{rel} embeds machine-specific path fragment {needle}"
            );
        }
        scanned += 1;
    }
    assert_eq!(scanned, 4);

    for name in [
        OUTCOME_RECEIPT_VECTORS_FIXTURE,
        OUTCOME_MEMORY_FIXTURE,
        OFFLINE_TRACE_FIXTURE,
    ] {
        let path = fixture(name);
        assert!(path.is_file(), "missing owned fixture {name}");
        assert!(
            path.starts_with(repo_root()),
            "fixture {name} resolves outside the checkout"
        );
    }
}

#[test]
fn owned_sources_present() {
    for rel in [
        "src/dataevol.rs",
        "src/orchestrate.rs",
        "src/chain_client19.rs",
        "docs/capability-settlement19.md",
    ] {
        let path = repo_root().join(rel);
        assert!(path.is_file(), "missing owned path {rel}");
    }
    let dataevol = fs::read_to_string(repo_root().join("src/dataevol.rs")).unwrap();
    assert!(dataevol.contains("price_paid_frac"));
    assert!(dataevol.contains("harness_revision"));
    assert!(dataevol.contains("dataset_lineage_root"));
    assert!(dataevol.contains("fallback_used"));
    let orch = fs::read_to_string(repo_root().join("src/orchestrate.rs")).unwrap();
    assert!(orch.contains("chain_client19"));
    assert!(orch.contains("maybe_settle_verified_outcome"));
    let client = fs::read_to_string(repo_root().join("src/chain_client19.rs")).unwrap();
    assert!(client.contains("SettleOutcomeReceipt"));
    assert!(client.contains("NATIVE_CALL_SETTLE_OUTCOME_RECEIPT"));
}
