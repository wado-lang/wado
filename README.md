# Wado

Wado is a programming language targeting **Wasm/WASI** - Wasm in plain sight.

## Motivation

### Effect System = WASI Capabilities

Effects map directly to WASI capabilities, making side effects explicit and controllable:

```wado
fn download_and_save(url: String, path: String) with Http, FileSystem {
    let data = Http::get(url).body;
    FileSystem::write(path, data);
}
```

The `with` clause tells you exactly what a function can do. This enables:

- **Security**: Sandbox plugins with only the capabilities you grant
- **Testability**: Swap real effects with mocks via handlers
- **Clarity**: No hidden side effects

### Colorless Async

No async/await infection. Thanks to Wasm Stack Switching, all functions are "colorless":

```wado
fn fetch_all() with Http {
    let users = Http::get("/users");   // No await needed
    let posts = Http::get("/posts");   // Just works
    return (users, posts);
}
```

### Component Model First

Types are designed around the WebAssembly Component Model (CM):

- `struct` is Wasm GC internally, becomes CM `record` at boundaries
- `enum` vs `variant` distinction matches CM exactly
- Native `Stream<T>` and `Future<T>` types for WASI P3

### Built-in Reactive Signals

Wado has built-in support for reactive state (often called "signals" in other frameworks):

```wado
use {observe} from "core:reactive";

let reactive mut count = 0;           // Mutable reactive state
let reactive doubled = || count * 2;  // Derived value

observe(|| {
    println(`Count: {count}, Doubled: {doubled}`);
});

count += 1;  // Automatically propagates to `doubled` and triggers observe()
```

Why built-in instead of a library?

- **Compiler optimization**: Dependencies are analyzed at compile-time, generating precise Wasm update code with no runtime tracking overhead
- **Ergonomics**: No wrapper functions like `useState()`, `ref()`, or `createSignal()`
- **Automatic dependency tracking**: `observe()` automatically tracks reactive values accessed within the closure—no manual subscription needed
- **No virtual DOM**: Updates compile to direct mutations, not diffing
- **Context-aware**: In CLI, updates are synchronous; in event-looped environments (browser/GUI), updates may be batched for efficiency

## Hello World

```wado
#!/usr/bin/env wado
use { println, Stdout } from "core:cli";

// run() is the entry point of the wasi:cli's Command world
fn run() with Stdout {
    println("Hello, world!");
}
```

Run it:

```sh
wado run example/hello.wado
```

Compile to WebAssembly:

```sh
wado compile example/hello.wado # generates example/hello.wado
wado compile -o example/hello.wasm example/hello.wado # ditto
wado compile --format wasm example/hello.wado # ditto

wado compile --format wat example/hello.wado # generates example/hello.wat with WAT format
wado compile -o example/hello.wat example/hello.wado  # ditto
```

## Documentation

- [Cheatsheet](docs/cheatsheet.md) - Quick syntax reference
- [Language Specification](spec.md) - Full language reference
- [Compiler Implementation](docs/compiler.md) - Compiler internals and feature checklist
- [Benchmarks](benchmark/README.md) - Performance benchmarks vs C and JavaScript, and so on
- [Other Documentation](docs) - ADR, research notes, TODOs, etc.

## Development

### Install `cargo`

```sh
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

### Install `wasmtime`

```sh
cargo install wasmtime
```

### Build and Test

```sh
cargo build
cargo test
```

### On Your Task Done

```sh
make on-task-done # format, clippy-fix, update-bundled, test
```

### What's Done

There are E2E test fixtures in [wado-compiler/tests/fixtures/\*.wado](wado-compiler/tests/fixtures).

### VS Code Extension

The `wado-vscode/` directory contains a VS Code extension for syntax highlighting. It is not published to the marketplace, but you can install it locally for development:

```sh
make install-wado-vscode-dev    # install extension to ~/.vscode via symlink
make clean-wado-vscode-dev      # uninstall it from ~/.vscode
make update-wado-vscode-grammar # regenerate syntax files after changing syntax.rs
```

See [wado-vscode/README.md](wado-vscode/README.md) for more details.

## Authors

Copyright (c) 2026, FUJI Goro (a.k.a. gfx). Some rights reserved.

## License

MIT

See [LICENSE](LICENSE) for details.
