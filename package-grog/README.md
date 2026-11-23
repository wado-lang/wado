# Grog — Protocol Buffers for Wado

`wado-lang:grog` is a Kiln generator that reads a `.proto` and writes Wado
declarations, and the runtime library the generated code encodes and decodes
through. It reads proto2, proto3 and editions 2023, 2024 and 2026.
[`docs/design.md`](./docs/design.md) holds the design, and
[`TODO.md`](./TODO.md) what Grog does not read yet.

## Using Grog

A consumer names Grog twice: as the generator, and as the runtime the generated
code imports. The output imports the runtime by the name the `use` gave the
generator.

```toml
[build-dependencies]
"wado-lang:grog" = { version = "^0.0.39" }

[dependencies]
"wado-lang:grog" = { version = "^0.0.39" }
```

```wado
use fs from "core:fs";
use grog from "wado-lang:grog";
use onnx from "./onnx.proto" with { generator: { module: "wado-lang:grog" } };

let model = grog::decode::<onnx::ModelProto, ByteList>(fs::read("model.onnx")?)?;
let bytes = grog::encode(&model);
```

A generator named by a relative path is refused, since a path names no package
the output could import.

## What Grog Generates

Each message becomes a `struct` implementing `Message`, and each enum an `enum`
or a `variant` implementing `Enumeration`. A field's Wado type and default say
its presence:

| Protobuf                                  | Wado                                    |
| ----------------------------------------- | --------------------------------------- |
| `double` / `float`                        | `f64` / `f32`                           |
| `int32`, `sint32`, `sfixed32`             | `i32`                                   |
| `int64`, `sint64`, `sfixed64`             | `i64`                                   |
| `uint32`, `fixed32` / `uint64`, `fixed64` | `u32` / `u64`                           |
| `bool`                                    | `bool`                                  |
| `string`                                  | `String`                                |
| `bytes`                                   | `ByteList`                              |
| an enum                                   | an `enum`, or a `variant` if it is open |
| a message                                 | a `struct`                              |
| implicit presence                         | `T = zero`                              |
| explicit presence, a message field        | `Option<T> = null`                      |
| `repeated T`                              | `List<T> = []`                          |
| `map<K, V>`                               | `HashMap<K, V> = HashMap::new()`        |
| a `oneof`                                 | `Option<V> = null`, `V` a `variant`     |

- Presence follows the file's rules. In proto3 a field without a label is
  implicit and `optional` is explicit. In proto2 every field is explicit. An
  edition resolves `features.field_presence` from the field outward, through
  each enclosing message to the file. A message field and a `oneof` member
  always have presence. An implicit field is left off the wire while it holds
  its zero.
- A `oneof` is one field holding a `variant` with a case per member, so at most
  one member is set, and setting one replaces the other.
- A repeated scalar is packed where the file's rules say so, and the decoder
  reads either form.
- The encoding of an integer is not in its Wado type: `sint32` and `sfixed32`
  are both `i32`, and the generated calls name the runtime's `SInt32` or
  `SFixed32`.

### Enums

A proto3 enum, and an edition enum not marked `CLOSED`, is open: it holds every
number, declared or not. It becomes a `variant` with one payload-free case per
value and an `Unrecognized(i32)` case for the rest, so an unknown number
survives a round trip. An open enum compares and orders by its number, so
`Unrecognized(1)` and the case numbered 1 are one value, and a decode turns
the first into the second. A closed enum is a plain `enum`, and a number it
does not declare is kept among the message's unknown fields.

A value drops its enum's `SCREAMING_SNAKE` prefix and is PascalCased, so
`FieldPresence`'s `FIELD_PRESENCE_UNKNOWN` becomes `Unknown`. A value whose
remainder would start with a digit keeps the prefix. A second name for a number
an earlier value holds is an alias, an associated `const` naming that case. An
unset enum field reads as the first value.

### Names

A field named by a keyword gets a trailing `_`. A nested message or enum is
emitted at the top level, named by joining its path, and two declarations that
collide stop the build naming both. A declaration named like a prelude type,
the runtime import or `HashMap` takes a trailing `_` too, as does a field named
`unknown_fields`. A `reserved` range produces nothing.

## Decoding

`decode` starts from `Default` and merges; `merge` reads into an existing
message. A scalar read twice keeps the last value, a message read twice merges
the second into the first, and a repeated field appends, so the concatenation
of two encodings decodes as their merge.

Every message has an `unknown_fields: ByteList`. A field the schema does not
declare, or one arriving with a wire type the schema does not give it, is
appended there as it was read and written back after the known fields, so an
old reader forwards a new message.

A decode bounds nesting at 100 levels, and its time and memory grow linearly
with the input, with no tighter bound on memory. A caller that reads untrusted
input bounds the input's length.

### Large Messages

A repeated message field `items` also gets a function, `items_of`, that reads
its values one at a time out of an encoded message a `LinearBuffer` holds or
reads. It drops the other fields without decoding them, and reads a stream only
as far as the value it decodes. So a large message is never held whole, and no
byte of it lies in the GC heap:

```wado
use { println } from "core:cli";
use fs from "core:fs";
use { LinearBuffer } from "core:linear";
use scip from "./scip.proto" with { generator: { module: "wado-lang:grog" } };

let bytes = fs::read_linear("index.scip")?;
for let doc of scip::Index::documents_of(LinearBuffer::of(bytes)) {
    println(doc?.relative_path);
}
```

`grog::delimited::<M>(buffer)` reads messages one after another, each behind
its length as a varint, which is how a stream carries several.
`encode_delimited` writes one.

## Encoding

Encoding is deterministic within Grog: fields are written in field-number
order, and a map in the order its entries were inserted. Protobuf defines no
canonical encoding, so this is not a claim about any other encoder. `encode`
panics on a message of 2 GiB or more, which the specification does not allow.
