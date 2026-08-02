use crate::model::RelationGraph;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    pub code: &'static str,
    pub detail: String,
}

pub fn validate(_graph: &RelationGraph) -> Vec<Diagnostic> {
    // The task's diagnostics belong in resolve.rs, not this decoy validator.
    Vec::new()
}

