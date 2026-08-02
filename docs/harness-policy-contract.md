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

## Independent evidence manifests

Verification runs produce a bounded `fractal.evidence_manifest.v1` sidecar at
`.fractal/evidence/<sha256>.json`. The canonical JSON bytes are the content
address, so retries deduplicate identical evidence and never change the
immutable execution `graph_hash`. Manifests contain policy/node/attempt
identity, graph/commit/diff hashes, criterion IDs, verifier argv identities,
exit and duration values, output hashes, protected status, and pass/fail/
unavailable states. Prompts, raw logs, environment values, secrets, absolute
paths, and chain-of-thought are not persisted. The relative sidecar path is
added to the node's artifact and verification evidence references.

Public tests and operator-owned protected checkers are separate argv
processes. Protected checker paths stay outside the agent worktree; the
checker receives a disposable copy with a sanitized offline environment,
bounded output, and a wall timeout. Mutating that copy, duplicating the public
invocation, or omitting a required registry entry is an explicit fail/unknown
verdict. A model-verifier record is emitted only for a separately configured
model process; a public test exit code is never reused as a model or hidden
verdict. Missing required evidence is recorded as `weak_verifier` in the
learning/failure graph rather than being promoted to an unverified success.

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

## Worker-provider compatibility

Runtime provider eligibility is fail-closed and is based only on controls the
installed CLI documents. A read-only `--version` probe records a sanitized
version in the enforcement report; missing, unparseable, or older-than-tested
versions are unavailable. Worker commands never use dangerous bypass flags
(`--dangerously-*`, `--yolo`, or blanket `--force`).

The v1 contract has no unrestricted-shell sentinel. A non-empty
`allowed_commands` list is therefore a bounded shell grant, not permission to
run arbitrary commands. Providers without a native command allowlist cannot
claim that route:

| Provider | Network-deny + no shell (`allowed_commands: []`) | Bounded shell commands | Network scope |
| --- | --- | --- | --- |
| Codex Sol/Luna | unavailable (cannot disable shell) | eligible with workspace-write/network config | deny or broad allow only |
| Claude | eligible: `-p`, `acceptEdits`, `Read/Edit/Glob/Grep`, and explicit WebFetch/WebSearch/Bash denials | unavailable until a future unrestricted-shell contract | deny; broad allow is detected when no shell is granted |
| Cursor Agent | unavailable (no documented shell/tool deny) | unavailable | never inferred from `--sandbox enabled` |
| Hermes | eligible through `chat -q -Q` with the `file` toolset, isolated `HERMES_HOME`, and `HERMES_WRITE_SAFE_ROOT` | unavailable (terminal has no enforceable command allowlist) | deny; broad allow is detected when no shell is granted |

Scoped destinations (`allow_scoped`, `retrieval_only`, or a non-empty
`allowed_destinations`) are unavailable for these worker CLIs because none
exposes a destination-level network control. The report preserves each
control's `enforced`, `detected`, or `unavailable` status and includes the
reason when the aggregate provider route fails.
