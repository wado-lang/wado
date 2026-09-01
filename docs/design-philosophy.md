# Design Philosophy

Why Wado is the way it is.

## Why Wado exists

Wado was born from a practical need: embedding small Wasm modules in JavaScript projects without the binary-size explosion that comes with existing Wasm-targeting languages. Those bundle their own memory-management runtime into every `.wasm` file, so even a trivial program ships a bloated binary. Wado targets Wasm GC and lets the host own the collector, so nothing ships inside the module but the program's own logic.

Targeting the Component Model and WASI and nothing else is what makes that affordable. There is no legacy backend to carry and no glue layer to retrofit, so the platform's collector, its calling convention and its capability model can be used as they are rather than approximated.

## Wasm in plain sight

The principle the rest of the page descends from: Wado is in complete control of the wasm it emits. The code you read is the code that runs, and the emitted WAT is something you can reason about by looking at the source.

Two things follow from it rather than having to be pursued separately.

Binaries come out small. Nothing is in the module that the program did not ask for, because nothing was put there by a layer Wado does not own.

Unpredictability has nowhere to enter. A construct that only the compiler can expand, a runtime that acts between the source and the machine, a cost that appears without being written — each of these is something the source no longer accounts for, and Wado does not have a place to keep one.

Generation is not the same thing, and is not refused: it is asked to produce ordinary source you can open. See [Kiln](./wep-2026-04-12-kiln.md).

## Principles

### Readable without context switching

Knowing what a call does should not require opening a file you were not already reading. The mechanisms that would break that are all present — overloading, coercion, derivation — so what carries the principle is a requirement on each of them: they should be predictable.

One spelling should reach one declaration. Resolution should follow a fixed ladder rather than a search over everything in scope, and no two candidates should be separated by a preference a reader has to have memorised. A distant file should at most be able to make a call legal that was not; it should never change what a legal call meant.

This is the least checkable principle here — it names a property nothing measures. It is kept because what it guards against costs more than a claim that could be checked is worth.

### Type-safe by design

Strong static typing with no escape hatch type like `any`. That removes the reason to reach for the defensive patterns — excessive `try-catch`, runtime type checks — that accumulate in dynamically-typed codebases.

### Errors are values

Errors are handled with `Result<T, E>` and `Option<T>`, not by unwinding exceptions. Control flow stays local and visible; there is no stack unwinding as a language feature.

### Fast enough to be the reason

Speed is a requirement, not a bonus. A language is chosen for what it can be used for, and being an order of magnitude off native takes options away — so Wado aims to be close enough to native that the choice is never made against it on that ground. Readable and fast are both required; a language that gives up either has given up a reason to exist.

Compilation is the other half. Time spent waiting on a build is time not spent on the problem, and a slow compiler quietly reshapes how people work around it.

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

## Shaped by agentic coding

Wado is designed and directed by a human and built with coding agents, and that has shaped the language — though not in the direction the phrase usually points.

The more complexity a system carries, the more an AI gets it wrong. This is not a fact about AI. People get it wrong too, in the same places and for the same reason; the agent merely arrives at those places sooner and more often, which is what makes it a good instrument for finding them.

So what came of building this way is not features for AI. Code a person can read and write easily is code an agent can read and write easily, and the ways a language can be made pleasant for a model are the ways it is made pleasant for a reader. Aiming at the model directly buys nothing that aiming at the reader does not already buy, and it costs the thing that matters most: that there is one audience to design for instead of two.

AI-friendliness, as a goal held apart from readability, is an illusion. Build the language for people; where a model needs something further, that is the documentation's problem and the tooling's, not the language's.

The one exception at present is power-assert. Its output exists for the moment a program is wrong and someone has to find out why, and it serves whoever is reading — which is the shape an exception has to have to be worth making.
