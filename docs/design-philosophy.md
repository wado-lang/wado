# Design Philosophy

Why Wado is the way it is.

## Why Wado exists

Wado was born from a practical need: embedding small Wasm modules in JavaScript projects without the binary-size explosion that comes with existing Wasm-targeting languages.

Existing solutions bundle their own memory-management runtime into every `.wasm` file, so even a trivial program ships a bloated binary. Wado takes a different approach: by targeting Wasm GC, the garbage collector is provided by the host runtime (wasmtime today; browsers once the Component Model lands), so nothing ships inside the module but the program's own logic.

The timing matters too. With the Wasm Component Model and WASI 0.3 maturing, Wado is designed from the ground up for this platform — no legacy backend to carry, no glue layer to retrofit. It targets that platform and nothing else.

## Principles

### Wasm in plain sight

No macros. The code you read is the code that runs, so the emitted WAT is something you can reason about by looking at the source.

### Readable without context switching

Explicit over implicit: no implicit type conversions, no function overloading, no hidden dependencies. You should never have to jump to another file to know what a function does.

### Type-safe by design

Strong static typing with no escape hatch like `any`. That removes the reason to reach for the defensive patterns — excessive `try-catch`, runtime type checks — that accumulate in dynamically-typed codebases.

### Errors are values

Errors are handled with `Result<T, E>` and `Option<T>`, not by unwinding exceptions. Control flow stays local and visible; there is no stack unwinding as a language feature.

### Small binaries

Leaning on Wasm GC instead of bundling a runtime keeps `.wasm` output compact. This is the core motivation behind the language.

## Effects are WASI capabilities

The most Wado-specific idea: every effect a function can perform is part of its type. Effects map directly onto WASI capabilities, so a side effect is never hidden — it appears in the signature.

```wado
use { println, Stdout } from "core:cli";

export fn run() with Stdout {
    println("Hello, world!");
}
```

The `with Stdout` clause says exactly what this function can touch. That one idea buys three things:

- Security: a plugin runs with only the capabilities you grant it.
- Testability: real effects can be swapped for mocks via handlers.
- Clarity: there are no hidden side effects to discover.

`Stdout` is not a Wado invention; it is `wasi:cli`'s standard-output interface. Effects surface as `interface` — a namespace of operations — and `resource` — a handle to something outside the program; the platform's capability boundaries and the language's effects are the same thing. One `interface` is at once a WASI interface, a Component Model import/export, and a user-defined effect, so effects are not only declared but handled: swap a real one for a mock, or interpret your own. See [`example/http_get.wado`](../example/http_get.wado) for a larger example that threads an outbound HTTP `Client` capability through the type.

## The platform's vocabulary, both ways

Wado didn't invent a type system and then map it onto WIT — its constructs are WIT's type kinds: `interface`, `record` → `struct`, `variant`, `enum`, `flags`, `resource`, `world`, `func` → `fn`, `async func` / `stream` / `future`, `tuple` → `[a, b]`, and `option` / `result` / `list` → `Option` / `Result` / `List`. The `wasi:*` standard library is generated from WIT this way.

It runs both directions. Wado synthesizes WIT from your declarations and bundles it into the component, so any other Component Model language can bind to it — and when both ends are Wado, it skips the canonical ABI and shares Wasm GC types directly.

## The language, in one breath

Wado takes Rust as its base and adapts it for this platform:

- Rust's structs, `impl` blocks, generics, traits, and exhaustive pattern matching — but no lifetimes, no borrow checker, no `unsafe`. Memory is the Wasm garbage collector's job, and values have value semantics: assigning or passing deep-copies, and only `&T` / `&mut T` share.
- Rust's `enum` is split to match the Component Model's type kinds: `variant` for sum types with payloads, `enum` for bare discriminants, `flags` for bitsets.
- TypeScript's influence shows in the surface: template strings with backtick interpolation, and ES-module-style `use { … } from "…"` imports.

## Informed by agentic coding

Wado is developed entirely through agentic coding: AI agents write the code while the human handles design and direction. That is not a side note — it shaped the language. After a stretch of intensive agentic work, a few things were clear:

- Agents are fast but literal. Implicit behavior multiplies across a codebase, so predictable, explicit semantics work better — hence no implicit conversions, no overloading, no macros.
- Agents drift toward defensive code. Without type safety they pile on runtime checks and nested error handling; strong static types remove the reason to.
- Exceptions break their reasoning. Non-local control flow is hard to predict, so failures are values that stay visible at the call site.

The result is a language where common agentic-coding pitfalls are removed by design, not by convention.
