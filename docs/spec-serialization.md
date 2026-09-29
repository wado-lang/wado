# Serialization

`core:serde` is a format-agnostic serialization framework. One pair of traits
serves every format:

<!-- {"fixture":"spec_serialization_traits.wado"} -->

```wado
pub trait Serialize with () {
    fn serialize<S: Serializer>(&self, s: &mut S) -> Result<(), SerializeError>;
}

pub trait Deserialize with () {
    fn deserialize<D: Deserializer>(d: &mut D) -> Result<Self, DeserializeError>;
}

test {
    assert json::to_string(&Reading { c: Celsius { deg: 21 } }).unwrap() == "21";
    assert json::from_string::<Reading>("21").unwrap().c.deg == 21;
}
```

A format implements `Serializer`, `Deserializer`, or both, and a value's
`Serialize` / `Deserialize` impl works with any of them. The formats in the
standard library are `core:json` and `core:cbor`, which do both, and
`core:args`, which only deserializes.
[`core:serde`](./stdlib-core-serde.md) lists each trait's methods.

`Deserialize::deserialize` has no `self`: it builds a new value from what the
deserializer reads. Typed deserialization is pulled by the value's impl. A
format's `deserialize_any` hands the input to a `Visitor` instead, for input
whose shape is not known ahead.

The data model a `Serializer` accepts is:

- integers as `i32`, `i64`, `i128`, `u32`, `u64`, `u128`, with the narrower
  types widened to `i32` / `u32`;
- floats as `f32` and `f64`. `f16` and `bf16` are written as the `f32` they
  widen to, and read back by rounding the number once, straight to the half;
- `bool`, `char`, a string, a byte string, and null;
- a sequence, a map with runtime keys, and a struct with fields named at compile
  time;
- a variant case, written as its type name, its case name, and its
  discriminant, either alone or with one payload.

Failure is one of two fixed error types, whichever direction failed.
`SerializeError` carries a kind (`UnsupportedValue`, `Custom`) and a message.
`DeserializeError` carries a kind (`UnexpectedType`, `MissingField`,
`UnknownVariant`, `DuplicateField`, `InvalidValue`, `Overflow`,
`MalformedInput`, `TrailingData`, `Eof`, `DepthLimitExceeded`, `Custom`), a
message, and the byte offset into the input, or `-1` where the failure has no
position. Nesting deeper than a format's `max_depth` is a `DepthLimitExceeded`
error, not a trap.

A format is self-describing (SD) when the input names what it holds, such as
`core:json` writing a struct as an object keyed by field name. A
self-describing format resolves each key to a field by its wire name. A
non-self-describing (NSD) format leaves the reader to know what the input holds,
as [`core:args`](#command-line-arguments-coreargs) does. The same `Deserialize`
impl reads both. `example/json_nsd.wado` is an NSD JSON format written outside
the standard library: a struct is an array of its fields in declaration order,
a unit variant case is its discriminant, and a payload case is
`[discriminant, payload]`.

Rationale: [WEP: Serialization and Deserialization](./wep-2026-02-28-serde.md).

## Serialization Names

A struct field, enum case, variant case, or flags member goes on the wire under
its name in the source, unchanged: `user_name` stays `user_name` and `Red`
stays `Red`. This is its wire name. [Wire Names](./spec-attributes.md#wire-names)
states how `#[wire(...)]` renames one.

## Serialized Shapes

A derived impl writes each kind of type in one shape:

| Type      | Written as                                                                             |
| --------- | -------------------------------------------------------------------------------------- |
| `struct`  | a struct with its type name and each field's wire name and value, in declaration order |
| `enum`    | a unit variant case: the live case's wire name and discriminant                        |
| `variant` | a unit case as `enum` writes one; a payload case with its one payload                  |
| `flags`   | a sequence of the set members' wire names, in declaration order                        |
| tuple     | a sequence of its elements                                                             |

Reading back is the mirror. A variant is read by its discriminant where the
format reports one, as an NSD format does, and by its wire name otherwise.

A variant is externally tagged, as in Rust's serde, and no other representation
exists. In JSON:

| Value                            | JSON                          |
| -------------------------------- | ----------------------------- |
| `Color::Red`                     | `"Red"`                       |
| `Shape::Circle(5.0)`             | `{"Circle": 5.0}`             |
| `Shape::Rectangle([10.0, 20.0])` | `{"Rectangle": [10.0, 20.0]}` |

The standard library's own impls:

| Type                                 | Written as                                 |
| ------------------------------------ | ------------------------------------------ |
| integers, floats, `bool`, `char`     | the matching data model entry              |
| `String`                             | a string                                   |
| `ByteList`, `ByteArray`, `ByteSlice` | a byte string                              |
| `()`                                 | null                                       |
| `Option<T>`                          | null for `None`; the held value for `Some` |
| `Result<T, E>`                       | a variant with cases `Ok` and `Err`        |
| `List<T>`                            | a sequence                                 |
| `TreeMap<K, V>`                      | a map                                      |
| `TreeSet<T>`                         | a sequence                                 |

A text format spells every map key as a string, so there a key must be a scalar:
an integer, a float, `bool`, `char` or a string. A non-string key is written as
the string of its spelling (`{"1": …}` for the integer `1`) and parsed back from
it. Any other key type is an `UnsupportedValue` error on writing and an
`UnexpectedType` error on reading. A binary format with keys of its own, such as
`core:cbor`, writes the key as the value it is.

A format with no byte string of its own writes one as a sequence of `u8`. `()`
and `None` both write null, so `Option<()>` carries no information: `Some(())`
reads back as `None`. A `()` anywhere else round-trips. Likewise
`Some(None)` and `None` of an `Option<Option<T>>` are both null in JSON.

## Missing, Repeated, and Unknown Fields

A missing field is a `MissingField` error unless the field declares a default
(`f: T = expr`, see [Struct Field Defaults](./spec-types.md#struct-field-defaults)).
A defaulted field is optional and falls back to its default when absent. This is
the only mechanism for an optional field, and no type is special-cased. A bare
`Option<T>` field is required: a self-describing format needs its key present,
though the value may be null. `Option<T> = null` is the optional one. For a
zero-value fallback, write the zero literal (`= 0`, `= ""`, `= false`, `= []`,
`= null`).

<!-- {"fixture":"spec_serialization_optional_fields.wado"} -->

```wado
struct Config {
    host: String,            // required: MissingField if absent
    port: i32 = 8080,        // optional: 8080 if absent
    tags: List<String> = [], // optional: [] if absent
}

test {
    let c = from_string::<Config>(`{"host":"h"}`).unwrap();
    assert c.port == 8080 && c.tags == [];
    assert from_string::<Config>(`{"port":1}`).unwrap_err().kind matches { MissingField };
}
```

`#[wire(default)]` does not exist. Writing it is a compile error that points to
a field default instead.

Deserialization rejects a repeated field or key by default. A format that
overrides `Deserializer::on_duplicate_key` may accept one instead.

A self-describing format skips a key that names no field, whatever value it
holds.

## Bound-Driven Serialize / Deserialize

`Serialize` and `Deserialize` derive on demand, as
[Derivation Policy](./spec-traits.md#derivation-policy) states, so a type needs
no marker to be serializable. An anonymous struct, which no marker can name, is
serializable the same way:

<!-- {"fixture":"spec_serialization_bound_driven.wado"} -->

```wado
use { to_string } from "core:json";

struct Point { x: i32, y: i32 }              // no impl marker needed

test {
    let json = to_string(&Point { x: 1, y: 2 });
    assert json.unwrap() == `{"x":1,"y":2}`;
    let anon = to_string(&{ x: 1, y: 2 });       // the same JSON, from an anonymous struct
    assert anon.unwrap() == json.unwrap();
}
```

The marker `impl Serialize for T;`
([Compiler-Synthesized `impl`](./spec-traits.md#compiler-synthesized-impl))
forces the impl where no bound asks for it. `#[wire(...)]` attributes apply
with or without it.

Deriving on demand means a type becomes serializable the moment some code asks,
and a field added later extends its wire shape. A hand-written impl is the
control where that matters.

## JSON Module (`core:json`)

<!-- {"fixture":"spec_serialization_json.wado"} -->

```wado
use { to_string, from_string } from "core:json";

struct User { name: String, age: i32 }

test {
    let user = User { name: "Alice", age: 30 };

    // Serialize to JSON string
    let json = to_string::<User>(&user).unwrap();
    assert json == `{"name":"Alice","age":30}`;

    // Deserialize from JSON string
    let back = from_string::<User>(json).unwrap();
    assert back.name == "Alice" && back.age == 30;
}
```

Serializing a `NaN` or an infinite float is an `Err`. Deserializing malformed
input, a type mismatch, or input with trailing data is an `Err`.

`core:json` writes `i64`, `u64`, `i128`, and `u128` as a JSON number while the
magnitude is at most 2^53 - 1, the largest integer a JavaScript number holds
exactly, and as a JSON string of decimal digits beyond it. Reading any integer
type accepts either form.

## Command-Line Arguments (`core:args`)

`core:args` is a non-self-describing, parse-only `Deserializer` over `argv`. An
argument type is an ordinary struct. Each field is a `--long` option, unless
`#[wire(positional)]` marks it as a positional.

<!-- {"fixture":"spec_serialization_args.wado"} -->

```wado
use { parse } from "core:args";

struct Cli {
    #[wire(positional)] input: String,
    jobs: i32 = 1,
    verbose: bool = false,
}

test {
    let cli = parse::<Cli>(["in.txt", "--jobs", "4", "--verbose"]).unwrap();
    assert cli.input == "in.txt" && cli.jobs == 4 && cli.verbose;
}
```

`parse::<T>(argv)` takes the arguments as a `List<String>` and performs no
effect, so a test or a dispatcher passes them in. `from_env::<T>()` reads them
from `core:cli::args()` and requires `Environment`. Neither sees the program
name. Both return `Result<T, ArgsError>`, whose kind is one of `UnknownOption`,
`MissingValue`, `MissingArgument`, `UnknownSubcommand`, `InvalidValue`, and
`TooManyPositionals`.

### Options

Only long options exist: `--name value`, `--name=value`, and a bare `--name`
for a `bool` flag. There are no short options, no bundling, and no prefix
abbreviation. `-` and `_` fold in an option name, so `--dry-run` binds
`dry_run`. A value never starts with `--`, so `--name --next` is `MissingValue`
rather than a value, while a single `-` starts one (`--delta -5`). After a bare
`--`, every token is positional.

A field's type and whether it declares a default decide its arity:

| Field              | On the command line                         |
| ------------------ | ------------------------------------------- |
| `T`                | required `--name <value>`                   |
| `T = expr`         | optional `--name <value>`; absent is `expr` |
| `Option<T> = null` | optional `--name <value>`; absent is `null` |
| `bool = false`     | flag `--name`; absent is `false`            |
| `List<T> = []`     | repeatable, zero or more times              |
| `List<T>`          | repeatable, at least once                   |

A repeated option may be interspersed with others, and always takes a value.
A struct whose every field has a default parses an empty `argv` without error.

A token converts to a scalar with `LenientFromStr`, so `--jobs 0x10`,
`--retries 1_000`, and `--verbose=1` parse. A failed conversion is
`InvalidValue`. An `enum`-typed option takes a case's wire name as its value,
and a value naming no case is `InvalidValue`.

### Positionals and Subcommands

A `#[wire(positional)]` field is filled from bare tokens, in declaration order
and never by `--name`. A defaulted positional is optional, and a
`List<T> = []` positional takes every remaining bare token. A bare token beyond
the last positional is `TooManyPositionals`.

A subcommand set is a positional field whose type is a `variant`. Its tag is the
leading bare token, matched against the case's wire name, and the case's payload
parses the tokens after it. A tag naming no case is `UnknownSubcommand`.
`#[wire(name_policy = "kebab-case")]` on the variant makes `AddRemote` the tag
`add-remote`; no case folding happens beyond the wire name.

<!-- {"fixture":"spec_serialization_subcommands.wado"} -->

```wado
struct AddArgs { #[wire(positional)] path: String, all: bool = false }

#[wire(name_policy = "kebab-case")]
variant Command {
    Add(AddArgs),          // tag `add`
    Remote(RemoteCmd),     // nested: the payload holds another variant
}

#[wire(name_policy = "kebab-case")]
variant RemoteCmd { List }

struct Cli {
    verbose: bool = false,
    #[wire(positional)] command: Command,
}

test {
    let cli = parse::<Cli>(["--verbose", "add", "--all", "x"]).unwrap();
    assert cli.verbose && cli.command matches { Add(a) && a.all && a.path == "x" };
    assert parse::<Cli>(["remote", "list"]).unwrap().command matches { Remote(List) };
}
```

Options before the tag bind to the outer struct and options after it to the
subcommand, so `prog --verbose add --all x` works. Nesting needs nothing more.

Positionals bind greedily in declaration order, so a well-formed struct puts
required positionals before optional ones, at most one variadic positional, and
that one last, and never a variadic positional beside a subcommand. A struct
that breaks this is an error where it is declared.

Rationale: [WEP: Command-Line Argument Parsing](./wep-2026-06-22-core-args.md).
