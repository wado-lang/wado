# Design Philosophy

Why Wado is the way it is.

## Why Wado exists

Wado was born from a practical need: embedding small Wasm modules in JavaScript projects without the binary-size explosion that comes with existing Wasm-targeting languages. Those bundle their own memory-management runtime into every `.wasm` file, so even a trivial program ships a bloated binary. Wado targets Wasm GC and lets the host own the collector, so nothing ships inside the module but the program's own logic.

That is not one principle among the ones below. It is the one they answer to: where a goal here conflicts with shipping no runtime, the runtime stays out.

Targeting the Component Model and WASI and nothing else is what makes it affordable. There is no legacy backend to carry and no glue layer to retrofit, so the platform's collector, its calling convention and its capability model can be used as they are rather than approximated.

## Principles

### Wasm in plain sight

The code you read is the code that runs, so the emitted WAT is something you can reason about by looking at the source.

Generation is asked for one thing: what it produces must be ordinary source you can open. [Kiln](./wep-2026-04-12-kiln.md) runs a generator in a sandbox and writes plain `.wado` to disk, so a whole dialect of Wado can live outside the language without any tool having to learn it, and the result is read the same way as the rest. A macro fails that — not because expanding code is wrong, but because its expansion exists nowhere you can open.

### Readable without context switching

Knowing what a call does should not require opening a file you were not already reading. The mechanisms that would break that are all present — overloading, coercion, derivation — so what carries the principle is a requirement on each of them: they should be predictable.

One spelling should reach one declaration. Resolution should follow a fixed ladder rather than a search over everything in scope, and no two candidates should be separated by a preference a reader has to have memorised. A distant file should at most be able to make a call legal that was not; it should never change what a legal call meant.

Where the source does not show which rule fired, saying so is the diagnostic's job, and the language service's before the compiler is run.

This is the least checkable principle here — it names a property nothing measures. It is kept because what it guards against costs more than a claim that could be checked is worth.

### Type-safe by design

Strong static typing with no escape hatch type like `any`. That removes the reason to reach for the defensive patterns — excessive `try-catch`, runtime type checks — that accumulate in dynamically-typed codebases.

### Errors are values

Errors are handled with `Result<T, E>` and `Option<T>`, not by unwinding exceptions. Control flow stays local and visible; there is no stack unwinding as a language feature.

## Effects are WASI capabilities

The most Wado-specific idea: every effect a function can perform is part of its type, and those effects are WASI's capabilities rather than a parallel invention. The platform's capability boundaries and the language's effects are the same thing.

```wado
use { println, Stdout } from "core:cli";

export fn run() with Stdout {
    println("Hello, world!");
}
```

The `with Stdout` clause says what this function can touch. That one idea buys three things:

- Security: a plugin runs with only the capabilities you grant it.
- Testability: real effects can be swapped for mocks via handlers.
- Clarity: an effect is declared, not discovered.

A few operations are exempt by design — ambient logging is the standing example — and an exemption is a decision with a name and a page, never a gap. What is being defended is that effects stay controllable, not that none escape.

## The platform's vocabulary, both ways

Wado did not invent a type system and then map it onto WIT. Its constructs are WIT's type kinds, which is why the `wasi:*` standard library can be generated from WIT rather than wrapped.

It runs both directions: Wado synthesizes WIT from your declarations and bundles it into the component, so any other Component Model language can bind to what you wrote.

## The language, in one breath

Wado takes Rust as its base and adapts it to this platform. Rust's shape without the parts that exist to manage memory by hand, since the collector is the host's; and the Component Model's type kinds where Rust has one, since that is the vocabulary the platform speaks. The surface borrows from TypeScript where TypeScript is the more familiar spelling.

## Informed by agentic coding

Wado is written by AI agents, with the human handling design and direction — as it was before Wado, which is where these came from, and as it still is, which is what keeps testing them.

- Agents are fast but literal. Implicit behavior multiplies across a codebase, so predictable semantics work better — hence no macros, and an overloading whose winner is decided by a fixed ladder rather than by what happens to be in scope.
- Agents drift toward defensive code. Without type safety they pile on runtime checks and nested error handling; strong static types remove the reason to.
- Exceptions break their reasoning. Non-local control flow is hard to predict, so failures are values that stay visible at the call site.

These are constraints the language was shaped around, not conventions asked of the people using it.
