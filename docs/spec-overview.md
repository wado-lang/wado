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

A rule the language has adopted but the compiler has not built yet stays in the
specification. A note right after the rule marks it, saying what the compiler
does until then:

> Not yet implemented: a function writes a mutable global without declaring an
> effect.

## Behavior Classes

Where the specification leaves behavior open, it says which of three classes
the behavior belongs to.

- _Unspecified_: the specification lists the permitted outcomes, and the
  compiler chooses one. The choice may change with the optimization level or the
  compiler version, but a compiled program keeps it on every host. A correct
  program is correct under every listed outcome.
- _Host-defined_: unspecified, except that the compiled program need not fix
  the choice, so the host may make it. The same compiled program may behave
  differently on two hosts. Each case says whether the choice stays fixed on
  one host. What a world import returns is host-defined within
  what its interface promises: `InsecureSeed` may return the same value every
  time, for one.
- _Unconstrained_: the specification lists no outcomes, and the compiler may
  assume the case never arises. Its effects may appear anywhere in the program
  and at any later time. Only these guarantees remain: the Wasm instance stays
  memory-safe, components stay isolated from each other, no capability is used
  that the world's imports did not grant, and GC references stay well-typed. A
  trap, a wrong value, a loop that never ends, or data leaking inside the
  instance are all possible.

A _contract violation_ is a program reaching an operation outside the contract
the operation states, such as calling an `_unchecked` function on input it does
not accept. It is always a bug, and each operation says which class of behavior
its violation has. Unconstrained behavior arises from a contract violation and
from nothing else.

Any build may detect a contract violation and trap, and
[Contract Checks](./spec-assertions.md#contract-checks) says which builds do.
No build may trap on unspecified or host-defined behavior that is not a
contract violation.

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
21. [Diagnostics](./spec-diagnostics.md): the warnings lints report on code the compiler accepts.
22. [Testing](./spec-testing.md): `test` blocks, their outcomes and how `wado test` finds them.

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
