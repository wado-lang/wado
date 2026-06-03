# WEP: Default Trait

## Context

Many types have a natural "zero value" or "empty value" — `0` for integers, `""` for strings, `[]` for arrays. A `Default` trait provides a uniform interface for obtaining these values, enabling generic code to construct default instances without knowing the concrete type.

Rust's `Default` trait has proven valuable for:

- Generic containers that need to create empty/default elements
- Compiler internals (e.g., serde deserialization for missing fields)
- Builder patterns where most fields have sensible defaults

## Decision

### Trait Definition

`Default` is a trait in `core:prelude` with a single static method:

```wado
#[comp_feature("default")]
pub trait Default {
    fn default() -> Self;
}
```

The `#[comp_feature("default")]` attribute allows the compiler to reference this trait internally (e.g., for serde deserialization of optional fields).

### No Compiler Synthesis

Unlike `Serialize`/`Deserialize`, there is **no** compiler-synthesized implementation for `Default`. Users must write `impl Default for MyType` manually. This keeps the feature simple and explicit — the user always controls what the default value is.

### Standard Library Implementations

The following types implement `Default` in the standard library:

| Type                      | `default()` |
| ------------------------- | ----------- |
| `i8`, `i16`, `i32`, `i64` | `0`         |
| `u8`, `u16`, `u32`, `u64` | `0`         |
| `i128`, `u128`            | `0`         |
| `f32`, `f64`              | `0.0`       |
| `bool`                    | `false`     |
| `char`                    | `'\0'`      |
| `String`                  | `""`        |
| `List<T>`                 | `[]`        |
| `Option<T>`               | `null`      |
| `TreeMap<K, V>`           | `{}`        |

`Result<T, E>` does **not** implement `Default` because there is no obvious choice between `Ok` and `Err`.

### Usage

```wado
// Direct usage
let n = i32::default();       // 0
let s = String::default();    // ""

// In generic code
fn make_default<T: Default>() -> T {
    return T::default();
}

let x = make_default::<i32>();      // 0
let arr = make_default::<List<String>>();  // []

// User-defined types
struct Config {
    host: String,
    port: i32,
    debug: bool,
}

impl Default for Config {
    fn default() -> Config {
        return Config {
            host: "localhost",
            port: 8080,
            debug: false,
        };
    }
}

let config = Config::default();
```

### Compiler Usage via `comp_feature`

The `#[comp_feature("default")]` attribute registers `Default` in the compiler's `TypeTable`, allowing internal passes to:

- Look up whether a type implements `Default`
- Call `T::default()` in synthesized code (e.g., serde deserialization filling missing optional fields)

This does **not** auto-derive `Default` for any type — it only makes the trait visible to the compiler.

## Consequences

- **Simple**: One trait, one method, no special syntax.
- **Explicit**: No magic derivation. Users always know what the default value is by reading the `impl`.
- **Extensible**: `comp_feature` allows future compiler passes to leverage `Default` without changing the trait itself.
- **No partial initialization**: Struct partial initialization syntax (`Config { port: 3000, ..default }`) is out of scope for this WEP and may be addressed separately.
