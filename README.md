# Wado

Wado is a new programming language targeting **Wasm/WASI** - Wasm in plain sight.

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
let reactive mut count = 0;           // Mutable reactive state
let reactive doubled = || count * 2;  // Derived value

count += 1;  // Automatically propagates to `doubled`
```

Why built-in instead of a library?

- **Compiler optimization**: Dependencies are analyzed at compile-time, generating precise Wasm update code with no runtime tracking overhead
- **Ergonomics**: No wrapper functions like `useState()`, `ref()`, or `createSignal()`
- **No virtual DOM**: Updates compile to direct mutations, not diffing
- **Context-aware**: In CLI, updates are synchronous; in event-looped environments (browser/GUI), updates may be batched for efficiency

## Hello World

```wado
use {println, Stdout} from "core:cli";

fn main() with Stdout {
    println("Hello, world!");
}
```

Run it:

```sh
wado run example/hello.wado
```

Or compile to WebAssembly:

```sh
wado compile -o hello.wasm example/hello.wado
wado compile -o hello.wat example/hello.wado  # Text format
```

## Documentation

- [Language Specification](spec.md) - Full language reference
- [Compiler Implementation](compiler.md) - Compiler internals and feature checklist

## Building

```sh
cargo build
cargo test
```

## License

MIT
