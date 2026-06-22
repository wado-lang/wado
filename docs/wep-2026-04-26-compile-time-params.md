# WEP: Compile-Time Parameters

## Context

Programs frequently need build inputs that vary between environments: API endpoints differ between development, staging, and production; build identifiers come from CI; feature flags are toggled at compile time. The values are not known when the source is written but must be baked into the artifact at compile time.

Existing Wado compile-time literals cover related but distinct needs:

- `#file`, `#line`, `#function` — source location
- `#data` — co-located inline data via `__DATA__`
- `#include_str`, `#include_bytes` — external file contents

None of these accept values from outside the source tree. Rust's `env!()` solves the same problem but reads process environment variables implicitly, which makes builds non-deterministic and hides which environment variables a project actually consumes.

This WEP adds named, declarative compile-time parameters whose values come from the `wado` invocation (CLI flags or — when explicitly opted in — environment variables).

### Why an Attribute on `global`, Not a New Literal

A natural-looking alternative is a dedicated literal such as `#param("API_URL")`, mirroring `#include_str`. The chosen design instead reuses `global` with a `#[param]` attribute:

```wado
#[param]
global API_URL: String = "http://localhost";
```

Reasons:

- No new compile-time literal needs to be added; the language surface stays small.
- The `global`'s type annotation supplies the parameter's type — no turbofish required.
- The initializer expression provides the fallback value, reusing the existing semantics that allows arbitrary expressions for `global` initializers.
- Read sites are ordinary global references that interact naturally with format strings, pattern matching, and other expressions.
- IDE/LSP can attach hover information about `#[param]` declarations without learning a new literal form.

## Decision

### Syntax

```wado
#[param]
global API_URL: String = "http://localhost";

#[param(from_env = "DATABASE_URL")]
global DATABASE_URL: String = "postgres://localhost";

#[param(from_env = "PORT")]
global PORT: i32 = 8080;

#[param(name = "build.id")]
global BUILD_ID: String = "dev";

#[param(name = "MY_APP_VERSION", from_env = "MY_APP_VERSION")]
global VERSION: String = "0.0.0-local";
```

The attribute is allowed on `global` declarations. The parameter name defaults to the global's identifier and can be overridden with `name = "..."`. Optional named arguments:

| Argument   | Type     | Description                                                                                             |
| ---------- | -------- | ------------------------------------------------------------------------------------------------------- |
| `name`     | `String` | Parameter name used by `-D NAME=value`. Defaults to the global's identifier.                            |
| `from_env` | `String` | Environment variable name to read at compile time. Optional. The env var name is independent of `name`. |

The `name` value is an arbitrary non-empty string. Wado does not restrict its character set: any string suits the source declaration, and matching it from a CLI invocation or environment variable is the caller's responsibility (subject to the `-D NAME=value` parser splitting on the first `=` and the host shell's quoting rules).

Multiple `#[param]` declarations may share the same name across packages; see [Flat Namespace](#flat-namespace).

### Constraints

- Allowed only on `global` (immutable). `global mut` with `#[param]` is a compile error: parameters bind a compile-time value, and pairing that with runtime mutability mixes two unrelated concepts.
- `pub` is allowed. The global is then visible to other modules in the same package, just like any `pub global`.
- `export` is out of scope for v1 (see [Future Extensions](#future-extensions)).
- The initializer expression must satisfy the existing `global` rules. It is the fallback when no override resolves.

### Resolution Sources

Each `#[param]` declaration resolves independently. Sources are checked in this priority order, highest first:

1. CLI override: `-D NAME=value` (or `--define NAME=value`)
2. Environment variable named by `from_env`, if the attribute declares one
3. The initializer expression

If a higher-priority source produces a value, lower-priority sources are not consulted.

CLI overrides and environment variables yield strings. The compiler converts each to the global's declared type (v1 supports a fixed set of built-in types; see [Supported Types](#supported-types-v1)). A failed conversion is not, in itself, fatal: what happens is decided by the [Resolution Policy](#resolution-policy).

The initializer expression is type-checked as written and is the value that takes effect whenever an override is absent or its conversion is rejected by the policy.

### Resolution Policy

Three things can go wrong while resolving parameters. Each is a separate diagnostic class with a configurable level (`error`, `warn`, or `ignore`), set on the CLI:

| Class             | Situation                                                       | Flag              | Default  |
| ----------------- | --------------------------------------------------------------- | ----------------- | -------- |
| unknown `-D` name | `-D NAME=value` whose `NAME` matches no `#[param]` declaration  | `--param-unknown` | `error`  |
| invalid value     | an override resolved, but it could not be converted to the type | `--param-invalid` | `error`  |
| missing value     | no override resolved; the initializer (default) would be used   | `--param-missing` | `ignore` |

The defaults are strict where a mistake is likely (`unknown`, `invalid`) and lenient where falling back to the declared default is the normal case (`missing`). Each level means:

- `error` — a compile error (or, where the check can only happen at runtime, a runtime trap; see below).
- `warn` — emit a diagnostic and use the initializer's default value (for `unknown`, ignore the stray `-D`).
- `ignore` — silently use the default.

`--param-unknown` is relaxed to `warn`/`ignore` for the case the per-package scoping extension will eventually address: a single CLI invocation that targets several build configurations and legitimately passes a `-D` not declared in every one.

#### Policy Applies as Early as It Can

The flags choose _what_ a bad value means, not _when_ it is detected. The compiler applies the chosen policy at the earliest point it can know the outcome:

- For the built-in parameter types (see [Supported Types](#supported-types-v1)), conversion is performed by the compiler at resolution time, so `--param-invalid` is enforced at compile time — a bad value with the default policy fails the build.
- For user-defined types, the compiler cannot convert without evaluating user code. v1 does not support them (see [Supported Types](#supported-types-v1)); v2 adopts [`LenientFromStr`](./wep-2026-06-22-lenient-from-str.md) and evaluates it through the wasm-CTFE backend, at which point the same policy is enforced at compile time. Until then there is no path on which a user-typed conversion runs.

This "diagnose as early as possible" rule keeps one policy meaning across both timings: the flag says how a bad value is handled; the compiler shifts the check left whenever it has enough information.

### Flat Namespace

Parameter names live in a single flat namespace shared across the entire compilation unit (root package and all dependencies). Two `#[param]` declarations with the same parameter name — whether the name is taken from the identifier or set explicitly with `name = "..."`, in the same package or in different packages — are both subject to the same `-D NAME=value` override.

This mirrors how operating system environment variables work. Library authors are responsible for choosing names that do not collide with applications or other libraries (the convention is to prefix names, e.g., `MYLIB_LOG_LEVEL`). Wado does not introduce a structured namespace because the package manifest does not provide a globally unique short name to use as a prefix: dependency keys in `[dependencies]` are import-local aliases, and the canonical package identity is a structured descriptor (`registry+URL/ns:name@version`) unsuitable as a name component.

The `from_env` value is also a free-form string and shares the operating system's environment namespace; the same convention applies.

### Conversion

v1 converts override strings to the declared type entirely inside the compiler — there is no user-facing conversion trait. The accepted set is fixed (see [Supported Types](#supported-types-v1)) and may be more lenient than general string parsing (e.g. `bool` accepts `"1"` / `"0"`); leniency here does not touch `FromStr` or other parsing in user code. These accepted forms match the built-in impls of [`LenientFromStr`](./wep-2026-06-22-lenient-from-str.md), the general forgiving-parse trait, so v2 — which adopts that trait and evaluates it at compile time (see [Future Extensions](#future-extensions)) — preserves v1 behavior for the built-in types.

#### Trimming

The conversion trims leading and trailing ASCII whitespace before parsing. This applies to `String` as well: parameter values cannot carry meaningful leading or trailing whitespace.

#### Supported Types (v1)

v1 supports `#[param]` only on these built-in types. The compiler converts them itself at resolution time (no compile-time function evaluation needed), so the [policy](#resolution-policy) is enforced at compile time.

| Type                              | Accepted form                                          |
| --------------------------------- | ------------------------------------------------------ |
| `String`                          | Any string (after trim, identity)                      |
| `i8`, `i16`, `i32`, `i64`, `i128` | Decimal integer (sign optional)                        |
| `u8`, `u16`, `u32`, `u64`, `u128` | Decimal non-negative integer                           |
| `f32`, `f64`                      | Standard floating-point literal (e.g., `3.14`, `-1e9`) |
| `bool`                            | `"true"`, `"false"`, `"1"`, `"0"` (case-insensitive)   |

Anything else — a hex prefix on an integer, `"yes"` for `bool` — fails conversion and is handled per `--param-invalid`. The accepted set may be extended in future WEPs.

A `#[param]` on any other type is a compile error in v1: `#[param] on <Type>: only built-in types are supported (planned for v2)`. The conversions above are implemented natively in the compiler. v2 lifts the restriction by adopting [`LenientFromStr`](./wep-2026-06-22-lenient-from-str.md) and evaluating user impls at compile time through the wasm-CTFE backend (see [niri Stage 5](./wep-2026-04-27-nir-interpreter.md)).

### Resolution Timing and Optimization

When an override resolves, the param-resolution pass converts the string natively (the compiler owns the parsers for `String`, the integer types, the float types, and `bool`) and:

- on success, replaces the global's initializer with a literal of the resolved value;
- on failure, applies `--param-invalid`: at `error` the build fails; at `warn`/`ignore` it leaves the original initializer in place (with or without a diagnostic).

No interpreter is involved, so this runs at the pass's position (after symbol resolution, before lowering). After the rewrite the global is an ordinary `global` whose initializer is a literal:

- A scalar literal (numeric, `bool`) is eligible for Constant Global Promotion (CGP) to an immutable Wasm constant.
- A `String` literal participates in the existing lazy-initialization path.

No new optimization is required; bake-in falls out of the existing global pipeline plus CGP.

### CLI Surface

`wado compile`, `wado run`, `wado serve`, `wado test`, and `wado dump` all accept `-D NAME=value` (repeatable) plus the three policy flags from [Resolution Policy](#resolution-policy). All are parsed before module loading so that resolution can happen during compilation.

```sh
wado compile -D API_URL=https://prod.example.com -D PORT=80 app.wado
wado run -D LOG_LEVEL=debug script.wado

# Relax the policy: tolerate a stray -D and a bad value, fall back to defaults
wado compile --param-unknown=warn --param-invalid=warn -D EXTRA=1 -D PORT=eighty app.wado

# Tighten the policy: require every parameter to be supplied
wado compile --param-missing=error -D API_URL=… -D PORT=80 -D BUILD_ID=… app.wado
```

### Out of Scope (v1)

These are intentionally deferred:

- User-defined parameter types — v1 supports only the built-in types ([Supported Types](#supported-types-v1)) and adds no conversion trait. Arbitrary types need compile-time evaluation; v2 adopts [`LenientFromStr`](./wep-2026-06-22-lenient-from-str.md) and evaluates it through the wasm-CTFE backend ([niri Stage 5](./wep-2026-04-27-nir-interpreter.md)).
- Parameter file (`WADO_PARAM_FILE`) — keep core feature minimal; can be added once the trade-offs are concrete.
- Per-package scoping for `-D` — the flat namespace is enough for the v1 use cases. Add a structured override key (e.g., `-D 'auth-lib:NAME=...'` keyed on `[dependencies]` import names) only if user demand emerges.
- Per-parameter `required` — `--param-missing=error` already enforces "every parameter must be supplied" globally. A per-declaration `#[param(required)]` (and the matching initializer-less `global` syntax) is a separate, finer-grained feature; see [Future Extensions](#future-extensions).
- `export` interaction — exposing a `#[param]` global at the Component Model boundary, and the question of whether a consumer can override a producer's compiled-in value, is an independent design problem.

## Implementation Strategy

### Manifest

No changes to `wado.toml`. `#[param]` is purely a source-level construct.

### Compiler Pipeline

1. **Parser**: `#[param]` is recognized as a global attribute. The parser accepts the named-argument form (`from_env = "..."`).
2. **Symbol/Type pass**: each `#[param]` declaration registers its parameter name (`name = "..."` if given, otherwise the global's identifier) and the optional `from_env` mapping in a per-compilation parameter table.
3. **Param resolution pass** (new, runs after symbol resolution and before TIR lowering):
   - Reject `#[param]` on a non-built-in type (the v1 [Supported Types](#supported-types-v1) restriction).
   - For each `-D NAME=value`, look up `NAME` in the parameter table; apply `--param-unknown` to misses.
   - For each `#[param]` declaration, in priority order, take a value from `-D`, then from the declared `from_env` variable; if none, apply `--param-missing` and keep the initializer.
   - For an overridden global, convert the string natively. Success → replace the initializer with the resolved literal. Failure → apply `--param-invalid` (keep the initializer unless the level is `error`).
4. **Existing optimizations** (constant folding, CGP, etc.) operate on the rewritten globals without further changes.

### `CompilerHost`

A new method on `CompilerHost` provides the `-D` overrides and access to environment variables, so that the compiler crate itself remains pure:

```rust
trait CompilerHost {
    fn param_override(&self, name: &str) -> Option<String>;
    fn env_var(&self, name: &str) -> Option<String>;
    // ... existing methods
}
```

The CLI host implements these by reading `clap` arguments and `std::env::var` respectively. The test host can stub both. The three policy levels travel with the existing compile options, not the host.

### Conversion (no new traits)

v1 adds no traits or prelude functions. The resolution pass converts override strings to the built-in types directly in Rust — the same parsing the runtime `FromStr` impls perform, plus trimming and the documented leniency. `FromStr` already exists in `core:prelude` and is unchanged.

v2 adopts [`LenientFromStr`](./wep-2026-06-22-lenient-from-str.md) — a `Result`-returning, forgiving sibling of `FromStr` — and executes user impls through the wasm-CTFE backend, replacing the native built-in conversions with direct evaluation.

### Errors

Structural mistakes in the declaration are always errors:

| Condition                                           | Error                                                         |
| --------------------------------------------------- | ------------------------------------------------------------- |
| `#[param]` on `global mut`                          | `#[param] cannot be applied to a mutable global`              |
| `#[param]` argument other than `name` or `from_env` | `unknown #[param] argument: <name>`                           |
| `name = ""` (empty string)                          | `#[param] name must not be empty`                             |
| `#[param]` on a non-built-in type                   | `#[param] on <Type>: only built-in types are supported in v1` |

The three resolution diagnostics report at the level set by their flag ([Resolution Policy](#resolution-policy)); the message is the same whether it surfaces as an error or a warning:

| Class         | Flag (default)             | Message                                                 |
| ------------- | -------------------------- | ------------------------------------------------------- |
| unknown name  | `--param-unknown` (error)  | `unknown compile-time parameter: <NAME>`                |
| invalid value | `--param-invalid` (error)  | `cannot parse "<value>" as <Type> for parameter <NAME>` |
| missing value | `--param-missing` (ignore) | `compile-time parameter <NAME> was not provided`        |

For an `invalid` value sourced from `from_env`, the message names the environment variable instead of the parameter. At `warn`/`ignore`, `invalid` and `missing` fall back to the initializer's default.

### Documentation

- Update `docs/cheatsheet.md` with a short Compile-Time Parameters section.
- No `core:prelude` reference changes — v1 adds no traits or functions to the prelude.

## Consequences

### Positive

- Programs can declare typed, named build inputs in source, with defaults co-located with the declaration.
- No new literal syntax; `#[param]` slots into the existing attribute family alongside `#[inline]`, `#[expect_trap]`, etc.
- Type and default come from the `global` itself, eliminating turbofish and avoiding redundant manifest schemas.
- Bake-in semantics fall out of the existing global pipeline plus CGP — minimal new optimization work.
- Build inputs are explicit and verified at compile time: undeclared names and unconvertible values both fail the build by default, catching typos in names _and_ values before an artifact ships.
- One policy meaning across timings: `--param-{unknown,invalid,missing}` say _what_ a bad input means; the compiler diagnoses as early as it can (compile time for v1's built-in types).
- `from_env` makes the set of consumed environment variables a documented, opt-in subset, avoiding Rust `env!()`'s "any env var leaks in" failure mode.

### Trade-offs

- Flat namespace pushes collision avoidance to library authors. Acceptable: the same constraint applies to OS environment variables, which library authors already deal with.
- v1 supports only built-in parameter types. Accepted deliberately: every v1 use case (API URLs, ports, build IDs, flags) is a built-in type, and built-ins convert at compile time without compile-time function evaluation. User-defined types wait for v2's wasm-CTFE backend rather than forcing a runtime-conversion path with weaker (runtime) diagnostics.
- v1's native conversion duplicates the parsing logic of the runtime `FromStr` impls. Accepted because it is small and avoids an interpreter; v2 removes it by adopting [`LenientFromStr`](./wep-2026-06-22-lenient-from-str.md) and executing it directly through wasm-CTFE.
- `String` parameters cannot carry meaningful surrounding whitespace because trim is unconditional. Acceptable for the target use cases (API URLs, identifiers, level names).

### Future Extensions

- Arbitrary user types via [`LenientFromStr`](./wep-2026-06-22-lenient-from-str.md), evaluated through the wasm-CTFE backend ([niri Stage 5](./wep-2026-04-27-nir-interpreter.md)) — v2's headline addition: the conversion runs at compile time, so `--param-invalid` keeps its compile-time meaning for user types too.
- Parameter file (`WADO_PARAM_FILE` or `wado.toml [params]`) once the convenience of grouping many overrides is demanded.
- Per-package scoping for `-D` to override a specific dependency's parameter without affecting same-named parameters elsewhere.
- Per-declaration `#[param(required)]` with initializer-less `global` syntax, for parameters that have no sensible default (finer-grained than the global `--param-missing=error`).
- `export #[param]` and the associated cross-component override design.
