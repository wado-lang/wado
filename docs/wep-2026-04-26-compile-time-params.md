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

CLI overrides and environment variables yield strings. They are converted to the global's declared type via the [`FromParam`](#fromparam-trait) trait. Conversion failure is a compile error.

The initializer expression is type-checked as written; no `FromParam` is involved on that path.

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
    type Err;
    fn from_param(s: &String) -> Result<Self, Self::Err>;
}
```

`FromParam` is intentionally separate from `FromStr` (the general string parsing trait introduced alongside this WEP). `FromParam` is permitted to be more lenient — for example, accepting both `"true"` and `"1"` for `bool` — without affecting the semantics of general string parsing in user code.

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

Anything else — for example, a hex prefix on an integer, or `"yes"` for `bool` — is a parse error and therefore a compile error. The accepted set may be extended in future WEPs.

User-defined types may implement `FromParam` to be usable as `#[param]` types. This is supported but not exercised by the v1 implementation; the focus is the built-in set above.

### Resolution Timing and Optimization

A `#[param]` global is resolved during compilation, before any runtime initialization sequencing. After resolution the global behaves like any other `global` whose initializer happens to be a literal:

- Constant Global Promotion (CGP) folds it to an immutable Wasm constant when the resolved value is scalar (numeric, `bool`).
- For object-typed parameters (currently only `String`), the value participates in the existing lazy-initialization path.

No new optimization is required. The naturally-arising constant folding is sufficient for the common cases.

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
   - For overridden globals, run the resolved string through `FromParam::from_param` for the declared type. Failure → compile error.
   - Replace the global's initializer in the AST/TIR with a literal of the resolved value.
4. **Existing optimizations** (CGP, etc.) operate on the rewritten globals without further changes.

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

This WEP introduces both:

- `FromStr` — Rust-compatible. Implementations for `i*`, `u*`, `f*`, and `String` are retrofit on top of the existing `T::from_str` functions in `core:prelude`. `bool::from_str` is added with the strict `"true"` / `"false"` accepted set.
- `FromParam` — separate trait, lenient where it makes sense (`bool` accepts `"1"` / `"0"`), trims unconditionally.

Both traits live in `core:prelude` and are auto-imported.

### Errors

| Condition                                           | Error                                                            |
| --------------------------------------------------- | ---------------------------------------------------------------- |
| `#[param]` on `global mut`                          | `#[param] cannot be applied to a mutable global`                 |
| `#[param]` argument other than `name` or `from_env` | `unknown #[param] argument: <name>`                              |
| `name = ""` (empty string)                          | `#[param] name must not be empty`                                |
| `-D NAME=value` for an undeclared parameter         | `unknown compile-time parameter: NAME`                           |
| `FromParam::from_param` returns `Err`               | `cannot parse "<value>" as <Type> for parameter <NAME>`          |
| `from_env` value present but not parseable          | same as above, with the env var name mentioned in the diagnostic |

### Documentation

- Update `docs/cheatsheet.md` with a short Compile-Time Parameters section.
- Update `core:prelude` reference (`docs/stdlib-core-prelude.md`) with `FromParam` and `FromStr`.

## Consequences

### Positive

- Programs can declare typed, named build inputs in source, with defaults co-located with the declaration.
- No new literal syntax; `#[param]` slots into the existing attribute family alongside `#[inline]`, `#[expect_trap]`, etc.
- Type and default come from the `global` itself, eliminating turbofish and avoiding redundant manifest schemas.
- Bake-in semantics fall out of the existing global pipeline plus CGP — minimal new optimization work.
- Build inputs are explicit: only declared parameters can be set, and `-D` typos are caught.
- `from_env` makes the set of consumed environment variables a documented, opt-in subset, avoiding Rust `env!()`'s "any env var leaks in" failure mode.

### Trade-offs

- Flat namespace pushes collision avoidance to library authors. Acceptable: the same constraint applies to OS environment variables, which library authors already deal with.
- `FromParam` is a dedicated trait and not `FromStr`, so there is some duplication. The benefit — keeping lenient parameter parsing out of general string parsing semantics — is judged worth the cost.
- `String` parameters cannot carry meaningful surrounding whitespace because trim is unconditional. Acceptable for the target use cases (API URLs, identifiers, level names).

### Future Extensions

- Parameter file (`WADO_PARAM_FILE` or `wado.toml [params]`) once the convenience of grouping many overrides is demanded.
- Per-package scoping for `-D` to override a specific dependency's parameter without affecting same-named parameters elsewhere.
- Optional/required parameter distinction, with `Option<T>` typing or an explicit `required` flag.
- `export #[param]` and the associated cross-component override design.
