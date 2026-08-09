# WEP: Compile-Time Data Providers

## Context

Some libraries are dominated not by code but by data: Unicode and CLDR tables,
time-zone databases, message catalogs, font glyph sets, emoji and script tables,
unit-conversion tables, geodata. They share a shape — the library ships
everything, and each program uses a sliver. A program that uppercases Turkish
text needs one language's casing rules, not every language's; a program
formatting one time zone needs one zone, not the whole database.

No library can fix this for itself. Which fraction a program uses is a
whole-program fact, known only after the compiler has resolved what the program
reaches. The library author can guess and offer feature flags, which pushes the
problem onto every consumer and gets it wrong for most of them; or ship
everything and be too large to use. Both are the status quo across languages.

This WEP adds the missing capability to the toolchain: a package can declare
that some of its components are backed by a **data provider**, a component the
compiler runs at build time to produce exactly the data that program needs. The
mechanism is a language feature open to any package, not a fixture serving one
first-party library. [`core:icu`](./wep-2026-08-09-core-icu.md) is its first
consumer and its proof, not its purpose.

### Why not Kiln

[Kiln](./wep-2026-04-12-kiln.md) already provides most of the plumbing:
sandboxed, deterministic, content-addressed compile-time components invoked by
the compiler and cached by content. But Kiln is deliberately narrowed to
"IDL → Wado source", and data provisioning differs on all three axes that
narrowing fixes:

- The output is a binary asset wired into a prebuilt component, not `.wado`
  source that the frontend then parses.
- The trigger is which exported symbols a program actually reaches, not a schema
  file named at a `use` site.
- The invocation is once per (module, program) after reachability is known, not
  once per declaration site.

So this is a sibling mechanism that reuses Kiln's infrastructure — the sandbox,
the content-addressed cache, host-delegated execution — under its own contract,
rather than a widening of Kiln's charter.

## Decision

### Provider-backed components

A package may ship three things instead of one:

- **Implementation components** — prebuilt wasm components holding the code with
  no data baked in. They receive their data when the consuming program is built.
- **A provider component** — a wasm component exporting the `data-provider`
  world, which turns a set of used symbols into the data those implementation
  components need.
- **Data assets** — opaque files in the package that only the provider reads.

The package's Wado surface is ordinary Wado source importing those
implementation components. Nothing about being provider-backed appears in the
API, so a consumer writes an ordinary `use` and never learns the mechanism
exists.

The provider's source language is unconstrained: the contract is defined over
the component, so a library whose slicer only exists in Rust, C, or Go ships
that slicer compiled to wasm.

### The provider contract

```wit
package core:provider;

interface types {
  record request {
    module: string,         // the provider-backed module being served
    symbols: list<string>,  // the symbols this program actually reaches
  }
  record blob {
    component: string,      // which implementation component this data is for
    data: list<u8>,
  }
  record response {
    blobs: list<blob>,
  }
}

world data-provider {
  import provider-host;
  use types.{request, response};
  export provide: func(req: request) -> response;
}
```

One invocation per (provider-backed module, program), after reachability is
known. A component that receives no blob is one no live symbol reached, and it
is not linked at all — neither its code nor its data enters the output.

How a use site's `with { ... }` options reach the provider is deliberately
absent; see "Options".

### The sandbox

The provider runs on the consumer's machine at build time, so what it cannot do
is as much of the contract as what it can. Its world imports one host interface:

```wit
interface provider-host {
  /// Read a file from the provider's own package as raw bytes. Reads are
  /// scoped to that package, recorded, and contribute to the cache key.
  read-asset: func(name: string) -> result<list<u8>, host-error>;

  /// Report a diagnostic, surfaced as an ordinary compile diagnostic.
  emit-diagnostic: func(diagnostic: diagnostic);
}
```

Two properties matter, and both are structural rather than policy:

A provider reads its own package and nothing else. It cannot see the consuming
program's source, its dependencies, or any path on the machine. It learns which
of its own symbols were used — names from its own API — and nothing further about
the program that used them. Kiln's `read-file`, which resolves user files
relative to a declaration site, is deliberately absent: a data provider has no
business in the consumer's tree, so the two host interfaces stay disjoint and
`core:kiln/kiln-host` is left untouched.

A provider is a pure function. No clocks, no randomness, no network, no sockets,
no environment. Violating that would require exporting a different world, which
the compiler refuses to load. Determinism is what makes the output cacheable and
reproducible builds possible; it is not a flag anyone can turn off.

Because third-party code runs here, the sandbox also carries a resource ceiling —
fuel and a wall-clock deadline — so a malformed or hostile provider fails the
build instead of hanging it. This is a requirement of the mechanism, not a
deferred nicety: a compile-time component that can run forever is a supply-chain
denial of service.

### Reachability drives everything

The set of reached symbols comes from the elaborator's `liveness` pass
([elaborator rearchitecture](./wep-2026-05-26-elaborator-rearchitecture.md)),
which already computes the closure of items reachable from the export boundary
and already feeds both `reify` and the
[unused-import diagnostics](./wep-2026-05-16-unused-diagnostics.md). Data
provisioning is a third consumer of the same result.

For free functions this needs nothing new: Wado's explicit named imports make the
imported name itself the usage signal, so no whole-program call-graph pass is
added.

Methods are the part the pass does not answer today. It classifies free functions
and globals, and seeds every method as a live root so that no method is reported
dead — a soundness choice that defers method-level detection to a follow-up slice
its own design names. Until that lands, a live type implies every one of its
methods, so a provider-backed library must carry its coarse split in its types
rather than in its methods.

Over-keeping is always safe: it over-bundles data and never changes behaviour.

### Options

A provider-backed module's `with { ... }` options are typed against a declaration
on its surface, so the compiler validates a use site before any provider runs,
and merges across sites by type: `list<T>` options union, scalar options must
agree or a conflict is a diagnostic. That much is settled and independent of
carriage.

How the declaration is written, and how the validated value reaches the provider,
is not. Kiln solves the same problem for generator options but not in a form this
mechanism can adopt verbatim: a Kiln generator carries its options as a typed
argument in a world it synthesizes for itself, whereas providers all conform to
one `data-provider` world. Decide it when the mechanism is implemented.

### Packaging and versioning

The provider, the implementation components, the data assets, and the Wado
surface are one package. That is not a convenience — it is what keeps them
coherent. Slicing data for version N and instantiating it in a component built
for version N−1 is exactly the failure this mechanism could otherwise introduce,
and a single package version makes it unrepresentable.

Distribution is the ordinary package path, so a provider-backed library is
published, fetched, and locked like any other. A first-party library may instead
ship with the toolchain; that changes where the package comes from and nothing
about how it works.

### Caching

An invocation's cache key covers the package identity and version, the sorted
symbol set, the canonical options, the provider component's hash, and the assets
it read. Repeat builds skip the provider entirely.

Recorded reads follow Kiln's model: the reads observed on the previous successful
run participate in the next key. This is sound because a provider is
deterministic — its read set is a function of the other key inputs — and it is
the same technique build systems use for header dependencies.

### Hosts that cannot run a provider

Running a provider requires a wasm runtime with Component Model support, which
the native toolchain has and a browser-embedded one may not. The degradation is
clean, and better than Kiln's: a provider-backed module's _type surface_ is
prebuilt and needs no provider, so name resolution, type checking, hover, and
go-to-definition all work untouched. Only producing a binary is unavailable, and
a host that cannot run providers is typically a host that does not generate code
anyway.

### Reporting what data costs

The mechanism exists to control size, so the toolchain reports what was
included: per live symbol, the bytes its data contributed. Without it a user has
no way to discover that one import is responsible for most of their binary, which
is precisely the situation the mechanism is meant to end. This is the same
posture as [optimizer remarks](./wep-2026-06-03-optimizer-remarks.md) — the
compiler explains a decision the user cannot otherwise see.

## Alternatives considered

### Leave it to the library

Feature flags, or one crate per data set, is what most ecosystems do. It works
only when the consumer knows which fraction they need and is willing to maintain
that knowledge as their code changes. It is wrong by default, it is wrong again
after every refactor, and it cannot express "the data for exactly the symbols
this program reaches" at all, since no library can see that.

### Ship everything, once

Bake the full data into the component and accept the size. It is the simplest
thing that works and is the right answer for small data sets — which is why this
mechanism is opt-in per package rather than a rule. It stops working as soon as
the data is large and the usage is narrow, which is the entire class this WEP
addresses.

### Prebuilt variants

Publish one component per plausible configuration — per locale set, per feature
subset — and let the consumer pick. The combinatorics defeat it: the useful
configurations are the power set of the capabilities crossed with the locale set,
so either the matrix is unpublishable or the offered variants are the same
guesses feature flags make.

### A native slicer in the toolchain

For a first-party library the slicer could be ordinary Rust linked into the
toolchain: no component, no sandbox, no protocol, and a much smaller design.
Rejected because it forecloses the point. A slicer inside the compiler serves
only libraries shipped with the compiler, so every third-party library dominated
by data would be back to feature flags, and each new one would arrive as a
compiler patch. What is being built is a capability of the language, and a
capability only its authors can use is not one.

## Consequences

- A library whose value is data becomes publishable without forcing its size on
  every consumer. This is a class of library that currently either does not get
  written or gets written as a pile of feature flags.
- The compiler runs third-party code at build time. The sandbox and the resource
  ceiling are what make that acceptable, so neither is optional, and both are
  load-bearing parts of the contract rather than hardening added later.
- A provider-backed package is more work to build than an ordinary one — a
  slicer, data assets, and data-free components instead of one library — so the
  mechanism earns its keep only where the data genuinely dominates. That is the
  intended filter.
- Builds gain a compile-time execution step. It is content-addressed, so it is
  paid once per distinct configuration rather than per build.
- Type checking stays independent of it: surfaces are prebuilt, so editors and
  wasm-hosted toolchains work with no provider at all.
- Data and code cannot drift apart, because they are versioned as one package.

## Open questions

- [ ] The declaration surface: how a package states which components are
      provider-backed, which provider serves them, and where its data assets
      live. The manifest is the obvious home, alongside Kiln's `generator` field.
- [ ] The options protocol, per "Options".
- [ ] Method-level reachability in the `liveness` pass, without which a live type
      implies every method.
- [ ] Whether a provider may depend on another provider's output. Kiln allows
      generator DAGs; the analogous case here has no motivating example yet, and
      forbidding it keeps invocation a single flat step.
- [ ] The fuel and deadline defaults, and whether a consumer can raise them for a
      provider they trust.

## Implementation

- [ ] The `data-provider` world and `provider-host`, with the shared diagnostic
      types hoisted out of `core:kiln/kiln-host` so both hosts use one shape.
- [ ] The declaration surface and its manifest schema.
- [ ] The provisioning phase: aggregate live symbols and options off `liveness`,
      invoke the provider, embed each blob into its component, compose, and cache
      by content.
- [ ] The resource ceiling: fuel plus a wall-clock deadline, with the failure
      reported against the use site.
- [ ] Per-symbol data-cost reporting.
- [ ] [`core:icu`](./wep-2026-08-09-core-icu.md) as the first consumer, which is
      what proves the contract against a real slicer rather than a toy one.

## References

- [Kiln](./wep-2026-04-12-kiln.md) — the compile-time execution and caching
  infrastructure this reuses.
- [`core:icu`](./wep-2026-08-09-core-icu.md) — the first consumer.
- [research: splitting large libraries](./research-library-splitting.md) — the
  levers and hard constraints, measured.
- [Wasm CM Component Import](./wep-2026-06-26-wasm-cm-component-import.md) — how
  implementation components are consumed and composed.
- [Package Manifest](./wep-2026-02-14-package-manifest.md) — where the
  declaration surface will live.
