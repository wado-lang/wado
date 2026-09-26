# Wado Language Specification

Wado is a programming language targeting Wasm/WASI -- Wasm in plain sight.

## Status

The specification is the `spec-*.md` files, one per area of the language. It is
normative: it says what the language is meant to be, and you read a program's
meaning from here. It is not a record of what the compiler happens to
do today. The [index](./README.md) lists the files.

So if the specification and the implementation disagree, something is wrong.
Which of the two is wrong is not decided in advance: the specification can be
the mistaken one. That gets settled when the disagreement is found.

What is not allowed is leaving the disagreement in place as an accepted
difference. If it is not resolved, it becomes a Known gap in the WEP that
proposed the rule, saying what the disagreement is and what it admits. The
specification itself records no bugs.

## Overview

| Item      | Description               |
| --------- | ------------------------- |
| Name      | Wado                      |
| Extension | `.wado`                   |
| Paradigm  | Imperative, Effect System |
| Typing    | Static, Strong, Inferred  |
| Target    | Wasm/WASI                 |

See also: [Cheatsheet](./cheatsheet.md) for quick syntax reference.

## Design Philosophy

See [Design Philosophy](./design-philosophy.md). The rules those principles
produced are stated here, each where it applies: [Memory Model](./spec-memory.md#memory-model),
[Concurrency Model](./spec-components.md#concurrency-model), [Effect System](./spec-effects.md#effect-system).

## Appendix

### Naming Conventions

| Element            | Style            |
| ------------------ | ---------------- |
| Package name       | `kebab-case`     |
| Module/file name   | `snake_case`     |
| Primitive types    | `lowercase`      |
| User-defined types | `UpperCamelCase` |
| Enum/variant cases | `UpperCamelCase` |
| Functions          | `snake_case`     |
| Local variables    | `snake_case`     |

Component Model interop: The compiler automatically converts between Wado conventions and WIT conventions (kebab-case) at component boundaries.

### Terminology

- Wasm: WebAssembly (not WASM)
- WASI: WebAssembly System Interface
- CM: Wasm Component Model
- module: a Wado file
- package: a collection of modules, described by one `wado.toml`
- Wado standard library: consists of `core:` and `wasi:`
- effect: the concept; e.g., "the `Stdout` effect"
- effect interface: the declaration (`interface Stdout { ... }`); synonyms in literature: "effect signature", "effect type"
- operation: a function in an effect interface; synonym: "effect operation"
- handler: provides implementations for operations
- hosted world: a world that a runtime knows how to instantiate and drive (e.g., `wasi:cli/command` for `wado run`, `wasi:http/service` for `wado serve`); informally called "well-known world"
- library world: a world that defines a component's public API for composition with other components, rather than for direct execution by a runtime
