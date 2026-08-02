# Relation diagnostics fixture

Implement `src/resolve.rs` with stdlib-only JSON parsing and deterministic
diagnostics.  The same input shape and exact-trim label lookup as the sibling
fixture apply.  In addition to successful lookup, report
`unresolved_relation` for missing node labels, `ambiguous_relation` with a
stable sorted candidate list for duplicate labels, and `cycle_detected` when
directed edges contain a cycle.  Malformed JSON and missing fields must return
`malformed_graph`, never panic.  Emit only compact JSON on stdout and do not
write files or inspect the network.  `src/graph_utils.rs` is a decoy.
