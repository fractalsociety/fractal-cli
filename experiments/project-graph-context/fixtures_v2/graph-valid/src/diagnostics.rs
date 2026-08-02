use crate::model::RelationGraph;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    pub code: &'static str,
    pub detail: String,
}

pub fn validate(_graph: &RelationGraph) -> Vec<Diagnostic> {
    // Kept separate from relation lookup as a localization decoy.
    Vec::new()
}

