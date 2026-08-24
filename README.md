# Fractal CLI

Fractal turns a product request into a structured PRD, a dependency-aware
execution graph, and a coordinated multi-agent build. It is designed for work
that is too broad for one coding session: Fractal exposes independent lanes,
leases each node to exactly one worker, records evidence, recovers stale work,
and keeps assigning ready tasks until the graph is complete.

The project includes a Rust CLI, a local and hosted graph experience, a native
macOS voice front end, graph lineage receipts, and adapters for Codex, Cursor,
Claude, and Hermes.

## Contents

- [Why Fractal](#why-fractal)
- [Quick start](#quick-start)
- [Multi-agent execution](#multi-agent-execution)
- [Hybrid isolated-worktree execution](#hybrid-isolated-worktree-execution)
- [Worker, coordinator, and architect roles](#worker-coordinator-and-architect-roles)
- [Scale-out provider pool](#scale-out-provider-pool)
- [Graph inspection and testing](#graph-inspection-and-testing)
- [Safety and repository hygiene](#safety-and-repository-hygiene)
- [Development setup](#development-setup)
- [Voice and external handoff](#voice)

## Why Fractal

Most agent runners treat a build as one long prompt. Fractal treats it as a
state machine. The canonical `.fractal/project.fractal` document stores nodes,
dependencies, ownership, completion status, and evidence. Workers do not choose
arbitrary work or mutate a side-channel state file; they atomically check out a
ready node through the Rust controller.

The multi-agent runtime provides:

- dependency-aware parallelism, so only genuinely unblocked work is leased;
- unique worker identities and collision-resistant checkout;
- one assignment listener per worker, bounded leases, heartbeat renewal, and
  stale-worker reclamation;
- automatic next-task assignment after accepted completion;
- governed graph expansion when workers are idle and no useful parallel lane
  exists;
- specialist teams with one leader and five workers, formed continuously while
  resource and quality gates allow it;
- heterogeneous execution across Codex, Cursor, Claude, and Hermes;
- deterministic tests for duplicate ownership, stale generations, retries,
  starvation, dependency violations, and logical makespan;
- local graph boards plus authenticated Fractal Society synchronization.

## Quick start

Build and install the CLI:

```sh
git clone https://github.com/fractalsociety/fractal-cli.git
cd fractal-cli
cargo build --release
install -m 755 target/release/fractal ~/.cargo/bin/fractal
fractal version
```

Run `fractal` without arguments for the interactive front door, or submit a
request directly:

```sh
fractal submit 'Build a small issue tracker with tests and documentation'
```

To hand a complete request from another desktop assistant to Fractal Voice,
keep the request on standard input:

```sh
fractal handoff --name 'Issue Tracker' <<'FRACTAL_REQUEST'
Build a small issue tracker with tests and documentation.
FRACTAL_REQUEST
```

The managed workflow creates the project, plans the PRD, compiles its graph,
starts workers, verifies completed nodes, and preserves state for resume.

## Multi-agent execution

Fractal supports three complementary ways to add capacity:

1. Human-opened agent terminals join an existing project with one command.
2. The architect launches bounded six-agent specialist teams automatically.
3. Hybrid mode launches the local provider roster in isolated Git worktrees
   and integrates dependency-ready results through Fractal.

For manual terminals, start or verify the coordinator in the project root:

```sh
fractal coordinator --repo .
```

Then run this in every additional agent window already inside that project:

```sh
fractal join --role worker
```

No provider selector is required. The joining process discovers its client,
registers a stable identity, asks the coordinator for work, checks out the
assigned node, and keeps its lease alive. When a completion is accepted, the
same worker is offered the next dependency-ready node. If no coordinator loop
is present, join can use a short one-shot coordinator transaction to reserve a
distinct ready node safely.

Useful operational variants:

```sh
# Poll once and emit a machine-readable result.
fractal join --role worker --once --json

# Use a stable identity across restarts.
FRACTAL_AGENT_ID=codex/reviewer \
FRACTAL_AGENT_LABEL='Codex · Reviewer' \
fractal join --role worker

# Inspect running managed projects.
fractal status --running

# Pause one project without destroying completed graph state.
fractal pause --project PROJECT_NAME
```

### Hybrid isolated-worktree execution

Use hybrid mode when a graph exposes parallel-safe tasks with concrete file
ownership. Cursor, Codex, Claude, and Hermes workers receive separate detached
worktrees; Fractal commits only each node's declared
`files_or_systems_affected` and `expected_artifact`, cherry-picks successful
results one at a time, and runs trusted verification against the integrated
branch.

```sh
export FRACTAL_AGENTS='codex,cursor'
fractal --offline run --local --hybrid --graph-file path/to/graph.json
```

Hybrid mode requires an existing commit and a clean tracked workspace. Build
nodes that make no declared source change fail. Undeclared source changes and
integration conflicts also fail closed; generated build directories remain in
the disposable task worktree and are not committed.

### Worker, coordinator, and architect roles

The worker is deliberately narrow: it receives one structured assignment,
claims that graph node, operates inside its scope, reports evidence, and either
completes or releases the lease.

The coordinator owns assignment flow. It reconciles worker heartbeats, prevents
double ownership, reclaims expired leases, validates completion generations,
and chains successful workers onto subsequent ready nodes. When all workers are
idle because the frontier is too narrow, it can request a bounded graph
amendment instead of inventing ungoverned tasks.

The architect is the scale controller. Each admitted specialist team contains
one planning/review leader and five implementation workers. Teams are formed
only when the graph has a coherent five-node mission and CPU, memory, cooldown,
CI, backlog, and measured-improvement gates permit more load.

Preview one architect admission cycle without launching processes:

```sh
fractal architect --repo . --once --json
```

Launch continuously until stopped or constrained by policy:

```sh
fractal architect --repo . --launch
fractal architect --repo . --stop
```

Use `--max-teams`, `--max-load-per-core`, `--min-free-memory-gib`, and
`--min-improvement-bps` to tune the envelope. A zero `--max-teams` means there
is no policy count cap; resource and graph-quality gates still apply.

### Scale-out provider pool

The in-process executor can opt into a 20–42 slot heterogeneous pool. The four
core providers must be explicit and their binaries must be available on `PATH`:

```sh
export FRACTAL_AGENT_POOL='codex=6,cursor=6,claude=6,hermes=6'
fractal run --local --graph-file path/to/graph.json
```

OpenCode is an optional fifth provider. Its default model is
`zai-coding-plan/glm-5.3`; set `FRACTAL_OPENCODE_MODEL` to override it:

```sh
export FRACTAL_AGENT_POOL='codex=5,cursor=5,claude=5,hermes=5,opencode=4'
```

The Codex lead planner is separate from those worker slots. Each provider has
independent capacity, so a slow or failed provider does not stop healthy slots
from consuming the ready queue. Invalid counts, missing providers, unavailable
binaries, and totals outside the verified envelope fail closed. See
[`docs/heterogeneous-agent-pool.md`](docs/heterogeneous-agent-pool.md) for the
contract, benchmark, and rollback details.

## Graph inspection and testing

Open the current graph experience:

```sh
fractal graph open
```

Serve a committed graph by content hash and open its board:

```sh
fractal graph board sha256:GRAPH_HASH
```

Use `--no-open` for a headless server, and inspect a running board API with:

```sh
fractal graph status --json
fractal graph show sha256:GRAPH_HASH --json
```

To test many independent agent windows without risking a production project,
seed a disposable 36-node graph with 12 immediately ready lanes:

```sh
fractal graph seed-parallel-test \
  --repo /tmp/parallel-join-test \
  --nodes 36 \
  --first-wave 12
cd /tmp/parallel-join-test
fractal coordinator --repo .
```

Open more terminals in that folder and run `fractal join --role worker` in each.
Increase `--nodes` and `--first-wave` within the CLI limits for a larger stress
test.

## Safety and repository hygiene

Fractal separates portable source from machine-local runtime state. Do not
commit `.fractal/`, `.squad/`, `target/`, `dist*/`, profiling output, tokens,
cookies, model caches, or local absolute paths. Keep credentials in the
provider's normal login store or environment and never put them in a graph
instruction, PRD, fixture, or command argument.

Before publishing a change, run:

```sh
cargo fmt --all -- --check
cargo test --no-fail-fast
cargo clippy --all-targets -- -D warnings
gitleaks git --redact
```

Security fixtures use unmistakably fake values to prove secret-redaction and
prompt-sanitization behavior. They are tests, not configuration examples.

## Repository layout

- `src/` — Rust CLI, graph transitions, coordinator, architect, and workers.
- `crates/fractal-chain/` — signed execution receipts and graph lineage.
- `execution-graph/` — the local live graph viewer.
- `macos/FractalVoice/` — native menu-bar voice application.
- `schemas/` — versioned graph, catalog, and reconciliation contracts.
- `docs/` — runtime and data-contract documentation.
- `scripts/` — routing, release, model, and DataEvol adapters.

## Development setup

The CLI currently consumes runtime contracts and graph evolution from the
private `fractalsociety/FractalRuntime` repository. Keep both repositories as
siblings:

```text
~/fractal-cli
~/FractalRuntime
```

Then build and test:

```sh
cd ~/fractal-cli
cargo test --no-fail-fast
cargo clippy --all-targets -- -D warnings
cargo build --release
install -m 755 target/release/fractal ~/.cargo/bin/fractal
```

Run `fractal version` to verify the installed binary.

## Read-only master-graph reconciliation

`fractal graph reconcile` consumes a frozen `fractal.repository_inventory.v1`
artifact and one or more current reports produced by `fractal graph audit`.
It emits `fractal.graph_reconcile.v1` canonical JSON, reports unresolved drift
with a nonzero exit status, and never writes a repository graph or Git state.
Only `--output` is writable; omit it to print the report.

```sh
fractal graph audit \
  --inventory artifacts/audit/repository-inventory.json \
  --shard 0/1 \
  --report /tmp/fractal-audit.json

fractal graph reconcile \
  --inventory artifacts/audit/repository-inventory.json \
  --audit /tmp/fractal-audit.json \
  --baseline artifacts/audit/master-graph-reconcile-baseline.json \
  --output /tmp/fractal-reconcile.json
```

The audit command is evidence generation; reconciliation is the read-only
freshness and identity gate over exactly `fractalmaster`, `fractal-cli`,
`fractalchain`, `FractalRuntime`, `Fractalwork`, and
`fractalsociety-website`. Repeated runs over unchanged evidence produce
byte-identical output and hashes exclude wall-clock fields.

## Fractal Society

Authenticated projects publish their portable `.fractal/project.fractal`
execution state to [fractalsociety.com](https://fractalsociety.com). Planner
heartbeats, dependency waves, agent checkout, and completion state are reflected
in the live online graph.

## Voice

Fractal ships one voice interface with two interchangeable backends:

- **Moonshine v2 Medium Streaming** is the default. Speech recognition runs
  locally on the device and finalized text enters the same normalized intent,
  risk classification, confirmation, and execution-graph pipeline as typed
  input.
- **Superwhisper** remains available as an optional macOS compatibility
  backend.

Install the isolated Moonshine runtime and verified model once:

```sh
fractal voice setup
fractal voice engines
```

Then speak a command:

```sh
fractal voice
```

Press Enter when you have finished speaking. Read-only requests can run
immediately. Requests that modify a project require the existing typed
confirmation gate:

```sh
fractal voice --confirm --repo /path/to/trusted/project
```

Use dictation when you want to review the transcript without executing it:

```sh
fractal dictate
```

To launch the optional Superwhisper integration instead:

```sh
fractal voice \
  --engine superwhisper \
  --mode-key YOUR_FRACTAL_COMMAND_MODE_KEY
```

The mode key can also be provided through
`FRACTAL_SUPERWHISPER_MODE_KEY` (or
`FRACTAL_SUPERWHISPER_DICTATE_MODE_KEY` for dictation).

Moonshine is installed under `~/.fractal/voice/moonshine`, while its model is
cached under `~/.fractal/models/moonshine-v2-medium-streaming`. The setup uses
the pinned official `moonshine-voice` package. Once setup is complete,
transcription does not require a cloud account or send microphone audio to
Fractal Society. Spoken text is passed as structured data, never interpolated
into a shell command.

### Fractal Voice for macOS

The repository includes a lightweight native menu-bar companion under
`macos/FractalVoice`. Its first-run guide explains the shortcut and build flow.

Fractal Voice applies a deterministic local vocabulary layer before handing a
transcript to the intent engine. Built-in product terminology is supplemented
by personal vocabulary in `~/.fractal/voice/vocabulary.json` and, when a project
is active, `.fractal/vocabulary.json`. Both files use this format:

```json
{
  "schema": "fractal.voice-vocabulary.v1",
  "terms": ["AcmeGraph", "My Product"],
  "corrections": {
    "acme graph": "AcmeGraph",
    "my product mishearing": "My Product"
  }
}
```

Only listed phrases are corrected. Unknown identifiers, paths, numbers, and
command syntax remain unchanged. Set `FRACTAL_PROJECT_DIR` when launching the
macOS app to explicitly select a project's vocabulary.
Press `⌥Space` once to start local recording. Fractal detects the end of the
utterance after a short natural pause; pressing the shortcut again remains a
manual stop override.
Fractal speaks the interpreted request with the locally installed Kokoro 82M model and
asks for confirmation. Answer yes or no by voice, or use the matching buttons.
After approval, Fractal asks for a project name, repeats it, and requires a
second spoken or clicked confirmation before any build starts. A rejected
request returns to request recording; a rejected name returns to naming.
Each conversational reply opens the microphone automatically and advances after
speech followed by roughly one second of silence. A silent microphone closes
after 60 seconds; press `⌥Space` to resume that question.

The macOS release is a lightweight app containing the pinned native runtimes.
On first launch it explains and automatically downloads IBM Granite Speech 4.1
2B Q4_K_M, its speech projector, Kokoro 82M, and the `af_heart` voice into
`~/.fractal/models`. Downloads resume after interruption and every model must
pass its pinned SHA-256 checksum. After that one-time installation, speech
recognition and output run locally without sending microphone audio to a cloud
service. Granite
receives the built-in, personal, and active-project terminology as a keyword
bias list, then the deterministic vocabulary layer cleans exact configured
mishearings before the instruction reaches Fractal.

Each shortcut-triggered build receives a fresh workspace beneath
`~/fractal-projects`. The companion automatically approves only reversible
project creation in that managed location. Destructive requests and requests
with external side effects remain blocked for terminal review.

Prepare the pinned inference runtime, then build the signed lightweight app
bundle and distributable archive:

```sh
xcodebuild -downloadComponent MetalToolchain
scripts/prepare-granite-speech.sh
scripts/build-macos-app.sh
```

The release builder includes the native Granite runtime and Xcode's compiled
MLX Metal shader bundle, but not model weights. The app downloads model weights
from pinned revisions on first launch. A versioned
`~/.fractal/voice-engine.json` selects the default `granite-local` transcription
and `kokoro-local` speech providers and reserves fields for custom local models
and future API providers.

Local builds receive an ad-hoc signature. Set `FRACTAL_CODESIGN_IDENTITY` to a
Developer ID Application certificate when producing a public build; that
archive must also be notarized before distribution outside your own Mac.

Artifacts are written to:

```text
dist/Fractal Voice.app
dist/FractalVoice-macOS.zip
```

The app bundle contains the matching `fractal` binary, native runtimes,
checksum manifests, and third-party license notice. From the menu bar you can reopen onboarding,
inspect activity, open generated projects, or stop all running Fractal builds.
Finalized text is sent over stdin to the managed-project ingest boundary; it is
never interpolated into a command line.

### ChatGPT Desktop and other external apps

Sandboxed desktop apps can start the same managed build without using Fractal's
interactive trust flow or any local bridge:

```sh
fractal handoff --name 'Hello World' <<'FRACTAL_REQUEST'
Build a very simple Hello World app.
FRACTAL_REQUEST
```

The request is written to an owner-only, short-lived `.fractalbuild` file in the
per-user temporary directory and delivered to Fractal Voice through macOS
LaunchServices. The native app validates the file, deletes it after one read,
creates the named managed project, and starts the normal PRD, execution-graph,
agent checkout, verification, GitHub, and Fractal Society pipeline. Request text
is never placed in a URL or shell argument.
