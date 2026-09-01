# Design Philosophy

Why Wado is the way it is.

## Why Wado exists

Wado started from a practical problem: putting small Wasm modules into JavaScript projects without the binary getting huge. Existing Wasm-targeting languages bundle their own memory-management runtime into every `.wasm` file, so even a trivial program ships a large binary.

Wado targets Wasm GC and lets the host runtime own the garbage collector. Nothing ships inside the module except the program's own logic.

Targeting the Component Model and WASI, and nothing else, is what makes that possible. There is no older backend to keep working and no glue layer to retrofit. The platform's collector, its calling convention and its capability model can be used directly instead of approximated.

## Wasm in plain sight

Everything below follows from one idea: Wado is in full control of the wasm it emits. The code you read is the code that runs, and you can work out the emitted WAT by looking at the source.

Two things come out of that on their own, without having to be chased separately.

Binaries stay small. Nothing ends up in the module that the program did not ask for, because there is no layer outside Wado's control that could put it there.

Unpredictability has no way in. A construct only the compiler can expand, a runtime that sits between the source and the machine, a cost that appears without being written down — each of these is something the source stops accounting for. Wado has nowhere to keep one.

Generating code is a different thing, and it is allowed. The requirement is that what comes out is ordinary source you can open and read. See [Kiln](./wep-2026-04-12-kiln.md).

## Principles

### Readable without context switching

Working out what a call does should not send you to a file you were not already reading.

There is no function overloading. A name does not stand for a set of free functions to pick between. What does exist is operator overloading, and method calls that pick among trait candidates using the types at the call site. Coercion and derivation work the same way. None of these is refused. What is asked of each is that it be predictable.

In practice that means three things. One spelling should reach one declaration. Resolution should follow a fixed order, not a search over everything in scope. And a file somewhere else should never change what an already-valid call means — at most it can make a call valid that was not valid before.

This is the least checkable principle here, because nothing measures it. It is kept anyway: what it prevents is worth more than the neatness of a claim that could be verified.

### Type-safe by design

Strong static typing, with no escape hatch type like `any`. That takes away the reason to write defensively — the piled-up `try-catch`, the runtime type checks — that dynamically-typed codebases accumulate.

### Errors are values

Errors travel in `Result<T, E>` and `Option<T>`. There are no exceptions to unwind. Control flow stays local and visible, and stack unwinding is not a language feature.

### Fast enough to be the reason

Speed is a requirement, not a bonus. People pick a language for what they can use it for, and being ten times slower than native takes uses off the table. Wado aims to stay close enough to native that speed is never the reason to choose something else. Readable and fast are both required. A language that gives up either one has given up a reason to exist.

Compile time is the other half of this. Time spent waiting for a build is time not spent on the problem, and people quietly rearrange how they work to avoid a slow compiler.

## Effects are WASI capabilities

The most Wado-specific idea: every effect a function can perform is part of its type, and those effects are WASI's own capabilities rather than a separate invention. One `interface` is three things at once — a WASI interface, a Component Model import or export, and a user-defined effect. The platform's capability boundaries and the language's effects are the same thing.

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

A few operations are exempt on purpose. Ambient logging is the standing example. An exemption is a decision with a name and a page behind it, not a hole. What is being defended is that effects stay controllable — not that none of them ever escape.

## The platform's vocabulary, both ways

Wado did not invent a type system and then map it onto WIT. Its constructs are WIT's type kinds. That is why the `wasi:*` standard library can be generated from WIT instead of hand-wrapped.

It works in the other direction too. Wado builds WIT from your declarations and bundles it into the component, so any other Component Model language can bind to what you wrote.

## The language, in one breath

Wado takes Rust as its base and adapts it to this platform.

It keeps Rust's shape but drops the parts that exist for managing memory by hand, because the collector belongs to the host. Where Rust has one kind of type, Wado uses the Component Model's several, because that is the vocabulary the platform speaks. The surface borrows from TypeScript wherever TypeScript's spelling is the more familiar one.

## Shaped by agentic coding

Wado is designed and directed by a human, and built with coding agents. That has shaped the language — but not in the direction the phrase usually suggests.

The more complexity a system carries, the more an AI gets wrong. This is not really a fact about AI. People get the same things wrong, in the same places, for the same reasons. The agent just arrives at those places sooner and more often, which is what makes it good at finding them.

So what came out of building this way is not features for AI. Code a person can read and write easily is code an agent can read and write easily. The things that make a language pleasant for a model are the things that make it pleasant for a reader. Aiming at the model directly buys nothing that aiming at the reader does not already buy, and it costs something real: you now have two audiences to design for instead of one.

AI-friendliness, treated as a goal separate from readability, is an illusion. Build the language for people. Where a model needs something more, that is a job for the documentation and the tooling, not for the language.

There is one exception right now: power-assert. Its output exists for the moment a program is wrong and someone has to find out why, and it helps whoever is reading. That is the shape an exception has to have to be worth making.
