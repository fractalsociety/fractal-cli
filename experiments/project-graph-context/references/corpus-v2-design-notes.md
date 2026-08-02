# Corpus v2 design references

This note records design provenance without reproducing source-project text.
The task families abstract common engineering failure modes observed in local
fixtures: durable state must distinguish missing, malformed, and expired
records; a UI board must keep stable filter/focus order and recover from
optimistic failures; graph relations need deterministic unresolved/ambiguous/
cycle diagnostics; and policy runners need bounded retry, checkpoint, denial,
and hard-budget transitions.

The benchmark's primary-source requirements are represented as executable
properties rather than copied prompts or evaluator artifacts:

* behavioral clauses include malformed, boundary, and negative inputs;
* gold/reference code is applied only in private quality worktrees;
* mutation cases include scope and policy bypasses;
* reports expose hashes and sanitized counts, not answer text;
* no package manager, network call, paid model episode, or external service is
  needed for a quality run.

The dependency shape and behavior fingerprint in each intent manifest are the
canonical deduplication keys.  Human-readable titles are not deduplication
keys and may change without changing split identity.

