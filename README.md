# The Wado Programming Language

A type-safe, high-level WebAssembly.

## Why Wado?

Wado was born from a practical need: embedding small Wasm modules in JavaScript projects without the binary size explosion that comes with existing Wasm-targeting languages.

Existing solutions bundle their own memory management runtime into every `.wasm` file, resulting in bloated binaries even for simple tasks. Wado takes a different approach: by leveraging **Wasm GC**, the garbage collector is provided by the Wasm runtime itself (currently wasmtime; browser support pending Wasm Component Model), keeping your binaries minimal.

The timing matters too. With Wasm Component Model and WASI P3 maturing in 2026, Wado is designed from the ground up for this new era — no legacy baggage, no retrofitting.

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

## Examples

### Hello World

```wado
#!/usr/bin/env wado run
use { println, Stdout } from "core:cli";

// run() is the entry point of the wasi:cli/command hosted world
export fn run() with Stdout {
    println("Hello, world!");
}
```

Run it:

```sh
wado run example/hello.wado
```

Compile to WebAssembly:

```sh
wado compile example/hello.wado # generates example/hello.wasm
wado compile -o example/hello.wasm example/hello.wado # ditto
wado compile --format wasm example/hello.wado # ditto

wado compile --format wat example/hello.wado # generates example/hello.wat with WAT format
wado compile -o example/hello.wat example/hello.wado  # ditto
```

### FizzBuzz

```wado
use { println, Stdout } from "core:cli";

variant FizzBuzz {
    Fizz,
    Buzz,
    FizzBuzz,
    Number(i32),
}

fn classify(n: i32) -> FizzBuzz {
    if n % 15 == 0 { return FizzBuzz::FizzBuzz; }
    if n % 3 == 0 { return FizzBuzz::Fizz; }
    if n % 5 == 0 { return FizzBuzz::Buzz; }
    return FizzBuzz::Number(n);
}

export fn run() with Stdout {
    for let mut i = 1; i <= 20; i += 1 {
        println(match classify(i) {
            Fizz => "Fizz",
            Buzz => "Buzz",
            FizzBuzz => "FizzBuzz",
            Number(n) => `{n}`,
        });
    }
}
```

```sh
wado run example/fizzbuzz.wado
```

## Key Features

### Language Basics

- Static typing with generics — strong type system with generic types, functions, and methods. No `any` escape hatch
- Rust-like semantics without lifetimes — value semantics, references (`&T`, `&mut T`), and structs with `impl` blocks, but memory is managed by Wasm GC, so no lifetime annotations needed
- Familiar syntax — borrows from Rust (structs, `impl`, `let`/`let mut`) and JavaScript/TypeScript (template strings with backticks, ES module-style `use {...} from "..."` imports)

### Effect System

Effects map directly to WASI capabilities, making side effects explicit and controllable:

```wado
use { println, Stdout } from "core:cli";
use { Url } from "core:url";
use { Client, Request, Response, ErrorCode, Fields, Scheme, Trailers } from "wasi:http";

fn scheme_of(url: Url) -> Scheme {
    return match url.scheme {
        "http" => Scheme::Http,
        "https" => Scheme::Https,
        scheme => Scheme::Other(scheme),
    };
}

fn send_get(url: Url) -> Response with Client {
    let headers = Fields::new();
    let [trailers_rx, trailers_tx] = Future::<Result<Option<Trailers>, ErrorCode>>::new();
    let [req, _req_future] = Request::new(headers, null, trailers_rx, null);
    req.set_scheme(Option::Some(scheme_of(url)));
    req.set_authority(Option::Some(url.authority()));
    req.set_path_with_query(Option::Some(url.path_with_query()));

    let result = Client::send(req).wait();
    trailers_tx.write(Result::Ok(null));

    return match result {
        Ok(resp) => resp,
        Err(e) => panic(`HTTP request failed: {e}`),
    };
}

export fn run() with Stdout, Client {
    let url = Url::parse("https://httpbin.org/get").unwrap();
    let resp = send_get(url);
    println(`Status: {resp.get_status_code() as i32}`);
}
```

`Client::send(...).wait()` is colorless async: the `wait()` suspends the task while the
runtime drives the request, without coloring `send_get` or `run` as `async`. See
[`example/http_get.wado`](example/http_get.wado) for the full version (request body,
streaming the response body, scheme handling).

The `with` clause tells you exactly what a function can do. This enables:

- **Security**: Sandbox plugins with only the capabilities you grant
- **Testability**: Swap real effects with mocks via handlers
- **Clarity**: No hidden side effects

## Design Principles

### What You See Is What You Get

No macros. The code you read is the code that runs — Wasm in plain sight.

### Readable Without Context Switching

Explicit over implicit. No implicit type conversions, no function overloading, no hidden dependencies. You shouldn't need to jump to other files to understand what a function does.

### Type-Safe by Design

Strong static typing with no escape hatches like `any`. This prevents the defensive programming patterns (excessive `try-catch`, runtime type checks) that tend to creep into dynamically-typed codebases.

### No Exception Unwinding

Errors are handled explicitly with `Result<T, E>` instead of unwinding exceptions. This makes control flow predictable and easier to reason about.

### Minimal Binary Size

By leveraging Wasm GC instead of bundling a runtime, Wado produces compact `.wasm` files. This is the core motivation behind the language.

## Status

Wado is experimental. The core language — syntax, static typing, generics, closures, modules — is implemented and functional.

However:

- **WASI P3 is not yet finalized**: The spec is at release candidate stage, and runtime support is limited
- **Wasm Component Model** is not yet supported in browsers
- **Wasm stack switching** is not yet available in wasmtime

That said, Wado is already usable for its original purpose: embedding lightweight Wasm modules in JS projects where binary size matters.

## Future Directions

### Full WASI P3 Integration

As the spec finalizes and runtime support matures, Wado will leverage async streams, futures, and the full capability model.

## Informed by Agentic Coding

Wado is developed entirely through agentic coding — AI agents write the entire code while the human handles design decisions and project management.

This isn't just a curiosity; it shaped the language itself. After a year of intensive agentic coding experience, certain patterns became clear:

- **Agents excel at volume but struggle with ambiguity.** Implicit behaviors get multiplied across a codebase. Explicit, predictable semantics work better.
- **Agents tend toward defensive programming.** Without type safety, they pepper code with `hasattr` checks and nested `try-except` blocks. Strong types eliminate this need.
- **Exceptions break agent reasoning.** Non-local control flow is hard to predict. `Result<T, E>` keeps everything visible.

The result: a language where common agentic coding pitfalls are eliminated by design, not convention.

## Documentation

- [Cheatsheet](docs/cheatsheet.md) - Quick syntax reference
- [Language Specification](docs/spec.md) - Full language reference
- [Compiler Implementation](docs/compiler.md) - Compiler internals and feature checklist
- [Benchmarks](benchmark/README.md) - Performance benchmarks vs C and JavaScript, and so on
- [Other Documentation](docs) - WEP, research notes, etc.

## Development

Wado is developed entirely through agentic coding — AI agents write all the code while the human handles language design and project management.

### Development Process

This approach requires active management:

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

1. Every push to `main` (re)opens a **Release PR** that bumps `[workspace.package].version` in `Cargo.toml`, regenerates `Cargo.lock`, and updates `CHANGELOG.md` from PRs merged since the previous tag.
2. Merging the Release PR pushes tag `v<next>`, which triggers `.github/workflows/release.yml` to build pre-built binaries for five targets in parallel — Linux (`x86_64`, `aarch64`), macOS (Apple Silicon), Windows (`x86_64`, `aarch64`) — and publishes them to a [GitHub Release](https://github.com/wado-lang/wado/releases) with `SHA256SUMS.txt`.
3. Default bump is **patch**. Add a `tagpr:minor` or `tagpr:major` label on the Release PR to override.

To bump the workspace version locally without going through tagpr:

```sh
mise run bump-version 0.2.0           # explicit
mise run bump-version --bump minor    # patch / minor / major
mise run bump-version --check 0.2.0   # CI guard: fail unless versions match
```

## Benchmarks

Per-commit performance tracking is published to GitHub Pages. Every push to `main` records runtime and binary size metrics.

- [Runtime Performance](https://wado-lang.github.io/wado/benchmarks/runtime/) — execution time for integer, float, array, string, and compression workloads (compiled at `-O2`, run on wasmtime)
- [Wasm Binary Size](https://wado-lang.github.io/wado/benchmarks/wasm-size/) — `.wasm` output size for representative programs (compiled at `-Os`)

See [benchmark/README.md](benchmark/README.md) and [wasm-size/README.md](wasm-size/README.md) for local benchmark instructions and comparison results against other programming languages.

## Authors

Copyright (c) 2026, FUJI Goro (a.k.a. gfx). Some rights reserved.

## License

MIT

See [LICENSE](LICENSE) for details.
