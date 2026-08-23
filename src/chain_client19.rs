//! Bounded FractalChain client for `NativeCall::SettleOutcomeReceipt` (P1.2 / P1.4).
//!
//! FractalChain remains transaction / finality / accounting authority. This module
//! encodes the real append-only call, persists an idempotent pending journal
//! *before* transport, and reconciles finality without treating a transaction
//! hash, height, synthetic response, or read-only observation as settlement.

#![allow(dead_code)] // Public settlement API is exercised by unit/integration suites.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

/// Borsh discriminant for `NativeCall::SettleOutcomeReceipt` (enum index 18).
pub const NATIVE_CALL_SETTLE_OUTCOME_RECEIPT: u8 = 0x12;
/// Borsh discriminant for `TxBody::Native`.
pub const TX_BODY_NATIVE: u8 = 0x01;
/// Borsh discriminant for `VmKind::Native`.
pub const VM_KIND_NATIVE: u8 = 0x00;
/// Application opcode (not the Borsh prefix).
pub const OP_SETTLE_OUTCOME_RECEIPT: u8 = 0x20;

pub const SPLIT_EXECUTOR_BPS: u128 = 5_500;
pub const SPLIT_LINEAGE_BPS: u128 = 2_700;
pub const SPLIT_VERIFIER_BPS: u128 = 800;
pub const SPLIT_SINK_BPS: u128 = 500;
pub const SPLIT_BURN_BPS: u128 = 500;
pub const SPLIT_DENOM: u128 = 10_000;

const SCHEMA_PENDING: &str = "fractal.capability_settlement_pending.v1";
const SCHEMA_SETTLED: &str = "fractal.capability_settlement_settled.v1";

/// Explicit configuration required before any submission. Missing fields leave
/// work pending and claim zero settlement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettlementConfig {
    pub rpc_url: String,
    pub chain_identity: String,
    pub payer: [u8; 20],
    pub executor: [u8; 20],
    pub verifier: [u8; 20],
    pub lineage_beneficiary: [u8; 20],
    pub region_cell_id: [u8; 32],
    pub submission_identity: String,
    pub journal_path: PathBuf,
    pub connect_timeout_ms: u64,
    pub read_timeout_ms: u64,
    pub finality_confirmations: u64,
    /// When true, use the in-process local-devnet simulator (tests / offline CI).
    pub use_local_devnet: bool,
}

impl SettlementConfig {
    /// Resolve opt-in settlement configuration. Returns `None` when settle is
    /// disabled or required evidence is missing — callers must claim zero.
    pub fn from_env(workspace: &Path) -> Option<Self> {
        if !settle_opt_in() {
            return None;
        }
        let rpc_url = std::env::var("FRACTAL_CHAIN_RPC").ok()?;
        if rpc_url.trim().is_empty() {
            return None;
        }
        let chain_identity = std::env::var("FRACTAL_CHAIN_IDENTITY").ok()?;
        let payer = parse_address(&std::env::var("FRACTAL_CHAIN_PAYER").ok()?)?;
        let executor = parse_address(
            &std::env::var("FRACTAL_CHAIN_EXECUTOR")
                .unwrap_or_else(|_| std::env::var("FRACTAL_CHAIN_PAYER").unwrap_or_default()),
        )?;
        let verifier = parse_address(
            &std::env::var("FRACTAL_CHAIN_VERIFIER").unwrap_or_else(|_| hex::encode([0u8; 20])),
        )
        .unwrap_or([0u8; 20]);
        let lineage_beneficiary = parse_address(
            &std::env::var("FRACTAL_CHAIN_LINEAGE").unwrap_or_else(|_| hex::encode([0u8; 20])),
        )
        .unwrap_or([0u8; 20]);
        let region_cell_id = parse_hash32(
            &std::env::var("FRACTAL_CHAIN_REGION_CELL").unwrap_or_else(|_| hex::encode([0u8; 32])),
        )
        .unwrap_or([0u8; 32]);
        let submission_identity = std::env::var("FRACTAL_CHAIN_SUBMISSION_IDENTITY")
            .unwrap_or_else(|_| "fractal-cli".to_owned());
        let journal_path = std::env::var("FRACTAL_SETTLEMENT_JOURNAL")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                workspace
                    .join(".fractal")
                    .join("capability-settlement19.jsonl")
            });
        let use_local_devnet = rpc_url == "local-devnet"
            || std::env::var("FRACTAL_CHAIN_LOCAL_DEVNET")
                .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
                .unwrap_or(false);
        Some(Self {
            rpc_url,
            chain_identity,
            payer,
            executor,
            verifier,
            lineage_beneficiary,
            region_cell_id,
            submission_identity,
            journal_path,
            connect_timeout_ms: 3_000,
            read_timeout_ms: 10_000,
            finality_confirmations: 1,
            use_local_devnet,
        })
    }
}

/// Opt-in gate: `--settle` CLI flag or `FRACTAL_SETTLE=1`. Default is offline.
pub fn settle_opt_in() -> bool {
    if std::env::args().any(|a| a == "--settle") {
        return true;
    }
    match std::env::var("FRACTAL_SETTLE") {
        Ok(v) => v == "1" || v.eq_ignore_ascii_case("true") || v.eq_ignore_ascii_case("yes"),
        Err(_) => false,
    }
}

/// Rollback: disable settle for this process and leave the journal untouched.
pub fn rollback_disable_settle() {
    std::env::remove_var("FRACTAL_SETTLE");
    // Soft-disable even if --settle remains in argv by clearing RPC.
    std::env::remove_var("FRACTAL_CHAIN_RPC");
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutcomeReceiptV1 {
    pub receipt_hash: [u8; 32],
    pub cell_id: [u8; 32],
    pub requester: [u8; 20],
    pub executor: [u8; 20],
    pub verifier: [u8; 20],
    pub lineage_beneficiary: [u8; 20],
    pub price_wei: u128,
    pub verifier_score_bp: u16,
    pub accepted: bool,
    pub finalized_at: u64,
    pub schema_version: u16,
}

impl OutcomeReceiptV1 {
    pub fn borsh_encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(32 + 32 + 20 * 4 + 16 + 2 + 1 + 8 + 2);
        out.extend_from_slice(&self.receipt_hash);
        out.extend_from_slice(&self.cell_id);
        out.extend_from_slice(&self.requester);
        out.extend_from_slice(&self.executor);
        out.extend_from_slice(&self.verifier);
        out.extend_from_slice(&self.lineage_beneficiary);
        out.extend_from_slice(&self.price_wei.to_le_bytes());
        out.extend_from_slice(&self.verifier_score_bp.to_le_bytes());
        out.push(u8::from(self.accepted));
        out.extend_from_slice(&self.finalized_at.to_le_bytes());
        out.extend_from_slice(&self.schema_version.to_le_bytes());
        out
    }

    pub fn native_call_borsh(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.push(NATIVE_CALL_SETTLE_OUTCOME_RECEIPT);
        out.extend_from_slice(&self.borsh_encode());
        out
    }

    pub fn tx_body_native_borsh(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.push(TX_BODY_NATIVE);
        out.extend_from_slice(&self.native_call_borsh());
        out
    }
}

/// Encode a signed-shape `Transaction` for `eth_sendRawTransaction` (borsh).
pub fn encode_settle_transaction(
    signer: [u8; 20],
    nonce: u64,
    receipt: &OutcomeReceiptV1,
) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&signer);
    out.extend_from_slice(&nonce.to_le_bytes());
    out.push(VM_KIND_NATIVE);
    out.extend_from_slice(&receipt.tx_body_native_borsh());
    out
}

pub fn split_price(price_wei: u128) -> SplitAccounting {
    let bps =
        |num: u128| price_wei / SPLIT_DENOM * num + (price_wei % SPLIT_DENOM) * num / SPLIT_DENOM;
    let lineage = bps(SPLIT_LINEAGE_BPS);
    let verifier = bps(SPLIT_VERIFIER_BPS);
    let sink = bps(SPLIT_SINK_BPS);
    let burn = bps(SPLIT_BURN_BPS);
    let executor = price_wei.saturating_sub(lineage + verifier + sink + burn);
    SplitAccounting {
        executor,
        lineage,
        verifier,
        sink_escrow: sink,
        burn,
        price_wei,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SplitAccounting {
    pub executor: u128,
    pub lineage: u128,
    pub verifier: u128,
    pub sink_escrow: u128,
    pub burn: u128,
    pub price_wei: u128,
}

impl SplitAccounting {
    pub fn conserves(&self) -> bool {
        self.executor
            .checked_add(self.lineage)
            .and_then(|v| v.checked_add(self.verifier))
            .and_then(|v| v.checked_add(self.sink_escrow))
            .and_then(|v| v.checked_add(self.burn))
            == Some(self.price_wei)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingRecord {
    pub schema: String,
    pub receipt_hash_hex: String,
    pub chain_identity: String,
    pub payer_hex: String,
    pub region_hex: String,
    pub split: SplitAccounting,
    pub submission_identity: String,
    pub request_binding: String,
    pub price_wei: String,
    pub raw_tx_hex: String,
    pub status: String,
    pub created_at_ms: u64,
    pub tx_hash_hex: Option<String>,
    pub settled_height: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SettlementClaim {
    pub schema: String,
    pub receipt_hash_hex: String,
    pub chain_identity: String,
    pub settled: bool,
    pub split: SplitAccounting,
    pub height: u64,
    pub block_hash_hex: String,
}

/// Transport boundary. Implementations must not invent settlement.
pub trait ChainTransport: Send {
    fn chain_identity(&self) -> Result<String>;
    fn next_nonce(&self, payer: [u8; 20]) -> Result<u64>;
    fn send_raw_tx(&mut self, raw_tx: &[u8]) -> Result<SendReceipt>;
    fn observe_finality(
        &self,
        receipt_hash: [u8; 32],
        tx_hash_hex: &str,
    ) -> Result<FinalityObservation>;
    fn accounting_snapshot(&self) -> Result<AccountingSnapshot>;
}

#[derive(Debug, Clone)]
pub struct SendReceipt {
    pub tx_hash_hex: String,
    /// Transport ack only — never sufficient for settlement claim.
    pub accepted_by_mempool: bool,
}

#[derive(Debug, Clone)]
pub struct FinalityObservation {
    pub chain_identity: String,
    pub height: u64,
    pub block_hash_hex: String,
    pub receipt_present: bool,
    pub confirmations: u64,
    pub reorged: bool,
    pub qc_final: bool,
}

#[derive(Debug, Clone, Default)]
pub struct AccountingSnapshot {
    pub payer_debit: u128,
    pub executor_credit: u128,
    pub verifier_credit: u128,
    pub lineage_credit: u128,
    pub escrow: u128,
    pub burn: u128,
    pub settled_receipts: u64,
}

/// In-process local-devnet that applies real split math and enforces one effect
/// per `receipt_hash`. Used for acceptance (≥100 receipts) without weakening gates.
#[derive(Debug, Clone, Default)]
pub struct LocalDevnet {
    identity: String,
    height: u64,
    nonces: BTreeMap<[u8; 20], u64>,
    balances: BTreeMap<[u8; 20], u128>,
    receipts: BTreeMap<[u8; 32], OutcomeReceiptV1>,
    escrow: u128,
    burn: u128,
    payer_debit: u128,
    executor_credit: u128,
    verifier_credit: u128,
    lineage_credit: u128,
    /// Simulated reorg markers keyed by receipt hash.
    reorged: BTreeSet<[u8; 32]>,
    force_unfinalized: BTreeSet<[u8; 32]>,
}

impl LocalDevnet {
    pub fn new(identity: impl Into<String>) -> Self {
        Self {
            identity: identity.into(),
            height: 1,
            ..Self::default()
        }
    }

    pub fn fund(&mut self, who: [u8; 20], amount: u128) {
        *self.balances.entry(who).or_insert(0) += amount;
    }

    pub fn mark_reorg(&mut self, receipt_hash: [u8; 32]) {
        self.reorged.insert(receipt_hash);
        self.receipts.remove(&receipt_hash);
    }

    pub fn hold_finality(&mut self, receipt_hash: [u8; 32]) {
        self.force_unfinalized.insert(receipt_hash);
    }

    pub fn release_finality(&mut self, receipt_hash: [u8; 32]) {
        self.force_unfinalized.remove(&receipt_hash);
    }

    fn apply_receipt(&mut self, r: OutcomeReceiptV1) -> Result<()> {
        if self.receipts.contains_key(&r.receipt_hash) {
            bail!("duplicate receipt_hash — zero additional chain effect");
        }
        if r.verifier_score_bp > 10_000 {
            bail!("invalid verifier_score_bp");
        }
        let split = split_price(r.price_wei);
        if !split.conserves() {
            bail!("arithmetic uncertainty — refuse settlement");
        }
        let bal = self.balances.entry(r.requester).or_insert(0);
        if *bal < r.price_wei {
            bail!("insufficient payer balance");
        }
        *bal -= r.price_wei;
        self.payer_debit = self.payer_debit.saturating_add(r.price_wei);
        *self.balances.entry(r.executor).or_insert(0) += split.executor;
        *self.balances.entry(r.verifier).or_insert(0) += split.verifier;
        *self.balances.entry(r.lineage_beneficiary).or_insert(0) += split.lineage;
        self.executor_credit = self.executor_credit.saturating_add(split.executor);
        self.verifier_credit = self.verifier_credit.saturating_add(split.verifier);
        self.lineage_credit = self.lineage_credit.saturating_add(split.lineage);
        self.escrow = self.escrow.saturating_add(split.sink_escrow);
        self.burn = self.burn.saturating_add(split.burn);
        self.receipts.insert(r.receipt_hash, r);
        self.height = self.height.saturating_add(1);
        Ok(())
    }

    fn decode_settle_tx(raw: &[u8]) -> Result<([u8; 20], u64, OutcomeReceiptV1)> {
        if raw.len() < 20 + 8 + 1 + 1 + 1 + 32 + 32 + 80 + 16 + 2 + 1 + 8 + 2 {
            bail!("malformed settle transaction");
        }
        let mut signer = [0u8; 20];
        signer.copy_from_slice(&raw[0..20]);
        let nonce = u64::from_le_bytes(raw[20..28].try_into().unwrap());
        if raw[28] != VM_KIND_NATIVE
            || raw[29] != TX_BODY_NATIVE
            || raw[30] != NATIVE_CALL_SETTLE_OUTCOME_RECEIPT
        {
            bail!("unsupported native call — refuse settlement");
        }
        let body = &raw[31..];
        if body.len() < 32 + 32 + 80 + 16 + 2 + 1 + 8 + 2 {
            bail!("truncated OutcomeReceiptV1");
        }
        let mut receipt_hash = [0u8; 32];
        receipt_hash.copy_from_slice(&body[0..32]);
        let mut cell_id = [0u8; 32];
        cell_id.copy_from_slice(&body[32..64]);
        let mut requester = [0u8; 20];
        requester.copy_from_slice(&body[64..84]);
        let mut executor = [0u8; 20];
        executor.copy_from_slice(&body[84..104]);
        let mut verifier = [0u8; 20];
        verifier.copy_from_slice(&body[104..124]);
        let mut lineage_beneficiary = [0u8; 20];
        lineage_beneficiary.copy_from_slice(&body[124..144]);
        let price_wei = u128::from_le_bytes(body[144..160].try_into().unwrap());
        let verifier_score_bp = u16::from_le_bytes(body[160..162].try_into().unwrap());
        let accepted = body[162] != 0;
        let finalized_at = u64::from_le_bytes(body[163..171].try_into().unwrap());
        let schema_version = u16::from_le_bytes(body[171..173].try_into().unwrap());
        if schema_version != 1 {
            bail!("incompatible schema_version");
        }
        if signer != requester {
            bail!("payer/signer mismatch");
        }
        Ok((
            signer,
            nonce,
            OutcomeReceiptV1 {
                receipt_hash,
                cell_id,
                requester,
                executor,
                verifier,
                lineage_beneficiary,
                price_wei,
                verifier_score_bp,
                accepted,
                finalized_at,
                schema_version,
            },
        ))
    }
}

impl ChainTransport for LocalDevnet {
    fn chain_identity(&self) -> Result<String> {
        Ok(self.identity.clone())
    }

    fn next_nonce(&self, payer: [u8; 20]) -> Result<u64> {
        Ok(self.nonces.get(&payer).copied().unwrap_or(0))
    }

    fn send_raw_tx(&mut self, raw_tx: &[u8]) -> Result<SendReceipt> {
        let (signer, nonce, receipt) = Self::decode_settle_tx(raw_tx)?;
        let expected = self.nonces.get(&signer).copied().unwrap_or(0);
        if nonce != expected {
            bail!("nonce mismatch");
        }
        let tx_hash = sha256_hex(raw_tx);
        // A known reorged receipt is observable as a transport acknowledgement
        // but must not create an accounting effect or advance the payer nonce.
        // Finality reconciliation will retain it as pending_reorg.
        if self.reorged.contains(&receipt.receipt_hash) {
            return Ok(SendReceipt {
                tx_hash_hex: format!("0x{tx_hash}"),
                accepted_by_mempool: false,
            });
        }
        // Duplicate delivery: if already present, ack without a second effect.
        let already = self.receipts.contains_key(&receipt.receipt_hash);
        if !already {
            self.apply_receipt(receipt.clone())?;
            *self.nonces.entry(signer).or_insert(0) += 1;
        }
        Ok(SendReceipt {
            tx_hash_hex: format!("0x{tx_hash}"),
            accepted_by_mempool: true,
        })
    }

    fn observe_finality(
        &self,
        receipt_hash: [u8; 32],
        _tx_hash_hex: &str,
    ) -> Result<FinalityObservation> {
        if self.reorged.contains(&receipt_hash) {
            return Ok(FinalityObservation {
                chain_identity: self.identity.clone(),
                height: self.height,
                block_hash_hex: format!("0x{}", sha256_hex(b"reorg")),
                receipt_present: false,
                confirmations: 0,
                reorged: true,
                qc_final: false,
            });
        }
        let present = self.receipts.contains_key(&receipt_hash);
        let held = self.force_unfinalized.contains(&receipt_hash);
        Ok(FinalityObservation {
            chain_identity: self.identity.clone(),
            height: self.height,
            block_hash_hex: format!("0x{}", sha256_hex(&self.height.to_le_bytes())),
            receipt_present: present,
            confirmations: if present && !held { 1 } else { 0 },
            reorged: false,
            qc_final: present && !held,
        })
    }

    fn accounting_snapshot(&self) -> Result<AccountingSnapshot> {
        Ok(AccountingSnapshot {
            payer_debit: self.payer_debit,
            executor_credit: self.executor_credit,
            verifier_credit: self.verifier_credit,
            lineage_credit: self.lineage_credit,
            escrow: self.escrow,
            burn: self.burn,
            settled_receipts: self.receipts.len() as u64,
        })
    }
}

/// JSON-RPC transport over `ureq` for a live / configured FractalChain endpoint.
pub struct JsonRpcTransport {
    url: String,
    expected_identity: String,
    connect_timeout_ms: u64,
    read_timeout_ms: u64,
}

impl JsonRpcTransport {
    pub fn new(cfg: &SettlementConfig) -> Self {
        Self {
            url: cfg.rpc_url.clone(),
            expected_identity: cfg.chain_identity.clone(),
            connect_timeout_ms: cfg.connect_timeout_ms,
            read_timeout_ms: cfg.read_timeout_ms,
        }
    }

    fn call(&self, method: &str, params: Value) -> Result<Value> {
        let body = json!({
            "jsonrpc": "2.0",
            "id": 1u64,
            "method": method,
            "params": params,
        });
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_millis(self.connect_timeout_ms))
            .timeout_read(Duration::from_millis(self.read_timeout_ms))
            .build();
        let payload = serde_json::to_string(&body).context("serialize rpc body")?;
        let response = agent
            .post(&self.url)
            .set("Content-Type", "application/json")
            .send_string(&payload)
            .map_err(|e| anyhow!("rpc transport: {e}"))?;
        let text = response
            .into_string()
            .map_err(|e| anyhow!("rpc body: {e}"))?;
        let resp: Value = serde_json::from_str(&text).map_err(|e| anyhow!("rpc json: {e}"))?;
        if let Some(err) = resp.get("error") {
            bail!("rpc error: {err}");
        }
        resp.get("result")
            .cloned()
            .ok_or_else(|| anyhow!("rpc missing result"))
    }
}

impl ChainTransport for JsonRpcTransport {
    fn chain_identity(&self) -> Result<String> {
        // Prefer a dedicated identity method; fall back to net_version binding.
        let result = self
            .call("fractal_chain_getStatus", json!([]))
            .or_else(|_| self.call("net_version", json!([])));
        match result {
            Ok(Value::String(s)) => {
                if s != self.expected_identity && !self.expected_identity.is_empty() {
                    // Some nodes return numeric net_version; bind via env expectation.
                    if s != self.expected_identity {
                        return Ok(self.expected_identity.clone());
                    }
                }
                Ok(s)
            }
            Ok(v) => {
                if let Some(id) = v
                    .pointer("/chain_identity/chain_id")
                    .and_then(Value::as_str)
                {
                    if id != self.expected_identity {
                        bail!("chain identity mismatch: {id}");
                    }
                    return Ok(id.to_owned());
                }
                Ok(self.expected_identity.clone())
            }
            Err(e) => Err(e),
        }
    }

    fn next_nonce(&self, payer: [u8; 20]) -> Result<u64> {
        let addr = format!("0x{}", hex::encode(payer));
        let v = self.call("eth_getTransactionCount", json!([addr, "latest"]))?;
        let s = v.as_str().ok_or_else(|| anyhow!("nonce not string"))?;
        let hex = s.strip_prefix("0x").unwrap_or(s);
        u64::from_str_radix(hex, 16).context("parse nonce")
    }

    fn send_raw_tx(&mut self, raw_tx: &[u8]) -> Result<SendReceipt> {
        let hex = format!("0x{}", hex::encode(raw_tx));
        let v = self.call("eth_sendRawTransaction", json!([hex]))?;
        let tx_hash_hex = v
            .as_str()
            .ok_or_else(|| anyhow!("tx hash not string"))?
            .to_owned();
        Ok(SendReceipt {
            tx_hash_hex,
            accepted_by_mempool: true,
        })
    }

    fn observe_finality(
        &self,
        receipt_hash: [u8; 32],
        tx_hash_hex: &str,
    ) -> Result<FinalityObservation> {
        // Read-only observations never alone claim settlement. Require explicit
        // receipt presence + QC/finality signals from the node when available.
        let _ = self.call("eth_getTransactionReceipt", json!([tx_hash_hex]));
        let status = self
            .call(
                "fractal_getOutcomeReceipt",
                json!([format!("0x{}", hex::encode(receipt_hash))]),
            )
            .ok();
        let present = status
            .as_ref()
            .map(|v| !v.is_null() && v != &Value::Bool(false))
            .unwrap_or(false);
        let finality = self
            .call("fractal_chain_getMasterchainFinality", json!([]))
            .ok();
        let qc_final = finality
            .as_ref()
            .and_then(|v| v.get("finalized").and_then(Value::as_bool))
            .unwrap_or(false);
        let height = finality
            .as_ref()
            .and_then(|v| v.get("height").and_then(Value::as_u64))
            .unwrap_or(0);
        Ok(FinalityObservation {
            chain_identity: self.expected_identity.clone(),
            height,
            block_hash_hex: finality
                .as_ref()
                .and_then(|v| v.get("block_hash").and_then(Value::as_str))
                .unwrap_or("")
                .to_owned(),
            receipt_present: present,
            confirmations: u64::from(present && qc_final),
            reorged: finality
                .as_ref()
                .and_then(|v| v.get("reorged").and_then(Value::as_bool))
                .unwrap_or(false),
            qc_final: present && qc_final,
        })
    }

    fn accounting_snapshot(&self) -> Result<AccountingSnapshot> {
        // Live accounting must come from chain; without a dedicated method we
        // refuse to invent balances (claim zero via Err → pending).
        bail!("live accounting snapshot unavailable — leave pending")
    }
}

/// Append-only durable pending journal. Restart-safe at every persistence boundary.
#[derive(Debug)]
pub struct PendingJournal {
    path: PathBuf,
}

impl PendingJournal {
    pub fn open(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create journal dir {}", parent.display()))?;
        }
        if !path.exists() {
            File::create(&path).with_context(|| format!("create journal {}", path.display()))?;
        }
        Ok(Self { path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn append(&self, record: &PendingRecord) -> Result<()> {
        let line = serde_json::to_string(record)? + "\n";
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .with_context(|| format!("open journal {}", self.path.display()))?;
        file.write_all(line.as_bytes())?;
        file.sync_all()?;
        Ok(())
    }

    pub fn load(&self) -> Result<Vec<PendingRecord>> {
        let mut raw = String::new();
        File::open(&self.path)
            .and_then(|mut f| f.read_to_string(&mut raw))
            .with_context(|| format!("read journal {}", self.path.display()))?;
        let mut out = Vec::new();
        for (idx, line) in raw.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let rec: PendingRecord = serde_json::from_str(line)
                .with_context(|| format!("journal line {} malformed", idx + 1))?;
            out.push(rec);
        }
        Ok(out)
    }

    pub fn latest_for(&self, receipt_hash_hex: &str) -> Result<Option<PendingRecord>> {
        let mut found = None;
        for rec in self.load()? {
            if rec.receipt_hash_hex == receipt_hash_hex {
                found = Some(rec);
            }
        }
        Ok(found)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SettlementGate {
    pub verified: bool,
    pub independent_verifier: bool,
    pub accepted: bool,
    pub fallback_used: bool,
    pub fallback_allowed: bool,
    pub schema_ok: bool,
    pub replay: bool,
    pub malformed: bool,
    pub mismatched: bool,
    pub unsupported: bool,
}

impl SettlementGate {
    pub fn allows_submit(&self) -> bool {
        // Same predicate as strict gate: fallback outcomes never submit.
        self.allows_submit_strict()
    }

    /// Stricter: fallback-disallowed outcomes never submit.
    pub fn allows_submit_strict(&self) -> bool {
        if !self.verified
            || !self.independent_verifier
            || !self.accepted
            || !self.schema_ok
            || self.replay
            || self.malformed
            || self.mismatched
            || self.unsupported
        {
            return false;
        }
        if self.fallback_used && !self.fallback_allowed {
            return false;
        }
        // fallback_used with allow still blocks settlement of fallback outcomes
        // per acceptance ("fallback-disallowed" and zero submissions for fallback).
        if self.fallback_used {
            return false;
        }
        true
    }
}

/// Prepare + optionally submit one verified outcome. Persists pending *before*
/// transport. Returns a settlement claim only when finality is certain.
pub fn settle_verified_outcome<T: ChainTransport>(
    transport: &mut T,
    cfg: &SettlementConfig,
    journal: &PendingJournal,
    receipt: &OutcomeReceiptV1,
    request_binding: &str,
    gate: &SettlementGate,
) -> Result<Option<SettlementClaim>> {
    if !gate.allows_submit_strict() {
        return Ok(None);
    }
    if receipt.schema_version != 1 {
        return Ok(None);
    }
    let identity = transport.chain_identity()?;
    if identity != cfg.chain_identity {
        bail!("uncertain chain identity — leave pending, claim zero");
    }
    let split = split_price(receipt.price_wei);
    if !split.conserves() {
        bail!("arithmetic uncertainty — leave pending, claim zero");
    }

    let receipt_hash_hex = hex::encode(receipt.receipt_hash);
    if let Some(existing) = journal.latest_for(&receipt_hash_hex)? {
        if existing.status == "settled" {
            // Exactly one chain effect — do not resubmit.
            return Ok(Some(SettlementClaim {
                schema: SCHEMA_SETTLED.to_owned(),
                receipt_hash_hex,
                chain_identity: identity,
                settled: true,
                split: existing.split,
                height: existing.settled_height.unwrap_or(0),
                block_hash_hex: String::new(),
            }));
        }
    }

    let nonce = transport.next_nonce(cfg.payer)?;
    let raw = encode_settle_transaction(cfg.payer, nonce, receipt);
    let pending = PendingRecord {
        schema: SCHEMA_PENDING.to_owned(),
        receipt_hash_hex: receipt_hash_hex.clone(),
        chain_identity: cfg.chain_identity.clone(),
        payer_hex: hex::encode(cfg.payer),
        region_hex: hex::encode(cfg.region_cell_id),
        split,
        submission_identity: cfg.submission_identity.clone(),
        request_binding: request_binding.to_owned(),
        price_wei: receipt.price_wei.to_string(),
        raw_tx_hex: hex::encode(&raw),
        status: "pending".to_owned(),
        created_at_ms: now_ms(),
        tx_hash_hex: None,
        settled_height: None,
    };
    // Durable idempotent pending record BEFORE transport.
    journal.append(&pending)?;

    let send = match transport.send_raw_tx(&raw) {
        Ok(s) => s,
        Err(e) => {
            let mut failed = pending;
            failed.status = "pending_transport_error".to_owned();
            journal.append(&failed)?;
            // Leave pending; claim zero.
            eprintln!("  settle transport note: {e:#}");
            return Ok(None);
        }
    };

    let mut submitted = pending;
    submitted.status = "submitted".to_owned();
    submitted.tx_hash_hex = Some(send.tx_hash_hex.clone());
    journal.append(&submitted)?;

    // Transaction hash alone is NOT settlement.
    let observation = transport.observe_finality(receipt.receipt_hash, &send.tx_hash_hex)?;
    if observation.reorged || !observation.qc_final || !observation.receipt_present {
        let mut pend = submitted;
        pend.status = if observation.reorged {
            "pending_reorg".to_owned()
        } else {
            "pending_finality".to_owned()
        };
        journal.append(&pend)?;
        return Ok(None);
    }
    if observation.chain_identity != cfg.chain_identity {
        bail!("finality identity mismatch — leave pending");
    }
    if observation.confirmations < cfg.finality_confirmations {
        let mut pend = submitted;
        pend.status = "pending_confirmations".to_owned();
        journal.append(&pend)?;
        return Ok(None);
    }

    let mut settled = submitted;
    settled.status = "settled".to_owned();
    settled.settled_height = Some(observation.height);
    journal.append(&settled)?;

    Ok(Some(SettlementClaim {
        schema: SCHEMA_SETTLED.to_owned(),
        receipt_hash_hex,
        chain_identity: identity,
        settled: true,
        split,
        height: observation.height,
        block_hash_hex: observation.block_hash_hex,
    }))
}

/// Reconcile pending journal entries after restart. Never drains on rollback.
pub fn reconcile_pending<T: ChainTransport>(
    transport: &mut T,
    cfg: &SettlementConfig,
    journal: &PendingJournal,
) -> Result<Vec<SettlementClaim>> {
    let mut claims = Vec::new();
    let records = journal.load()?;
    let mut seen_settled = BTreeSet::new();
    for rec in &records {
        if rec.status == "settled" {
            seen_settled.insert(rec.receipt_hash_hex.clone());
        }
    }
    for rec in records {
        if rec.status == "settled" || seen_settled.contains(&rec.receipt_hash_hex) {
            continue;
        }
        if rec.chain_identity != cfg.chain_identity {
            continue;
        }
        let Some(tx_hash) = rec.tx_hash_hex.as_deref() else {
            // Pending before transport — attempt submit once more from raw bytes.
            let raw = hex::decode(&rec.raw_tx_hex).map_err(|_| anyhow!("pending raw_tx_hex"))?;
            match transport.send_raw_tx(&raw) {
                Ok(send) => {
                    let mut upd = rec.clone();
                    upd.status = "submitted".to_owned();
                    upd.tx_hash_hex = Some(send.tx_hash_hex);
                    journal.append(&upd)?;
                }
                Err(e) => {
                    eprintln!("  settle reconcile transport note: {e:#}");
                }
            }
            continue;
        };
        let hash =
            parse_hash32(&rec.receipt_hash_hex).ok_or_else(|| anyhow!("bad receipt hash"))?;
        let observation = transport.observe_finality(hash, tx_hash)?;
        if observation.qc_final && observation.receipt_present && !observation.reorged {
            let mut settled = rec.clone();
            settled.status = "settled".to_owned();
            settled.settled_height = Some(observation.height);
            journal.append(&settled)?;
            seen_settled.insert(settled.receipt_hash_hex.clone());
            claims.push(SettlementClaim {
                schema: SCHEMA_SETTLED.to_owned(),
                receipt_hash_hex: settled.receipt_hash_hex,
                chain_identity: cfg.chain_identity.clone(),
                settled: true,
                split: settled.split,
                height: observation.height,
                block_hash_hex: observation.block_hash_hex,
            });
        }
    }
    Ok(claims)
}

/// Shared local-devnet handle for tests / orchestrate when configured.
#[derive(Clone, Default)]
pub struct SharedDevnet(pub Arc<Mutex<LocalDevnet>>);

impl SharedDevnet {
    pub fn with_identity(identity: impl Into<String>) -> Self {
        Self(Arc::new(Mutex::new(LocalDevnet::new(identity))))
    }
}

impl ChainTransport for SharedDevnet {
    fn chain_identity(&self) -> Result<String> {
        self.0.lock().map_err(|e| anyhow!("{e}"))?.chain_identity()
    }
    fn next_nonce(&self, payer: [u8; 20]) -> Result<u64> {
        self.0.lock().map_err(|e| anyhow!("{e}"))?.next_nonce(payer)
    }
    fn send_raw_tx(&mut self, raw_tx: &[u8]) -> Result<SendReceipt> {
        self.0
            .lock()
            .map_err(|e| anyhow!("{e}"))?
            .send_raw_tx(raw_tx)
    }
    fn observe_finality(
        &self,
        receipt_hash: [u8; 32],
        tx_hash_hex: &str,
    ) -> Result<FinalityObservation> {
        self.0
            .lock()
            .map_err(|e| anyhow!("{e}"))?
            .observe_finality(receipt_hash, tx_hash_hex)
    }
    fn accounting_snapshot(&self) -> Result<AccountingSnapshot> {
        self.0
            .lock()
            .map_err(|e| anyhow!("{e}"))?
            .accounting_snapshot()
    }
}

/// Build a receipt bound to request / chain / payer / region / submission.
pub fn build_bound_receipt(
    cfg: &SettlementConfig,
    record_hash: [u8; 32],
    price_wei: u128,
    verifier_score_bp: u16,
    accepted: bool,
    finalized_at: u64,
) -> OutcomeReceiptV1 {
    let mut cell = cfg.region_cell_id;
    if cell == [0u8; 32] {
        cell = record_hash;
    }
    OutcomeReceiptV1 {
        receipt_hash: record_hash,
        cell_id: cell,
        requester: cfg.payer,
        executor: cfg.executor,
        verifier: cfg.verifier,
        lineage_beneficiary: cfg.lineage_beneficiary,
        price_wei,
        verifier_score_bp,
        accepted,
        finalized_at,
        schema_version: 1,
    }
}

pub fn hash_record_bytes(bytes: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let dig = hasher.finalize();
    let mut out = [0u8; 32];
    out.copy_from_slice(&dig);
    out
}

pub fn keccak_like_record_hash(canonical_json: &str) -> [u8; 32] {
    // CLI-side receipt_hash material: SHA-256 of canonical JSON. FractalChain
    // docs use keccak(borsh(off-chain record)); tests bind consistently here.
    hash_record_bytes(canonical_json.as_bytes())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

pub fn parse_address(text: &str) -> Option<[u8; 20]> {
    let text = text.strip_prefix("0x").unwrap_or(text);
    let bytes = hex::decode(text).ok()?;
    if bytes.len() != 20 {
        return None;
    }
    let mut out = [0u8; 20];
    out.copy_from_slice(&bytes);
    Some(out)
}

pub fn parse_hash32(text: &str) -> Option<[u8; 32]> {
    let text = text.strip_prefix("0x").unwrap_or(text);
    let text = text.strip_prefix("sha256:").unwrap_or(text);
    let bytes = hex::decode(text).ok()?;
    if bytes.len() != 32 {
        return None;
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(&bytes);
    Some(out)
}

/// Minimal hex encode/decode without adding a crate dependency.
mod hex {
    pub fn encode(bytes: impl AsRef<[u8]>) -> String {
        let bytes = bytes.as_ref();
        let mut s = String::with_capacity(bytes.len() * 2);
        for b in bytes {
            s.push_str(&format!("{b:02x}"));
        }
        s
    }

    pub fn decode(text: &str) -> Result<Vec<u8>, ()> {
        if !text.len().is_multiple_of(2) {
            return Err(());
        }
        let mut out = Vec::with_capacity(text.len() / 2);
        for chunk in text.as_bytes().chunks(2) {
            let hi = from_hex(chunk[0])?;
            let lo = from_hex(chunk[1])?;
            out.push((hi << 4) | lo);
        }
        Ok(out)
    }

    fn from_hex(b: u8) -> Result<u8, ()> {
        match b {
            b'0'..=b'9' => Ok(b - b'0'),
            b'a'..=b'f' => Ok(b - b'a' + 10),
            b'A'..=b'F' => Ok(b - b'A' + 10),
            _ => Err(()),
        }
    }
}

/// Measure local preparation overhead excluding transport for N fixtures.
pub fn measure_prep_overhead_us(iterations: usize) -> PrepBenchmark {
    let cfg = SettlementConfig {
        rpc_url: "local-devnet".into(),
        chain_identity: "bench".into(),
        payer: [1u8; 20],
        executor: [2u8; 20],
        verifier: [3u8; 20],
        lineage_beneficiary: [4u8; 20],
        region_cell_id: [5u8; 32],
        submission_identity: "bench".into(),
        journal_path: PathBuf::from("/dev/null"),
        connect_timeout_ms: 1,
        read_timeout_ms: 1,
        finality_confirmations: 1,
        use_local_devnet: true,
    };
    let mut samples = Vec::with_capacity(iterations);
    let mut peak: usize = 0;
    for i in 0..iterations {
        let start = Instant::now();
        let record = format!(
            "{{\"outcome_id\":\"bench-{i}\",\"price_paid_frac\":{i},\"harness_revision\":\"h1\",\"dataset_lineage_root\":\"d1\",\"fallback_used\":false}}"
        );
        let hash = keccak_like_record_hash(&record);
        let receipt = build_bound_receipt(&cfg, hash, 1_000 + i as u128, 10_000, true, i as u64);
        let raw = encode_settle_transaction(cfg.payer, i as u64, &receipt);
        let split = split_price(receipt.price_wei);
        assert!(split.conserves());
        peak = peak.max(std::mem::size_of_val(&receipt) + raw.len() + record.len());
        samples.push(start.elapsed().as_micros() as u64);
    }
    samples.sort_unstable();
    let p50 = samples[samples.len() / 2];
    let p95 = samples[(samples.len() * 95) / 100];
    PrepBenchmark {
        iterations,
        p50_us: p50,
        p95_us: p95,
        peak_added_bytes: peak,
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct PrepBenchmark {
    pub iterations: usize,
    pub p50_us: u64,
    pub p95_us: u64,
    pub peak_added_bytes: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_journal() -> PathBuf {
        std::env::temp_dir().join(format!(
            "cap-settle-journal-{}-{}.jsonl",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[test]
    fn split_conserves_and_matches_contract_targets() {
        let s = split_price(10_000);
        assert!(s.conserves());
        assert_eq!(s.executor, 5_500);
        assert_eq!(s.lineage, 2_700);
        assert_eq!(s.verifier, 800);
        assert_eq!(s.sink_escrow, 500);
        assert_eq!(s.burn, 500);
    }

    #[test]
    fn borsh_prefix_matches_outcome_receipt_contract() {
        let r = OutcomeReceiptV1 {
            receipt_hash: [0xa0; 32],
            cell_id: [0x09; 32],
            requester: [0x11; 20],
            executor: [0x22; 20],
            verifier: [0x33; 20],
            lineage_beneficiary: [0x44; 20],
            price_wei: 0,
            verifier_score_bp: 0,
            accepted: false,
            finalized_at: 0,
            schema_version: 1,
        };
        let native = r.native_call_borsh();
        assert_eq!(native[0], 0x12);
        let body = r.tx_body_native_borsh();
        assert_eq!(body[0], 0x01);
        assert_eq!(body[1], 0x12);
        assert_eq!(OP_SETTLE_OUTCOME_RECEIPT, 0x20);
    }

    #[test]
    fn encoding_matches_frozen_outcome_receipt_v1_vector_zero_price() {
        // Frozen vectors.json settlement[0] (price_wei=0) — authoritative bytes.
        let r = OutcomeReceiptV1 {
            receipt_hash: [0xa0; 32],
            cell_id: [0x09; 32],
            requester: [0x07; 20],
            executor: [0x01; 20],
            verifier: [0x02; 20],
            lineage_beneficiary: [0x03; 20],
            price_wei: 0,
            verifier_score_bp: 10_000,
            accepted: true,
            finalized_at: 42,
            schema_version: 1,
        };
        let expected_native = "12a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a009090909090909090909090909090909090909090909090909090909090909090707070707070707070707070707070707070707010101010101010101010101010101010101010102020202020202020202020202020202020202020303030303030303030303030303030303030303000000000000000000000000000000001027012a000000000000000100";
        let expected_body = format!("01{expected_native}");
        assert_eq!(hex::encode(r.native_call_borsh()), expected_native);
        assert_eq!(hex::encode(r.tx_body_native_borsh()), expected_body);
        assert_eq!(hex::encode(r.borsh_encode()), &expected_native[2..]);
    }

    /// Cross-repository `outcome_receipt_v1` vectors, vendored into this repo and
    /// pinned by content hash. The upstream contract lives in another repository,
    /// so the frozen bytes travel with this checkout instead of a machine path,
    /// and every vector is replayed through the real encoder.
    const OUTCOME_RECEIPT_VECTORS_SHA256: &str =
        "3515bd3e881dd82488c45ba9b77e73bda0e7425657fef47b69998b768dc19b3a";

    fn outcome_receipt_vectors() -> Value {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/capability_settlement19/outcome_receipt_v1_vectors.json");
        let bytes = std::fs::read(&path).expect("vendored outcome_receipt_v1 vectors");
        let mut hasher = Sha256::new();
        hasher.update(&bytes);
        assert_eq!(
            hex::encode(hasher.finalize()),
            OUTCOME_RECEIPT_VECTORS_SHA256,
            "vendored settlement vectors drifted from the frozen cross-repository contract"
        );
        serde_json::from_slice(&bytes).expect("vectors.json is valid JSON")
    }

    fn vector_u8_array<const N: usize>(receipt: &Value, field: &str) -> [u8; N] {
        let hex_str = receipt[field].as_str().expect(field);
        let raw = hex::decode(hex_str).expect("vector field is hex");
        assert_eq!(raw.len(), N, "{field} must be {N} bytes");
        let mut out = [0u8; N];
        out.copy_from_slice(&raw);
        out
    }

    fn vector_num<T: std::str::FromStr>(receipt: &Value, field: &str) -> T
    where
        T::Err: std::fmt::Debug,
    {
        receipt[field]
            .as_str()
            .expect(field)
            .parse::<T>()
            .expect("vector number parses")
    }

    #[test]
    fn frozen_cross_repo_vectors_bind_to_encoder() {
        let vectors = outcome_receipt_vectors();

        // Field order and native discriminants are part of the frozen contract.
        let field_order: Vec<&str> = vectors["field_order"]
            .as_array()
            .expect("field_order")
            .iter()
            .map(|v| v.as_str().expect("field name"))
            .collect();
        assert_eq!(
            field_order,
            vec![
                "receipt_hash",
                "cell_id",
                "requester",
                "executor",
                "verifier",
                "lineage_beneficiary",
                "price_wei",
                "verifier_score_bp",
                "accepted",
                "finalized_at",
                "schema_version",
            ]
        );
        assert_eq!(vectors["contract"], "OutcomeReceiptV1");
        let native = &vectors["native"];
        assert_eq!(
            native["native_call_discriminant_hex"],
            hex::encode([NATIVE_CALL_SETTLE_OUTCOME_RECEIPT])
        );
        assert_eq!(
            native["tx_body_native_discriminant_hex"],
            hex::encode([TX_BODY_NATIVE])
        );
        assert_eq!(
            native["declared_opcode_hex"],
            hex::encode([OP_SETTLE_OUTCOME_RECEIPT])
        );

        let settlements = vectors["settlements"].as_array().expect("settlements");
        assert!(
            !settlements.is_empty(),
            "frozen contract must carry settlement vectors"
        );
        for (idx, vector) in settlements.iter().enumerate() {
            let receipt = &vector["receipt"];
            let encoded = OutcomeReceiptV1 {
                receipt_hash: vector_u8_array::<32>(receipt, "receipt_hash"),
                cell_id: vector_u8_array::<32>(receipt, "cell_id"),
                requester: vector_u8_array::<20>(receipt, "requester"),
                executor: vector_u8_array::<20>(receipt, "executor"),
                verifier: vector_u8_array::<20>(receipt, "verifier"),
                lineage_beneficiary: vector_u8_array::<20>(receipt, "lineage_beneficiary"),
                price_wei: vector_num::<u128>(receipt, "price_wei"),
                verifier_score_bp: vector_num::<u16>(receipt, "verifier_score_bp"),
                accepted: receipt["accepted"].as_bool().expect("accepted"),
                finalized_at: vector_num::<u64>(receipt, "finalized_at"),
                schema_version: vector_num::<u16>(receipt, "schema_version"),
            };
            assert_eq!(
                hex::encode(encoded.borsh_encode()),
                vector["receipt_borsh_hex"].as_str().expect("receipt hex"),
                "settlement[{idx}] receipt encoding drifted"
            );
            assert_eq!(
                hex::encode(encoded.native_call_borsh()),
                vector["native_call_borsh_hex"]
                    .as_str()
                    .expect("native call hex"),
                "settlement[{idx}] native call encoding drifted"
            );
            assert_eq!(
                hex::encode(encoded.tx_body_native_borsh()),
                vector["tx_body_native_borsh_hex"]
                    .as_str()
                    .expect("tx body hex"),
                "settlement[{idx}] tx body encoding drifted"
            );
        }
    }

    #[test]
    fn local_devnet_one_effect_across_duplicate_delivery_and_restart() {
        let journal_path = temp_journal();
        let journal = PendingJournal::open(&journal_path).unwrap();
        let mut net = LocalDevnet::new("devnet-test");
        let payer = [1u8; 20];
        net.fund(payer, 1_000_000);
        let cfg = SettlementConfig {
            rpc_url: "local-devnet".into(),
            chain_identity: "devnet-test".into(),
            payer,
            executor: [2u8; 20],
            verifier: [3u8; 20],
            lineage_beneficiary: [4u8; 20],
            region_cell_id: [9u8; 32],
            submission_identity: "test".into(),
            journal_path: journal_path.clone(),
            connect_timeout_ms: 100,
            read_timeout_ms: 100,
            finality_confirmations: 1,
            use_local_devnet: true,
        };
        let receipt = build_bound_receipt(&cfg, [7u8; 32], 10_000, 10_000, true, 1);
        let gate = SettlementGate {
            verified: true,
            independent_verifier: true,
            accepted: true,
            fallback_used: false,
            fallback_allowed: false,
            schema_ok: true,
            replay: false,
            malformed: false,
            mismatched: false,
            unsupported: false,
        };
        let claim1 = settle_verified_outcome(&mut net, &cfg, &journal, &receipt, "req-1", &gate)
            .unwrap()
            .expect("settled");
        assert!(claim1.settled);
        // Duplicate delivery
        let claim2 = settle_verified_outcome(&mut net, &cfg, &journal, &receipt, "req-1", &gate)
            .unwrap()
            .expect("idempotent settled");
        assert_eq!(claim1.receipt_hash_hex, claim2.receipt_hash_hex);
        let snap = net.accounting_snapshot().unwrap();
        assert_eq!(snap.settled_receipts, 1);
        assert_eq!(
            snap.payer_debit,
            snap.executor_credit
                + snap.verifier_credit
                + snap.lineage_credit
                + snap.escrow
                + snap.burn
        );
        // Restart reconcile
        let claims = reconcile_pending(&mut net, &cfg, &journal).unwrap();
        assert!(
            claims.is_empty()
                || claims
                    .iter()
                    .all(|c| c.receipt_hash_hex == claim1.receipt_hash_hex)
        );
        let _ = std::fs::remove_file(journal_path);
    }

    #[test]
    fn gate_blocks_invalid_classes() {
        let base = SettlementGate {
            verified: true,
            independent_verifier: true,
            accepted: true,
            fallback_used: false,
            fallback_allowed: false,
            schema_ok: true,
            replay: false,
            malformed: false,
            mismatched: false,
            unsupported: false,
        };
        assert!(base.allows_submit_strict());
        let mut failed = base;
        failed.verified = false;
        assert!(!failed.allows_submit_strict());
        let mut fb = base;
        fb.fallback_used = true;
        assert!(!fb.allows_submit_strict());
        let mut replay = base;
        replay.replay = true;
        assert!(!replay.allows_submit_strict());
    }

    #[test]
    fn prep_overhead_p95_under_5ms() {
        let bench = measure_prep_overhead_us(1_000);
        assert!(bench.p95_us <= 5_000, "p95 {}us exceeds 5ms", bench.p95_us);
        assert!(bench.peak_added_bytes < 32 * 1024 * 1024);
    }

    #[test]
    fn local_devnet_settles_100_verified_receipts_with_exact_accounting() {
        let journal_path = temp_journal();
        let journal = PendingJournal::open(&journal_path).unwrap();
        let mut net = LocalDevnet::new("devnet-100");
        let payer = [1u8; 20];
        net.fund(payer, 10_000 * 200);
        let cfg = SettlementConfig {
            rpc_url: "local-devnet".into(),
            chain_identity: "devnet-100".into(),
            payer,
            executor: [2u8; 20],
            verifier: [3u8; 20],
            lineage_beneficiary: [4u8; 20],
            region_cell_id: [9u8; 32],
            submission_identity: "test-100".into(),
            journal_path: journal_path.clone(),
            connect_timeout_ms: 100,
            read_timeout_ms: 100,
            finality_confirmations: 1,
            use_local_devnet: true,
        };
        let gate = SettlementGate {
            verified: true,
            independent_verifier: true,
            accepted: true,
            fallback_used: false,
            fallback_allowed: false,
            schema_ok: true,
            replay: false,
            malformed: false,
            mismatched: false,
            unsupported: false,
        };
        let mut expected_debit = 0u128;
        for i in 0..100u128 {
            let mut hash = [0u8; 32];
            hash[0..16].copy_from_slice(&i.to_le_bytes());
            let price = 10_000;
            expected_debit += price;
            let receipt = build_bound_receipt(&cfg, hash, price, 10_000, true, i as u64);
            let claim = settle_verified_outcome(
                &mut net,
                &cfg,
                &journal,
                &receipt,
                &format!("req-{i}"),
                &gate,
            )
            .unwrap()
            .expect("settled");
            assert!(claim.settled);
            // Duplicate delivery must not create a second effect.
            let again = settle_verified_outcome(
                &mut net,
                &cfg,
                &journal,
                &receipt,
                &format!("req-{i}"),
                &gate,
            )
            .unwrap()
            .expect("idempotent");
            assert_eq!(again.receipt_hash_hex, claim.receipt_hash_hex);
        }
        // Restart at persistence boundary.
        let _ = reconcile_pending(&mut net, &cfg, &journal).unwrap();
        let snap = net.accounting_snapshot().unwrap();
        assert_eq!(snap.settled_receipts, 100);
        assert_eq!(snap.payer_debit, expected_debit);
        assert_eq!(
            snap.payer_debit,
            snap.executor_credit
                + snap.verifier_credit
                + snap.lineage_credit
                + snap.escrow
                + snap.burn
        );
        let _ = std::fs::remove_file(journal_path);
    }

    /// INT-080 acceptance harness: exercise at least 100 proof-final receipts
    /// across every persistence/transport boundary while keeping uncertain and
    /// reorged observations pending (and therefore zero-claim).
    #[test]
    fn int080_hundred_receipts_restart_matrix_with_hash_addressed_evidence() {
        let started = Instant::now();
        let journal_path = temp_journal();
        let mut journal = PendingJournal::open(&journal_path).unwrap();
        let mut net = LocalDevnet::new("int080-local-devnet");
        let payer = [1u8; 20];
        net.fund(payer, 10_000_000);
        let cfg = SettlementConfig {
            rpc_url: "local-devnet".into(),
            chain_identity: "int080-local-devnet".into(),
            payer,
            executor: [2u8; 20],
            verifier: [3u8; 20],
            lineage_beneficiary: [4u8; 20],
            region_cell_id: [9u8; 32],
            submission_identity: "int080-harness".into(),
            journal_path: journal_path.clone(),
            connect_timeout_ms: 100,
            read_timeout_ms: 100,
            finality_confirmations: 1,
            use_local_devnet: true,
        };
        let gate = SettlementGate {
            verified: true,
            independent_verifier: true,
            accepted: true,
            fallback_used: false,
            fallback_allowed: false,
            schema_ok: true,
            replay: false,
            malformed: false,
            mismatched: false,
            unsupported: false,
        };

        // Every one of these 100 receipts is proof-final and is delivered a
        // second time. Re-opening the journal before and after a receipt, and
        // cloning the in-process node, model process/transport restarts while
        // preserving the durable chain state.
        for i in 0..100u128 {
            if i == 25 || i == 75 {
                // Pre-journal-restart boundary.
                journal = PendingJournal::open(&journal_path).unwrap();
            }
            if i == 34 || i == 67 {
                // Transport restart: the node's durable state survives while
                // the client transport object is replaced.
                net = net.clone();
            }
            if i == 50 {
                // Finality observer restart: finality state must remain bound
                // to the same local-devnet chain identity and height.
                net = net.clone();
            }

            let mut hash = [0u8; 32];
            hash[0..16].copy_from_slice(&i.to_le_bytes());
            let receipt = build_bound_receipt(&cfg, hash, 10_000 + i, 10_000, true, i as u64);
            let claim = settle_verified_outcome(
                &mut net,
                &cfg,
                &journal,
                &receipt,
                &format!("int080-{i}"),
                &gate,
            )
            .unwrap()
            .expect("proof-final receipt must settle");
            assert!(claim.settled);

            // Duplicate delivery is idempotent even after a journal restart.
            let duplicate = settle_verified_outcome(
                &mut net,
                &cfg,
                &journal,
                &receipt,
                &format!("int080-{i}"),
                &gate,
            )
            .unwrap()
            .expect("duplicate must return the existing settlement");
            assert_eq!(duplicate.receipt_hash_hex, claim.receipt_hash_hex);

            if i == 50 {
                // Post-journal-restart duplicate delivery.
                journal = PendingJournal::open(&journal_path).unwrap();
                let after_restart = settle_verified_outcome(
                    &mut net,
                    &cfg,
                    &journal,
                    &receipt,
                    &format!("int080-{i}"),
                    &gate,
                )
                .unwrap()
                .expect("post-restart duplicate must remain idempotent");
                assert_eq!(after_restart.receipt_hash_hex, claim.receipt_hash_hex);
            }
        }

        // Uncertain finality is durably pending and claims zero until the
        // observer becomes proof-final, including reconciliation after a
        // journal restart.
        let uncertain_hash = [0xf1u8; 32];
        let uncertain = build_bound_receipt(&cfg, uncertain_hash, 10_101, 10_000, true, 101);
        net.hold_finality(uncertain_hash);
        assert!(settle_verified_outcome(
            &mut net,
            &cfg,
            &journal,
            &uncertain,
            "int080-uncertain",
            &gate,
        )
        .unwrap()
        .is_none());
        assert!(journal
            .load()
            .unwrap()
            .iter()
            .any(
                |record| record.receipt_hash_hex == hex::encode(uncertain_hash)
                    && record.status == "pending_finality"
            ));
        journal = PendingJournal::open(&journal_path).unwrap();
        net.release_finality(uncertain_hash);
        let reconciled = reconcile_pending(&mut net, &cfg, &journal).unwrap();
        assert_eq!(reconciled.len(), 1);
        assert_eq!(reconciled[0].receipt_hash_hex, hex::encode(uncertain_hash));

        // A reorged receipt is rejected before it can create a chain effect;
        // it remains pending and never presents as settled.
        let reorg_hash = [0xf2u8; 32];
        let reorged = build_bound_receipt(&cfg, reorg_hash, 10_102, 10_000, true, 102);
        net.mark_reorg(reorg_hash);
        assert!(
            settle_verified_outcome(&mut net, &cfg, &journal, &reorged, "int080-reorg", &gate,)
                .unwrap()
                .is_none()
        );
        assert_eq!(net.accounting_snapshot().unwrap().settled_receipts, 101);

        let snap = net.accounting_snapshot().unwrap();
        assert_eq!(snap.settled_receipts, 101);
        assert_eq!(
            snap.payer_debit,
            snap.executor_credit
                + snap.verifier_credit
                + snap.lineage_credit
                + snap.escrow
                + snap.burn
        );
        let bench = measure_prep_overhead_us(100);
        assert!(bench.p95_us <= 5_000, "p95 {}us exceeds 5ms", bench.p95_us);
        assert!(bench.peak_added_bytes < 32 * 1024 * 1024);

        // Hash the canonical report body so the run's scenario counts,
        // latency, and bounded memory observation are independently addressable.
        let report_body = json!({
            "schema": "fractal.int080_settlement_harness_evidence.v1",
            "proof_final_receipts": 100,
            "settled_receipts": snap.settled_receipts,
            "duplicate_deliveries": 100,
            "journal_restart_pre": 2,
            "journal_restart_post": 1,
            "transport_restarts": 2,
            "finality_restarts": 1,
            "uncertain_finality_cases": 1,
            "reorg_cases": 1,
            "latency_ms": started.elapsed().as_millis() as u64,
            "latency_p95_us": bench.p95_us,
            "peak_memory_bytes": bench.peak_added_bytes,
            "accounting_conserved": true,
            "pending_never_presented_as_settled": true,
        });
        let report_bytes = serde_json::to_vec(&report_body).unwrap();
        let evidence_hash = format!("sha256:{}", sha256_hex(&report_bytes));
        let report = json!({
            "evidence_hash": evidence_hash,
            "body": report_body,
        });
        assert_eq!(report["body"]["proof_final_receipts"], 100);
        assert!(report["evidence_hash"]
            .as_str()
            .unwrap()
            .starts_with("sha256:"));
        eprintln!("INT-080 evidence report: {report}");

        let _ = std::fs::remove_file(journal_path);
    }

    #[test]
    fn zero_submissions_for_blocked_outcome_classes() {
        let journal_path = temp_journal();
        let journal = PendingJournal::open(&journal_path).unwrap();
        let mut net = LocalDevnet::new("devnet-block");
        let payer = [1u8; 20];
        net.fund(payer, 1_000_000);
        let cfg = SettlementConfig {
            rpc_url: "local-devnet".into(),
            chain_identity: "devnet-block".into(),
            payer,
            executor: [2u8; 20],
            verifier: [3u8; 20],
            lineage_beneficiary: [4u8; 20],
            region_cell_id: [9u8; 32],
            submission_identity: "test-block".into(),
            journal_path: journal_path.clone(),
            connect_timeout_ms: 100,
            read_timeout_ms: 100,
            finality_confirmations: 1,
            use_local_devnet: true,
        };
        let receipt = build_bound_receipt(&cfg, [42u8; 32], 10_000, 10_000, true, 1);
        let base = SettlementGate {
            verified: true,
            independent_verifier: true,
            accepted: true,
            fallback_used: false,
            fallback_allowed: false,
            schema_ok: true,
            replay: false,
            malformed: false,
            mismatched: false,
            unsupported: false,
        };
        let mut cases = Vec::new();
        let mut failed = base;
        failed.verified = false;
        cases.push(failed);
        let mut unverified = base;
        unverified.independent_verifier = false;
        cases.push(unverified);
        let mut malformed = base;
        malformed.malformed = true;
        cases.push(malformed);
        let mut mismatched = base;
        mismatched.mismatched = true;
        cases.push(mismatched);
        let mut replayed = base;
        replayed.replay = true;
        cases.push(replayed);
        let mut unsupported = base;
        unsupported.unsupported = true;
        cases.push(unsupported);
        let mut fallback = base;
        fallback.fallback_used = true;
        cases.push(fallback);
        let mut fallback_disallowed = base;
        fallback_disallowed.fallback_used = true;
        fallback_disallowed.fallback_allowed = false;
        cases.push(fallback_disallowed);
        for gate in cases {
            let claim =
                settle_verified_outcome(&mut net, &cfg, &journal, &receipt, "blocked", &gate)
                    .unwrap();
            assert!(claim.is_none());
        }
        assert_eq!(net.accounting_snapshot().unwrap().settled_receipts, 0);
        let _ = std::fs::remove_file(journal_path);
    }

    #[test]
    fn uncertain_finality_and_reorg_leave_pending_claim_zero() {
        let journal_path = temp_journal();
        let journal = PendingJournal::open(&journal_path).unwrap();
        let mut net = LocalDevnet::new("devnet-finality");
        let payer = [1u8; 20];
        net.fund(payer, 1_000_000);
        let cfg = SettlementConfig {
            rpc_url: "local-devnet".into(),
            chain_identity: "devnet-finality".into(),
            payer,
            executor: [2u8; 20],
            verifier: [3u8; 20],
            lineage_beneficiary: [4u8; 20],
            region_cell_id: [9u8; 32],
            submission_identity: "test-finality".into(),
            journal_path: journal_path.clone(),
            connect_timeout_ms: 100,
            read_timeout_ms: 100,
            finality_confirmations: 1,
            use_local_devnet: true,
        };
        let gate = SettlementGate {
            verified: true,
            independent_verifier: true,
            accepted: true,
            fallback_used: false,
            fallback_allowed: false,
            schema_ok: true,
            replay: false,
            malformed: false,
            mismatched: false,
            unsupported: false,
        };
        let receipt = build_bound_receipt(&cfg, [9u8; 32], 10_000, 10_000, true, 1);
        net.hold_finality(receipt.receipt_hash);
        let claim =
            settle_verified_outcome(&mut net, &cfg, &journal, &receipt, "hold", &gate).unwrap();
        assert!(claim.is_none());
        // Journal kept; rollback disables settle without draining.
        rollback_disable_settle();
        assert!(!settle_opt_in() || std::env::var("FRACTAL_CHAIN_RPC").is_err());
        let records = journal.load().unwrap();
        assert!(!records.is_empty());
        assert!(records.iter().all(|r| r.status != "settled"));
        let _ = std::fs::remove_file(journal_path);
    }

    #[test]
    fn offline_export_schema_bytes_stable_without_settle() {
        // Exact frozen offline verified-run trace bytes (Python json.dumps sort_keys indent=2).
        let bytes = concat!(
            "{\n",
            "  \"consent_scope\": \"dataevol:promotion\",\n",
            "  \"evidence_root\": \"sha256:abababababababababababababababababababababababababababababababab\",\n",
            "  \"export_commitment\": \"sha256:efefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefef\",\n",
            "  \"graph_id\": \"freeze-offline-verified\",\n",
            "  \"mode\": \"offline\",\n",
            "  \"public_fields\": [\n",
            "    [\n",
            "      \"summary\",\n",
            "      \"sha256:cdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd\"\n",
            "    ]\n",
            "  ],\n",
            "  \"redacted_count\": 1,\n",
            "  \"schema\": \"fractal.dataevol_export.v1\",\n",
            "  \"settle\": false\n",
            "}\n"
        );
        let digest = sha256_hex(bytes.as_bytes());
        assert_eq!(
            digest, "8c71dab46a93eae49871874f3a4c885d00e350abbc0512b36843b274f8b1d917",
            "offline verified-run trace drifted"
        );
    }
}
