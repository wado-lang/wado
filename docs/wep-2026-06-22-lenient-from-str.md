# WEP: Lenient String Parsing (`LenientFromStr`)

## Context

Human-supplied strings become typed values throughout a program: CLI arguments (`core:cli::args()`), environment variables, config entries, query strings, compile-time parameters ([Compile-Time Parameters](./wep-2026-04-26-compile-time-params.md), v2). Being human-typed, they carry surface variation a strict parser rejects — casing, alternate spellings (`1` / `0` for a boolean), radix prefixes, digit separators.

`core:prelude` already has `FromStr`, but it is strict by design: it backs hot, machine-facing paths (router path segments, the JSON deserializer's `from_str_range`) where `"TRUE"` or `"0x2A"` must be rejected. Loosening it would break those.

So a parallel, forgiving conversion is missing. `core:cli::args()` returns a raw `List<String>`; every caller parses by hand.

## Decision

Add `LenientFromStr` to `core:prelude` (auto-imported), the forgiving sibling of `FromStr`.

```wado
pub trait LenientFromStr {
    type Err;
    fn from_str_lenient(s: &String) -> Result<Self, Self::Err>;
}
```

- Returns `Result`: leniency widens the accepted _spellings_, it does not salvage invalid input. An undenotable string still yields `Err`.
- No range variant — its consumers (args, env, config, params) are not the allocation-sensitive paths `FromStr::from_str_range` serves.
- `from_str_lenient` sits beside `from_str` in method completion.

### A Separate Trait, Not `FromStr` or `TryFrom<&String>`

Strictness and leniency are independent capabilities — a wire-format type wants only strict `FromStr`, a "human duration" only lenient parsing — so they are separate traits. Against the general `TryFrom<&String>` ([Conversion Traits](./wep-2026-03-16-conversion-traits.md)), `LenientFromStr` is the _named_ contract: the name promises forgiving, human-friendly spellings.

### Leniency Contract

An impl accepts the common human-equivalent spellings for its type (see built-ins) and never panics — an undenotable value is `Err`. Leniency forgives _spelling_, not _meaning_: `"0x2A"` → `42`, `"forty-two"` → `Err`.

It does not touch whitespace. `from_str_lenient` does not trim, so surrounding whitespace yields `Err`; trimming is the caller's concern (`s.trim()`, or a consumer-level policy — e.g. compile-time params trims each override before converting). Keeping the conversion faithful avoids silently discarding input and resolves whitespace-significant types (`char`, `String`) uniformly.

### Built-In Implementations

| Type         | Accepted                                                                                   |
| ------------ | ------------------------------------------------------------------------------------------ |
| `String`     | Any string (identity)                                                                      |
| `char`       | Exactly one Unicode scalar                                                                 |
| `i8`..`i128` | Decimal, or `0x` / `0o` / `0b` (case-insensitive); optional leading sign                   |
| `u8`..`u128` | As integers, but unsigned (a leading `-` is `Err`)                                         |
| `f32`, `f64` | Decimal with optional `e` exponent; `nan` / `inf` / `infinity` / `±inf` (case-insensitive) |
| `bool`       | `true` / `false` / `1` / `0` (case-insensitive)                                            |

- Integers and floats ignore `_` anywhere in the digit body (`1_000`, `0xFF_FF`). `,` is _not_ a separator — it collides with the locale decimal comma and with future list/tuple delimiters; `_` matches Wado's own numeric literals.
- A leading zero is not octal: `010` is `10`, only `0o12` is octal.
- Numeric impls preprocess (strip `_`, split off sign / radix prefix) then delegate to `from_str_radix` / `from_str` (the float `FromStr` already accepts `nan` / `inf` / `infinity`); `bool` and `char` are handled directly. No impl alters whitespace.

Future WEPs may extend the set (e.g. `core:temporal` multi-format dates).

### No Blanket Impl (Yet)

A blanket `impl<T: FromStr> LenientFromStr for T` would drop the per-type boilerplate but overlaps the `bool` impl. Wado grants concrete-over-general priority only for _variadic_ impls ([Variadic Type Parameters](./wep-2026-03-14-variadic-type-parameters.md)); general specialization is unadopted (soundness unsettled — `research-from-into-framework.md`). So built-in impls stay explicit; revisit if specialization lands.

### User Types

A user type implements `LenientFromStr` directly, independently of `FromStr` (either, both, or neither). Newtypes inherit it from their base type — `type Port = u16` parses like `u16` with no extra impl.

## Consumers

- Runtime CLI arguments / environment variables: `core:cli` gains a typed parse path, e.g. `i32::from_str_lenient(&args[1].trim())`. The ergonomic surface (a helper that trims and parses) is a `core:cli` follow-up.
- Compile-time parameters v2: v1 converts built-in types natively with accepted forms matching these impls, so v2 — evaluating `LenientFromStr` for arbitrary types via wasm-CTFE ([niri Stage 5](./wep-2026-04-27-nir-interpreter.md)) — preserves built-in behavior.

## Implementation Strategy

- Define the trait in `core:prelude` (`lib/core/prelude/traits.wado`), auto-imported.
- Add the built-in impls and a single shared `Err` type, `LenientParseError` (a short reason string). One unified error keeps the associated-type story simple and gives the infallible `String` impl a never-returned `Err`; numeric impls map `ParseIntError` / `ParseFloatError` into it.
- Document in `docs/stdlib-core-prelude.md` and the cheatsheet.

## Consequences

- One discoverable contract for forgiving external input, shared by CLI args, env, config, and params.
- `FromStr` stays strict; hot paths untouched.
- The conversion is faithful: it never trims or otherwise discards input, so whitespace-significant values (`char`, a space; `String`, leading indent) survive. Callers trim when they want to.
- Built-in impls mostly wrap `FromStr`, so little parsing logic is duplicated.
- A type wanting both parses implements two traits — acceptable, since the contracts differ.

### Future Extensions

- Blanket `impl<T: FromStr> LenientFromStr for T`, once specialization is settled.
- A derive for enums (case-insensitive discriminant names).
- Lenient date/time parsing in `core:temporal`.
- A `core:cli` typed-argument API on the trait.
