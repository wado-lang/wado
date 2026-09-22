# WEP: Half-Precision Primitives (`f16` / `bf16`)

## Context

A machine learning checkpoint is mostly half precision, and most of the ones
published today are bf16. [Loam](./wep-2026-09-20-loam.md) reads such a
checkpoint and emits a module whose input and output tensors carry its element
type, so a graph declaring `float16` or `bfloat16` stops the build naming a type
Wado does not have. `core:cbor` wants the same type for a different reason: RFC 8949
§4.2.1 asks a canonical encoder to emit the shortest float that preserves the
value, and without an `f16` the ladder stops at binary32.

Neither caller computes in half precision. On WebGPU a `shader-f16` device
takes f16 in a storage buffer as is, any device unpacks it with
`unpack2x16float`, and bf16 is the top half of an f32, so a shift in the shader
is its whole conversion. A canonical CBOR encoder narrows, checks the round
trip, and writes bytes. Both need a type that carries bits, and neither needs
arithmetic.

Wasm 3.0 has no half precision type, so nothing below the compiler supplies one
either. Whatever Wado declares, the engine sees 16 bits.

Two ways to declare such a type are already in use, and neither fits. `i128`
and `u128` are prelude structs, so a `List` of one is an array of references,
and a checkpoint of a million weights becomes a million GC objects. A newtype
over `u16` inherits `u16`'s operators, so `a + b` compiles to an integer
addition of two bit patterns.

The third way fits. `v128` is a primitive whose representation the compiler
knows, and the Component Model has no type for it. A half precision type needs
both of those properties, with `u16` as the representation, so that a `List` of
it is a packed 16-bit array.

## Decision

### The types are storage-only primitives

`f16` and `bf16` are declared as every primitive is, in `core:prelude`, and the
compiler knows their representation as it knows `v128`'s: a `u16` in every
position. A `List<f16>` is the same packed array as a `List<u16>`, and a struct
field of either packs the same way.

They carry no arithmetic today. `+` and the bitwise operators are rejected, on
the same path that rejects them for `v128`. Neither caller computes in half
precision, and Wasm has no instruction to lower an operator to. Whether Wado
grows half precision arithmetic later is open; nothing decided here forecloses
it.

The comparisons are the exception, and they are not arithmetic. `Eq`, `Ord` and
`Default` are written in `core:prelude/half.wado`, and each answers what `f32`
answers for the widened value. Widening is exact, so that is a complete rule
rather than a convention to remember: a NaN equals nothing, the two zeroes are
one value, and both readings differ from what comparing the bits would give.
Since Wasm has no half comparison either, the operator dispatches to the impl
the way a struct's does, which is the one place a primitive does so.

`bf16` is kept rather than converted to `f16` on load. That conversion is the
one lossy direction, because bf16 has f32's exponent range and f16 does not,
and it would touch every byte of a payload the loader otherwise passes through
unchanged.

### Neither type crosses a component boundary

The Component Model has no half precision type. Its `defvaltype` runs from
`bool` through the integer widths to `f32` and `f64`, and nothing narrower is
proposed. Lowering one as a `u16` would put a bit pattern on the wire under a
name that says integer, and no other language's bindings would read it back as
a float.

So an `export fn` naming either type is a compile error, as it is for `v128`.
Every `export fn` is checked, not only the one the world names: each one lands
on the component's surface. A component that wants to carry those bytes declares
a `list<u16>`.

### The representation does not wait on the FP16 proposal

Core Wasm's half precision proposal is at phase 2. It adds an `f16` SIMD lane
type, and it states that f16, like i8 and i16, is not a first class value type.
No version of it gives Wado an f16 local, parameter or global. Wasm 3.0's
packed storage types are `i8` and `i16`, and the proposal adds none, so a GC
array of half precision values is an array of `i16` whatever ships.

So `u16` in every position is settled rather than provisional, and so are the
refusal at the component boundary and the whole API above it. What the proposal
would reach is the body of the widening and of `from_f32`, one prelude function
each. Its conversions run between `f16x8` and `f32x4`, and its scalar
`f32.load_f16` reads linear memory, which is not where a `List<f16>` lives.

### What the compiler knows

The representation, that there is no arithmetic, which casts are rejected, the
bit reinterpretations, and that neither type crosses a component boundary.
Everything else is written in Wado in the prelude: the widening to `f32`,
`Display`, `Inspect`, the conversion traits, and serialization. None of it
needs a compiler that knows more.

Below the Wasm-shaped IR the types are `u16`, so a dump of that IR reads `u16`,
as it does for a newtype. The IR cannot act on the distinction, so it does not
carry it.

### There is no `as` for either type

`f32 as u32` converts a value: `1.0` becomes `1`. A `f16 as u16` would
reinterpret bits: `1.0` becomes `15360`. One syntax cannot carry both meanings,
and `as` already carries the first one everywhere else.

So neither type takes part in `as` at all. The one cast that still works is the
one every type has, between a newtype and the type it wraps, which shares a
representation and converts nothing. Every other `as` naming `f16` or `bf16` on
either side is a compile error, and the error names what to write instead:
`to_bits`, `f32::from`, `f16::from_f32`, or, between the two half types,
nothing.

### Bits go through `to_bits`, values through `From`

```wado
impl f16 {
    pub fn to_bits(&self) -> u16;
    pub fn from_bits(bits: u16) -> f16;
    pub fn from_f32(v: f32) -> f16;
}

impl From<f16> for f32 { }
impl From<f16> for f64 { }
impl TryFrom<f32> for f16 { type Error = ConvertError; }
impl TryFrom<f64> for f16 { type Error = ConvertError; }
```

`bf16` carries the same set.

Everywhere else in the prelude `From` preserves a value, as `From<u8> for u128`
does. So bit reinterpretation is not written as `From`. An
`impl From<u16> for f16` would say that `15360` becomes `1.0`, which repeats
the `as` confusion in a second spelling. Bits go through `to_bits` and
`from_bits`, the names `f32` and `f64` already use.

`From` widens, and widening either type into either float is exact. `TryFrom`
narrows, and answers `Ok` only where the value survives the round trip, which
is the question a canonical encoder's float ladder asks. `from_f32` narrows
unconditionally, rounding to nearest even and saturating to an infinity. It is
a named method rather than a cast, so a lossy step is never silent.

### The bit accessors are unsigned across the family

`f32::to_bits` answers `u32`, `f64::to_bits` answers `u64`, and `from_bits`
takes the same. The half types follow that rule instead of being an exception
to it.

The signed forms mirrored the Wasm opcode names, `i32.reinterpret_f32` and its
siblings. Almost every caller in the corpus then undid the signedness on the
next token, casting a result straight back to `u32`, or casting an unsigned bit
pattern to `i64` to pass it in. A bit pattern is not a negative number.

The half types would have to work around it too. Every negative `f16` pattern
is above `0x7FFF`, so a signed parameter would refuse
`f16::from_bits(0xBC00)`.

The `builtin::` layer keeps the opcode names and their signedness, because it
is the Wasm-shaped layer. The conversion lives in the one prelude wrapper above
it.

### Display widens, Inspect uses exponent notation

`${x}` widens to `f32` and prints as `f32` does, so every format specifier
works unchanged. `${x:?}` prints in exponent notation, which is what a weight's
magnitude reads best in. A tensor that cannot be printed cannot be debugged.

### Serialization goes through `f32`

`Serialize` widens to `f32`; `Deserialize` reads an `f32` and narrows with
`from_f32`. The `Serializer` and `Deserializer` traits gain no methods.

Reading rounds rather than using `TryFrom`. A serialized number is decimal text
or a wider binary float, and almost none of those land exactly on a half
precision value, so an exact-or-fail read would reject ordinary documents.
Writing is exact, so a round trip through a format that preserves `f32`
preserves the value.

### What is deliberately absent

Float literal coercion. `let x: f16 = 1.5;` would need the compiler to round,
which means writing the prelude's `from_f32` a second time in Rust, and the two
copies would drift. A literal value is written as its bits.

Conversion between `f16` and `bf16`. Each direction loses something the other
keeps: `f16` has the shorter exponent range, `bf16` the shorter mantissa.

## Roadmap

Nothing outstanding. What the types do not yet reach is in Known gaps, and none
of it is committed work.

## Known gaps

No arithmetic, so every computation widens to `f32` and narrows again to store.
A generic body bounded on `Add` cannot be instantiated at either type.

A comparison widens both operands and calls, where `f32`'s is one instruction.
The bits decide every case on their own — equal bits are equal values unless
both are NaN, and different bits are different values unless both are zero — so
an implementation that never widens exists. Nothing measured has asked for it.

No associated constants. `f16::NAN` and its siblings cannot be written, because
a constant initializer is a literal and these types have none.

No `FromStr`, so a half precision value cannot be parsed from text directly.

A half precision tensor cannot be part of a component's public API, so a Loam
module that exports one has to widen it or hand out its bytes.

`core:cbor` still stops its canonical float ladder at binary32, so
`to_bytes_canonical` may differ from a reference deterministic encoder on
float-bearing values. The type it needed now exists; the encoder does not use
it.

`core:simd` has no `f16x8`, so nothing widens eight weights at once. The FP16
proposal's lane operations are where that would come from, and Loam's CPU arm
is the caller that would ask for it.

A `List<f16>` cannot be viewed as bytes. `AsByteSlice` covers the byte-element
sequences and `String`, and WasmGC offers no way to read an `array i16` as an
`array i8`, so anything that takes bytes takes a copy. `wasi:webgpu` writes a
buffer from a `List<u8>`, which puts that copy between a tensor and the GPU.

WGSL's `f16` is behind the `shader-f16` feature, which requires 16-bit access
in uniform and storage buffers. Qualcomm devices do not offer that access, so
the feature is unavailable there however well the hardware computes in f16.
`unpack2x16float` is core WGSL and needs no feature.

Neither WGSL nor the FP16 proposal has a bf16 type, so widening bf16 is a shift
wherever it runs.

The narrower dtypes stay out of the type system altogether. int4 packs two
elements to a byte with a scale per block, and f8 ships with its scales too;
WGSL has a type for neither and unpacks both from `u32` with shifts. They
remain bytes in a checkpoint and a tag inside a generator.
