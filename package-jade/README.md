# Jade — JSON Schema for Wado

Jade reads, derives and validates JSON Schema Draft 2020-12. A package names it
under `[dependencies]` and imports it from `lib:jade`.

## Deriving a Schema

`schema_for` derives the schema of a Wado type from the shape `core:json`
writes it in.

```wado
use { schema_for } from "lib:jade";
use { to_string } from "core:json";

struct Point { x: i32, y: i32 = 0 }

let schema = to_string(&schema_for::<Point>()).unwrap();
```

## Validating a Document

`Validator::compile` resolves every reference in a schema once, and `validate`
reports each keyword a document fails, by JSON Pointer into the document and
into the schema.

```wado
use { println } from "core:cli";
use { Document, Schema, Validator } from "lib:jade";

let validator = Validator::compile(&Schema::parse(#"{"type": "integer", "minimum": 0}"#).unwrap()).unwrap();
let doc = Document::parse("-1").unwrap();
for let e of validator.validate(&doc.root()).unwrap_err() {
    println(`${e}`);   // ": must be at least 0 (/minimum)"
}
```

## Formats

Jade treats `format` strictly. A format means what Draft 2020-12 defines, by
the grammar the specification cites, and nothing looser. Jade defines no format
of its own: it writes no format name the specification does not define, such as
OpenAPI's `date-time-local`. A derived schema takes a `format` only when every
text its type writes meets it, so `Uuid` takes `uuid`, while `Instant` takes
none, as an expanded year falls outside RFC 3339.
