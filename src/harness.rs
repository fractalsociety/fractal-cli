//! P0.4 — intent-family → harness selection.
//!
//! Deterministically maps a classified intent family to a reusable harness
//! (the abstract, verification-gated procedure the compiler will later turn into
//! an execution graph). This is the front-door selection: it picks a *default
//! starter harness* per family with a stable id. Wiring the DataEvol harness
//! registry (`dataevol harness register-compiled` / `route`) as the authoritative
//! source is a later step (P1.1 chains the selected harness into
//! `fractal-harnessc`); until then the family→harness map here is the source of
//! truth and is deterministic so the same intent always selects the same harness.

/// A deterministically selected harness for an intent family.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HarnessSelection {
    /// Stable harness id (schema-like, versioned).
    pub harness_id: String,
    /// The normalized intent family the selection was keyed on.
    pub family: String,
    /// Where the selection came from (default catalog vs. DataEvol registry).
    pub source: String,
}

/// Default starter harnesses keyed by normalized intent family.
///
/// The intent may arrive as `nl.<family>` (from the NL work builder) or bare
/// `<family>` (from the classifier); [`normalize_family`] handles both.
const DEFAULT_CATALOG: &[(&str, &str)] = &[
    ("code", "harness.code_repair.v1"),
    ("answer", "harness.reasoned_answer.v1"),
    ("plan", "harness.plan_decompose.v1"),
    ("analyze", "harness.analysis.v1"),
    ("summarize", "harness.summarize.v1"),
    ("research", "harness.retrieval.v1"),
    ("execute", "harness.tool_execute.v1"),
    ("verify", "harness.verify.v1"),
];

/// The harness used when no family-specific starter is known.
const FALLBACK_HARNESS_ID: &str = "harness.generic_task.v1";

/// Normalize an intent string to a family key, stripping an `nl.` prefix and
/// lowercasing.
pub(crate) fn normalize_family(intent: &str) -> String {
    intent
        .trim()
        .strip_prefix("nl.")
        .unwrap_or(intent.trim())
        .to_ascii_lowercase()
}

/// Deterministically select a starter harness for a classified intent.
///
/// Never fails: an unknown family maps to [`FALLBACK_HARNESS_ID`] so the front
/// door always yields a harness for the graph compiler.
pub(crate) fn select_harness(intent: &str) -> HarnessSelection {
    let family = normalize_family(intent);
    let harness_id = DEFAULT_CATALOG
        .iter()
        .find(|(key, _)| *key == family)
        .map(|(_, id)| (*id).to_owned())
        .unwrap_or_else(|| FALLBACK_HARNESS_ID.to_owned());
    HarnessSelection {
        harness_id,
        family,
        source: "default-catalog".to_owned(),
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::{normalize_family, select_harness, FALLBACK_HARNESS_ID};

    #[test]
    fn strips_nl_prefix_and_lowercases() {
        assert_eq!(normalize_family("nl.Code"), "code");
        assert_eq!(normalize_family("  research "), "research");
    }

    #[test]
    fn code_intent_selects_code_repair_harness() {
        let selection = select_harness("nl.code");
        assert_eq!(selection.harness_id, "harness.code_repair.v1");
        assert_eq!(selection.family, "code");
    }

    #[test]
    fn selection_is_deterministic() {
        assert_eq!(select_harness("nl.research"), select_harness("research"));
    }

    #[test]
    fn unknown_family_falls_back() {
        let selection = select_harness("nl.somethingelse");
        assert_eq!(selection.harness_id, FALLBACK_HARNESS_ID);
        assert_eq!(selection.family, "somethingelse");
    }
}
