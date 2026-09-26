# Compiler Attributes

Wado uses `#[...]` attributes (item-level) and `#![...]` inner attributes (module-level) to control compiler behavior.

## User-Facing Attributes

These attributes are part of the language surface and can be used in any Wado source file.

### `#[inline]` / `#[inline(always)]` / `#[inline(never)]`

Inlining hints for the optimizer. Applies to functions.

```wado
#[inline]              // hint: prefer inlining
fn small_helper() -> i32 { return 42; }

#[inline(always)]      // always inline (ignores threshold)
fn critical_path() -> i32 { return 1; }

#[inline(never)]       // never inline
fn error_handler() { panic("error"); }
```

### `#[benign(E, ...)]`

Lets a function perform the listed effects without declaring `with E`, and stops them from propagating to callers. It is meant for effects that are observationally pure, that is, unobservable through the function's interface. Only the named effects are suppressed. Others propagate normally, and the world import for each is still required. The compiler cannot verify observational purity, so this is an unchecked assertion that must be audited. See [WEP: Effect System and Randomness in Collections](./wep-2026-01-20-effect-system-randomness.md).

```wado
pub struct HashIndex {
    seed: u64, // private, and iteration order never reads it
}

impl HashIndex {
    #[benign(InsecureSeed)]
    pub fn new() -> HashIndex {
        let [seed, _] = InsecureSeed::get_insecure_seed(); // not required of callers
        return HashIndex { seed };
    }
}
```

### `#[secret]`

Hides a struct field from debug/inspect output (the `:?` format specifier).

```wado
struct Foo {
    pub name: String,
    #[secret]
    password: String, // excluded from `${foo:?}` output
}
```

### `#[allow(...)]`

Waives a lint on the item carrying it. As the module inner attribute
`#![allow(...)]` it waives the lint for every item in the file. There is no
`#[deny(...)]`. The lints are:

- `dead_code`: an unused or test-only item. See [WEP: Unused Diagnostics](./wep-2026-05-16-unused-diagnostics.md).
- `shadowed_name`: a binder that takes a name already reaching a known symbol.
- `undecided_effects`: a trait head that writes no `with` clause (see [The Trait Head](./spec-effects.md#the-trait-head)).

```wado
#[allow(dead_code)]
fn scaffolding() -> i32 {  // no "function `scaffolding` is never used"
    return 0;
}
```

### `#[param]` / `#[param(from_env = "...")]` / `#[param(name = "...")]`

Marks a `global` as a compile-time build input. The type annotation gives the type, the initializer is the fallback, and read sites are ordinary global references. Each parameter resolves highest-priority-first: `-D NAME=value` (alias `--define`) on the `wado` invocation, then `from_env`, then the initializer. Overrides are parsed into the declared scalar type with the `LenientFromStr` spellings. See [WEP: Compile-Time Parameters](./wep-2026-04-26-compile-time-params.md).

```wado
#[param]
global API_URL: String = "http://localhost";   // -D API_URL=...

#[param(from_env = "PORT")]
global PORT: i32 = 8080;                        // read from an env var

#[param(name = "build.id")]
global BUILD_ID: String = "dev";                // -D build.id=...
```

### `#[unavailable("reason")]`

Declares a name that is deliberately not offered, on a declaration with no body. A call to it is an error that reports the reason. The reason is the only argument, and a removal writes its version into it. The declaration reserves a name rather than a signature, so its parameters are never checked against a call. It goes on a module function, an `impl` method, or a trait method. See [WEP: Declared Absence](./wep-2026-09-13-declared-absence.md).

```wado
impl File {
    #[unavailable("write `open_with(Options::default())` instead")]
    pub fn open(&self);

    #[unavailable("removed in 0.5.0; use `open_with`")]
    pub fn open_timeout(&self);
}
```

### `#[expect_trap]`

Test block attribute. Marks a test that is expected to trap. The test passes if the body traps, and fails if it completes normally.

```wado
#[expect_trap]
test "panics on invalid input" {
    panic("bad input");
}
```

### `#[TODO]`

Test block attribute. Marks a test for an unimplemented feature. TODO tests are reported on a separate axis from regular pass/fail results:

- If the body traps, the test is pending. That is expected while the feature is unimplemented.
- If the body completes normally, the test is resolved. That is a hard failure: the `#[TODO]` attribute must be removed.

A pending TODO test never fails the run, and a resolved one always does. See [Test Outcome Model](./spec-testing.md#test-outcome-model).

```wado
#[TODO]
test "not yet implemented" {
    panic("TODO: implement this");
}
```

### `#[timeout_ms(N)]`

Test block attribute. Overrides the default test timeout (5000ms). `N` is an integer literal specifying the timeout in milliseconds.

```wado
#[timeout_ms(30000)]
test "slow computation" {
    let result = expensive_computation();
    assert result == 42;
}
```

### `#[synopsis]`

Test block attribute. The test runs like any other, and `wado doc` renders its body as the module's `## Synopsis` section, a usage example that is compiled and so stays current. See [WEP: Synopsis Tests](./wep-2026-04-26-synopsis-tests.md).

```wado
#[synopsis]
test {
    let p = Point { x: 3, y: 4 };
    assert p.length() == 5.0;
}
```

### `#[wire(name = "...")]` / `#[wire(name_policy = "...")]` / `#[wire(positional)]`

Controls serialization and deserialization of struct fields and enum cases. See [WEP: Serialization and Deserialization](./wep-2026-02-28-serde.md) and [`core:serde`](./stdlib-core-serde.md).

- `#[wire(name = "...")]` overrides the wire key of one field, or the wire name of one enum case.
- `#[wire(name_policy = "...")]` on a struct renames every field by a convention (`"camelCase"`, `"snake_case"`, `"kebab-case"`, ...).
- `#[wire(positional)]` marks a field as ordinal: it is resolved by position, never by name. Name-only and sequence-only formats ignore the hint. [`core:args`](./wep-2026-06-22-core-args.md) uses it to bind a bare token to the field.

A field is optional on deserialization when it has a default value (`f: T = expr`), and it falls back to that expression when absent. This is the only mechanism for optional fields.

## Standard Library Attributes

These attributes are used in the standard library (`lib/`) to wire Wado code to Wasm and the Component Model. They are not intended for user code.

### `#![no_prelude]`

Module-level inner attribute. Prevents the automatic import of `core:prelude`. Used by low-level modules that define the prelude itself or that operate below the prelude layer.

```wado
#![no_prelude]
// This module does not import core:prelude
```

### `#![TODO]`

Module-level inner attribute. Marks the entire module as TODO for `wado test`. The source must parse successfully (otherwise the attribute cannot be recognized), but compilation errors are tolerated:

- If compilation fails, the module is reported as a single pending TODO entry.
- If compilation succeeds, all test blocks are implicitly treated as `#[TODO]` tests.
- If the module compiles and all tests pass, it is reported as resolved. That is a hard failure: the `#![TODO]` attribute must be removed.

See [Test Outcome Model](./spec-testing.md#test-outcome-model).

```wado
#![TODO]

test "not yet implemented" {
    panic("TODO");
}
```

### `#![generated]`

Module-level inner attribute. Indicates that the module contains machine-generated code (e.g. from `wado-from-idl` or `gale`). It does not change how the module compiles. Tools read it: Kiln stamps it on every file it generates, and deletes a stamped file that the current run did not produce.

The attribute accepts optional metadata so that generators can attach provenance information directly to the attribute instead of as free-form comments. Two argument shapes are supported inside the parentheses:

- Scalar `key = "value"` pairs (e.g. `by = "wado-from-idl"`).
- List `key = ["v1", "v2", ...]` pairs whose values are a comma-separated list of string literals (e.g. `sources = ["a.wit", "b.wit"]`).

Conventional keys are `by` (the tool that produced the file) and `sources` (the list of source paths it was generated from). Unknown keys are tolerated, so generators can introduce additional metadata without requiring a spec change.

```wado
#![generated]

#![generated(by = "wado-from-idl", sources = ["deps/random.wit"])]

#![generated(by = "wado-from-idl", sources = ["cli.wit", "clocks.wit"])]
```

### `#![wasm_module("name")]`

Module-level inner attribute. All items in this module are compiled into a separate Wasm core module with the given name, which owns its own linear memory.

The Component Model requires a component to provide a linear memory and a `realloc` function for data crossing the boundary. `core:allocator` provides both as the core module `"mem"`, the only `wasm_module` in the standard library.

```wado
#![wasm_module("mem")]
#![no_prelude]

global mut heap_offset: i32 = 8;

#[allocator("bump")]
export fn bump_realloc(oldptr: i32, oldsize: i32, align: i32, newsize: i32) -> i32 {
    // ...
}
```

### `#[allocator("name")]`

Marks a function in a `wasm_module` as the `realloc` implementation named `name`. The world selects which one the component uses: `bump` for CLI programs, `freelist` for HTTP services, and `debug` for the test world. `debug` never reuses freed memory and fills it with `0xFF`.

### `#[export_name("name")]`

Overrides the Wasm export name of a function within its core module.

### `#[canonical("namespace", "name")]`

Declares that a bodyless function is imported rather than defined. Used in `core:builtin` to map intrinsic declarations to their imports.

| Namespace       | Description                                                    |
| --------------- | -------------------------------------------------------------- |
| `"wasi"`        | CM canonical builtins (streams, futures, tasks)                |
| `"mem"`         | Exports of the `"mem"` core module (`realloc`)                 |
| `"wasm:<path>"` | Exports of an imported core-wasm asset (e.g. the bundled libm) |

```wado
#[canonical("wasi", "stream-new")]
fn stream_new() -> i64;

#[canonical("mem", "realloc")]
fn realloc(oldptr: i32, oldsize: i32, align: i32, newsize: i32) -> i32;
```

### `#[compiler_item("name")]`

Binds a stdlib declaration to the language item of that name, such as `#[compiler_item("option")]` on `variant Option` or `#[compiler_item("display")]` on the `Display` trait. It is valid only in `core:*` modules, and an error elsewhere.

### `#![stdlib("path")]`

Module-level inner attribute. Names the bundled stdlib module a file is, such as `#![stdlib("core:cbor")]`. The file is that module however it was loaded, so a file an editor opens directly is the same module an import reaches.

### `#[cm("namespace:pkg/interface@version")]` / `#[cm_params(...)]`

Links Wado definitions (interfaces, worlds, resources, enums) to their Component Model names. See [Attribute Syntax for Component Model Linking](./spec-components.md#attribute-syntax-for-component-model-linking).

### `#[retain(...)]` / `#[result(...)]`

What a call does with the reference parameters it is handed: whether its result
aliases one, and whether it keeps one past the return. Neither is a safety
condition, since every referent is GC-managed and cannot dangle. The compiler
reads both from a function's body. These attributes are for a declaration that
has none: a `core:builtin` primitive, a Component Model import, a `.wasm` /
`.wat` asset import. See
[WEP: Value Semantics and Reference Retention](./wep-2026-01-12-value-semantics-and-retention.md).

```wado
#[result(part_of = arr)]
pub fn array_get_ref<T>(arr: &Array<T>, idx: i32) -> &T;

#[retain(value, into = arr)]
pub fn array_set<T>(arr: &mut Array<T>, idx: i32, value: T);

#[retain(elements_of = src, into = dst)]
pub fn array_copy<T>(dst: &mut Array<T>, dst_offset: i32, src: &Array<T>, src_offset: i32, len: i32);
```

`#[result(owned)]` says the result is freshly allocated; `#[result(part_of = p)]`
says it is part of `p`.

`#[retain(...)]` names one retained thing and repeats where there is more than
one, so each carries its own destination. A bare name is the parameter itself
and `elements_of = p` is that parameter's elements; `into = q` names the
parameter it lands in, and without it the destination is unknown. Silence is the
conservative reading.

Both are an error on a function with a body, which states these facts itself,
and on a `trait` or `interface` method requirement: a call to one is statically
dispatched to an impl that has a body, so the impl states it.

### `#[immediate(...)]`

Names a parameter that becomes a Wasm immediate: the argument's literal value is
encoded into the instruction itself.

```wado
#[immediate(value)]
pub fn v128_const(value: i128) -> v128;
```

It names one parameter, unquoted, and repeats for a second. Like
`#[retain(...)]`, it belongs to a declaration with no body, because a body is
called rather than encoded as one instruction. A `trait` or `interface` method
requirement is an error for the same reason: it reaches an impl, which is
called.

### `#[trap(...)]`

When a call to a declaration with no body traps. Silence means it may trap.
`#[trap(never)]` says it never traps, and a check names the one condition it
traps on:

```wado
#[trap(never)]
pub fn f64_sqrt(x: f64) -> f64;

#[trap(outside = arr, at = idx)]
#[trap(unset = arr)]
pub fn array_get_value<T>(arr: &Array<T>, idx: i32) -> T;

#[trap(outside = dst, at = dst_offset, len = len)]
#[trap(outside = src, at = src_offset, len = len)]
pub fn array_copy<T>(dst: &mut Array<T>, dst_offset: i32, src: &Array<T>, src_offset: i32, len: i32);

#[result(owned)]
#[trap(negative = len, result_len = len)]
pub fn array_new<T>(len: i32) -> Array<T>;
```

`negative = p` traps when `p` is below zero. `outside = a` traps unless the
range from `at` (0 when absent) of `len` elements (1 when absent) lies within
the array `a`, and says the call does not replace `a`. `unset = a` traps when
the element read holds no value: `array_new` leaves a reference element empty,
while a primitive element always holds one. Each attribute states one
check and repeats for another; the call traps where any fails. `result_len = p`
is no check: it says the returned array holds `p` elements, so a later check
against it can be proved. Running out of memory is not a trap any of these
describe.

It is an error on a function with a body, which states when it traps itself,
and on a `trait` or `interface` method requirement, for the reason `#[retain]`
is.

### `#[linear_memory(...)]`

How a call to a declaration with no body touches linear memory: `read` or
`write`. Silence means it touches none. A linear-memory address is a plain
`i32`, so no parameter type says this, and the attribute is the only source.

```wado
#[linear_memory(read)]
pub fn i32_load(addr: i32) -> i32;

#[linear_memory(write)]
pub fn i32_store(addr: i32, value: i32);
```

It is written once, and is an error where `#[trap]` is.
