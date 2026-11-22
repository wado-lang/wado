# Grog Design

The rules come from the specification, `vendor/protobuf-spec`, rather than from
an implementation. [The README](../README.md) states what Grog generates; this
file says why it is built this way.

## One Package: the Generator and Its Runtime

Nothing of protobuf is in the standard library or the compiler. The alternative
was a stdlib format over `core:serde`, with the schema's decisions carried as
field attributes. Every one of those decisions then becomes a compiler feature:
a field number, an integer encoding, implicit presence, an open enum, a oneof,
packing. `core:serde`'s shape is a struct of named fields walked in order, and
protobuf is not that shape. Generated code that calls a protobuf runtime
directly states each decision where it is made, and the compiler knows nothing
about any of them.

## A Consumer Names Grog Twice

The runtime is an ordinary dependency, not something the generator injects. A
build dependency does not reach the code it generates, and a hidden dependency
would be one the consumer cannot see, pin or audit. Writing the entry twice is
the price. Kiln tells a generator the spec it was invoked under
(`Request.module`), and the output imports the runtime by that name.

## The Decoder Reads Any Input

The decoder's `Reader<B>` reads any `B: Input`: a `ByteSlice` for `decode`,
which copies nothing, or a `&LinearArray<u8>` for the readers over a
`LinearBuffer`. So `merge_field` takes the reader's input as a type parameter.
A decode of a `ByteList` copied into linear memory first would not do: its
reader refers into a local array, and a generic message could keep that
reference, so the compiler refuses it.

## The Length Prefix Is a Size Pass

A `LEN` record writes its payload's length ahead of the payload, so an encoder
has to know how long a submessage is before it writes one. Encoding is two
passes over the same generated traversal. `size` computes every submessage's
length and pushes it to a list; `write` writes bytes and reads the lengths back
in the same order. So no message needs a field to cache its own size.

The size pass is proportional to the number of fields, not the bytes: a `bytes`
or `string` field's length is known without reading it, so counting a
hundred-megabyte tensor is one addition. The alternatives, encoding each
submessage into a scratch buffer or shifting the payload to insert a length,
are proportional to bytes times nesting depth. A deep message holding a large
leaf is the shape that punishes both, and it is the shape Loam reads: a tensor
sits five levels down an ONNX model.

Two asserts keep the passes honest: the encoding is as long as the size pass
said, and `write` reads back every length the size pass recorded.

The size pass counts in `i64`. The specification limits an encoding to under
2 GiB, and a `ByteList` holds no more than that either, so `encode` panics on a
larger message. Counting in `i32` would let the length wrap and pass the check
as a smaller number.

## Decoding Sets No Limit on What It Builds

Two bytes on the wire are an empty submessage, and each one decodes to a whole
default struct of its type, so a schema with wide messages turns a small input
into a large heap. Protobuf's own parsers behave the same way. A limit on what
one decode may build would refuse valid messages too, so Grog sets none, and
leaves the bound on the input's length to the caller.

## No Canonical Encoding

The specification is explicit that serialization is not canonical across
implementations or versions, and map field order is unspecified. Grog's order
is deterministic because a build cache or a golden test needs that.

## The JSON Mapping Is a Separate Format

Protobuf defines a JSON projection with rules of its own for names, 64-bit
integers, and the well-known types. It is a different format that happens to
share a schema, so it belongs in a module of its own if it is ever wanted.

## The Name

Grog is fired clay, ground up and mixed back into raw clay so a mould holds its
shape through the firing. A `.proto` is a schema fired elsewhere. Grog grinds it
into Wado declarations, and Kiln fires the result.
