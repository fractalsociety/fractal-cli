# Kokoro static Swift stack

This directory vendors:

- `mlalma/kokoro-ios` 1.0.11 (`KokoroSwift`)
- `mlalma/MisakiSwift` 1.0.6 (`MisakiSwift`)

Their source files and resources are unmodified. The local package manifest
links both targets statically so MLX is loaded exactly once in Fractal Voice;
the upstream dynamic product manifests otherwise embed duplicate MLX runtime
classes in a command-line-built macOS app.

MLX Swift is pinned to 0.31.3, the first compatible release in this line that
emits its Metal shader resource bundle for downstream `swift build` products.

The corresponding upstream license texts are preserved as `KOKORO_LICENSE`
and `MISAKI_LICENSE`.
