# Jade — JSON Schema for Wado

Jade reads, derives and validates JSON Schema Draft 2020-12, and generates Wado
types from a schema. A package names it under `[dependencies]` and imports it
from `wado-lang:jade`. The name stands for JSON Assertion and Definition Engine.
[`docs/design.md`](./docs/design.md) holds the design: its four layers, and
what a derived schema promises.

## The Schema Model

A schema is `true`, `false` or an object, so `Schema` is a variant of
`Bool(bool)` and `Object(SchemaObject)`. `SchemaObject` has one field per
Draft 2020-12 keyword. A keyword the draft does not define is kept as written,
in `extra`. Writing a schema leaves an absent keyword out rather than writing
`null`.

`Schema::parse` reads a schema from JSON text. A keyword whose value breaks the
draft's rule for it is an error at its JSON Pointer.

### Documents

Jade reads a schema's text, and an instance it validates, into a `Document`
rather than into a Wado type. A `Document` holds its values in flat arrays in
document order, so reading one allocates a few arrays rather than one object
per value. A `Node` is a value inside it. Its equality is JSON Schema's: `1`
equals `1.0`, and an object's members compare whatever their order. `default`
and `const` hold a `Document`, and `enum` and `examples` a list of them.

A `Document` is read through `core:json`'s lexer, so a repeated key and a lone
surrogate are errors. A number is a `Number`, held exactly whatever its size and
precision, and two compare by their mathematical value. An exponent of more than
18 digits is an error, which keeps every exponent Jade computes within an
`i64`.

## Deriving a Schema

`schema_for` derives the schema of a Wado type from the shape `core:json`
writes it in.

```wado
use { schema_for } from "wado-lang:jade";
use { to_string } from "core:json";

struct Point { x: i32, y: i32 = 0 }

let schema = to_string(&schema_for::<Point>()).unwrap();
```

`JsonSchema` has no receiver, so a schema is built from the type alone:

```text
pub trait JsonSchema with () {
    fn json_schema(gen: &mut SchemaGenerator) -> Schema;
}
```

Jade implements it with one blanket `impl` per reflection kind (struct,
variant, enum, flags), as `core:serde` derives `Serialize`, so a type needs no
marker. A newtype inherits its base's impl. A closed struct, one with a field
narrower than the struct, derives no schema, just as it derives no `Serialize`.

### Definitions

`SchemaGenerator` collects the definitions a schema refers to. `define` writes
a named composite once as a definition and answers a `$ref` to it at each use.
It records the name before building, which is how a recursive type such as
`struct Node { next: Option<Node> }` ends. A composite with no name, such as an
anonymous struct, is built in place. `SchemaGenerator::new()` puts definitions
under `#/$defs/`, and `SchemaGenerator::openapi()` under
`#/components/schemas/`.

### The mapping

| Wado                                       | JSON Schema                                                                                   |
| ------------------------------------------ | --------------------------------------------------------------------------------------------- |
| `bool`                                     | `boolean`                                                                                     |
| `i8`…`i32`, `u8`…`u32`                     | `integer`, bounded by the type's range                                                        |
| `i64`, `u64`                               | `integer` in the type's range and within ±(2^53 − 1), or a decimal string in the type's range |
| `i128`, `u128`                             | a decimal string in the type's range                                                          |
| the float types                            | `number`                                                                                      |
| `char`                                     | `string` of one code point                                                                    |
| `String`                                   | `string`                                                                                      |
| `ByteList`, `ByteArray`, `ByteSlice`       | `string` with `contentEncoding: "base64url"`, unpadded                                        |
| the `core:temporal` types                  | `string` with `x-wado-format`                                                                 |
| `Uri`                                      | `string` with `format: "uri"` and `x-wado-format`                                             |
| `UriReference`                             | `string` with `format: "uri-reference"` and `x-wado-format`                                   |
| `Uuid`                                     | `string` with `format: "uuid"` and `x-wado-format`                                            |
| `Url`                                      | `string` with `x-wado-format`                                                                 |
| `()`                                       | `null`                                                                                        |
| `core:value`'s `Value`                     | `true`                                                                                        |
| `Option<T>`                                | `anyOf` of `T`'s schema and `null`                                                            |
| `List<T>`                                  | `array` of `T`                                                                                |
| `TreeSet<T>`, `HashSet<T>`                 | `array` of `T` with `uniqueItems`                                                             |
| `TreeMap<String, V>`, `HashMap<String, V>` | `object` whose `additionalProperties` is `V`'s schema                                         |
| a tuple                                    | `array` with `prefixItems`, and `minItems` and `maxItems` both its arity                      |
| `struct`                                   | `object`: `properties` under the wire names, `required`, each field's `default`               |
| `enum`                                     | `string` with `enum` of the wire names                                                        |
| `flags`                                    | `array` of the member wire names, with `uniqueItems`                                          |
| `variant`                                  | `oneOf`: a unit case as a `const` string, a payload case as an object of that one property    |
| `variant` with `#[wire(tag)]`              | `oneOf` of objects whose tag property is the case's `const` wire name                         |
| newtype                                    | its base's schema                                                                             |

A decimal string's range is a `pattern` anchored by `^` and `$`. The pattern
uses plain groups and `[0-9]` alone, which every regular expression engine reads
alike. An `Option` takes `anyOf` rather than `oneOf`, because in
`Option<Option<T>>` both branches accept `null`.

A tuple bounds its length on both sides, since `items: false` alone admits a
shorter array, which the decoder rejects. A payload case's object takes
`maxProperties: 1`, since the decoder rejects a second key there too.

### Fields

A field is `required` when it declares no default. A field with a default
takes the `default` keyword, written as `core:json` writes the value, or none
where JSON cannot hold the value, such as NaN. A bare `Option<T>` field is
required and may be `null`; an `Option<T> = null` field is optional.

A derived `Deserialize` skips a key that names no field, so the schema leaves
`additionalProperties` unset rather than `false`. A client may then send a field
that a newer server reads and this one ignores.

### Doc comments

`title` and `description` come from `///` doc comments. A definition takes its
type's comment, a property its field's, and a variant's `oneOf` branch its
case's. A property that is a `$ref` carries its field's comment beside the
`$ref`, where it wins over the definition's. When a comment has more than one
paragraph and the first is a single line, that line is the `title` and the rest
the `description`. Otherwise the whole comment is the `description`.

### From a framework

A handler's function type is enough to derive its schemas. A route registration
bounds the handler's argument and result, and inference settles both from the
handler:

```text
fn post<Req: JsonSchema + Deserialize, Res: JsonSchema + Serialize, effect E>(
    &mut self, path: String, handler: fn(Req) -> Res with E,
) { … }

app.post("/users", create_user);   // Req = CreateUser, Res = User
```

## Validating a Document

`Validator::compile` resolves every reference in a schema once, and `validate`
reports each keyword a document fails, by JSON Pointer into the document and
into the schema.

```wado
use { println } from "core:cli";
use { Document, Schema, Validator } from "wado-lang:jade";

let validator = Validator::compile(&Schema::parse(#"{"type": "integer", "minimum": 0}"#).unwrap()).unwrap();
let doc = Document::parse("-1").unwrap();
for let e of validator.validate(&doc.root()).unwrap_err() {
    println(`${e}`);   // ": must be at least 0 (/minimum)"
}
```

`validate` reports every keyword the instance fails, each error once. Under
`anyOf`, `oneOf`, `not` and `if` it reports only the keyword itself, since a
failing branch is not an error.

`compile` lays the subschemas out in one table, indexed by their `$id`, their
anchors and their JSON Pointer, and points every `$ref` at an index. A schema
without an `$id` takes an internal base URI. Two schemas that claim one URI, by
`$id` or by an anchor, are an error.

A subschema reached through several references is evaluated once per value and
per binding of the `$dynamicAnchor`s, so references do not multiply the work. A
`$ref` that comes back to a subschema at the value it left would recurse
forever; the specification leaves such a schema undefined, and `validate`
reports the reference that closes the cycle as failing.

`multipleOf` divides the decimals as written, so `0.3` is a multiple of `0.1`.
`pattern` and `patternProperties` use `wado-lang:re`, which reads ECMA-262 patterns in
Unicode mode and rejects at `compile` what it cannot match in linear time: a
backreference.

### More than one document

A remote `$ref` resolves only among the documents the caller supplies, so the
validator performs no effect of its own. A `SchemaSet` holds them, each under
the URI it was retrieved from, and `compile_in` compiles one of them:

```text
let mut set = SchemaSet::new();
set.insert(base, root);
set.insert(other_uri, other);
let validator = Validator::compile_in(&set, &base)?;   // CompileError
```

A reference takes the document retrieved from the URI it names. Failing that, it
takes the supplied document that declares the URI as its `$id`. A reference
reaches Draft 2020-12's meta-schemas without their being supplied. A reference
nothing reaches is no error. `compile` fails with `CompileError::Invalid` when a
document breaks a rule, and with `CompileError::Unresolved` when a reference
names a document nobody supplied. An error names the document by the URI it was
supplied under.

`compile_loading` fetches each missing document through a loader the caller
passes, whose effects are the caller's:

```text
let validator = Validator::compile_loading(&mut set, &base, fetch)?;   // LoadError<E>
```

It fails with `LoadError::Failed` when the loader does, and with
`LoadError::Invalid` when a document breaks a rule. The fetched documents stay
in the set, so none is fetched twice. The loader may keep a cache or wait on an
HTTP request. `SchemaSet::load` fetches the same documents without compiling.

## Generating Types

`generate` writes the Wado declarations a schema describes, as one module,
through `wado-lang:wadopoet`. The code uses `core:serde`'s derived `Serialize` and
`Deserialize`, and nothing of Jade's.

```wado
use { Schema, generate } from "wado-lang:jade";
use { ModuleSpec } from "wado-lang:wadopoet";

let schema = Schema::parse(#"{"title": "Point", "type": "object", "properties": {"x": {"type": "number"}}, "required": ["x"]}"#).unwrap();
let source = generate(&schema, &ModuleSpec::new("my-script", ["point.schema.json"])).unwrap();
```

`generate_in` does the same for a document of a `SchemaSet`, so a `$ref` into
another document declares what it reaches there in the same module.

### Through Kiln

As a Kiln generator, Jade turns an imported schema into that module. A schema
whose `$ref` names another file lists the file under `inputs`. The package names
Jade under `[build-dependencies]`, and the code needs nothing of Jade's at run
time.

```wado
use { Order } from "./order.schema.json"
    with {
        generator: {
            module: "wado-lang:jade",
            inputs: ["./address.schema.json"],
        },
    };
```

Each file is held under a `file:` URI built from its path, each segment
percent-encoded, and a remote input under its URL. In a file with no `$id`, a
relative `$ref` such as `./address.schema.json` therefore names the file beside
it. In a file with an `$id`, it resolves against the `$id`, as Draft 2020-12
says, and names an input only where one declares the resulting `$id`.

The generator takes one option, `root`, the name of the declaration the root
becomes.

### Declarations

Each `$defs` entry becomes a `pub` declaration named by its key. A root that is
only a `$ref`, as `schema_for` writes one, declares nothing more unless the
`root` option names it. Any other root takes the first name it finds: the `root`
option, then `title`, then the file's name (`create-user.schema.json` →
`CreateUser`). A struct, enum or variant written in place takes its parent's
name and its field's (`CreateUserAddress`).

A `$ref` resolves as the validator resolves it, so it names a document's root
or an entry of its `$defs`, which is what Jade declares. A reference to any
other subschema is an error. Another document's root and entries are declared
only where a reference reaches them.

### The mapping

D reads B's table backwards:

| JSON Schema                                                         | Wado                          |
| ------------------------------------------------------------------- | ----------------------------- |
| a string with an `x-wado-format` naming a `core:*` type             | the type it names             |
| `integer` bounded by exactly an integer type's range                | that type, `i8`…`u32`         |
| B's shape for `i64`, `u64`, `i128` or `u128`                        | that type                     |
| `integer` with no bounds                                            | `i64`                         |
| `number`                                                            | `f64`                         |
| `string` with `format` `uuid`, `uri` or `uri-reference`             | `Uuid`, `Uri`, `UriReference` |
| `string` with `format: "date-time"`                                 | `Instant`                     |
| `string` with `contentEncoding: "base64url"`                        | `ByteList`                    |
| any other `string`                                                  | `String`                      |
| `null`                                                              | `()`                          |
| `anyOf` of `T` and `null`, or `type: [T, "null"]`                   | `Option<T>`                   |
| `array`                                                             | `List<T>`                     |
| `array` with `uniqueItems`                                          | `HashSet<T>`                  |
| `array` with `prefixItems`, and `minItems` and `maxItems` its arity | a tuple                       |
| `object` with `additionalProperties` alone                          | `HashMap<String, V>`          |
| `object` with `properties`                                          | `struct`                      |
| `enum` of strings                                                   | `enum`                        |
| a definition of B's shape for `flags`                               | `flags`                       |
| B's shape for `Result`                                              | `Result<T, E>`                |
| B's shapes of `oneOf`, tagged or not                                | `variant`                     |
| `true`, `{}`                                                        | `core:value`'s `Value`        |
| `$ref`                                                              | the declaration it names      |

`x-wado-format` comes before `format`, and a type it names takes no assertion
beside `type`. A `format` gives a type only where that type reads every string
the format admits and writes only such strings, with `date-time` the one
exception.

An array with `uniqueItems` whose items are an `enum` of strings is a `flags`
only where it is a definition of its own, as B writes one, where its members'
wire names all come back under one `name_policy`, and where it has at most 32
members. Otherwise it is a `HashSet` of an `enum`. A definition that admits
`null` besides a struct, enum or variant written in place is the `Option` of
that type, which is declared under the definition's name followed by `Value`.

A keyword the table does not read is an error at its JSON Pointer,
`$dynamicRef` among them. An annotation is not, and neither are `$id`, `$anchor`
or `$defs`. D accepts `additionalProperties: false`, since the type never
writes another key.

### Fields and names

A property the schema does not list as `required` takes its `default` where it
has one, and is `Option<T> = null` otherwise. A default is written as a literal
where its type has one: a boolean, a number, a string, `null`, or an enum
member. Any other default is decoded from its JSON text. A default its own
schema rejects, with `format` asserted, is an error, as the field could not
hold it.

A `description` becomes the doc comment, after the `title` where there is one.
A struct, enum or variant written in place takes no doc comment: its property's
`description` is the field's.

A name in a schema is free text, such as `Create user (v2)`. D splits it into
words at each change of case and at each character that is not an ASCII letter
or digit, and drops those characters. A declaration and an enum member join the
words in PascalCase, keeping each word's case after its first letter, so
`HTTPServer` stays `HTTPServer`. A field joins them in snake_case.

A type whose wire names all come back from their Wado names under one
`name_policy` takes that policy; otherwise each name that does not come back
takes `#[wire(name = …)]`. A reserved name (`wado-lang:wadopoet`'s `is_reserved`)
takes a `_` suffix, so the property `in` becomes the field `in_`. A name that
leaves no word, that starts with a digit, or that converts to the same Wado name
as another in its scope is an error.

## Formats

A format means what Draft 2020-12 defines, by the grammar the specification
cites, and nothing looser. Jade writes no format name the specification does not
define, such as OpenAPI's `date-time-local`.
[`research-format.md`](./docs/research-format.md) records what the specifications
and implementations say about the date and time formats.

A derived schema takes a `format` only when every text its type writes meets it,
so `Uuid` takes `uuid`, while `Instant` takes none, as an expanded year falls
outside RFC 3339. It names the Wado type a string is the text of in
`x-wado-format`, written in symbol notation:

```json
{"type": "string", "x-wado-format": "core:temporal#Instant"}
```

JSON Schema reserves the `x-` prefix for annotations, so validators, Jade's
included, ignore it. D reads it to choose the type it generates.

The validator treats `format` as an annotation unless it is compiled with
`CompileOptions { assert_format: true }`. Then a string must match its format's
grammar, read strictly. Jade asserts `date-time`, `date`, `time`, `duration`,
`email`, `ipv4`, `ipv6`, `uri`, `uri-reference`, `iri`, `iri-reference`,
`uuid`, `uri-template`, `json-pointer`, `relative-json-pointer` and `regex`. A
`regex` is held to ECMA-262's syntax alone, so a backreference passes though
`pattern` rejects it. Any other format passes every string.

```text
let validator = Validator::compile(&schema, CompileOptions { assert_format: true })?;
```
