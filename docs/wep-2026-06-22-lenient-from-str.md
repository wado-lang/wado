# WEP: Lenient String Parsing (`LenientFromStr`)

## Context

Human-supplied strings become typed values throughout a program: CLI arguments (`core:cli::args()`), environment variables, config entries, query strings, compile-time parameters ([Compile-Time Parameters](./wep-2026-04-26-compile-time-params.md), v2). Being human-typed, they carry irrelevant surface noise — surrounding whitespace, casing, alternate spellings like `1` / `0` for a boolean.

`core:prelude` already has `FromStr`, but it is strict by design: it backs hot, machine-facing paths (router path segments, the JSON deserializer's `from_str_range`) where `" 1 "` or `"TRUE"` must be rejected. Loosening it would break those.

So a parallel, forgiving conversion is missing. `core:cli::args()` returns a raw `List<String>`; every caller trims and parses by hand.

## Decision

Add `LenientFromStr` to `core:prelude` (auto-imported), the forgiving sibling of `FromStr`.

```wado
pub trait LenientFromStr {
    type Err;
    fn from_str_lenient(s: &String) -> Result<Self, Self::Err>;
}
```

- Returns `Result`: leniency widens the accepted _surface forms_, it does not salvage invalid input. An undenotable string still yields `Err`.
- No range variant — its consumers (args, env, config, params) are not the allocation-sensitive paths `FromStr::from_str_range` serves.
- `from_str_lenient` sits beside `from_str` in method completion.

### A Separate Trait, Not `FromStr` or `TryFrom<&String>`

Strictness and leniency are independent capabilities — a wire-format type wants only strict `FromStr`, a "human duration" only lenient parsing — so they are separate traits. Against the general `TryFrom<&String>` ([Conversion Traits](./wep-2026-03-16-conversion-traits.md)), `LenientFromStr` is the _named_ contract: the name promises trimming and common human spellings.

### Leniency Contract

Every impl trims leading/trailing ASCII whitespace, accepts the common human-equivalent forms for its type, and never panics — an undenotable value is `Err`. Leniency forgives _form_, not _meaning_: `"  42  "` → `42`, `"forty-two"` → `Err`.

### Built-In Implementations

| Type         | Accepted (after trim)                                 |
| ------------ | ----------------------------------------------------- |
| `String`     | Any string (identity)                                 |
| `i8`..`i128` | Decimal integer, optional sign and `_` separators     |
| `u8`..`u128` | Decimal non-negative integer, optional `_` separators |
| `f32`, `f64` | Standard floating-point literal (`3.14`, `-1e9`)      |
| `bool`       | `true` / `false` / `1` / `0`, case-insensitive        |

Each impl trims then delegates to the matching `FromStr`; `bool` adds the `1` / `0` and case-insensitivity. Future WEPs may extend the set (e.g. `core:temporal` multi-format dates).

### No Blanket Impl (Yet)

A blanket `impl<T: FromStr> LenientFromStr for T` would drop the per-type boilerplate but overlaps the `bool` impl. Wado grants concrete-over-general priority only for _variadic_ impls ([Variadic Type Parameters](./wep-2026-03-14-variadic-type-parameters.md)); general specialization is unadopted (soundness unsettled — `research-from-into-framework.md`). So built-in impls stay explicit; revisit if specialization lands.

### User Types

A user type implements `LenientFromStr` directly, independently of `FromStr` (either, both, or neither).

## Consumers

- Runtime CLI arguments / environment variables: `core:cli` gains a typed parse path, e.g. `i32::from_str_lenient(&args[1])`. The ergonomic surface (a helper on the argument list) is a `core:cli` follow-up.
- Compile-time parameters v2: v1 converts built-in types natively with accepted forms matching these impls, so v2 — evaluating `LenientFromStr` for arbitrary types via wasm-CTFE ([niri Stage 5](./wep-2026-04-27-nir-interpreter.md)) — preserves built-in behavior.

## Implementation Strategy

- Define the trait in `core:prelude` (`lib/core/prelude/traits.wado`), auto-imported.
- Add the built-in impls (trim + delegate; `bool` extra), reusing `FromStr`'s error types (`ParseIntError`, `ParseFloatError`, `ParseBoolError`) as `Err`.
- Document in `docs/stdlib-core-prelude.md` and the cheatsheet.

## Consequences

- One discoverable contract for forgiving external input, shared by CLI args, env, config, and params.
- `FromStr` stays strict; hot paths untouched.
- Built-in impls wrap `FromStr`, so no parser is duplicated.
- A type wanting both parses implements two traits — acceptable, since the contracts differ and built-ins bridge them in one line.

### Future Extensions

- Blanket `impl<T: FromStr> LenientFromStr for T`, once specialization is settled.
- A derive for enums (case-insensitive discriminant names).
- Lenient date/time parsing in `core:temporal`.
- A `core:cli` typed-argument API on the trait.
