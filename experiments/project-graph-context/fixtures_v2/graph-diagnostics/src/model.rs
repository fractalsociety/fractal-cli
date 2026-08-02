#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Node {
    pub id: String,
    pub label: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Edge {
    pub from: String,
    pub to: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RelationGraph {
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
}

