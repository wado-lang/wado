# WEP: CBOR Standard Library (`core:cbor`)

## Context

Wado has a self-describing text format (`core:json`) but no binary interchange
format. CBOR — Concise Binary Object Representation, [RFC 8949] (STD 94,
obsoletes RFC 7049) — fills that gap. It is an IETF Internet Standard whose data
model is a strict superset of JSON's, it is self-describing (no schema needed),
it is compact, and it defines a deterministic-encoding profile suitable for
signing (COSE/CWT). The verbatim spec is vendored at
`wado-compiler/ref/rfc8949.txt` so implementation comments can cite exact
sections offline.

[RFC 8949]: https://www.rfc-editor.org/rfc/rfc8949.txt

The [serde framework](./wep-2026-02-28-serde.md) was designed from the start
"for JSON and CBOR": signed/unsigned and `f32`/`f64` are separate data-model
entries specifically so a CBOR encoder needs no runtime width probing, and
`DeserializeVariant::disc()` exists for integer-tagged variants. This WEP
realizes the CBOR half. Derived `Serialize`/`Deserialize` impls work unchanged —
a type that serializes to JSON serializes to CBOR with no source changes.

### Design goals

1. Reuse serde. No CBOR-specific compiler knowledge; `#[derive]`-equivalent
   synthesized impls drive both formats.
2. Bytes-primary I/O. CBOR is binary; the API is `ByteList`-in, `ByteList`-out,
   symmetric with the `core:json` redesign below.
3. Full RFC 8949 coverage on decode (variation-tolerant: any width, definite or
   indefinite length), and preferred serialization on encode.
4. A deterministic mode (`to_bytes_canonical`) for COSE-style signing.
5. Security by construction: bounded recursion, no allocation sized by
   untrusted input.

### Prior art

| Library             | Default encode | Canonical/deterministic                              | Bytes type                | Dynamic value                          |
| ------------------- | -------------- | ---------------------------------------------------- | ------------------------- | -------------------------------------- |
| Rust `ciborium`     | preferred      | — (separate crates)                                  | `serde_bytes` / `Vec<u8>` | `ciborium::Value` (has `Tag`, `Bytes`) |
| Rust `serde_cbor`   | preferred      | `to_vec` + canonical flag                            | `serde_bytes`             | `serde_cbor::Value`                    |
| Go `fxamacker/cbor` | options-based  | `CanonicalEncOptions`, `CTAP2`, `Core Deterministic` | native `[]byte`           | `cbor.RawMessage`, `any`               |
| Python `cbor2`      | preferred      | `canonical=True`                                     | native `bytes`            | native types + `CBORTag`               |

The two-mode split (a fast default plus an opt-in canonical encoder) is
universal. Every library distinguishes a byte string from an array of small
integers — the data-model gap serde currently has. This WEP closes both.

## Decision

### Module and public API

`core:cbor` is self-describing (structs encode as maps with text-string keys),
mirroring `core:json`. The API is bytes-only.

```wado
#![stdlib("core:cbor")]

/// Serialize to CBOR using preferred (shortest) serialization. Not guaranteed
/// to be deterministic across equal values whose maps were built in different
/// orders; use `to_bytes_canonical` when byte-exact reproducibility matters.
pub fn to_bytes<T: Serialize>(value: &T) -> Result<ByteList, SerializeError>;

/// Serialize using the Core Deterministic Encoding profile (RFC 8949 §4.2.1),
/// with the documented float caveat below.
pub fn to_bytes_canonical<T: Serialize>(value: &T) -> Result<ByteList, SerializeError>;

/// Deserialize from CBOR. `strict` (default `true`) rejects items the target
/// cannot represent; `strict = false` is only meaningful when deserializing
/// into `core:value`'s `Value`, where unrepresentable items fall to
/// `Value::Unknown` instead of erroring.
pub fn from_bytes<T: Deserialize, B: AsByteSlice>(input: B, strict: bool = true) -> Result<T, DeserializeError>;
```

There is no `to_bytes_pretty` (CBOR is binary). Diagnostic notation (RFC 8949
§8) is out of scope; if needed it becomes a separate debug helper later.

### Byte types and the `from_*`/`to_*` convention

CBOR's byte string (major type 2) has no serde data-model entry today, so
`List<u8>` serializes as an _array of integers_. Three prelude newtypes close the
gap, mirroring Rust's `[u8; N]` / `&[u8]` / `Vec<u8>`:

```wado
// in core:prelude
type ByteArray = Array<u8>;       // owned, fixed length (hashes, keys)
type ByteList  = List<u8>;        // owned, growable
type ByteSlice = ArraySlice<u8>;  // borrowed view (no copy)
```

Letting `from_*` accept any of the three without copying needs a byte-slice
conversion trait. Introducing it is deliberate: a byte sequence is a first-class
concept that deserves its own abstraction across the ecosystem, not an
incidental `List<u8>`. Wado has only `From`/`TryFrom` today (no `Into`, `AsRef`,
or slice-view trait), so this is part of the WEP's groundwork:

```wado
pub trait AsByteSlice {
    fn as_byte_slice(&self) -> ByteSlice;
}
// impl for ByteArray, ByteList, and ByteSlice (identity) — each a zero-copy view.
```

`from_*` accepts any `B: AsByteSlice`.

Convention, applied to both `core:cbor` and `core:json`:

- `from_*` accept any of the three byte types via `B: AsByteSlice`. `T` is
  normally inferred from context, so `let v: Foo = from_bytes(bytes)?` needs no
  turbofish.
- `to_*` always return an owned `ByteList`.

`Serialize` is implemented for all three (each encodes as a byte string).
`Deserialize` is implemented for `ByteList` (primary) and `ByteArray` (sized to
the decoded byte count — a fixed-length array's length is a runtime value, so
there is nothing to check at compile time); `ByteSlice` is a borrowed view and so
has no owned deserialize, exactly like Rust's `&[u8]: !DeserializeOwned`.

### serde framework changes (groundwork)

This WEP requires the following additions to `core:serde`. They are
format-agnostic and benefit every format.

#### A bytes entry in the data model

```wado
// Serializer
fn serialize_bytes(&mut self, v: ByteSlice) -> Result<(), SerializeError>;
// Deserializer
fn deserialize_bytes(&mut self) -> Result<ByteList, DeserializeError>;
```

`core:cbor` emits/reads a byte string (major type 2). `core:json` emits base64url
without padding and reads it back (RFC 8949 §6.1 advice for CBOR→JSON).

#### Visitor completion

The `Visitor` trait currently has `visit_f64` but no integer or bytes entries,
forcing large CBOR integers (> 2^53) through a lossy `f64`. This was a design
gap. Add, each with a default implementation so existing `Visitor` impls (which
only care about JSON) need no changes:

```wado
fn visit_i64(&mut self, v: i64) -> Result<Self::Value, DeserializeError>;     // default: visit_f64(v as f64)
fn visit_u64(&mut self, v: u64) -> Result<Self::Value, DeserializeError>;     // default: visit_f64(v as f64)
fn visit_i128(&mut self, v: i128) -> Result<Self::Value, DeserializeError>;   // default: visit_f64(v as f64)
fn visit_u128(&mut self, v: u128) -> Result<Self::Value, DeserializeError>;   // default: visit_f64(v as f64)
fn visit_bytes(&mut self, v: ByteList) -> Result<Self::Value, DeserializeError>;   // default: UnexpectedType
fn visit_unknown(&mut self, raw: ByteList) -> Result<Self::Value, DeserializeError>; // default: UnexpectedType
```

`visit_unknown` is only ever called by `core:cbor` (in lenient mode); JSON
represents every value it parses, so it never calls it.

#### `FieldSchema::lookup` keyed by bytes

The synthesized field lookup currently takes `&String`:

```wado
fn lookup(input: &String, start: i32, end: i32) -> Option<i32>;   // before
fn lookup(key: ByteSlice) -> Option<i32>;                          // after
```

A `String` input is wrong once deserializer input is bytes (below). The new
signature takes the text-key byte range directly. The lookup is synthesized per
type by the compiler, so the change is mechanical in the struct-deserialize
synthesizer; hand-written `Deserializer` impls (`core:json`, `core:json_nsd`,
`core:router`) pass a `ByteSlice` of their buffer.

#### Bytes-primary deserialization for `core:json`

serde crosses the I/O boundary, and that boundary is bytes (UTF-8), not
`String`. The serialize side already hands back a raw byte buffer (`to_bytes`).
The deserialize side is made symmetric:

- `JsonDeserializer.input` changes from `String` to `ByteList` (scanning is
  already byte-based via `get_byte_unchecked`, so the logic barely changes).
- `from_bytes` becomes the primary entry; there is no `from_string`.
- UTF-8 validation is localized to string-token construction: JSON structure is
  ASCII, so only string _content_ can be multi-byte. `read_json_string` builds a
  `String` from the token's byte range with checked construction, rejecting
  invalid UTF-8 as `MalformedInput`.

With bytes-based input, `FieldSchema::lookup(ByteSlice)` is satisfied identically
by JSON and CBOR — the `String`/bytes impedance mismatch that the original serde
design papered over disappears.

#### `core:json` becomes bytes-only

```wado
pub fn to_bytes<T: Serialize>(value: &T) -> Result<ByteList, SerializeError>;
pub fn to_bytes_canonical<T: Serialize>(value: &T) -> Result<ByteList, SerializeError>;   // sorted keys, RFC 8785-style
pub fn to_bytes_pretty<T: Serialize>(value: &T) -> Result<ByteList, SerializeError>;
pub fn from_bytes<T: Deserialize, B: AsByteSlice>(input: B) -> Result<T, DeserializeError>;
```

The string-returning entries (`to_string`, `to_string_pretty`, `from_string`)
are removed. `core:json` gains `to_bytes_canonical` (lexicographically sorted
keys) for parity with CBOR and for canonical JSON use cases. `core:json_nsd` and
`core:router` follow the same input/`FieldSchema` migration.

#### `core:value` replaces `core:json_value`

A single dynamic value type serves both formats; see below.

### Encoding: serde data model → CBOR

| serde call                          | CBOR encoding                                                                                                                          |
| ----------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------- |
| `serialize_i32` / `serialize_i64`   | major type 0 (≥ 0) or 1 (< 0), preferred shortest argument                                                                             |
| `serialize_u32` / `serialize_u64`   | major type 0, preferred shortest                                                                                                       |
| `serialize_i128` / `serialize_u128` | major 0/1 if it fits the u64 magnitude range; otherwise bignum tag 2 (non-negative) / 3 (negative) wrapping the big-endian byte string |
| `serialize_f32`                     | `f9`/`fa` head, IEEE-754 binary32 (additional info 26)                                                                                 |
| `serialize_f64`                     | binary64 (additional info 27)                                                                                                          |
| `serialize_bool`                    | `0xf4` (false) / `0xf5` (true)                                                                                                         |
| `serialize_null`                    | `0xf6` (null)                                                                                                                          |
| `serialize_char`                    | text string of one Unicode scalar                                                                                                      |
| `serialize_string`                  | text string, major type 3 (never escaped)                                                                                              |
| `serialize_bytes`                   | byte string, major type 2                                                                                                              |
| `begin_seq(n)` / tuple              | array, major type 4, definite length `n`                                                                                               |
| `begin_map(n)` / `begin_struct(n)`  | map, major type 5, definite length, text-string keys                                                                                   |
| unit variant                        | the variant name as a text string (external tagging)                                                                                   |
| `begin_variant`                     | single-pair map `{ name: payload }`                                                                                                    |

Notes:

- Integers are native. Unlike `core:json` (which stringifies values outside
  ±2^53 for JavaScript safety), CBOR has exact integers up to 64 bits and encodes
  them directly.
- Floats never use 16-bit form (Wado has no `f16`); see canonical caveat. NaN
  and ±Infinity are permitted (CBOR represents them natively), again unlike JSON
  which errors.
- Length is always known when serde starts a container (`begin_seq`/`begin_map`
  carry the count), so encoding always uses preferred definite-length form.

### Decoding

The decoder dispatches on the initial byte (RFC 8949 Appendix B jump table) and
is _variation-tolerant_ (RFC 8949 §4.1): it accepts any well-formed encoding,
preferred or not.

- Integers: read any argument width (0..23 inline, 1/2/4/8 trailing bytes) for
  major types 0/1; range-check into the requested target (`deserialize_i32`
  etc.), returning `Overflow` if out of range.
- Floats: binary16 → widen to `f32`; binary32 → `f32`; binary64 → `f64`.
- Text strings (major 3): UTF-8 validated — a string with invalid UTF-8 is
  well-formed but invalid (RFC 8949 §3.1), reported as `MalformedInput`.
- Byte strings (major 2): decoded to `ByteList`.
- Arrays/maps (major 4/5): both definite and indefinite length accepted.
- Tags (major 6): known tags are interpreted into the target's native form
  (e.g. bignum tag 2/3 → integer; date/time tag 0/1 → `core:temporal` type).
  An unknown tag wrapping a byte string is accepted when the target is
  `ByteList`/`ByteArray` (the content passes through); otherwise an unknown tag
  is a `strict`-mode error, or is preserved by `core:value` (below).
- Struct fields: each map key's text-byte range is matched via
  `FieldSchema::lookup(ByteSlice)`; unknown keys are skipped.
- Simple values: 20/21/22/23 → false/true/null/undefined; others are
  `strict`-mode errors or preserved by `core:value`.

Well-formedness and validity errors (RFC 8949 §5.3, Appendix F): duplicate map
keys → `DuplicateField` (§5.6); odd map item count, reserved additional info
28–30, a `break` outside an indefinite item → `MalformedInput`; premature end →
`Eof`; bytes left after the top-level item → `TrailingData`.

### Canonical encoding (`to_bytes_canonical`)

Implements RFC 8949 §4.2.1 Core Deterministic Encoding:

- Preferred (shortest) argument for every integer, length, and tag.
- Definite-length only.
- Map keys sorted by the bytewise lexicographic order of their _encoded_ form.
  `TreeMap` and struct fields iterate in insertion/declaration order, so the
  canonical encoder collects key/value encodings and sorts by encoded key before
  emitting.
- `Value::Unknown` cannot be re-canonicalized (its bytes are opaque) and is
  rejected with `SerializeError`.

#### Float caveat (intentional deviation)

RFC 8949 §4.2.1 requires the shortest float that preserves the value, which can
be binary16. Wado has no `f16`, so the canonical encoder's float ladder stops at
binary32: it emits `f32` where a fully-conformant encoder would emit `f16`.

Consequently `to_bytes_canonical` is byte-identical to a reference deterministic
encoder for integers, map ordering, and lengths, but **may differ on
float-bearing values**. For COSE/CWT signing over data containing floats, this is
a real interoperability hazard and must be documented at the call site. Emitting
`f16` as raw bits (without an `f16` type) in canonical mode only is a possible
future refinement.

### `core:value`: the unified dynamic value

`core:value` replaces `core:json_value` with a single format-agnostic `Value`
used by both `core:json` and `core:cbor`. It is a general dynamic value, so it
deliberately contains no CBOR-specific concepts (no `Tag`, no `Simple`): a single
opaque escape hatch, `Unknown`, carries anything the model cannot represent.

```wado
#![stdlib("core:value")]

pub variant Value {
    Null,
    Bool(bool),
    Int(i64),
    UInt(u64),
    Int128(i128),
    UInt128(u128),
    Float(f64),                      // binary16/32/64 all widened to f64
    String(String),
    Bytes(ByteList),                 // a byte-string *value*
    List(List<Value>),
    Object(TreeMap<String, Value>),  // string keys only, insertion order preserved
    Undefined,                       // CBOR simple 23; also present in JS/MsgPack data
    Unknown(ByteList),               // raw encoded bytes of an unrepresentable item
}
```

#### `Bytes` versus `Unknown`

These are different and must not be conflated:

- `Bytes(ByteList)` is a _semantic byte-string value_ (a hash, a blob). It is
  format-neutral and re-encodes in any format (CBOR byte string; JSON base64).
- `Unknown(ByteList)` holds the _raw encoded bytes_ of an item the value model
  could not parse (an unassigned simple value, an uninterpreted tag, reserved
  space). It is format-specific: only the originating format can splice it back
  verbatim. JSON cannot render it.

#### Tags and simple values collapse into native arms or `Unknown`

There is no `Tag` arm. Known tags are interpreted into native arms during decode
(date/time 0/1 → number/string, bignum 2/3 → `Int128`/`UInt128`, base64 hints
21–23, self-described 55799), matching RFC 8949 §6.1's advice that a CBOR→JSON
converter uses a tag's content and ignores the tag number. Genuinely unknown
tags, unassigned simple values (0–19, 32–255 — 24–31 cannot occur), and reserved
forms are either a `strict`-mode `DeserializeError` or, when `strict = false`,
preserved as `Unknown(raw_bytes)`. The cost is that an unknown tag's content is
opaque in `Value` (tags that matter are handled by typed `Deserialize` impls
instead); the benefit is that `Value` stays a clean, format-neutral model.

#### Integer decode: narrowest fit

A decoded integer goes into the smallest arm that holds it, so equal numbers
have one representation and canonical re-encoding is shortest-form:

- non-negative: `≤ i64::MAX` → `Int`; else `≤ u64::MAX` → `UInt`; else `≤
  i128::MAX` → `Int128`; else `≤ u128::MAX` → `UInt128`.
- negative: `≥ i64::MIN` → `Int`; else → `Int128` (covers CBOR's
  `-2^64 .. -2^63-1`).
- bignum (tag 2/3) within these ranges decodes to the matching arm; beyond
  `i128`/`u128` it is `strict`-mode `Overflow` or lenient `Unknown` (Wado has no
  arbitrary-precision integer).

#### Map keys

`Object` has string keys only. A CBOR map with a non-string key cannot be
represented and is a `strict`-mode `DeserializeError`; a lenient stringifying
mode (integer key → decimal string, with the collision caveat of RFC 8949 §6.1)
is possible future work, not the default.

#### `null`/`undefined` and typed targets

Both `null` and `undefined` deserialize to `Option::None`. In a non-`Option`
target, `undefined` is an `UnexpectedType` error.

### CBOR ↔ JSON via `Value` (RFC 8949 §6)

Round-tripping CBOR through `Value` and serializing it with `core:json` may hit
arms JSON cannot express: `Bytes`, `Undefined`, `Unknown`, non-finite `Float`.
The default is to error (`SerializeError`). A lossy conversion following RFC 8949
§6.1 substitution (bytes → base64 string, tag content used, `undefined`/non-finite
→ `null`) is deferred; it is additive and does not affect the fixed `core:json`
API above.

### Date/time mapping

Typed timestamps map to [`core:temporal`](./wep-2026-06-05-core-temporal.md):

- CBOR tag 1 (epoch-based, numeric) ↔ `Instant`.
- CBOR tag 0 (RFC 3339 text) ↔ `ZonedDateTime`.
- In JSON, both serialize as an RFC 3339 string.

These serde impls live in `core:cbor`/`core:json` and depend on the
`core:temporal` MVP types. Because `core:temporal` ships definitions first, the
impls are tracked as TODO here rather than blocking the format work.

### Module/source layout

The encoder/decoder live in `lib/core/cbor.wado`, with the value type in
`lib/core/value.wado` (replacing `json_value.wado`). Shared low-level helpers
(argument/length varint read/write, UTF-8 validation already in `core:json`) are
reused via `pub` functions where practical, as `core:json` and `core:json_nsd`
already share code.

### Security considerations (RFC 8949 §10)

- Bounded recursion depth for nested arrays/maps/tags, returning an error rather
  than exhausting the stack.
- Never pre-allocate a container from an untrusted declared length (a 2-byte
  header can claim a billion elements); grow incrementally as items are read.
- Reject duplicate map keys and trailing data so a decode result is unambiguous.
- Validate UTF-8 in text strings.

## Consequences

- Binary interchange with the same derived `Serialize`/`Deserialize` as JSON; no
  per-type work for users.
- The serde data model finally has a bytes entry (`ByteArray`/`ByteList`/
  `ByteSlice` + `serialize_bytes`/`deserialize_bytes`/`visit_bytes`), benefiting
  any binary format.
- Migration touches `core:json`, `core:json_nsd`, and `core:router`
  (`FieldSchema` signature, bytes-based input, bytes-only `core:json` API) plus
  the struct-deserialize synthesizer in the compiler. `core:json_value` becomes
  `core:value`.
- The `Visitor` gains lossless integer/bytes entries, removing the prior
  precision loss for large CBOR integers.
- `to_bytes_canonical` is deterministic for integers, lengths, and map order but
  not byte-identical to a reference encoder for floats (no `f16`) — a documented
  COSE caveat.
- This WEP specifies the design; `core:cbor`/`core:temporal` serde
  implementations are deferred (see TODO).

## TODO

- [x] Vendor RFC 8949 at `wado-compiler/ref/rfc8949.txt`
- [ ] prelude: `AsByteSlice` trait — new, since Wado has only `From`/`TryFrom`
      today
- [ ] prelude: `ByteArray`/`ByteList`/`ByteSlice` newtypes and their
      `AsByteSlice` and `Serialize`/`Deserialize` impls
- [ ] serde: `serialize_bytes`/`deserialize_bytes`; `visit_i64`/`u64`/`i128`/
      `u128`/`bytes`/`unknown` with defaults; `FieldSchema::lookup(ByteSlice)`
- [ ] compiler: emit the new `lookup` signature from the struct-deserialize
      synthesizer
- [ ] `core:json`/`core:json_nsd`: `ByteList` input, bytes-only API
      (`to_bytes`/`to_bytes_canonical`/`to_bytes_pretty`/`from_bytes`);
      `core:router` follows
- [ ] `core:value` (replacing `core:json_value`)
- [ ] `core:cbor` encoder (preferred serialization)
- [ ] `core:cbor` decoder (variation-tolerant, definite + indefinite)
- [ ] `to_bytes_canonical` (sorted keys, shortest forms)
- [ ] tags: bignum (2/3); date/time (0/1) via `core:temporal`
- [ ] tests: RFC 8949 Appendix A vectors, round-trip, canonical determinism,
      well-formedness/Appendix F rejection, security limits
