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

Treat that file and the other controller files under `.fractal/` as
Fractal-owned state. Do not manually edit graph nodes, edges, assignments,
timestamps, hashes, checkpoints, sync state, or closeout state.

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

After the user explicitly says to send that invitation, repeat the same command
with `--yes`. Fractal Society creates a secure seven-day invitation and sends
it through the configured production mail service. Report success only when
Fractal prints `Invitation email sent successfully`.

Preview a public X help request:

```sh
fractal connect-x --project 'coffee-2'

fractal share-x \
  --project 'coffee-2' \
  --handle '@helper' \
  --message 'Please help with task 2.1 and spare agent compute for tests.'
```

Use `fractal connect-x` when the account is not connected; it transfers the
existing CLI login into the browser with a short-lived, single-use handoff, then
opens X authorization. Read the generated post back to the user. Only after
they explicitly approve that exact public post, repeat `fractal share-x` with
`--yes`. Report success only when Fractal returns the final X post URL. Do not
post arbitrary agent-authored content, silently change the tagged handle, reuse
an old confirmation, or treat voice transcription alone as confirmation.

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
  fractal ingest --source codex --format text --stdin
```

Fractal must print `Accepted` before you tell the user it succeeded. The lead
planner consumes accepted amendments at the next safe boundary between
execution waves, recompiles the graph, publishes the new branch, and lets
workers claim its nodes. Never edit `.fractal/project.fractal` directly.

Use `fractal stop --all` only when the user explicitly wants every running
Fractal build stopped.

### Pause one project from an external desktop agent

When the user asks ChatGPT Desktop, Codex Desktop, or another external agent to
pause a specific project, first identify its exact running name and then pause
only that project:

```sh
fractal status --running
fractal pause --project 'EXACT_PROJECT_NAME'
fractal status --running
```

`fractal pause` is a visible alias for `fractal stop`. It halts the selected
coordinator and its workers while preserving completed graph waves so the
project remains resumable. The project name may be its folder name, graph slug,
or absolute workspace path. If a short name is ambiguous, use the absolute path
printed by `fractal status --running`.

Report success only after Fractal prints `Stopped PROJECT` and the final status
no longer lists that project. Never substitute `--all` unless the user
explicitly asks to pause every running build. Do not kill agent or terminal
processes directly.

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
