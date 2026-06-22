# WEP: Lenient String Parsing (`LenientFromStr`)

## Context

External, human-supplied strings have to become typed values all over a program: command-line arguments (`core:cli::args()`), environment variables, configuration entries, query strings, and compile-time parameters ([WEP: Compile-Time Parameters](./wep-2026-04-26-compile-time-params.md), v2). A human types these, so they carry surface noise the writer considers irrelevant — surrounding whitespace, casing, common alternate spellings (`1` / `0` for a boolean).

Wado already has `FromStr` in `core:prelude`, but it is the strict, Rust-compatible parser. It runs on hot, machine-facing paths — the router matching request-path segments, the JSON deserializer threading its input buffer — where `from_str_range` parses a byte range without allocating and where accepting `" 1 "` or `"TRUE"` would be wrong. Loosening `FromStr` to be forgiving would corrupt those semantics.

What is missing is a parallel, deliberately forgiving conversion. Today `core:cli::args()` returns a raw `List<String>` with no typed parse path; each caller trims and parses by hand.

## Decision

Add `LenientFromStr`, a sibling of `FromStr` in `core:prelude` (auto-imported), for forgiving string-to-value conversion.

### Trait

```wado
pub trait LenientFromStr {
    type Err;
    fn from_str_lenient(s: &String) -> Result<Self, Self::Err>;
}
```

- Returns `Result`: leniency accepts more _surface forms_, it does not salvage genuinely invalid input. A string that does not denote a value still yields `Err`, which the caller (CLI parser, params resolution) recovers from.
- One method, no range-based variant. `FromStr` has `from_str_range` for allocation-sensitive hot paths; this trait's consumers (args, env, config, params) are not those paths.
- The method name `from_str_lenient` sits next to `from_str` in method completion on every type.

### Why a Separate Trait, Not `FromStr` or `TryFrom<&String>`

- `FromStr` must stay strict (router / JSON correctness). Strictness and leniency are independent capabilities of a type, so they are independent traits: a "human duration" type might offer only lenient parsing, and a wire-format type might offer only strict `FromStr`.
- `TryFrom<&String>` ([Conversion Traits](./wep-2026-03-16-conversion-traits.md)) is the general fallible conversion; `LenientFromStr` is the _named, forgiving_ one. The name is the contract — implementors promise to trim and to accept common human spellings.

### Leniency Contract

Every `LenientFromStr` impl:

1. Trims leading and trailing ASCII whitespace before interpreting the input.
2. Accepts the common human-equivalent surface forms for its type (see built-ins).
3. Never panics; reports an undenotable value as `Err`.

Leniency is forgiving of _form_, not of _meaning_: `"  42  "` parses as `42`, but `"forty-two"` is an `Err`.

### Built-In Implementations

| Type         | Accepted (after trim)                                   |
| ------------ | ------------------------------------------------------- |
| `String`     | Any string (identity)                                   |
| `i8`..`i128` | Decimal integer, optional sign and `_` digit separators |
| `u8`..`u128` | Decimal non-negative integer, optional `_` separators   |
| `f32`, `f64` | Standard floating-point literal (`3.14`, `-1e9`)        |
| `bool`       | `true` / `false` / `1` / `0`, case-insensitive          |

Each built-in impl trims, then delegates the core parse to the corresponding `FromStr` impl; `bool` adds the `1` / `0` and case-insensitive acceptance on top. The accepted set is extensible by future WEPs (e.g. `core:temporal` adding multi-format date/time parsing).

### No Blanket Impl (Yet)

A blanket `impl<T: FromStr> LenientFromStr for T` (default = trim then `FromStr`) would drop the per-type boilerplate, but it overlaps with the `bool` impl that needs extra leniency. Wado grants concrete-over-general priority only for _variadic_ impls ([Variadic Type Parameters](./wep-2026-03-14-variadic-type-parameters.md), Coherence Rules); general blanket specialization is not adopted, its soundness being unsettled (see `research-from-into-framework.md`). So this WEP ships explicit built-in impls. A blanket impl is a future ergonomic improvement, contingent on Wado settling a specialization story.

### User Types

A user type implements `LenientFromStr` directly. It is independent of `FromStr`: a type may implement either, both, or neither.

## Consumers

- Runtime CLI arguments and environment variables. `core:cli` gains a typed parse path over `args()` / the environment, e.g. `i32::from_str_lenient(&args[1])`. The exact ergonomic surface (a helper on the argument list, etc.) is a `core:cli` follow-up; this WEP defines the trait.
- Compile-time parameters v2. v1 converts the built-in parameter types natively in the compiler; its accepted forms are specified to match this trait's built-in impls, so v2 — which evaluates `LenientFromStr` for arbitrary types through the wasm-CTFE backend ([niri Stage 5](./wep-2026-04-27-nir-interpreter.md)) — is behavior-preserving for the built-in types.

## Implementation Strategy

- Define `LenientFromStr` in `core:prelude` (`lib/core/prelude/traits.wado`), auto-imported alongside `FromStr`.
- Add explicit built-in impls: each trims then delegates to `FromStr`; `bool` adds the extra acceptances.
- Reuse the existing `FromStr` error types (`ParseIntError`, `ParseFloatError`, `ParseBoolError`) as the associated `Err`.
- Document in `docs/stdlib-core-prelude.md` and the cheatsheet's prelude-traits section.

## Consequences

### Positive

- One named, discoverable contract for "parse forgiving external input," reused across CLI args, env, config, and compile-time params.
- `FromStr` stays strict; hot machine-facing paths are untouched.
- Built-in impls are thin wrappers over `FromStr`, so there is no parser duplication.

### Trade-offs

- A type that wants both strict and lenient parsing implements two traits. Acceptable: they are genuinely different contracts, and the built-ins bridge them with one-line impls.
- No blanket impl, so each built-in needs an explicit (trivial) impl. Revisited if Wado adopts specialization.

### Future Extensions

- Blanket `impl<T: FromStr> LenientFromStr for T` once a specialization story is settled.
- A derive for enums (parse a discriminant name case-insensitively).
- Lenient date/time parsing in `core:temporal`.
- A `core:cli` typed-argument API built on the trait.
