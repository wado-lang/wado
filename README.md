# The Wado Programming Language

Wado is a statically typed, garbage-collected, and effectful language that targets **only** the WebAssembly Component Model and WASI 0.3+. It is designed for embedding small, type-safe Wasm applications where performance and binary size matter.

You can use Wado to build CLI applications and HTTP services, or try it directly in your browser.

[Gale](package-gale), an ANTLR4-compatible parser generator written in Wado, shows what Wado can achieve in performance and binary size. In our [SQL parsing benchmark](benchmark/README.md#sql-parse), its generated parser is much faster than ANTLR4's Java parser and slightly faster than a hand-written Rust parser.

Visit [wado-lang.org](https://wado-lang.org) for the documentation, playground, and blog. The [design philosophy](docs/design-philosophy.md) explains the reasoning behind the language.

## Hello World

This is a complete Wado program that prints "Hello, world!" to stdout:

```wado
#!/usr/bin/env wado run
use { println, Stdout } from "core:cli";

export fn run() with Stdout {
    println("Hello, world!");
}
```

The function signature makes the program's role and capabilities explicit:

- `run()` is the entry point of the `wasi:cli/command` world.
- `with Stdout` declares that the program needs the `wasi:cli/stdout` capability.

Save it as `hello.wado` and run it with the Wado CLI:

```sh
wado run hello.wado                    # compile and run it
wado compile -o hello.wasm hello.wado  # compile to Wasm
wado compile -o hello.wat  hello.wado  # or to WAT
```

[Try Wado in your browser](https://wado-lang.org/playground/) without installing anything. The playground runs the compiler as Wasm, compiles your code, and executes the result entirely in your browser.

## Status

Wado is experimental. The core language is implemented. You can build and run CLI applications and HTTP services today using the Wado CLI. Wado HTTP services have also been tested on [Cloudflare Workers](cloudflare-worker/), and the playground provides compilation, execution, and language services entirely in the browser.

Wado runs in browsers today through transpilation. Its long-term vision is to run directly on native browser support for the WebAssembly Component Model. Other platform developments central to that vision include WASI 1.0 and GC across component boundaries.

## Installation

### Prebuilt binaries

Prebuilt binaries are published on
[GitHub Releases](https://github.com/wado-lang/wado/releases/latest) for:

- Linux (`x86_64`, `aarch64`) — `tar.gz`
- macOS (`arm64`) — `tar.gz`
- Windows (`x86_64`, `aarch64`) — `zip`

Each archive contains the `wado` binary plus `LICENSE` and `README.md`.

On macOS and Linux, run the commands below to download, verify, and install the Wado CLI. The provenance check requires the GitHub CLI (`gh`). Make sure `~/bin` is in your `PATH` before running `wado`.

```sh
# Download the latest release and verify its checksum and build provenance

BASE=https://github.com/wado-lang/wado/releases/latest/download
ASSET="wado-$(uname -s | tr A-Z a-z)-$(uname -m).tar.gz"

curl -fsSLO "$BASE/$ASSET"

if command -v shasum >/dev/null; then
  curl -fsSL "$BASE/SHA256SUMS.txt" | shasum -a 256 --ignore-missing -c -
else
  curl -fsSL "$BASE/SHA256SUMS.txt" | sha256sum --ignore-missing -c -
fi
gh attestation verify --repo wado-lang/wado "$ASSET"

tar xzf "$ASSET"
mkdir -p ~/bin && install -m 755 "${ASSET%.tar.gz}/wado" ~/bin/wado

wado --version # verify installation
```

Every release archive carries a signed [build provenance attestation](https://docs.github.com/en/actions/security-guides/using-artifact-attestations)
binding the file to the workflow run that built it; the `gh attestation verify`
step above checks it.

On Windows, download the matching `wado-windows-*.zip`, verify its checksum against `SHA256SUMS.txt`, and check its provenance with `gh attestation verify --repo wado-lang/wado <archive>`. Extract the archive and place `wado.exe` in a directory on your `PATH`.

### From source

If you have a Rust toolchain installed:

```sh
cargo install --git https://github.com/wado-lang/wado wado-cli
```

This builds the current `main` branch from source.

## The Wado CLI

- `wado compile FILE` - Compiles Wado source to Wasm/WAT
- `wado run FILE` - Runs Wado source using Wasmtime
- `wado test FILE` - Runs Wado tests using Wasmtime
- `wado doc FILE` - Shows the documentation for a Wado source file
- `wado format FILE` - Formats a Wado source file
- `wado dump FILE` - Dumps the internal representation of a Wado source file

## Documentation

The main documentation site is [wado-lang.org](https://wado-lang.org). You can also read the documentation in this repository:

- [Design Philosophy](docs/design-philosophy.md) — why Wado is the way it is
- [Cheatsheet](docs/cheatsheet.md) — quick syntax reference
- [Language Specification](docs/spec.md) — full language reference
- [Compiler Implementation](docs/compiler.md) — compiler internals and feature checklist
- [Benchmarks](benchmark/README.md) — performance vs C, JavaScript, and others
- [Other Documentation](docs) — WEPs, research notes, and more

## Development

The following sections cover working on Wado itself, releasing it, and tracking its performance.

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
mise trust                 # trusts the mise.toml config (first time only)
mise run on-task-started   # installs all project tools
mise tasks                 # lists available tasks
```

### VS Code Extension

The `wado-vscode/` directory contains a VS Code extension for syntax highlighting. It is not published to the marketplace, but you can install it locally for development:

```sh
mise run install-wado-vscode-dev    # install extension to ~/.vscode via symlink
mise run clean-wado-vscode-dev      # uninstall it from ~/.vscode
mise run update-wado-vscode-grammar # regenerate syntax files after changing syntax.rs
```

See [wado-vscode/README.md](wado-vscode/README.md) for more details.

### After Making Changes

```sh
mise run on-task-done # format, clippy-fix, update resources, test
```

### Releasing

Releases are made roughly weekly by merging a release PR managed by [tagpr](https://github.com/Songmu/tagpr).

How it works:

1. Every push to `main` opens a **Release PR** that bumps `[workspace.package].version` in both `Cargo.toml` and `wado.toml` (kept in lockstep so the CLI and the published Wado packages ship one version), regenerates `Cargo.lock`, and updates `CHANGELOG.md` from PRs merged since the previous tag.
2. Merging the Release PR pushes tag `v<next>`, which triggers `.github/workflows/release.yml`.
3. The default version bump is **patch**. Add a `tagpr:minor` or `tagpr:major` label to the Release PR to override it.

`tagpr` is the single version manager: the workspace version is bumped only by the Release PR, never by hand. Do not edit `[workspace.package].version` in `Cargo.toml` or `wado.toml` directly.

## Benchmarks

Per-commit performance tracking is published to GitHub Pages. Every push to `main` records runtime and binary size metrics.

- [Runtime Performance](https://wado-lang.github.io/wado/benchmarks/runtime-throughput/) — throughput (work per second, higher is better) for integer, float, array, string, and compression workloads (run on wasmtime at `-O1`/`-O2`/`-O3`)
- [Wasm Binary Size](https://wado-lang.github.io/wado/benchmarks/wasm-size/) — `.wasm` output size for representative programs (compiled at `-Os`)

For comparison results against other programming languages:

- [benchmark/README.md](benchmark/README.md)
- [wasm-size/README.md](wasm-size/README.md)

## How It Is Developed

### Agentic Coding

The compiler toolchain is developed entirely by coding agents under human direction:

- **General solutions**: Agents tend to write code tailored to the tests at hand. Human review steers them toward fixes that address the whole class of problems.
- **Minimal code**: Agents tend to generate more code than necessary. Keeping the compiler small and general requires regular pruning.
- **Architectural review**: Local fixes can accumulate into larger design problems. We've redesigned the compiler architecture from the ground up once so far.

### AI-Guided Optimization

**AI-guided optimization** is a technique where you show generated code to a coding agent and have it identify optimization opportunities. The agent's output is non-deterministic, but the insights can be turned into deterministic compiler rules.

Wado's optimizer is developed using this approach:

```text
Agent finds pattern → Human reviews → Deterministic optimization rule added
```

Show the generated WAT to an agent and ask it to spot inefficiencies. Review the suggestions, then implement them as permanent optimization passes.

## Authors

Copyright (c) 2026, FUJI Goro (a.k.a. gfx). Some rights reserved.

## License

MIT

See [LICENSE](LICENSE) for details.
