# WEP: Deterministic Math Library (libm) Integration

## Context

IEEE 754 pins only the basic arithmetic operators (`+`, `-`, `*`, `/`, `sqrt`).
The transcendental functions — `sin`, `log`, `exp`, `pow` — are left to the
implementation, so the same source produces different bits depending on the C
library, the CPU architecture, and the operating system it ran on.

WebAssembly closes part of that gap: the arithmetic operators it defines are
IEEE 754 conformant, rounding is fixed to round-to-nearest-ties-to-even, and
there is no extended intermediate precision to vary. But Wasm defines no
transcendental instructions and WASI defines no math interface, so a program
that takes `sin` from its host inherits the host's libm — and loses determinism
at exactly the point where Wasm stops guaranteeing it.

Wado needs this beyond portability of results. Compile-time execution must agree
with run-time execution; with a host-provided transcendental, that agreement
would depend on the machine that ran the build.

## Decision

Bundle one fixed implementation of the transcendental functions with the
toolchain and link it into the program. Math is never a host import.

### Choice of implementation

Rust's `libm` crate (MIT OR Apache-2.0), derived from musl's libm.

| Criterion   | Rust `libm`                   | musl libm      | fdlibm         |
| ----------- | ----------------------------- | -------------- | -------------- |
| License     | MIT / Apache-2.0              | MIT            | permissive     |
| Language    | Rust                          | C              | C              |
| Wasm build  | native target, no C toolchain | needs wasi-sdk | needs wasi-sdk |
| `no_std`    | yes                           | no             | no             |
| Maintenance | active, musl-derived          | active         | quiescent      |

`no_std` is what makes it a leaf: the bundled artifact pulls in no libc and no
WASI, so bundling it adds no import to the program.

### Division of labour: instructions before library

Anything Wasm already pins — `sqrt`, `abs`, `ceil`, `floor`, `trunc`, `nearest`,
`min`, `max`, `copysign` — stays a Wasm instruction and never reaches the
bundled library. The library supplies only what has no instruction: the
trigonometric, hyperbolic, exponential, logarithmic, power, and remainder
families, for both `f32` and `f64`. Splitting on "does Wasm pin it" keeps the
bundled surface minimal at no cost to determinism.

### Form: a core module, not a component

The library's whole surface is scalars in, scalars out. It holds no state a
caller can observe, owns no handles, and passes no aggregates. So it is bundled
as a core Wasm module linked inside the produced component, not as a component
of its own: a canonical-ABI boundary would buy nothing that scalars need, and
the core-module form lets the bundled code share the program's linear memory
rather than standing up its own.

Only the functions a program actually reaches survive into the output — the
bundled module is pruned to the used export set when it is linked, so a program
that calls `sin` does not carry `pow`.

The pruning reaches the library's tables too — two thirds of the asset's 5.4 KB
of rodata is `exp2`'s alone. The asset carries a `wado.dataref` custom section
naming the rodata ranges each function reads, and the prune keeps only the
ranges the surviving functions claim: a program calling `sin` keeps 344 of the
5,448 bytes. `mise run update-bundled` resolves that map from the `linking` and
`reloc.CODE` sections of a `--emit-relocs` build and drops them, since they hold
byte offsets into code that the `.wat` round trip invalidates.

### Surface: methods, not a math module

The bundled asset is not a user-visible module. The prelude attaches its
functions to the primitive float types, so user code writes `x.sin()` and never
names the asset. There is no `core:math` re-exporter — with a single consumer it
would be pure indirection. The prelude reaches the asset through the ordinary
core-Wasm asset import mechanism
([WebAssembly Module Import](./wep-2026-01-10-wasm-import.md)); nothing about the
bundled library is special-cased in the import path.

### Versioning

The bundled implementation is pinned per toolchain release. Its numeric results
are observable behaviour, so upgrading it is a deliberate release decision, not
an automatic dependency bump — two programs built by one compiler version agree
bit for bit.

## Consequences

- Math is as deterministic as arithmetic: identical results across operating
  systems, architectures, and Wasm engines, and identical between compile-time
  and run-time evaluation.
- Math needs no host capability, so it works in every world — including ones
  with nothing to import it from.
- A program carries the code and the tables for the math it uses. Pruning bounds
  both to the reachable set, but a program spanning many families pays for them.
- The bundled implementation can be slower than a host's routines built on
  architecture-specific instructions. Determinism is the deliberate trade.
- Platform-specific extended precision is unavailable by construction. That is
  the point, not a limitation to mitigate.

## Alternatives considered

### Host math functions

Rejected: this is the status quo the WEP exists to avoid, and WASI defines no
math interface to import from in the first place.

### Software floating point

Rejected: determinism down to the arithmetic operators, at a cost in speed and
complexity that buys nothing, since Wasm already pins those operators.

### Lookup tables with interpolation

Rejected: precision bounded by table size is the wrong trade for a
general-purpose library.

### A standard deterministic math interface

Deferred. If Wasm or WASI ever defines a math interface carrying a determinism
guarantee, it supersedes the bundled library. Until then, bundling is the only
way to obtain the guarantee.

## References

- [Rust libm](https://github.com/rust-lang/libm)
- [Floating-point portability (Japanese)](https://zenn.dev/mod_poppo/articles/floating-point-portability)
- [WebAssembly floating-point semantics](https://webassembly.github.io/spec/core/exec/numerics.html#floating-point-operations)
- [WEP: WebAssembly Module Import Support](./wep-2026-01-10-wasm-import.md)
