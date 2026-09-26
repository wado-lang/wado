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

Lets a function perform the listed effects without declaring `with E`, and stops them from propagating to callers. It is meant for effects that are observationally pure, that is, unobservable through the function's interface. Only the named effects are suppressed. Others propagate normally, and the world import for each is still required. The compiler cannot verify observational purity, so this is an unchecked assertion that must be audited.

An argument names an effect the way a `with` clause does, by the name the function's module gives it, an import alias included. A name that reaches no effect there is an error. An effect of the same name declared in another module is a different effect, and stays required.

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

Rationale: [WEP: Effect System and Randomness in Collections](./wep-2026-01-20-effect-system-randomness.md).

### `#[ambient]`

Exempts a function's body from effect checking. [Ambient Functions](./spec-effects.md#ambient-functions) states the rule.

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

- `dead_code`: an unused or test-only free function or global (see [The `dead_code` Lint](#the-dead_code-lint)).
- `shadowed_name`: a binder that takes a name already reaching a known symbol.
- `undecided_effects`: a trait head that writes no `with` clause (see [The Trait Head](./spec-effects.md#the-trait-head)).

```wado
#[allow(dead_code)]
fn scaffolding() -> i32 {  // no "function `scaffolding` is never used"
    return 0;
}
```

#### The `dead_code` Lint

The `dead_code` lint warns about a free function or a global that the program
does not use. An item is used when one of these roots reaches it:

- A `pub` or `export` item. An `internal` item is not a root, because nothing
  outside the package can reach it.
- A function whose name a world export names.
- A method. A method is not itself reported, and a free function that only a
  method calls counts as used.
- A struct field default, an associated constant's value, and the default body
  of an `interface` or `resource` operation.

A trait's default body counts as reached only where a call lands on it, so a
function that only an unreached default body calls is unused.

An item the roots do not reach is reported one of two ways:

- Reached from a `test` block: "only used by tests". Compiling for the test
  world omits this warning, since there those tests are what the item is for.
- Reached from nothing: "never used".

An item in the standard library or in a `#![generated]` module is never
reported.

Rationale: [WEP: Unused Diagnostics](./wep-2026-05-16-unused-diagnostics.md).

### `#[param]` / `#[param(from_env = "...")]` / `#[param(name = "...")]`

Marks a `global` as a compile-time build input. The type annotation gives the type, the initializer is the fallback, and read sites are ordinary global references.

```wado
#[param]
global API_URL: String = "http://localhost";   // -D API_URL=...

#[param(from_env = "PORT")]
global PORT: i32 = 8080;                        // read from an env var

#[param(name = "build.id")]
global BUILD_ID: String = "dev";                // -D build.id=...
```

It takes two optional arguments:

- `name = "..."` is the name `-D` sets. It defaults to the global's own name.
- `from_env = "..."` names an environment variable read at compile time. It is
  independent of `name`.

Each is a non-empty string, and any other argument is an error. `#[param]` on a
`global mut` is an error. The global may be `pub`.

The declared type must be a built-in scalar: `String`, `char`, `bool`, `f32`,
`f64`, or an integer type from `i8` to `u128`. Any other type is an error.

#### Resolution

Each parameter takes the first of these that supplies a value:

1. `-D NAME=value` (alias `--define`) on the `wado` invocation.
2. The `from_env` variable, where one is named and set.
3. The tool's own default for the parameter, such as the log level `wado test`
   sets for `core:log`. The user did not write a tool default, so one that
   matches no parameter, or does not convert, is dropped silently.
4. The initializer, type-checked as written.

A supplied value is trimmed of surrounding whitespace, then converted to the
declared type with the spellings `LenientFromStr` accepts. So a `String` or
`char` parameter cannot carry surrounding whitespace.

Parameter names share one namespace across the whole compilation, dependencies
included. Same-named parameters in two packages answer to the same `-D`, so a
library prefixes its names.

#### Resolution Failures

The invocation sets a level for each of three failures. `error` fails the
build, `warn` reports and falls back to the initializer, and `ignore` falls back
silently.

| Failure                                     | Flag              | Default  |
| ------------------------------------------- | ----------------- | -------- |
| A `-D` name that matches no `#[param]`      | `--param-unknown` | `error`  |
| A value that does not convert to the type   | `--param-invalid` | `error`  |
| No value supplied, so the initializer holds | `--param-missing` | `ignore` |

Rationale: [WEP: Compile-Time Parameters](./wep-2026-04-26-compile-time-params.md).

### `#[unavailable("reason")]`

Declares a name that is deliberately not offered, on a declaration with no body. A call to it is an error that reports the reason. The reason is the only argument, and a removal writes its version into it. A missing or empty reason is an error.

```wado
impl File {
    #[unavailable("write `open_with(Options::default())` instead")]
    pub fn open(&self);

    #[unavailable("removed in 0.5.0; use `open_with`")]
    pub fn open_timeout(&self);
}
```

The declaration reserves a name rather than a signature. Its parameters and
return type are parsed but never resolved or type-checked, and may be left
empty. `self` is the exception: it decides whether the instance name or the
static name is reserved, since those are two names on one type.

A call that reaches the declaration is an error, and its arguments are never
counted or checked against the parameters. A fault inside an argument is still
reported. Naming the declaration without calling it is the same error. A
declaration that nothing names is not an error.

The declaration takes part in name resolution and nothing else:

- It never satisfies a trait requirement, so an `impl` whose method is
  `#[unavailable]` has not implemented the trait's method.
- On a trait it is not a requirement: no impl owes it. Every type implementing
  the trait answers to the name, through the type or through a bound.
- A reserved name answers only where no method of its kind (instance or static)
  does. Where another trait of the type has such a method, the call reaches that
  method. A method whose trait is not imported at the call site does not
  displace the reservation.
- Through a bound a call does not tell the kinds apart, so a reserved static
  beside an instance method of the same name is ambiguous, as two methods are.
- `wado doc` does not render it.

It goes on a module function, an `impl` method, or a trait method. Anywhere else
is an error. `export` is refused with it, since there is no function to lower at
the component boundary.

Rationale: [WEP: Declared Absence](./wep-2026-09-13-declared-absence.md).

### `#[expect_trap]`

Test block attribute. Marks a test that passes only if its body traps. [`#[expect_trap]` Attribute](./spec-testing.md#expect_trap-attribute) states the rule.

### `#[TODO]`

Test block attribute. Marks a test for an unimplemented feature, reported apart from pass/fail. [`#[TODO]` Attribute](./spec-testing.md#todo-attribute) states the rule.

### `#![TODO]`

Module-level inner attribute. Marks every test in the module as `#[TODO]`, and tolerates a module that fails to compile. The source must still parse, since otherwise the attribute cannot be recognized. [`#![TODO]` Modules](./spec-testing.md#todo-modules) states the outcomes.

### `#[timeout_ms(N)]`

Test block attribute. Overrides the default test timeout. [`#[timeout_ms(N)]` Attribute](./spec-testing.md#timeout_msn-attribute) states the rule.

### `#[synopsis]`

Test block attribute. The test runs like any other, and `wado doc` renders its body as the module's `## Synopsis` section, a usage example that is compiled and so stays current.

```wado
#[synopsis]
test {
    let p = Point { x: 3, y: 4 };
    assert p.length() == 5.0;
}
```

A synopsis documents the module that holds it, not one item. `wado test` counts
it in the ordinary pass/fail total, and it combines with `#[expect_trap]`,
`#[TODO]` and `#[timeout_ms]`. It is conventionally unnamed, since it is the
example rather than one case among several.

`wado doc` places the `## Synopsis` section after the module's `//!` doc and
before its items. The code is the test body exactly as written between the
outer braces, with its indentation removed, so nothing hidden sets it up. Each
`#[synopsis]` test in the module is one code block, in source order. A module
with none has no section. `wado doc` runs nothing.

Rationale: [WEP: Synopsis Tests](./wep-2026-04-26-synopsis-tests.md).

### `#[wire(...)]`

Controls how a declaration is serialized and deserialized. The framework it
customizes is in [Serialization](./spec-serialization.md),
and the library API in [`core:serde`](./stdlib-core-serde.md).

Rationale: [WEP: Serialization and Deserialization](./wep-2026-02-28-serde.md).

Each declaration reads its own keys:

| Declaration                                   | Keys                                       |
| --------------------------------------------- | ------------------------------------------ |
| `struct`, `enum`, `variant`, `flags`, newtype | `name_policy`                              |
| Struct field                                  | `name`, `number`, `encoding`, `positional` |
| Enum case                                     | `name`, `number`                           |
| Variant case                                  | `name`                                     |

A key the declaration does not read is an error, and so is `#[wire]` anywhere
else, a flags member included. The keys may be split across several `#[wire]`
attributes on one declaration, but each key is written once.

#### Wire Names

A name is written as in the source unless one of these keys changes it (see
[Serialization Names](./spec-serialization.md#serialization-names)).

- `#[wire(name = "...")]` sets the wire name of one field or case, exactly as
  written.
- `#[wire(name_policy = "...")]` on a type converts the source name of every
  field, case or member that has no `name` of its own. On a newtype, which has
  no members, it applies to the type's own name. The policies are
  `"camelCase"`, `"snake_case"`, `"SCREAMING_SNAKE_CASE"`, `"PascalCase"`,
  `"kebab-case"` and `"SCREAMING-KEBAB-CASE"`, and any other string is an error.
  A policy reads any source casing, so a `PascalCase` case and a `snake_case`
  field both convert.

```wado
#[wire(name_policy = "camelCase")]
struct Event {
    created_at: String,     // "createdAt"
    #[wire(name = "type")]
    event_type: String,     // "type"
}
```

#### Numbers and Encodings

`#[wire(number = N)]` gives a struct field the numeric key that a number-keyed
format such as `core:protobuf` reads. A format keyed by name ignores it, so a
numbered struct still serializes through `core:json` by name.

- A struct numbers every field or none. A struct with no fields satisfies this.
- `N` runs from 1 to 536870911, and 19000 to 19999 are reserved.
- Two fields of one struct never share a number.
- A numbered struct satisfies the `WireNumbered` bound that number-keyed formats
  require, so passing an unnumbered one is a bound error where the call is
  written.

No policy assigns numbers, so each number is written out on its field.

On an enum case, `N` is an `i32`, negative values included. It replaces the
case's position as its discriminant on every wire, `core:json_nsd` included. An
enum numbers every case or none, and two cases never share a number. A variant
case takes no number.

`#[wire(encoding = "...")]` chooses how a number-keyed format writes an integer
field. `"zigzag"` accepts `i32` and `i64`. `"fixed"` accepts any 32- or 64-bit
integer. Either also accepts an `Option` or `List` of such an integer, since the
encoding is the element's. Anything else, including any other encoding name, is
an error where the attribute is written. A format keyed by name ignores it.

```wado
struct Account {
    #[wire(number = 1)] id: i32 = 0,
    #[wire(number = 2)] #[wire(encoding = "zigzag")] delta: i64 = 0,  // sint64
    #[wire(number = 3)] #[wire(encoding = "fixed")] hash: u32 = 0,    // fixed32
}
```

Rationale: [WEP: Grog](./wep-2026-09-22-grog.md).

#### Positional Fields

`#[wire(positional)]` marks a struct field as ordinal: when deserializing, it is
filled by position and never matched by name. A format that resolves fields by
name, such as `core:json`, never fills it, so the field takes its default or is
reported missing. A sequence-only format such as `core:json_nsd` reads it in
order like any other field. Serializing is unaffected. `core:args` fills
positional fields from bare tokens (see [Command-Line Arguments](./spec-serialization.md#command-line-arguments-coreargs)).

#### Optional Fields

No `#[wire]` key makes a field optional: a field default does. [Missing,
Repeated, and Unknown Fields](./spec-serialization.md#missing-repeated-and-unknown-fields)
states the rule, `#[wire(default)]` included.

## Standard Library Attributes

These attributes are used in the standard library (`lib/`) to wire Wado code to Wasm and the Component Model. They are not intended for user code.

### `#![no_prelude]`

Module-level inner attribute. Prevents the automatic import of `core:prelude`. Used by low-level modules that define the prelude itself or that operate below the prelude layer.

```wado
#![no_prelude]
// This module does not import core:prelude
```

### `#![generated]`

Module-level inner attribute. Indicates that the module contains machine-generated code (e.g. from `wado-from-idl` or `gale`). It does not change how the module compiles. Tools read it; Kiln's use is in [Authoring a generator](./spec-modules.md#authoring-a-generator).

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

These attributes state what a call does with the reference parameters it is
handed: whether its result aliases one, and whether it keeps one past the
return. They are for a declaration with no body, whose retention cannot be read
from one: a `core:builtin` primitive, a Component Model import, a `.wasm` /
`.wat` asset import. [Reference Retention](./spec-memory.md#reference-retention)
states what retention is, and that no function type carries it.

```wado
#[result(part_of = arr)]
pub fn array_get_ref<T>(arr: &Array<T>, idx: i32) -> &T;

#[retain(value, into = arr)]
pub fn array_set<T>(arr: &mut Array<T>, idx: i32, value: T);

#[retain(elements_of = src, into = dst)]
pub fn array_copy<T>(dst: &mut Array<T>, dst_offset: i32, src: &Array<T>, src_offset: i32, len: i32);
```

`#[result(owned)]` says the result is freshly allocated; `#[result(part_of = p)]`
says it is part of `p`. `#[result]` takes exactly one of the two. Silence reads
as `owned`. So a declaration with a reference parameter whose result can share
storage must state one, and leaving it out is an error. A Component Model import
needs none, since the boundary copies and its result is always owned. Nor does
an [`#[unavailable]`](#unavailablereason) declaration, which is never called.

`#[retain(...)]` names one retained thing and repeats where there is more than
one, so each carries its own destination. A bare name is the parameter itself
and `elements_of = p` is that parameter's elements; `into = q` names the
parameter it lands in, and without it the destination is unknown. Silence is the
conservative reading.

Every parameter is named bare, never quoted, and a name that is not a parameter
of the declaration is an error. An argument the attribute does not take, or a
second retained thing in one `#[retain]`, is an error too. A malformed attribute
is never read as silence.

Both are an error on a function with a body, which states these facts itself,
and on a `trait` or `interface` method requirement: a call to one is statically
dispatched to an impl that has a body, so the impl states it.

Rationale: [WEP: Value Semantics and Reference Retention](./wep-2026-01-12-value-semantics-and-retention.md).

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

## Known gaps

- The `dead_code` lint sees only the `test` blocks of the modules the
  compilation loads. A function whose only user is a test in a file the
  compilation does not load, such as a test file that imports it, is reported
  "never used" rather than "only used by tests".
- `WireNumbered` holds only for the type a number-keyed format is handed. A
  field holding a struct without numbers is not bound by it, so that struct is
  reported when its bytes are produced rather than where the call is written.
- A trait may declare a name `#[unavailable]` while an `impl` of the trait
  supplies a body under that name. A call through the implementing type then
  reaches the body, so the reservation does not hold there.
