# Relation resolution fixture

Implement the parser and resolver in `src/resolve.rs` using Rust's standard
library only.  Input is a JSON object with `nodes` (objects containing string
`id` and `label`) and `edges` (objects containing string `from` and `to`).
Given a label argument, resolve its unique node and print compact JSON
`{"ok":true,"id":"..."}`.  Matching is exact after trimming surrounding
whitespace.  A missing label is an error with code `unresolved_relation`; a
duplicate label is `ambiguous_relation`.  Preserve input graph order and do
not mutate or write files.  `graph_utils.rs` is a decoy.
