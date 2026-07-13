# The Wado Programming Language

Wado is a statically-typed, high-level WebAssembly language and toolchain that targets only the WebAssembly Component Model and WASI 0.3+. The compiler is 100% agentic-coded. The language takes Rust as its base — made garbage-collected, with no lifetimes and no borrow checker — and borrows its surface syntax from TypeScript.

See [docs/design-philosophy.md](docs/design-philosophy.md) for the reasoning behind these choices, and [wado-lang.org](https://wado-lang.org) for the docs and blog.

## Installing

### Pre-built binary (recommended)

Download the latest release for your platform from
[GitHub Releases](https://github.com/wado-lang/wado/releases/latest).
Pre-built binaries are published for:

- Linux (`x86_64`, `aarch64`) — `tar.gz`
- macOS (Apple Silicon) — `tar.gz`
- Windows (`x86_64`, `aarch64`) — `zip`

Each archive contains the `wado` binary plus `LICENSE` and `README.md`.
Verify the download against `SHA256SUMS.txt` attached to the same release.

### From source

If you have a Rust toolchain installed:

```sh
cargo install --git https://github.com/wado-lang/wado wado-cli
```

This builds the current `main` branch from source. Re-run the same command
to update.

## Hello World

```wado
#!/usr/bin/env wado run
use { println, Stdout } from "core:cli";

// run() is the entry point of the wasi:cli/command hosted world
export fn run() with Stdout {
    println("Hello, world!");
}
```

```sh
wado run example/hello.wado                    # run it directly
wado compile -o hello.wasm example/hello.wado  # compile to Wasm
wado compile -o hello.wat  example/hello.wado  # or to WAT
```

## Status

Wado is experimental. The core language — static typing, generics, closures, modules, traits, pattern matching, the effect system — is implemented and functional, and already usable for its original purpose: embedding small, type-safe Wasm modules where binary size matters. It waits on the broader ecosystem — the Component Model in browsers, WASI 1.0, and GC across component boundaries — to reach its full shape.

## Documentation

- [Design Philosophy](docs/design-philosophy.md) — why Wado is the way it is
- [Cheatsheet](docs/cheatsheet.md) — quick syntax reference
- [Language Specification](docs/spec.md) — full language reference
- [Compiler Implementation](docs/compiler.md) — compiler internals and feature checklist
- [Benchmarks](benchmark/README.md) — performance vs C, JavaScript, and others
- [Other Documentation](docs) — WEPs, research notes, and more

These are also published, alongside the blog, at [wado-lang.org](https://wado-lang.org).

## Development

### Development Process

Developing entirely through agentic coding requires active management:

- **Refactoring guidance**: Left unchecked, agents generate case-specific code that only works for immediate tests. Regular intervention steers toward generalizable solutions.
- **Code minimization**: Agents tend to over-generate logic. Compilers need minimal, general-purpose code — the opposite of what agents naturally produce.
- **Periodic refactoring phases**: Without intervention, cruft accumulates. We've done one ground-up compiler architecture redesign so far.

### AI-Guided Optimization

AI-guided optimization is a technique where you show generated code to a coding agent and have it identify optimization opportunities. The agent's output is non-deterministic, but the insights can be turned into deterministic compiler rules.

Wado's optimizer is developed using this approach:

```
Agent finds pattern → Human reviews → Deterministic optimization rule added
```

Show the generated WAT to an agent and ask it to spot inefficiencies. Review the suggestions, then implement them as permanent optimization passes.

### Install Development Tools

This project uses [mise](https://mise.jdx.dev/) to manage dev tools. Install mise first:

```sh
curl -fsSL https://mise.run | sh
# Then add to your shell profile:
#   eval "$(~/.local/bin/mise activate bash)"  # for bash
#   eval "$(~/.local/bin/mise activate zsh)"   # for zsh
```

Then install project tools:

```sh
mise trust                 # trust the mise.toml config (first time only)
mise run on-task-started   # install all project tools
```

See [mise.toml](mise.toml) for the list of managed tools.

### Build and Test

```sh
cargo build
cargo test
```

### The Wado CLI

- `wado compile FILE` - Compile Wado source to Wasm/WAT
- `wado run FILE` - Run Wado source directly using Wasmtime
- `wado dump FILE` - Dump internal compiler state for debugging
- `wado format FILE` - Format Wado source code

### Examples That Already Work

There are E2E test fixtures in [wado-compiler/tests/fixtures/\*.wado](wado-compiler/tests/fixtures).

### VS Code Extension

The `wado-vscode/` directory contains a VS Code extension for syntax highlighting. It is not published to the marketplace, but you can install it locally for development:

```sh
mise run install-wado-vscode-dev    # install extension to ~/.vscode via symlink
mise run clean-wado-vscode-dev      # uninstall it from ~/.vscode
mise run update-wado-vscode-grammar # regenerate syntax files after changing syntax.rs
```

See [wado-vscode/README.md](wado-vscode/README.md) for more details.

### On Your Task Done

```sh
mise run on-task-done # format, clippy-fix, update resources, test
```

### Releasing

Releases are cut manually on a roughly weekly cadence via [tagpr](https://github.com/Songmu/tagpr).

How it works:

1. Every push to `main` (re)opens a **Release PR** that bumps `[workspace.package].version` in both `Cargo.toml` and `wado.toml` (kept in lockstep so the CLI and the published Wado packages ship one version), regenerates `Cargo.lock`, and updates `CHANGELOG.md` from PRs merged since the previous tag.
2. Merging the Release PR pushes tag `v<next>`, which triggers `.github/workflows/release.yml` to:
   - build pre-built binaries for five targets in parallel — Linux (`x86_64`, `aarch64`), macOS (Apple Silicon), Windows (`x86_64`, `aarch64`) — and publish them to a [GitHub Release](https://github.com/wado-lang/wado/releases) with `SHA256SUMS.txt`;
   - run `wado publish` to push the workspace's Wado packages to [GHCR](https://github.com/orgs/wado-lang/packages) as OCI artifacts.
3. Default bump is **patch**. Add a `tagpr:minor` or `tagpr:major` label on the Release PR to override.

tagpr is the single version manager: the workspace version is bumped only by the Release PR, never by hand. Do not edit `[workspace.package].version` in `Cargo.toml` or `wado.toml` directly — the release job fails if the two files disagree with the tag.

## Benchmarks

Per-commit performance tracking is published to GitHub Pages. Every push to `main` records runtime and binary size metrics.

- [Runtime Performance](https://wado-lang.github.io/wado/benchmarks/runtime-throughput/) — throughput (work per second, higher is better) for integer, float, array, string, and compression workloads (run on wasmtime at `-O1`/`-O2`/`-O3`)
- [Wasm Binary Size](https://wado-lang.github.io/wado/benchmarks/wasm-size/) — `.wasm` output size for representative programs (compiled at `-Os`)

See [benchmark/README.md](benchmark/README.md) and [wasm-size/README.md](wasm-size/README.md) for local benchmark instructions and comparison results against other programming languages.

## Authors

Copyright (c) 2026, FUJI Goro (a.k.a. gfx). Some rights reserved.

## License

MIT

See [LICENSE](LICENSE) for details.
