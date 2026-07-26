# Fractal CLI

Fractal CLI turns a natural-language or voice request into a structured PRD,
dependency-aware execution graph, and coordinated multi-agent build.

## Repository layout

- `src/` — the `fractal` command-line application.
- `crates/fractal-chain/` — signed execution receipts and graph lineage.
- `execution-graph/` — the local live graph viewer.
- `scripts/` — local routing and DataEvol adapters.

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
Press `⌃⌥Space` once to start local Moonshine recording, then press it again to
stop and immediately begin the project.

Each shortcut-triggered build receives a fresh workspace beneath
`~/fractal-projects`. The companion automatically approves only reversible
project creation in that managed location. Destructive requests and requests
with external side effects remain blocked for terminal review.

Build the signed local app bundle and distributable archive:

```sh
scripts/build-macos-app.sh
```

Local builds receive an ad-hoc signature. Set `FRACTAL_CODESIGN_IDENTITY` to a
Developer ID Application certificate when producing a public build; that
archive must also be notarized before distribution outside your own Mac.

Artifacts are written to:

```text
dist/Fractal Voice.app
dist/FractalVoice-macOS.zip
```

The app bundle contains the matching `fractal` binary, while Moonshine’s runtime
and model remain in the shared `~/.fractal` cache. From the menu bar you can
reopen onboarding, inspect activity, open generated projects, or stop all
running Fractal builds.
