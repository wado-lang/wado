# WEP: Compile-Time Parameters

## Context

Programs need build inputs that vary by environment — API endpoints, CI build IDs, feature flags — not known when the source is written but baked into the artifact at compile time.

Existing compile-time literals don't cover this: `#file` / `#line` / `#function` (location), `#data` (`__DATA__`), `#include_str` / `#include_bytes` (file contents) all read from the source tree. Rust's `env!()` reads process env vars implicitly, making builds non-deterministic and hiding which vars a project consumes.

This WEP adds named, declarative compile-time parameters fed by the `wado` invocation (CLI flags, or env vars when opted in).

### Why an Attribute on `global`, Not a New Literal

Rather than a dedicated literal like `#param("API_URL")`, the design reuses `global` with a `#[param]` attribute:

```wado
#[param]
global API_URL: String = "http://localhost";
```

- No new literal; the language surface stays small.
- The type annotation gives the parameter's type (no turbofish); the initializer gives the fallback.
- Read sites are ordinary global references, working with format strings, patterns, etc.
- IDE/LSP can hover `#[param]` without learning a new form.

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

`#[param]` applies to `global` declarations. Optional named arguments:

| Argument   | Type     | Description                                                              |
| ---------- | -------- | ------------------------------------------------------------------------ |
| `name`     | `String` | Parameter name for `-D NAME=value`. Defaults to the global's identifier. |
| `from_env` | `String` | Env var read at compile time. Independent of `name`.                     |

`name` is any non-empty string; matching it from the CLI or env is the caller's responsibility (the `-D NAME=value` parser splits on the first `=`). Names may collide across packages — see [Flat Namespace](#flat-namespace).

### Constraints

- `global` only (immutable). `#[param]` on `global mut` is an error — a compile-time value paired with runtime mutability mixes unrelated concepts.
- `pub` is allowed (visible like any `pub global`); `export` is out of scope ([Future Extensions](#future-extensions)).
- The initializer follows ordinary `global` rules and is the fallback when no override resolves.

### Resolution Sources

Each declaration resolves independently, highest priority first:

1. CLI override `-D NAME=value` (alias `--define`)
2. The `from_env` environment variable, if declared
3. The initializer expression

A higher source short-circuits the lower ones. Overrides (1, 2) are strings, converted to the declared type (see [Conversion](#conversion)); a failed conversion is handled per the [Resolution Policy](#resolution-policy), not automatically fatal. The initializer (3) is type-checked as written and takes effect whenever no override resolves or a conversion is rejected.

### Resolution Policy

Three failures can occur, each a diagnostic class with a CLI-set level (`error`, `warn`, `ignore`):

| Class             | Situation                                       | Flag              | Default  |
| ----------------- | ----------------------------------------------- | ----------------- | -------- |
| unknown `-D` name | `-D NAME=value` matching no `#[param]`          | `--param-unknown` | `error`  |
| invalid value     | override resolved but unconvertible to the type | `--param-invalid` | `error`  |
| missing value     | no override; the initializer would be used      | `--param-missing` | `ignore` |

Levels: `error` fails the build; `warn` diagnoses and falls back to the initializer (for `unknown`, ignores the stray `-D`); `ignore` falls back silently. Defaults are strict where a mistake is likely, lenient where the default is normal. (`--param-unknown=warn` suits one invocation targeting several build configs — the eventual per-package scoping case.)

The flags choose _what_ a bad value means, not _when_ it is caught: the compiler applies the policy as early as it can. v1's built-in types (see [Supported Types](#supported-types-v1)) convert at resolution time, so the policy is enforced at compile time. User types need v2; no path runs them in v1.

### Flat Namespace

Parameter names share one flat namespace across the whole compilation unit (root package and dependencies): same-named `#[param]`s — by identifier or explicit `name`, in any package — answer to the same `-D NAME=value`. This mirrors OS environment variables; library authors avoid collisions by prefixing (e.g. `MYLIB_LOG_LEVEL`). Wado has no structured namespace because the manifest offers no globally-unique short prefix: `[dependencies]` keys are import-local aliases, and canonical package identity (`registry+URL/ns:name@version`) is unusable as a name component. `from_env` shares the OS env namespace; same convention.

### Conversion

v1 converts override strings to the declared type natively in the compiler — no user-facing trait. The pass trims the override first (whitespace from a CLI/env value is noise), then converts. Accepted forms match the built-in impls of [`LenientFromStr`](./wep-2026-06-22-lenient-from-str.md) — radix prefixes, `_` digit separators, `nan`/`inf`, `1`/`0` for `bool` — so v2, which adopts that trait, preserves v1 behavior. (The trait itself does not trim; trimming is this pass's policy.)

#### Supported Types (v1)

v1 supports `#[param]` on the built-in scalar types only: `String`, `char`, the integer types, `f32` / `f64`, and `bool`. Their accepted spellings are the built-in impls of [`LenientFromStr`](./wep-2026-06-22-lenient-from-str.md).

An unaccepted value (a `"yes"` for `bool`, an out-of-range integer) is an invalid value, handled per `--param-invalid`. `#[param]` on any other type is an error — `#[param] on <Type>: only built-in types are supported in v1`. v2 lifts this by evaluating [`LenientFromStr`](./wep-2026-06-22-lenient-from-str.md) for arbitrary types through the wasm-CTFE backend ([niri Stage 5](./wep-2026-04-27-nir-interpreter.md)).

### Resolution Timing

On a resolved override the param-resolution pass trims and converts the string natively, then either replaces the global's initializer with the resulting literal, or — on failure — applies `--param-invalid` (keeping the initializer unless the level is `error`). No interpreter runs, so the pass sits after symbol resolution, before lowering. The rewritten global is then ordinary: scalar literals are eligible for Constant Global Promotion to a Wasm constant, `String` uses the existing lazy-init path. No new optimization is needed.

### CLI Surface

`wado compile`, `run`, `serve`, `test`, and `dump` accept `-D NAME=value` (repeatable) and the three policy flags, parsed before module loading.

```sh
wado compile -D API_URL=https://prod.example.com -D PORT=80 app.wado
wado run -D LOG_LEVEL=debug script.wado

# Relax: tolerate a stray -D and a bad value, fall back to defaults
wado compile --param-unknown=warn --param-invalid=warn -D EXTRA=1 -D PORT=eighty app.wado

# Tighten: require every parameter to be supplied
wado compile --param-missing=error -D API_URL=… -D PORT=80 -D BUILD_ID=… app.wado
```

### Out of Scope (v1)

- User-defined parameter types — built-in types only; arbitrary types need compile-time evaluation, deferred to v2 (wasm-CTFE / [`LenientFromStr`](./wep-2026-06-22-lenient-from-str.md)).
- Parameter file (`WADO_PARAM_FILE`) — added once the trade-offs are concrete.
- Per-package `-D` scoping (e.g. `-D 'auth-lib:NAME=...'`) — the flat namespace suffices for now.
- Per-declaration `#[param(required)]` — `--param-missing=error` already enforces this globally; the finer-grained form is a [Future Extension](#future-extensions).
- `export #[param]` and cross-component override — an independent design problem.

## Implementation Strategy

### Manifest

None. `#[param]` is purely source-level.

### Compiler Pipeline

1. Parser: accept `#[param]` (with `name` / `from_env`) on globals.
2. Symbol pass: register each parameter's name and optional `from_env` in a per-compilation table.
3. Param resolution pass (after symbol resolution, before lowering):
   - Reject `#[param]` on non-built-in types.
   - Map `-D NAME=value` to the table; apply `--param-unknown` to misses.
   - Per declaration, take a value from `-D`, then `from_env`; if none, apply `--param-missing` and keep the initializer.
   - On an override, convert natively: success replaces the initializer with the literal; failure applies `--param-invalid`.
4. Existing optimizations (constant folding, CGP) run unchanged on the rewritten globals.

v1 adds no traits or prelude functions — the pass parses in Rust, matching what the `FromStr` impls do plus trimming/leniency. `FromStr` is unchanged. v2 swaps the native path for evaluating [`LenientFromStr`](./wep-2026-06-22-lenient-from-str.md) via wasm-CTFE.

### `CompilerHost`

A new host method supplies overrides and env access, keeping the compiler crate pure:

```rust
trait CompilerHost {
    fn param_override(&self, name: &str) -> Option<String>;
    fn env_var(&self, name: &str) -> Option<String>;
    // ... existing methods
}
```

The CLI host reads `clap` args and `std::env::var`; the test host stubs both. Policy levels travel with the compile options, not the host.

### Errors

Structural mistakes are always errors:

| Condition                                          | Error                                                         |
| -------------------------------------------------- | ------------------------------------------------------------- |
| `#[param]` on `global mut`                         | `#[param] cannot be applied to a mutable global`              |
| `#[param]` argument other than `name` / `from_env` | `unknown #[param] argument: <name>`                           |
| `name = ""`                                        | `#[param] name must not be empty`                             |
| `#[param]` on a non-built-in type                  | `#[param] on <Type>: only built-in types are supported in v1` |

The three resolution diagnostics report at their flag's level ([Resolution Policy](#resolution-policy)); same message as error or warning:

| Class         | Flag (default)             | Message                                                 |
| ------------- | -------------------------- | ------------------------------------------------------- |
| unknown name  | `--param-unknown` (error)  | `unknown compile-time parameter: <NAME>`                |
| invalid value | `--param-invalid` (error)  | `cannot parse "<value>" as <Type> for parameter <NAME>` |
| missing value | `--param-missing` (ignore) | `compile-time parameter <NAME> was not provided`        |

An `invalid` value from `from_env` names the env var instead. At `warn` / `ignore`, `invalid` and `missing` fall back to the initializer.

### Documentation

- Add a short Compile-Time Parameters section to `docs/cheatsheet.md`.
- No `core:prelude` changes (v1 adds nothing to the prelude).

## Consequences

### Positive

- Typed, named build inputs declared in source, defaults co-located.
- No new literal syntax; `#[param]` joins `#[inline]`, `#[expect_trap]`, etc.
- Type and default come from the `global`, avoiding turbofish and manifest schemas.
- Verified at compile time: undeclared names and bad values fail the build by default, catching typos in names _and_ values.
- Bake-in falls out of the global pipeline plus CGP — minimal optimizer work.
- `from_env` documents consumed env vars as an opt-in subset, avoiding `env!()`'s leak-anything behavior.

### Trade-offs

- Flat namespace pushes collision avoidance onto library authors — same as OS env vars.
- v1 is built-in types only. Deliberate: every v1 use case (URLs, ports, IDs, flags) is built-in and converts at compile time without CTFE; user types wait for v2 rather than forcing a weaker runtime-conversion path.
- The native conversion duplicates `FromStr` parsing logic — small, and removed in v2 by [`LenientFromStr`](./wep-2026-06-22-lenient-from-str.md) over wasm-CTFE.
- The pass trims every override, so `String` / `char` parameters can't carry surrounding whitespace — fine for URLs, identifiers, level names. (The `LenientFromStr` trait itself is faithful; trimming is the pass's policy.)

### Future Extensions

- Arbitrary user types via [`LenientFromStr`](./wep-2026-06-22-lenient-from-str.md) over wasm-CTFE ([niri Stage 5](./wep-2026-04-27-nir-interpreter.md)): conversion runs at compile time, so `--param-invalid` keeps its compile-time meaning.
- Parameter file (`WADO_PARAM_FILE` or `wado.toml [params]`).
- Per-package `-D` scoping.
- Per-declaration `#[param(required)]` with initializer-less `global` syntax.
- `export #[param]` with cross-component override.
