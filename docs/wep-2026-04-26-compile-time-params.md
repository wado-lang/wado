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

CLI overrides and environment variables yield strings. They are converted to the global's declared type via the [`FromParam`](#fromparam-trait) trait. `FromParam` is a _total_ conversion — it does not return an error. A value it cannot convert is handled by one of the strategies in [Failure Handling](#failure-handling); none of them is, by itself, a compile error.

The initializer expression is type-checked as written; no `FromParam` is involved on that path. When a conversion is ignored (see [Failure Handling](#failure-handling)), the initializer expression is the value that takes effect.

### Unknown `-D` Names

`-D NAME=value` whose `NAME` does not match any `#[param]` declaration in the program is a compile error. This catches typos and prevents silent build drift. A future per-package scoping mechanism may relax this for cases where a single CLI invocation targets multiple build configurations.

### Flat Namespace

Parameter names live in a single flat namespace shared across the entire compilation unit (root package and all dependencies). Two `#[param]` declarations with the same parameter name — whether the name is taken from the identifier or set explicitly with `name = "..."`, in the same package or in different packages — are both subject to the same `-D NAME=value` override.

This mirrors how operating system environment variables work. Library authors are responsible for choosing names that do not collide with applications or other libraries (the convention is to prefix names, e.g., `MYLIB_LOG_LEVEL`). Wado does not introduce a structured namespace because the package manifest does not provide a globally unique short name to use as a prefix: dependency keys in `[dependencies]` are import-local aliases, and the canonical package identity is a structured descriptor (`registry+URL/ns:name@version`) unsuitable as a name component.

The `from_env` value is also a free-form string and shares the operating system's environment namespace; the same convention applies.

### `FromParam` Trait

CLI and environment variable values are strings. Conversion to the global's type uses a dedicated trait:

```wado
pub trait FromParam {
    fn from_param(s: &String) -> Self;
}
```

`FromParam` is a _total_ conversion: it returns `Self` directly, with no `Result` and no `Err` associated type. This is the central difference from `FromStr` (the general string parsing trait this WEP builds on, already in `core:prelude`), whose `from_str` returns `Result<Self, Self::Err>` so user code can recover from a parse failure.

A compile-time parameter has no caller to hand a `Result` to: the value is fixed before the program runs. Modeling it as a fallible conversion would force every read site, or the resolution pass, to invent an error policy. Making `FromParam` total pushes that policy into the conversion itself, which is where the per-type knowledge lives.

`FromParam` is also permitted to be more lenient than `FromStr` — for example, accepting both `"true"` and `"1"` for `bool` — without affecting the semantics of general string parsing in user code.

#### Failure Handling

Because `from_param` is total, an implementation cannot signal "this string is not a valid value" through its return type. On input it cannot convert, an implementation does one of:

1. Best-effort coercion — return a sensible value anyway (e.g. `String` is the trimmed input; `bool` treats `"1"`/`"0"` like `"true"`/`"false"`).
2. `panic` — signal that the input is unusable.

These two impl-level choices, combined with _where_ the conversion runs (see [Resolution Timing and Optimization](#resolution-timing-and-optimization)), produce the three observable outcomes:

- Best-effort → the returned value is used.
- A conversion the compiler performs at resolution time (every built-in impl) that fails is **not** a build error: the pass emits a warning, ignores the override, and falls back to the initializer expression. This is the "ignore with a warning" outcome.
- A `panic` reached only at runtime — possible when a user `FromParam` impl runs during global initialization rather than at resolution time — surfaces as a runtime trap. This escapes compile-time detection, so it is discouraged, but not prohibited.

Built-in implementations never reach the runtime-panic outcome: the resolution pass converts them itself, so an unconvertible value always becomes a compile-time warning plus the initializer fallback.

#### Trimming

All built-in `FromParam` implementations trim leading and trailing ASCII whitespace from the input before parsing. This applies to `String` as well: parameter values cannot contain meaningful leading or trailing whitespace.

#### Built-In Implementations

| Type                              | Accepted form                                          |
| --------------------------------- | ------------------------------------------------------ |
| `String`                          | Any string (after trim, identity)                      |
| `i8`, `i16`, `i32`, `i64`, `i128` | Decimal integer (sign optional)                        |
| `u8`, `u16`, `u32`, `u64`, `u128` | Decimal non-negative integer                           |
| `f32`, `f64`                      | Standard floating-point literal (e.g., `3.14`, `-1e9`) |
| `bool`                            | `"true"`, `"false"`, `"1"`, `"0"` (case-insensitive)   |

Anything else — for example, a hex prefix on an integer, or `"yes"` for `bool` — is unconvertible. Per [Failure Handling](#failure-handling), the resolution pass converts these built-in types itself, so an unconvertible value becomes a compile warning and a fallback to the initializer rather than a build error. The accepted set may be extended in future WEPs.

User-defined types may implement `FromParam` to be usable as `#[param]` types. This is supported but not exercised by the v1 implementation; the focus is the built-in set above. A user-typed conversion is left as a `from_param(...)` call in the initializer, so it risks the runtime-panic outcome; implementors should prefer best-effort coercion.

### Resolution Timing and Optimization

When an override resolves, the param-resolution pass handles the conversion by the global's declared type:

- Built-in type: the pass converts the resolved string natively (the compiler owns the parsers for `String`, the integer types, the float types, and `bool`). Success replaces the initializer with a literal of the resolved value; failure emits a warning and leaves the original initializer in place. No interpreter is involved, so this works at the pass's position (after symbol resolution, before lowering).
- User type: the pass rewrites the initializer to `<Type>::from_param("<resolved string>")`, an ordinary Wado expression, and leaves it to the rest of the pipeline.

After the rewrite the global behaves like any other `global` whose initializer is an expression:

- A literal scalar (numeric, `bool`) is eligible for Constant Global Promotion (CGP) to an immutable Wasm constant. `String` participates in the existing lazy-initialization path.
- A `from_param(...)` call for a user type is constant-folded when its impl allows it; otherwise it stays a runtime call in the initializer, and that is the only path on which a `from_param` `panic` can surface at runtime.

No new optimization is required. The only new behavior is the resolution pass converting built-in types and treating a failed conversion as a warning-and-fallback instead of a hard error.

### CLI Surface

`wado compile`, `wado run`, `wado serve`, `wado test`, and `wado dump` all accept `-D NAME=value` (repeatable). The flag is parsed before module loading so that resolution can happen during compilation.

```sh
wado compile -D API_URL=https://prod.example.com -D PORT=80 app.wado
wado run -D LOG_LEVEL=debug script.wado
```

### Out of Scope (v1)

These are intentionally deferred:

- Parameter file (`WADO_PARAM_FILE`) — keep core feature minimal; can be added once the trade-offs are concrete.
- Per-package scoping for `-D` — the flat namespace is enough for the v1 use cases. Add a structured override key (e.g., `-D 'auth-lib:NAME=...'` keyed on `[dependencies]` import names) only if user demand emerges.
- Optional vs required distinction — v1 always falls back to the initializer expression, so a parameter without an override is just the initializer's value.
- `export` interaction — exposing a `#[param]` global at the Component Model boundary, and the question of whether a consumer can override a producer's compiled-in value, is an independent design problem.

## Implementation Strategy

### Manifest

No changes to `wado.toml`. `#[param]` is purely a source-level construct.

### Compiler Pipeline

1. **Parser**: `#[param]` is recognized as a global attribute. The parser accepts the named-argument form (`from_env = "..."`).
2. **Symbol/Type pass**: each `#[param]` declaration registers its parameter name (`name = "..."` if given, otherwise the global's identifier) and the optional `from_env` mapping in a per-compilation parameter table.
3. **Param resolution pass** (new, runs after symbol resolution and before TIR lowering):
   - For each `-D NAME=value` from the CLI, look up `NAME` in the parameter table. Unknown names → compile error.
   - For each `#[param]` declaration, in priority order, find a string value from `-D`, then from the declared `from_env` environment variable, otherwise mark the global as using its initializer.
   - For overridden globals with a built-in declared type, convert the string natively: success → replace the initializer with the resolved literal; failure → warning, keep the initializer. For a user-typed global, rewrite the initializer to `<Type>::from_param("<resolved string>")`. No conversion outcome is a hard error (see [Failure Handling](#failure-handling)).
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

The CLI host implements these by reading `clap` arguments and `std::env::var` respectively. The test host can stub both.

### `FromParam` and `FromStr`

`FromStr` already lives in `core:prelude` (Rust-compatible, returning `Result<Self, Self::Err>`, with `from_str_range` as the fundamental operation). This WEP adds:

- `FromParam` — a separate, total trait (`fn from_param(s: &String) -> Self`). Built-in impls in the prelude delegate to the corresponding `FromStr` impl, trim unconditionally, are lenient where it makes sense (`bool` accepts `"1"` / `"0"`), and `panic` on an unconvertible value. The resolution pass mirrors these impls natively so it can convert a built-in parameter without an interpreter; a conversion the native path rejects becomes a warning-and-fallback (see [Failure Handling](#failure-handling)) rather than the impl's runtime `panic`.

`FromParam` lives in `core:prelude` and is auto-imported.

### Errors

| Condition                                           | Error                                            |
| --------------------------------------------------- | ------------------------------------------------ |
| `#[param]` on `global mut`                          | `#[param] cannot be applied to a mutable global` |
| `#[param]` argument other than `name` or `from_env` | `unknown #[param] argument: <name>`              |
| `name = ""` (empty string)                          | `#[param] name must not be empty`                |
| `-D NAME=value` for an undeclared parameter         | `unknown compile-time parameter: NAME` (error)   |

`FromParam` conversion never produces an error. When a built-in conversion cannot convert the resolved string (e.g. `-D PORT=eighty`), the resolution pass emits a warning instead and keeps the initializer:

| Condition                                       | Warning                                                                |
| ----------------------------------------------- | ---------------------------------------------------------------------- |
| built-in conversion fails on a `-D` value       | `cannot parse "<value>" as <Type> for parameter <NAME>; using default` |
| built-in conversion fails on a `from_env` value | same as above, naming the env var instead of the parameter             |

### Documentation

- Update `docs/cheatsheet.md` with a short Compile-Time Parameters section.
- Update `core:prelude` reference (`docs/stdlib-core-prelude.md`) with `FromParam` (`FromStr` is already documented there).

## Consequences

### Positive

- Programs can declare typed, named build inputs in source, with defaults co-located with the declaration.
- No new literal syntax; `#[param]` slots into the existing attribute family alongside `#[inline]`, `#[expect_trap]`, etc.
- Type and default come from the `global` itself, eliminating turbofish and avoiding redundant manifest schemas.
- Bake-in semantics fall out of the existing global pipeline plus CGP — minimal new optimization work.
- Build inputs are explicit: only declared parameters can be set, and a `-D` for an undeclared name is a hard error, catching typos in parameter _names_.
- `from_env` makes the set of consumed environment variables a documented, opt-in subset, avoiding Rust `env!()`'s "any env var leaks in" failure mode.

### Trade-offs

- Flat namespace pushes collision avoidance to library authors. Acceptable: the same constraint applies to OS environment variables, which library authors already deal with.
- A malformed parameter _value_ (e.g. `-D PORT=eighty`) is a warning, not an error, and the initializer's default silently takes effect. This trades strictness for build resilience; the warning is the signal that surfaces the misconfiguration. A future `required`/strict mode (see [Future Extensions](#future-extensions)) could promote it back to an error where a default is not acceptable.
- `FromParam` is a dedicated trait and not `FromStr`, so there is some duplication. The benefits — lenient parameter parsing kept out of general string parsing semantics, and a total conversion that never fails the build — are judged worth the cost.
- `String` parameters cannot carry meaningful surrounding whitespace because trim is unconditional. Acceptable for the target use cases (API URLs, identifiers, level names).

### Future Extensions

- Parameter file (`WADO_PARAM_FILE` or `wado.toml [params]`) once the convenience of grouping many overrides is demanded.
- Per-package scoping for `-D` to override a specific dependency's parameter without affecting same-named parameters elsewhere.
- Optional/required parameter distinction, with `Option<T>` typing or an explicit `required` flag.
- `export #[param]` and the associated cross-component override design.
