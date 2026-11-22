# Jade Design

## Four layers

"JSON Schema" names four separate capabilities. Jade keeps each one a layer of
its own, usable alone, and no layer depends on one above it:

```text
D  codegen      .json schema → .wado types        (Kiln generator)
C  validation   (Schema, document) → result
B  derivation   JsonSchema: T → Schema             (library code over Reflect*)
A  model        Schema  (written through core:serde to core:json)
```

A program that starts from its own types needs B, such as an HTTP framework that
publishes an OpenAPI document for its handlers. C and D serve a program that
starts from a schema someone else wrote. Nothing of JSON Schema is in the
compiler or the standard library.

## The specification first

Jade follows Draft 2020-12 wherever it reads or writes a schema, so that a
schema means the same thing to Jade as to every other tool. It defines no
assertion keyword, format or attribute of its own:

- A wire name comes from `#[wire(…)]`, which the decoder reads too.
- A description comes from a doc comment.
- A `format` means what Draft 2020-12 defines, read strictly. See
  [Formats](../README.md#formats).
- Its one keyword, `x-wado-format`, names a string's Wado type and is an
  annotation. See [Formats](../README.md#formats).

## What a schema promises

A derived schema describes the shape `core:json` writes, and the derived
`Deserialize` reads that shape. D reads the same table backwards. So, apart
from the cases [`TODO.md`](../TODO.md) lists, every value a type writes
validates against its schema, a document that validates decodes, and deriving
the schema of a generated type gives back the keywords D read.
