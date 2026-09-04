# Fractal Agent Operating Contract

These instructions apply to Codex and every other coding agent working in this
repository or in a project created by Fractal Voice.

## Fractal owns orchestration

Fractal is the controller for planning, dependency scheduling, task checkout,
agent labels, completion state, verification, graph publication, and closeout.
Use the Fractal execution graph instead of inventing a separate checklist or
silently doing an entire multi-task request yourself.

The portable project state is:

```text
.fractal/project.fractal
```

## Graph surfaces: do not confuse them

The Rust-backed snapshot rendered by `fractal-graph-ui.v1` is the canonical
viewer for CLI-managed execution state. Its authoritative read API is
`/api/snapshot` with schema `fractal.graph_snapshot.v1`, and its checked-in
browser assets are pinned by `execution-graph/fractal-graph-ui.manifest.json`.

For read-only inspection of a compiled CLI graph:

```sh
fractal graph board GRAPH_HASH
```

This is not FractalMaster Manual mode. When a user asks for the manual board
with Codex Fast, Claude, Cursor, GLM/ZCode, prompt copying, retry-safe checkout,
release, and completion controls, use `/Users/jamesstar/fractalmaster` and run
`python3 -m intelligence_graph.web_server --manual-prd ...`. Do not serve this
snapshot viewer in its place.

Do not restore or use the retired standalone Three.js graph
(`three-graph.js`), its `graph-state*.json` state, or the Python task-control
runtime. Compatibility import code exists only to migrate old projects once.
New graph behavior belongs in the Rust snapshot/controller and the shared
Fractal Society graph-ui package, never in a second local renderer.

Treat that file and the other controller files under `.fractal/` as
Fractal-owned state. Do not manually edit graph nodes, edges, assignments,
timestamps, hashes, checkpoints, sync state, or closeout state.

The Rust CLI is the only supported transition boundary. Never run
`execution-graph/task-state.py`, mutate `graph-state*.json`, call Python task
action endpoints, or derive dependencies from PRD markdown. Operators inspect
and transition through `fractal node`; Fractal-launched workers never write
their own transitions.

## Determine your mode first

### Fractal-launched worker

If `FRACTAL_WORKER` is set, Fractal has already:

1. selected a dependency-ready node;
2. atomically checked it out to this agent;
3. published the agent label to the local and online graph; and
4. supplied the exact node instruction in the current prompt.

In this mode:

- Do only the assigned node.
- Read `INTERFACE.md` and `.fractal/lead-prd.json` when present.
- Honor the established architecture, public interfaces, file ownership, and
  acceptance criteria.
- Depend only on completed predecessor work. Do not begin downstream nodes.
- Do not run `fractal` recursively and do not claim another graph node.
- Keep changes narrowly scoped so parallel workers do not conflict.
- Run the relevant formatter, compiler, tests, or focused verification.
- Report the files changed, checks run, and any remaining risk.
- Never claim success when required evidence is missing.

When the worker exits successfully, Fractal records `complete` with this
agent's identity and timestamp. When it fails or times out, Fractal records
`released` and routes repair work. The worker must not write either transition
itself.

### Lead/orchestrator

The first configured agent is the lead. The lead expands the request into the
PRD, architecture, acceptance criteria, dependency DAG, parallel waves, and
final closeout. With multiple agents, the lead should not take ordinary coding
nodes assigned to workers.

The lead must:

- distinguish parallel-safe nodes from sequential dependency chains;
- give every node a stable ID, bounded scope, concrete output, and acceptance
  evidence;
- avoid overlapping file ownership within the same parallel wave;
- require verification after implementation;
- close the project only when every acceptance criterion has concrete evidence.

### Top-level Codex session

If `FRACTAL_WORKER` is not set and the user asks for a multi-step build, route
the request through Fractal rather than implementing it as an untracked task.

For an interactive request:

```sh
fractal
```

Enter the user's request at the Fractal prompt. To use the non-interactive
boundary, start this command and pass the exact request through stdin:

```sh
fractal ingest \
  --source codex \
  --format text \
  --stdin \
  --repo "$PWD"
```

Never interpolate voice text directly into shell syntax. Send it as stdin so
apostrophes, newlines, backticks, `$()`, and other shell metacharacters remain
data.

Use the Coordinate backend only when the project explicitly requires it:

```sh
fractal --coordinate
```

The normal local multi-agent executor is the default.

### External desktop app

When ChatGPT Desktop or another sandboxed desktop agent is asked to start a
new named build, do not run interactive `fractal`, do not select a workspace,
and do not answer trust or agent-selection prompts. Use the native handoff:

```sh
fractal handoff --name 'Hello World' <<'FRACTAL_REQUEST'
Build a very simple Hello World app.
FRACTAL_REQUEST
```

This command writes an owner-only, short-lived `.fractalbuild` request, first
tries the explicit receiver at `/Applications/Fractal Voice.app`, and then
falls back to macOS bundle-ID discovery. If the desktop agent's sandbox blocks
both launch attempts, the command safely leaves the request queued; an already
running Fractal Voice app monitors the private queue and consumes it. Fractal
Voice deletes the request after validation, creates the managed project, starts
the standard planner and execution graph, publishes progress normally, and
uses the configured lead and workers. It does not use the deprecated local
bridge.

Treat either `Sent` or `Queued` as successful delivery. If the command says
`Queued`, do not retry or run Fractal directly; tell the user to keep Fractal
Voice running while it picks up the request.

Always pass the build description through stdin with a quoted heredoc delimiter
or the calling tool's direct stdin API. Do not interpolate the description into
shell syntax. `--name` must be the user-confirmed project name, not a shortened
version of the build prompt.

### Invite collaborators or ask for help

ChatGPT Desktop and other external agents may also use Fractal's authenticated
sharing commands. Email and X posting are external side effects: first collect
the exact recipient or X handle, project slug, permission, and help request.
Show the user what will be sent and obtain explicit confirmation. Never infer
confirmation from the original build request.

Preview an email invitation without sending it:

```sh
fractal invite \
  --project 'coffee-2' \
  --email 'helper@example.com' \
  --role contributor \
  --message 'Please help with task 2.1 and spare compute for the test wave.'
```

The preview opens in the project's Fractal Society Share dialog. The user may
confirm and send it there. If the user instead explicitly tells this agent to
send that exact invitation, repeat the same command with `--yes`. Fractal
Society creates a secure seven-day invitation and sends it through the
configured production mail service. Report success only when Fractal prints
`Invitation email sent successfully`.

For ChatGPT Desktop and every other external voice agent, use the fast preview
path:

```sh
fractal share-x \
  --project 'coffee-2' \
  --handle '@helper' \
  --message 'Please help with task 2.1 and spare agent compute for tests.' \
  --preview-only
```

`--preview-only` prints the exact server-generated post and `X_PREVIEW_READY`,
then exits successfully. It must not open the Fractal Society preview page.
Read that exact preview to the user and ask one concise yes-or-no question. Do
not run any preliminary command, do not retry the preview, and do not say the
preview screen failed to open.

After an explicit yes, immediately repeat the identical project, handle, and
message with `--yes` replacing `--preview-only`. Do not reread `AGENTS.md` and
do not recreate the preview:

```sh
fractal share-x \
  --project 'coffee-2' \
  --handle '@helper' \
  --message 'Please help with task 2.1 and spare agent compute for tests.' \
  --yes
```

Fractal sends the trusted `https://x.com/intent/tweet` URL through a private
handoff to Fractal Voice, which opens the free prefilled X composer. Report
success only after Fractal prints `Opened the approved X composer`. The user
still chooses Post. Never call `fractal connect-x`, never open
`/api/providers/x/*`, never use OAuth or an X API token, and never use the bare
`fractal share-x` command from an external desktop agent. Do not change the
approved text or infer approval from the original request.

## Operate an existing graph

Use Fractal's public commands rather than editing graph JSON:

```sh
fractal projects
fractal status --running
fractal graph open
fractal graph status --url http://127.0.0.1:8092
fractal resume PROJECT_NUMBER
fractal sync --repo "$PWD"
fractal stop --project PROJECT_NAME
```

To add work to a running build, pass the user's exact graph amendment through
stdin. Tasks use the visible `wave.position` number shown on the execution
graph:

```sh
printf '%s\n' \
  'Add to task 1.2 another branch that adds CSV export with tests.' |
  fractal ingest --source codex --format text --stdin --amend
```

For a bounded project-level addition that does not need a task anchor, pass the
instruction through the same explicit amendment transport. Fractal inserts one
peer task into the earliest unfinished build wave:

```sh
printf '%s\n' \
  'Inventory prior project graphs and build a linked master graph before downstream work.' |
  fractal ingest --source codex --format text --stdin --amend
```

Fractal must print `Accepted` before you tell the user it succeeded. The lead
planner consumes accepted amendments at the next safe boundary between
execution waves, recompiles the graph, publishes the new branch, and lets
workers claim its nodes. Explicit amendment mode never opens a typed
confirmation prompt and never falls through into creating a new project. Never
edit `.fractal/project.fractal` directly.

Use `fractal stop --all` only when the user explicitly wants every running
Fractal build stopped.

### Pause one project from an external desktop agent

When the user names a project to pause, call the pause command immediately with
that spoken name, then verify status:

```sh
fractal pause --project 'USER_SPOKEN_PROJECT_NAME'
fractal status --running
```

Do not use `fractal status --running` as a gate before issuing the named pause.
The coordinator may have died while the website or app still shows planning or
executing. In that case the live registry is intentionally empty, but
`fractal pause --project NAME` resolves the durable managed project, releases
stale checkouts, marks its graph halted, and synchronizes the website.

If the user says only “pause the build” with no name, run
`fractal status --running`. If exactly one live build is listed, use
`fractal pause` with no argument. If multiple builds are listed, copy the
absolute workspace path and pass it through `--project`.

`fractal pause` is a visible alias for `fractal stop`. It halts the selected
coordinator and its workers while preserving completed graph waves so the
project remains resumable. The project name may be its folder name, graph slug,
or absolute workspace path. If a short name is ambiguous, use the absolute path
printed by `fractal status --running`.

Report success only after Fractal prints `Stopped PROJECT` and the final status
no longer lists that project as live or stalled. `Stopped PROJECT` may also mean
Fractal successfully reconciled a dead coordinator that left the online graph
stuck. If it prints `Already paused`, report it as already paused—not as a
failure. Never tell the user a named build cannot be paused merely because
`status --running` found no live coordinator. Never substitute `--all` unless
the user explicitly asks to pause every running build, and never kill agent or
terminal processes directly.

### Explain a numbered execution-graph task

When the user asks what a visible task such as `2.4` means, run
`fractal projects` to resolve the named project and read its
`.fractal/project.fractal` file without changing it. Match the visible number
against `graph.nodes[].execution.task_number`, then explain the node title,
instruction, dependencies, parallel or sequential wave, assigned agent and
current state in plain language. If more than one project could match, ask for
the project name instead of guessing.

Reading the portable graph for an explanation is allowed; editing it is not.
If the user asks to add or change work, use the graph-amendment ingest command
described above and report success only after Fractal prints `Accepted`.

### Change project and repository visibility

ChatGPT Desktop and other external agents can preview a synchronized Fractal
Society and GitHub visibility change:

```sh
fractal visibility --project 'EXACT_PROJECT_NAME' --public
fractal visibility --project 'EXACT_PROJECT_NAME' --private
```

Always invoke this dedicated command directly. Never send a visibility request
through `fractal ingest`; an ingest acknowledgement or the word `Accepted` is
not evidence that either GitHub or Fractal Society changed.

The first command is warning-only and must leave both systems unchanged. Read
the warning to the user and wait for an explicit yes or no. A spoken “yes” or
“no” applies only to the immediately preceding visibility preview for that
exact project and target. On “no,” stop. On “yes,” repeat the identical command
with `--yes`:

```sh
fractal visibility --project 'EXACT_PROJECT_NAME' --public --yes
```

Never infer approval from a request to inspect, share, or build a project.
Making a repository public exposes its files and Git history, so do not bypass
the warning. Report completion only after Fractal says both the project graph
and GitHub repository have the requested visibility. Any GitHub CLI, network,
authentication, or synchronization error means the change failed and must be
reported as failed. If Fractal says the confirmed change was `Sent` or `Queued`
for Fractal Voice, explain that the trusted native app is completing it; do not
call it complete until Fractal Voice reports “Visibility updated.” This handoff
exists because desktop-agent sandboxes may not be allowed to use the user's
GitHub credential. The website offers the same guarded toggle on the execution
graph and project settings page.

If `.fractal/project.fractal` exists but no build is running, inspect
`fractal projects` and resume the registered project. Do not manufacture
checkout or completion entries. The current `fractal node` command is not a
replacement for the orchestrator.

## Graph and evidence rules

- A node is ready only when every incoming dependency is complete.
- Independent ready nodes may run in parallel.
- A checked-out node belongs to its recorded agent until completion or release.
- Do not overwrite another agent's changes or revert unrelated user work.
- Completion requires the node output plus its stated validation.
- Verification failures are real failures; fix them or leave the node released.
- Do not weaken tests, delete acceptance criteria, or bypass the evidence floor.
- Do not commit credentials, tokens, private keys, personal data, model caches,
  build artifacts, or `.env` files.
- Do not push, deploy, send messages, or trigger other external side effects
  unless the assigned node or user explicitly authorizes them.

## Fractal CLI repository checks

When the assigned work modifies Fractal CLI itself, use checks proportional to
the change. The normal full verification is:

```sh
cargo fmt --all -- --check
cargo test --no-fail-fast
cargo clippy --all-targets -- -D warnings
```

For native macOS changes, also run:

```sh
swift test --package-path macos/FractalVoice
```

Preserve existing user changes in a dirty worktree. Make source edits with
small, reviewable patches and report any check that could not be run.

## Squad Collaboration

This project uses squad for multi-agent collaboration. Run `squad help` for all commands and usage guide.
