# Capability Settlement 19 (FRAC Capability Economy P1.1 / P1.2 / P1.4)

Bounded CLI seam that keeps **DataEvol** as outcome-normalization authority and
**FractalChain** as transaction / finality / accounting authority. Ordinary CLI
outcome export and routing remain the default; settlement is opt-in.

## Owned surfaces

- `src/dataevol.rs` — settlement fields on normalized outcomes + unknown-field preserve
- `src/orchestrate.rs` — opt-in `--settle` after the independent verification floor
- `src/chain_client19.rs` — bounded client, pending journal, local-devnet, encoding
- `tests/capability_settlement19.rs` — freeze / acceptance harness
- `docs/capability-settlement19.md` — this contract

## Frozen baselines (pre-behavior-change)

Recorded before wiring settle into the offline default path:

| Artifact | Path | SHA-256 | Bytes |
| --- | --- | --- | --- |
| `export_outcome` example | `/Users/jamesstar/fractal-cli/crates/fractal-chain/examples/export_outcome.rs` | `c85f00b05be5e7d4a1c1644aff09d31baaf41f8cc1811aa7d502bf44d31fee4f` | 4560 |
| Outcome-memory fixture | `/Users/jamesstar/.fractal/outcome-memory.jsonl` | `9c9ab40f41432fc5ba55db3b3c305167d59980d9cb7b5a16fb29c9ad15f28b05` | 55247 |
| OutcomeReceiptV1 vectors | `/Users/jamesstar/fractal-projects/fractal-economic-self-healing-coordinator-1786584339843/contracts/outcome_receipt_v1/vectors.json` | `3515bd3e881dd82488c45ba9b77e73bda0e7425657fef47b69998b768dc19b3a` | 16597 |
| OutcomeReceiptV1 schema | `.../contracts/outcome_receipt_v1/schema.json` | `76b4e0e83606df82ee5cf4781822e94f0484cb087e9e9c2e06d384f63ac1f56c` | 1834 |
| OutcomeReceiptV1 contract | `.../contracts/outcome_receipt_v1/contract.json` | `04b218bee7c7fe9733e5a4204aeb8418e660114361af3d988a033dcc5a9c1342` | 2062 |
| Offline verified-run trace | see below | `8c71dab46a93eae49871874f3a4c885d00e350abbc0512b36843b274f8b1d917` | 517 |

### Offline verified-run trace

Canonical JSON (`json.dumps(..., indent=2, sort_keys=True)` + trailing newline):

```json
{
  "consent_scope": "dataevol:promotion",
  "evidence_root": "sha256:abababababababababababababababababababababababababababababababab",
  "export_commitment": "sha256:efefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefef",
  "graph_id": "freeze-offline-verified",
  "mode": "offline",
  "public_fields": [
    [
      "summary",
      "sha256:cdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd"
    ]
  ],
  "redacted_count": 1,
  "schema": "fractal.dataevol_export.v1",
  "settle": false
}
```

Rollback must restore byte-identical offline behavior: disable `--settle` /
`FRACTAL_SETTLE`, drain nothing, preserve the append-only pending journal, and
keep the export schema above unchanged when settle is off.

## P1.1 — Normalized outcome fields

After DataEvol accepts the core wire record, the owned ingest bridge attaches:

- `price_paid_frac`
- `harness_revision`
- `dataset_lineage_root`
- `fallback_used`

Unknown payload extras are preserved. DataEvol remains authority for acceptance
of the core outcome schema.

## P1.2 / P1.4 — Bounded FractalChain client + opt-in settle

Encoding uses the real append-only call shape:

- Application opcode `OP_SETTLE_OUTCOME_RECEIPT = 0x20`
- Borsh `NativeCall::SettleOutcomeReceipt` discriminant `0x12`
- `TxBody::Native` prefix `0x01`

Opt-in via `--settle` or `FRACTAL_SETTLE=1` plus explicit env:

- `FRACTAL_CHAIN_RPC` (`local-devnet` selects the in-process simulator)
- `FRACTAL_CHAIN_IDENTITY`
- `FRACTAL_CHAIN_PAYER`
- optional `FRACTAL_CHAIN_EXECUTOR`, `FRACTAL_CHAIN_VERIFIER`, `FRACTAL_CHAIN_LINEAGE`
- optional `FRACTAL_CHAIN_REGION_CELL`, `FRACTAL_CHAIN_SUBMISSION_IDENTITY`
- optional `FRACTAL_SETTLEMENT_JOURNAL`

Flow:

1. Independent verification floor must pass (verified + independent + accepted).
2. Bind request, chain identity, payer, receipt hash, region, split, submission identity.
3. Persist durable idempotent pending journal record **before** transport.
4. Submit encoded settle tx.
5. Reconcile finality — a tx hash, height, synthetic ack, or read-only observation
   alone is **not** settlement. Require receipt presence + QC/finality signals.
6. Missing config, unavailable capability evidence, uncertain finality, reorg,
   incompatible schema, persistence failure, or arithmetic uncertainty → leave
   pending, claim **zero** settlement, keep ordinary CLI usable.

Split conservation (basis points / 10_000): executor 5500, lineage 2700,
verifier 800, sink/escrow 500, burn 500. CLI price debit must equal chain payout
+ escrow + burn deltas.

## Acceptance gates

- Byte-stable offline output (frozen trace above).
- Local-devnet settlement of ≥100 verified receipts.
- Exactly one chain effect per receipt across duplicate delivery and restart at
  every persistence boundary.
- Exact equality between CLI price debits and chain payout + escrow + burn.
- All four new fields round-trip through DataEvol ingest.
- Zero submissions for failed / fallback-disallowed / unverified / malformed /
  mismatched / replayed / unsupported outcomes.
- p50/p95 prep overhead on 1_000 fixtures; **p95 ≤ 5 ms** excluding transport;
  peak added memory **< 32 MiB**; no regression in verified throughput.

## Verification commands

From `/Users/jamesstar/fractal-cli`:

```sh
cargo test --test capability_settlement19 --no-fail-fast
cargo test chain_client19 --no-fail-fast
cargo fmt --all -- --check
cargo test --no-fail-fast
cargo clippy --all-targets -- -D warnings
```

OutcomeReceiptV1 drift (coordinator repo):

```sh
python3 tools/check_outcome_receipt_v1_drift.py --fractalchain /Users/jamesstar/fractalchain
```

Owned encoding conformance (does not mutate FractalChain): `chain_client19` asserts
native/tx-body Borsh bytes against the frozen `vectors.json` settlement[0]
(`price_wei=0`) hex. The live FractalChain harness currently fails
`settlement_boundaries_conserve_exactly` on `u128::MAX` inside
`touch_capability_region` / `opportunity_score` overflow (`InvalidShape`) — that
path is owned by FractalChain core, not this CLI seam. Frozen vector hash + owned
byte match remain the CLI acceptance evidence.

DataEvol:

```sh
cd /Users/jamesstar/FractalDataevol && uv run pytest -q --tb=line
```

Coordinator regression aggregator:

```sh
cd /Users/jamesstar/fractal-projects/fractal-economic-self-healing-coordinator-1786584339843 && python3 ci.py
```

Unavailable required checks are blocking; no gate may be weakened.
