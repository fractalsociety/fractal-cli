# Harness policy contract (v1)

`fractal.harness_policy.v1` is the authority document for an execution graph.
It is optional at the project boundary, but absence is not implicit permissive
behaviour: the loader returns a named `builtin:safe-default.v1` provenance and
the same immutable policy hash is attached to the compiled graph and each node.
Projects can provide `.fractal/harness.yaml`; `.fractal/harness.json` and
`.fractal/harness_policy.json` are accepted equivalents.

## Loading and migration

The loader parses YAML/JSON into typed Rust values, trims and sorts set-like
lists, rejects unknown v1 fields, rejects absolute/traversal paths, and rejects
secret, chain-of-thought, and raw-log fields recursively. Integer limits stay in
the portable `fractal-cjson-v1` range. A future schema (`fractal.harness_policy.v2`,
for example) is retained in the diagnostic error and is fail-closed for
enforcement; it is never downgraded to v1.

The first external harness prototype used `version: fractal.harness.v1`. That
marker is migrated to `schema: fractal.harness_policy.v1` with a diagnostic.
Its workspace, command, network, secret, context, limits, verification,
artifact, termination, and learning fields are enforced. The old document's
`approval_required` list remains an approval gate, not a grant. New
`capabilities` (or migration alias `grants`) provide explicit per-capability
authority. Phase, verifier, evidence, and node-budget fields are now typed and
resolved; prose prompts, task/outcome schemas, and raw evidence capture remain
artifacts outside policy and are not interpreted as authority.

## Deny-by-default resolution

Every capability must resolve to a named grant. An unknown capability, a grant
with `enabled: false`, an empty command/write list, a denied network policy, or
an unapproved external side effect produces a `decision: deny` contract. A
project must explicitly grant writable globs, command strings, scoped network
destinations, secrets, and `external_side_effects: true`. The built-in default
contains harmless inference/control capability entries so legacy graph genomes
remain compilable, but grants no writes, commands, network, or secret names.

The compiler adds the following immutable fields:

* graph: `policy_schema`, `policy_hash`, and stable `policy_provenance`;
* node: `policy_hash`, `policy_provenance`, and `policy_contract` containing
  `capability`, `decision`, `sandbox_profile`, `allowed_writes`,
  `allowed_commands`, `network`, resolved `budgets`, `verifier_ids`,
  `evidence_requirements`, and `external_side_effects`.

The policy hash is a canonical SHA-256 of the normalized policy. Volatile
provenance/timestamp/hash keys are excluded from hash input; absolute checkout
paths therefore cannot make identical policy content produce different graph
hashes. The graph hash includes the policy fields, so changing authority or
limits produces a different reproducible graph. Runtime learning/failure data
is not consulted or mutated while compiling.

`fractal harness validate --repo PATH [--json]` is read-only and reports the
canonical hash, provenance, migration diagnostics, and validation failure. The
matching `show` command additionally prints normalized policy JSON. Neither
command writes `.fractal` state.

## Research grounding

The contract follows primary benchmark/runtime designs rather than copying
their code or data:

* SWE-bench runs repository tasks in reproducible, containerized environments;
  see the original paper and task harness, [SWE-bench: Can Language Models
  Resolve Real-World GitHub Issues?](https://arxiv.org/abs/2310.06770) and
  [the SWE-bench repository](https://github.com/princeton-nlp/SWE-bench).
* Inspect makes the task/dataset/solver/scorer boundary explicit and exposes
  sandbox and resource-limit controls; see the primary [Inspect AI
  documentation](https://inspect.aisi.org.uk/).
* SWE-rebench stresses fresh, decontaminated tasks so a benchmark cannot be
  passed by memorizing leaked patches; see [SWE-rebench: An Open and Live
  Benchmark for Software Engineering Agents](https://arxiv.org/abs/2505.20411).
* OpenAI's benchmark audit requires task-quality review rather than treating a
  score as sufficient evidence; see [Separating signal from noise in coding
  evaluations](https://openai.com/index/separating-signal-from-noise-coding-evaluations/),
  the primary [OpenAI Evals documentation](https://platform.openai.com/docs/guides/evals),
  and [OpenAI's evals repository](https://github.com/openai/evals).

These sources motivate immutable environment/policy provenance, explicit
solver capability grants, bounded sandboxes, fresh evaluation inputs, and an
independent verifier/evidence floor. They do not grant this runtime additional
permissions.
