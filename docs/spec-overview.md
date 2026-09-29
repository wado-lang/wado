# Wado Language Specification

Wado is a programming language that targets Wasm and WASI: Wasm in plain sight.

## Status

The specification is the `spec-*.md` files, one per area of the language. It is
normative: it says what the language is meant to be, and you read a program's
meaning from here. It is not a record of what the compiler happens to
do today. [Chapters](#chapters) lists the files in reading order.

The specification states what a program can observe. How the compiler produces
it, such as which copies it makes or what it inlines or folds, is not part of
the language, so the specification does not say.

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

## Chapters

Each chapter builds on the ones before it.

1. [Lexical Structure](./spec-lexical.md): whitespace, comments, identifiers and keywords.
2. [Literals](./spec-literals.md): numbers, strings, templates, tuples, lists and compile-time literals.
3. [Statements and Expressions](./spec-expressions.md): statements, variables, globals, operators and ranges.
4. [Types](./spec-types.md): primitives, strings, tuples, lists, newtypes, structs, enums and variants.
5. [Patterns](./spec-patterns.md): taking a value apart in `match`, `let` and `for`.
6. [Control Flow](./spec-control-flow.md): branches, loops, labeled blocks and error handling.
7. [Assertions](./spec-assertions.md): the `assert` statement and its failure message.
8. [Functions](./spec-functions.md): declarations, methods, generics, closures and default arguments.
9. [Memory Model](./spec-memory.md): value semantics and references.
10. [Traits](./spec-traits.md): declaring, implementing and bounding traits.
11. [Standard Traits](./spec-standard-traits.md): the traits behind `for-of`, comparison, operators and indexing.
12. [Static Reflection](./spec-reflection.md): the compile-time view of a type's members.
13. [Effect System](./spec-effects.md): declaring, propagating and handling effects.
14. [Module System](./spec-modules.md): visibility, imports and re-exports.
15. [Packages](./spec-packages.md): the manifest, dependencies and package specifiers.
16. [Kiln Generators](./spec-kiln.md): imports that a build-time generator writes.
17. [Worlds and Entry Points](./spec-worlds.md): what a program imports from its host and exports to it.
18. [Components](./spec-components.md): the Component Model boundary, concurrency and resources.
19. [Serialization](./spec-serialization.md): `Serialize`, `Deserialize` and wire formats.
20. [Compiler Attributes](./spec-attributes.md): the `#[...]` attributes.
21. [Testing](./spec-testing.md): `test` blocks, their outcomes and how `wado test` finds them.

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

At a component boundary, the compiler converts these names to WIT's
`kebab-case` and back.

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
- hosted world, library world: the two kinds of world, defined in
  [What is a World?](./spec-worlds.md#what-is-a-world); a hosted world is
  informally called a "well-known world"
