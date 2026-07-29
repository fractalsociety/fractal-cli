//! Replayable evidence root + consent-gated sanitized outcome export (P5.5).
//!
//! Two guarantees a run must offer before its outcome leaves the boundary:
//!
//! 1. **Replayable evidence root** — the run's ordered evidence commits to a
//!    single deterministic Merkle root, so an auditor can *recompute* (replay)
//!    it from the same evidence and confirm nothing was altered.
//! 2. **Consent-gated, sanitized export** — an outcome is exported to DataEvol
//!    only under explicit consent (deny-by-default), and only its `Public`
//!    fields cross the boundary; `Private` fields are redacted. The export binds
//!    to the evidence root and its own commitment, so the shared payload is
//!    itself auditable.
//!
//! Reuses the chain's keccak Merkle so the evidence root composes with the rest
//! of `fractal-chain`.

use crate::merkle::{keccak256, merkle_root, Hash256};

/// One ordered piece of run evidence (a labelled digest of an artifact).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvidenceEntry {
    /// Stable label (e.g. `node:acceptance`).
    pub label: String,
    /// Digest of the evidence artifact.
    pub digest: Hash256,
}

impl EvidenceEntry {
    /// New entry.
    pub fn new(label: impl Into<String>, digest: Hash256) -> Self {
        Self {
            label: label.into(),
            digest,
        }
    }

    /// Deterministic Merkle leaf: length-prefixed `label` then `digest`.
    #[must_use]
    pub fn leaf(&self) -> Hash256 {
        let label = self.label.as_bytes();
        let mut pre = Vec::with_capacity(8 + label.len() + 32);
        pre.extend_from_slice(&(label.len() as u64).to_be_bytes());
        pre.extend_from_slice(label);
        pre.extend_from_slice(&self.digest);
        keccak256(&pre)
    }
}

/// The deterministic evidence root over ordered entries. Recompute it from the
/// same evidence to *replay* — an equal root proves the evidence is unaltered.
#[must_use]
pub fn replayable_evidence_root(entries: &[EvidenceEntry]) -> Hash256 {
    let leaves: Vec<Hash256> = entries.iter().map(EvidenceEntry::leaf).collect();
    merkle_root(&leaves)
}

/// Replay verification: recompute the root and compare to `claimed_root`.
#[must_use]
pub fn verify_replay(entries: &[EvidenceEntry], claimed_root: Hash256) -> bool {
    replayable_evidence_root(entries) == claimed_root
}

/// Whether an outcome field may cross the boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sensitivity {
    /// Safe to export.
    Public,
    /// Must be redacted from the export.
    Private,
}

/// One field of the run outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutcomeField {
    /// Field name.
    pub key: String,
    /// Digest of the field value (values themselves never appear in the export).
    pub value_digest: Hash256,
    /// Whether the field is exportable.
    pub sensitivity: Sensitivity,
}

/// Explicit consent to export, deny-by-default.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Consent {
    /// Whether export is permitted at all.
    pub granted: bool,
    /// What the exporter agreed to share (non-empty when granted).
    pub scope: String,
}

/// A fail-closed export error.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ExportError {
    /// Consent was not granted.
    #[error("outcome export denied: consent not granted")]
    ConsentDenied,
    /// Consent was granted without a scope.
    #[error("outcome export denied: consent scope is empty")]
    EmptyScope,
}

/// A sanitized, consent-gated export payload destined for DataEvol.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SanitizedExport {
    /// The replayable evidence root the outcome is bound to.
    pub evidence_root: Hash256,
    /// Only the `Public` fields, in input order: `(key, value_digest)`.
    pub public_fields: Vec<(String, Hash256)>,
    /// How many `Private` fields were redacted.
    pub redacted_count: usize,
    /// The consented sharing scope.
    pub consent_scope: String,
    /// keccak commitment over the whole export — itself auditable.
    pub export_commitment: Hash256,
}

/// Build a consent-gated, sanitized export: deny-by-default, strip `Private`
/// fields, and bind the result to the replayable evidence root.
pub fn sanitized_export(
    entries: &[EvidenceEntry],
    fields: &[OutcomeField],
    consent: &Consent,
) -> Result<SanitizedExport, ExportError> {
    if !consent.granted {
        return Err(ExportError::ConsentDenied);
    }
    if consent.scope.trim().is_empty() {
        return Err(ExportError::EmptyScope);
    }

    let evidence_root = replayable_evidence_root(entries);
    let mut public_fields = Vec::new();
    let mut redacted_count = 0;
    for field in fields {
        match field.sensitivity {
            Sensitivity::Public => {
                public_fields.push((field.key.clone(), field.value_digest));
            }
            Sensitivity::Private => redacted_count += 1,
        }
    }

    let export_commitment = export_commitment(&evidence_root, &public_fields, &consent.scope);
    Ok(SanitizedExport {
        evidence_root,
        public_fields,
        redacted_count,
        consent_scope: consent.scope.clone(),
        export_commitment,
    })
}

fn export_commitment(
    evidence_root: &Hash256,
    public_fields: &[(String, Hash256)],
    scope: &str,
) -> Hash256 {
    let mut pre = Vec::new();
    pre.extend_from_slice(evidence_root);
    pre.extend_from_slice(&(public_fields.len() as u64).to_be_bytes());
    for (key, digest) in public_fields {
        let key = key.as_bytes();
        pre.extend_from_slice(&(key.len() as u64).to_be_bytes());
        pre.extend_from_slice(key);
        pre.extend_from_slice(digest);
    }
    let scope = scope.as_bytes();
    pre.extend_from_slice(&(scope.len() as u64).to_be_bytes());
    pre.extend_from_slice(scope);
    keccak256(&pre)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entries() -> Vec<EvidenceEntry> {
        vec![
            EvidenceEntry::new("node:implement", [1u8; 32]),
            EvidenceEntry::new("node:acceptance", [2u8; 32]),
        ]
    }

    fn fields() -> Vec<OutcomeField> {
        vec![
            OutcomeField {
                key: "summary".to_owned(),
                value_digest: [3u8; 32],
                sensitivity: Sensitivity::Public,
            },
            OutcomeField {
                key: "raw_worker_transcript".to_owned(),
                value_digest: [4u8; 32],
                sensitivity: Sensitivity::Private,
            },
        ]
    }

    #[test]
    fn evidence_root_is_replayable_and_tamper_evident() {
        let root = replayable_evidence_root(&entries());
        assert!(verify_replay(&entries(), root));

        // Any change to the evidence breaks the replay.
        let mut altered = entries();
        altered[0].digest[0] ^= 1;
        assert!(!verify_replay(&altered, root));

        // Order matters.
        let mut reordered = entries();
        reordered.reverse();
        assert!(!verify_replay(&reordered, root));
    }

    #[test]
    fn export_is_denied_without_consent() {
        let denied = Consent {
            granted: false,
            scope: "dataevol".to_owned(),
        };
        assert_eq!(
            sanitized_export(&entries(), &fields(), &denied),
            Err(ExportError::ConsentDenied)
        );
    }

    #[test]
    fn export_is_denied_with_empty_scope() {
        let no_scope = Consent {
            granted: true,
            scope: "   ".to_owned(),
        };
        assert_eq!(
            sanitized_export(&entries(), &fields(), &no_scope),
            Err(ExportError::EmptyScope)
        );
    }

    #[test]
    fn export_redacts_private_fields_and_binds_the_evidence_root() {
        let consent = Consent {
            granted: true,
            scope: "dataevol:promotion".to_owned(),
        };
        let export = sanitized_export(&entries(), &fields(), &consent).expect("export");

        assert_eq!(export.evidence_root, replayable_evidence_root(&entries()));
        assert_eq!(export.redacted_count, 1);
        assert_eq!(export.public_fields.len(), 1);
        assert_eq!(export.public_fields[0].0, "summary");
        assert!(!export
            .public_fields
            .iter()
            .any(|(key, _)| key == "raw_worker_transcript"));
        assert_eq!(export.consent_scope, "dataevol:promotion");
    }

    #[test]
    fn export_commitment_changes_with_scope_and_fields() {
        let consent = Consent {
            granted: true,
            scope: "scope-a".to_owned(),
        };
        let a = sanitized_export(&entries(), &fields(), &consent).unwrap();
        let a2 = sanitized_export(&entries(), &fields(), &consent).unwrap();
        assert_eq!(a.export_commitment, a2.export_commitment); // deterministic

        let other_scope = Consent {
            granted: true,
            scope: "scope-b".to_owned(),
        };
        let b = sanitized_export(&entries(), &fields(), &other_scope).unwrap();
        assert_ne!(a.export_commitment, b.export_commitment);
    }
}
