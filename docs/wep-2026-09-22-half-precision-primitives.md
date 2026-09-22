# WEP: Half-Precision Primitives (`f16` / `bf16`)

## Context

A machine learning checkpoint is mostly half precision, and most of the ones
published today are bf16. [Loam](./wep-2026-09-20-loam.md) reads such a
checkpoint and emits a module whose boundary tensors carry its element type, so
a graph declaring `float16` or `bfloat16` stops the build naming a type Wado
does not have. `core:cbor` wants the same type for a different reason: RFC 8949
§4.2.1 asks a canonical encoder to emit the shortest float that preserves the
value, and without an `f16` the ladder stops at binary32.

Neither caller computes in half precision. On WebGPU a `shader-f16` device
takes f16 in a storage buffer as is, any device unpacks it with
`unpack2x16float`, and bf16 is the top half of an f32, so a shift in the shader
is its whole conversion. A canonical CBOR encoder narrows, checks the round
trip, and writes bytes. Both need a type that carries bits across a boundary,
and neither needs arithmetic.

Wasm 3.0 has no half-precision type, so nothing below the compiler supplies one
either. Whatever Wado declares, the engine sees 16 bits.

Two shapes already exist for a type of this kind, and neither fits:

|                 | Declared as        | `List<T>` is           |
| --------------- | ------------------ | ---------------------- |
| `v128`          | a primitive        | a `v128` array         |
| `i128` / `u128` | a prelude `struct` | an array of references |
| wanted          | —                  | a packed 16-bit array  |

A `struct` is a GC object, so a checkpoint of a million weights becomes a
million references. A newtype over `u16` inherits `u16`'s operators, so `a + b`
compiles to an integer addition of two bit patterns. What is wanted is the
`v128` shape — a primitive the compiler knows the representation of, and one
the Component Model has no type for — with `u16` as that representation.

## Decision

### The types are storage-only primitives

`f16` and `bf16` are declared as every primitive is, in `core:prelude`, and the
compiler knows their representation as it knows `v128`'s: a `u16` in every
position. A `List<f16>` is the same packed array as a `List<u16>`, and a struct
field of either packs the same way.

They carry no arithmetic. `+`, the bitwise operators and the comparisons are
all rejected, through the same path that rejects them for `v128`.

Neither crosses a component boundary. The Component Model has no
half-precision type, and lowering one as a `u16` would put a bit pattern on
the wire under a name that says integer, which no other language's bindings
would read back as a float. An `export fn` naming either type is a compile
error, as it is for `v128`, and a WIT emission naming one is refused. Nothing
stops a component from carrying the same bytes as a `list<u16>` it has
declared as such.

`bf16` is kept rather than converted to `f16` on load. That conversion is the
one lossy direction — bf16 has f32's exponent range and f16 does not — and it
would touch every byte of a payload the loader otherwise passes through
unchanged.

### The compiler knows five things

The representation, that there is no arithmetic, which casts are rejected, the
bit reinterpretations, and that neither type crosses a component boundary.
Everything else — the widening to `f32`, `Display`, `Inspect`, the conversion
traits, and serialization — is written in Wado in the prelude, because nothing
about it needs a compiler that knows more.

Below the Wasm-shaped IR the types are `u16`. A distinction that IR cannot act
on is not one it carries, so a dump of it reads `u16`, as it does for a newtype.

### There is no `as` for either type

`f32 as u32` converts a value: `1.0` becomes `1`. A `f16 as u16` would
reinterpret bits: `1.0` becomes `15360`. One syntax cannot mean both, and the
one that reads as arithmetic must not be the one that means bits.

So neither type participates in `as` at all. The one cast that still works is
the one every type has: between a newtype and the type it wraps, which shares
a representation and converts nothing. Every other `as` naming `f16` or `bf16`
on either side is a compile error that names what to write instead — `to_bits`,
`f32::from`, `f16::from_f32`, or, between the two half types, nothing.

### Bits and values are separate faces

```wado
impl f16 {
    pub fn to_bits(&self) -> u16;
    pub fn from_bits(bits: u16) -> f16;
    pub fn from_f32(v: f32) -> f16;
}

impl From<f16> for f32 { }
impl From<f16> for f64 { }
impl TryFrom<f32> for f16 { type Error = PrecisionLoss; }
impl TryFrom<f64> for f16 { type Error = PrecisionLoss; }
```

`bf16` carries the same set.

`From` means a value-preserving conversion everywhere else in the prelude —
`From<u8> for u128` is the shape — so bit reinterpretation is not written as
`From`. An `impl From<u16> for f16` would say that `15360` becomes `1.0`, which
is the `as` confusion again in a second spelling. Bits go through `to_bits` and
`from_bits`, which is what `f32` and `f64` already spell them.

`From` widens, which is exact for both types into either float. `TryFrom`
narrows, and answers `Ok` only where the value survives the round trip, which
is the predicate a canonical encoder's float ladder asks. `from_f32` narrows
unconditionally, rounding to nearest even and saturating to an infinity, and is
a named method rather than a cast so that a lossy step is never silent.

### The bit accessors are unsigned across the family

`f32::to_bits` answers `u32`, `f64::to_bits` answers `u64`, and `from_bits`
takes the same. The half types follow that rule rather than stating an
exception to it.

The signed forms mirrored the Wasm opcode names, `i32.reinterpret_f32` and its
siblings, and almost every caller in the corpus undid the signedness on the
next token — casting a result straight back to `u32`, or casting an unsigned
bit pattern to `i64` to pass it in. A bit pattern is not a negative number, and
the half types make the wart structural: the interesting `f16` patterns are
above `0x7FFF`, so a signed parameter would refuse `f16::from_bits(0xBC00)`.

The `builtin::` layer keeps the opcode names and their signedness. It is the
Wasm-shaped layer, and the conversion lives in the one prelude wrapper above it.

### Display widens, Inspect uses exponent notation

`${x}` widens to `f32` and prints as `f32` does, so every format specifier
works unchanged. `${x:?}` prints in exponent notation, which is what a weight's
magnitude reads best in, and is what makes a tensor debuggable at all.

### Serialization goes through `f32`

`Serialize` widens to `f32`; `Deserialize` reads an `f32` and narrows with
`from_f32`. The `Serializer` and `Deserializer` traits gain no methods.

Reading must round rather than use `TryFrom`. A serialized number is decimal
text or a wider binary float, and almost none of those land exactly on a half
precision value, so an exact-or-fail read would reject ordinary documents.
Writing is exact in the other direction, so a round trip through a format that
preserves `f32` preserves the value.

### What is deliberately absent

Arithmetic, because these types carry bits.

`Eq` and `Ord`, because a comparison of bit patterns is not a comparison of
floats: it answers that `-0.0` differs from `+0.0` and that a NaN equals
itself. A comparison widens first and compares as `f32`.

Float literal coercion. `let x: f16 = 1.5;` would need the compiler to round,
which is the prelude's `from_f32` written a second time in Rust; the two would
drift. A literal value is written as its bits.

Conversion between `f16` and `bf16` in either direction, each of which loses
something the other keeps.

## Roadmap

1. The unsigned bit accessors. `f32` and `f64` answer and take `u32` / `u64`,
   and the callers that undid the signedness stop doing so. Finished when no
   caller in the corpus casts the signedness of a bit pattern.
2. The primitives. The two types, their `u16` representation, the rejection of
   arithmetic, of `as` and of a component boundary, and the bit
   reinterpretations. Finished when a `List<f16>` is the same packed array as a
   `List<u16>` and an `export fn` naming either type is refused by name.
3. The prelude. The exact widening, `to_bits` / `from_bits` / `from_f32`,
   `Display`, `Inspect`, `From` and `TryFrom`. Finished when the widening
   agrees with a reference table across zero, subnormals, the extreme normals,
   the infinities and NaN, for both types.
4. Serialization. `Serialize` and `Deserialize` for both types, in
   `core:serde`. Finished when a struct carrying a half precision field round
   trips through JSON and through CBOR.
5. The Loam element types. `float16` and `bfloat16` map to the new primitives.
   Finished when a graph whose boundary is half precision compiles and its
   tensor prints.

## Known gaps

No associated constants. `f16::NAN` and its siblings cannot be written, because
a constant initializer is a literal and these types have none.

No `FromStr`, so a half precision value cannot be parsed from text directly.

No conversion between `f16` and `bf16`.

A half precision tensor cannot be part of a component's public API, so a Loam
module that exports one has to widen it or declare its bytes. The Component
Model may gain a half precision type; until it does, there is nothing to
lower to.

`core:cbor` still stops its canonical float ladder at binary32, so
`to_bytes_canonical` may differ from a reference deterministic encoder on
float-bearing values. The type it needed now exists; the encoder does not use
it.

`core:simd` and `wasi:webgpu` know nothing of these types. There is no half
precision lane type in `v128`, and a GPU buffer of f16 is bytes on the Wado
side.

A dtype that never crosses a generated module's boundary stays out of the type
system. int4 packs two elements to a byte with a scale per block, and f8 ships
with its scales too; WGSL has a type for neither and unpacks both from `u32`
with shifts. They remain bytes in a checkpoint and a tag inside a generator.

The Wasm-shaped IR carries no half precision type, so a dump of it names `u16`
where the source named `f16`.
