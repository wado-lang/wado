# Serialization

`core:serde` is a format-agnostic serialization framework. One pair of traits
serves every format:

```wado
pub trait Serialize {
    fn serialize<S: Serializer>(&self, s: &mut S) -> Result<(), SerializeError>;
}

pub trait Deserialize {
    fn deserialize<D: Deserializer>(d: &mut D) -> Result<Self, DeserializeError>;
}
```

A format implements `Serializer` and `Deserializer`, and a value's `Serialize` /
`Deserialize` impl works with any of them. The formats in the standard library
are `core:json`, `core:json_nsd`, `core:cbor`, `core:protobuf`, and `core:args`.
[`core:serde`](./stdlib-core-serde.md) lists each trait's methods.

`Deserialize::deserialize` has no `self`: it builds a new value from what the
deserializer reads. Typed deserialization is pulled by the value's impl. A
format's `deserialize_any` hands the input to a `Visitor` instead, for input
whose shape is not known ahead.

The data model a `Serializer` accepts is:

- integers as `i32`, `i64`, `i128`, `u32`, `u64`, `u128`, with the narrower
  types widened to `i32` / `u32`;
- floats as `f32` and `f64`, with `f16` and `bf16` written as the `f32` they
  widen to;
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
position.

A format is self-describing (SD) when the input names what it holds, such as
`core:json` writing a struct as an object keyed by field name. A
non-self-describing (NSD) format leaves the reader to know it: `core:json_nsd`
writes a struct as an array of its fields in declaration order. The same
`Deserialize` impl reads both. A self-describing format resolves each key to a
field by its wire name.

Rationale: [WEP: Serialization and Deserialization](./wep-2026-02-28-serde.md).

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

The default variant representation is externally tagged, as in Rust's serde,
and it is the only one. In JSON:

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
| `TreeMap<String, V>`                 | a map                                      |

A format with no byte string of its own writes one as a sequence of `u8`. `()`
and `None` both write null, so `Option<()>` carries no information: `Some(())`
reads back as `None`. A `()` anywhere else round-trips. Likewise
`Some(None)` and `None` of an `Option<Option<T>>` are both null in JSON.

## Missing, Repeated, and Unknown Fields

A missing field is a `MissingField` error unless the field declares a default
(`f: T = expr`, see [Struct Field Defaults](./spec-types.md#struct-field-defaults)).
A defaulted field is optional and falls back to its default when absent. This is
the only mechanism for an optional field, and no type is special-cased: a bare
`Option<T>` field is required, and `Option<T> = null` is the optional one. For a
zero-value fallback, write the zero literal (`= 0`, `= ""`, `= false`, `= []`,
`= null`).

```wado
struct Config {
    host: String,            // required: MissingField if absent
    port: i32 = 8080,        // optional: 8080 if absent
    tags: List<String> = [], // optional: [] if absent
}
```

`#[wire(default)]` does not exist. Writing it is a compile error that points to
a field default instead.

Deserialization rejects a repeated field or key by default; a format overrides `Deserializer::on_duplicate_key` to be lenient. Nesting past a format's `max_depth` is a `DepthLimitExceeded` error, not a trap.

A self-describing format skips a key that names no field, whatever value it
holds.

## Serialization Names

A name on the wire is the name in the source, unchanged, unless an attribute
says otherwise:

| Element      | Source convention | Wire name by default     |
| ------------ | ----------------- | ------------------------ |
| Struct field | `snake_case`      | as written (`user_name`) |
| Enum member  | `PascalCase`      | as written (`Red`)       |
| Variant case | `PascalCase`      | as written (`Circle`)    |
| Flags member | `PascalCase`      | as written (`Read`)      |

`#[wire(name_policy = "...")]` on a struct, enum, variant, or flags type renames
each of its members by a case convention, and `#[wire(name = "...")]` renames one
field or case, taking precedence over the policy. A flags member reads only the
type's policy. [`#[wire(...)]`](./spec-attributes.md#wire) lists the options.

## Bound-Driven Serialize / Deserialize

The marker `impl Serialize for T;` ([Compiler-Synthesized `impl`](./spec-traits.md#compiler-synthesized-impl)) is optional: a `T: Serialize` bound is satisfied structurally once every field or case of `T` satisfies the trait — the same on-demand model `Eq` / `Ord` use ([Derivation Policy](./spec-traits.md#derivation-policy)). This is how an anonymous struct, which has no name for a marker, becomes serializable:

```wado
use { to_string } from "core:json";

struct Point { x: i32, y: i32 }              // no impl marker needed
let json = to_string(&Point { x: 1, y: 2 }); // Ok("{\"x\":1,\"y\":2}")
let anon = to_string(&{ x: 1, y: 2 });        // Ok("{\"x\":1,\"y\":2}") — anonymous struct
```

The explicit marker `impl Serialize for T;` still works — write it to force the impl with no bound present, or to attach `#[wire(name_policy = "...")]` customization. Like `Eq` / `Ord`'s marker, it is a conformance check: an ineligible field or case is a compile error at the marker's own span.

Deriving on demand means a type becomes serializable the moment some code asks,
and a field added later extends its wire shape. A hand-written impl is the
control where that matters.

## JSON Module (`core:json`)

```wado
use { to_string, from_string } from "core:json";

// Serialize to JSON string
let json = to_string::<User>(&user);   // Result<String, SerializeError>

// Deserialize from JSON string
let user = from_string::<User>(json);  // Result<User, DeserializeError>
```

JSON serialization returns `Err` for `NaN` and `Infinity` float values. JSON deserialization returns `Err` for malformed input, missing required fields, type mismatches, and trailing data.

`core:json` writes `i64`, `u64`, `i128`, and `u128` as a JSON number while the
magnitude is at most 2^53 - 1, the largest integer a JavaScript number holds
exactly, and as a JSON string of decimal digits beyond it. Reading any integer
type accepts either form.

## JSON NSD Module (`core:json_nsd`)

Non-self-describing JSON format. Structs are encoded as positional arrays (field names omitted), unit variants as discriminant integers, and payload variants as `[disc, payload]`.

```wado
use { to_string, from_string } from "core:json_nsd";

// Struct as positional array
let json = to_string::<User>(&user);   // Result: Ok("[\"Alice\",30]")

// Deserialize from positional array
let user = from_string::<User>(`["Alice",30]`);  // Result<User, DeserializeError>
```

The same `Serialize` and `Deserialize` trait impls work with both `core:json` and `core:json_nsd`.

## Command-Line Arguments (`core:args`)

`core:args` is a non-self-describing, parse-only `Deserializer` over `argv`. Argument types are ordinary structs with `impl Deserialize for T;`: fields become `--long` options, and fields marked `#[wire(positional)]` are filled from bare tokens in declaration order (required, optional, or variadic). Scalar tokens are converted with `LenientFromStr`.

```wado
use { parse } from "core:args";
use { Deserialize } from "core:serde";

struct Cli {
    #[wire(positional)] input: String,
    jobs: i32 = 1,
    verbose: bool = false,
}
impl Deserialize for Cli;

let cli = parse::<Cli>(["in.txt", "--jobs", "4", "--verbose"]);
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
An all-defaulted struct derives `Default`, so an empty `argv` cannot fail.

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
```

Options before the tag bind to the outer struct and options after it to the
subcommand, so `prog --verbose add --all x` works. Nesting needs nothing more.

Positionals bind greedily in declaration order, so a well-formed struct puts
required positionals before optional ones, at most one variadic positional, and
that one last, and never a variadic positional beside a subcommand. A violation
is not diagnosed; see [Known gaps](#known-gaps).

Rationale: [WEP: Command-Line Argument Parsing](./wep-2026-06-22-core-args.md).

## Known gaps

- `core:args` does not check a struct's positional declarations. Required after
  optional, a variadic before another positional, or a variadic beside a
  subcommand parses by greedy binding into a confusing `MissingArgument` or a
  swallowed tag, rather than failing where the struct is declared.
