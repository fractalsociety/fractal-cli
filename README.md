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
