# WEP: Linear Arrays — Arrays Outside the GC Heap

## Context

Every Wado sequence lives in the GC heap. `Array<T>`, `List<T>` and `ByteList`
are Wasm GC arrays, and so is a model's weight matrix.

### The engine's bounds check cannot be proven away

wasmtime checks each `array.get` against the array's length. Wado cannot remove
that check: WasmGC has no unchecked access, and Cranelift neither hoists the
check out of a loop nor shares it across blocks. A bounds check that Wado
writes, it can remove. `loop_version_bce` already deletes a `contract` check
whose bound is a struct field, in a loop version guarded once.

A matrix-vector product over a 32768 × 640 `f32` matrix, read as 4-lane
vectors, runs as follows. Each row is the same product over another storage,
best of six, on an optimized `wado`:

| Storage                                   | GB/s |
| ----------------------------------------- | ---- |
| GC array                                  | 10.9 |
| GC array, `unroll_array_loops`            | 14.4 |
| Linear memory, a check per access left in | 15.4 |
| Linear memory, no check                   | 24.0 |

A check that survives costs nearly all of the gain. A check the optimizer
removes costs nothing. Gemma 3 spends 84% of a token in such products.

### A large live array taxes every collection

The copying collector copies every object that survives a collection. A model's
weights survive all of them, so each collection copies the whole model.
Nothing in linear memory is traced or copied.

### Loading copies every byte twice more than it must

`fs::read` lifts each byte from linear memory into a GC array, and
`List::from_le_bytes` then decodes each element out of it. Wasm is
little-endian, so a file of little-endian `f32`s already has the layout of an
`f32` array in linear memory. The host's copy into linear memory could be the
last one.

### Linear memory has no guard between allocations

The allocator, the canonical ABI's buffers and every linear allocation share
one memory. A read past an allocation returns another one's bytes. A write past
it corrupts the allocator or a buffer the host is about to read, and nothing
traps. A GC array cannot be overrun this way. So the design must keep every
access in bounds without the engine's help.

## Decision

### `LinearArray<T>`: an owned, fixed-length array in linear memory

`core:linear` provides `LinearArray<T: AnyBits>`. It is affine and owns one
block from the program's allocator, taken with `realloc` and given back by its
`Drop`. Its length is fixed, as an `Array<T>`'s is.

```text
pub affine struct LinearArray<T: AnyBits> { … }

impl<T: AnyBits> LinearArray<T> {
    pub fn filled(len: i32, value: T) -> LinearArray<T>
        contract 0 <= len <= i32::MAX / builtin::size_of::<T>();
    pub fn from_slice<S: AsSlice<Elem = T>>(source: &S) -> LinearArray<T>;
    pub fn len(&self) -> i32;
    pub fn get(&self, index: i32) -> T
        contract 0 <= index < self.len();
    pub fn set(&mut self, index: i32, value: T)
        contract 0 <= index < self.len();
    pub fn duplicate(&self) -> LinearArray<T>;
    pub fn to_list(&self) -> List<T>;
    pub fn cast<U: AnyBits>(self) -> LinearArray<U>
        contract self.len() * builtin::size_of::<T>() % builtin::size_of::<U>() == 0;
}
```

It implements `IndexValue<i32>` and `IndexAssign<i32>`, so `a[i]` and
`a[i] = v` read as they do on a `List`. It implements neither `IndexRef` nor
`IndexRefMut`: an element is read and written by value, and no reference points
into linear memory.

The block's address is private, so no API hands out a pointer. Every byte is
written before anything can read it: there is no uninitialized constructor.

Every block is aligned to 16 bytes, whatever `T` is. The free-list allocator
aligns every block that way already. A `cast` keeps the block, so the elements
of the new type are aligned too.

`cast` moves the array and reinterprets its bytes in place, little-endian, as
Wasm reads them. Moving it leaves no second view, so no element is ever seen as
two types at once.

A value that holds a `LinearArray` is affine, as one holding a resource is.

### Why it is safe

Each way linear memory goes wrong maps to a rule the language already enforces:

- Use after free. The block is given back where its owner drops it. A read
  through a reference after the owner moved or dropped is refused, by
  [References into Affine Values](./spec-affine.md#references-into-affine-values).
- Double free. An affine value moves and is never copied, so one block has one
  owner.
- Out of bounds. Every element access states its range as a `contract`. An
  unmarked call checks it in every build. `loop_version_bce` deletes the check
  in a loop that keeps the index in range.
- Overlap. The allocator hands out disjoint blocks.
- Reading garbage. Every constructor writes all of the elements, and `AnyBits`
  admits only types for which every bit pattern is a value.
- A size past the address space. The constructor's contract bounds
  `len * size_of::<T>()` below `i32::MAX`.

A trap does not unwind, so it runs no `Drop`, and the block leaks with the
instance. The test world's `debug` allocator never reuses a freed block and
fills it with `0xFF`, so a read that escaped these rules shows in a test.

### `AnyBits`: the types every bit pattern is a value of

`AnyBits` is a compiler-known marker that no program implements. Its name is
its contract: any bits of the type's width make a value of it. It holds:

- the fixed-width integers `i8` to `i64` and `u8` to `u64`;
- the floats `f16`, `bf16`, `f32` and `f64`;
- `v128` and the `core:simd` types;
- a newtype over any of these, since a newtype has no invariant of its own.

The rest are left out, because some byte pattern is no value of theirs:

- `bool`, whose bytes other than 0 and 1 are no `bool`.
- `char`, where a surrogate or a number past `0x10FFFF` is no `char`.
- An `enum` or a `flags`, where a discriminant may be out of range or bits
  undeclared.
- A struct, a variant or a reference, any of which may hold a GC reference.
- `i128` and `u128`, which are GC objects.
- `()`, which has no width.

### It does not cross a component boundary

A `LinearArray` names a block in this instance's memory, so it may not appear in
an `export` signature. A value that needs to cross is converted with
`to_list()`.

### Built-ins

The `core:linear` module needs three built-ins:

| Built-in                                          | Lowers to                      | Used by                                |
| ------------------------------------------------- | ------------------------------ | -------------------------------------- |
| `builtin::load<T: AnyBits>(addr: i32) -> T`       | the `load` of `T`'s Wasm type  | `get`, `IndexValue`, `to_list`         |
| `builtin::store<T: AnyBits>(addr: i32, value: T)` | the `store` of `T`'s Wasm type | `set`, `IndexAssign`, the constructors |
| `builtin::size_of<T: AnyBits>() -> i32`           | a constant                     | addresses, the contracts               |

A type narrower than its Wasm type loads and stores at its own width: an `i8`
lowers to `i32.load8_s` and `i32.store8`, and an `f16` to `i32.load16_u` and
`i32.store16`. A newtype lowers as its base does.

A fourth is general-purpose: `builtin::memory_copy(dst: i32, src: i32, len: i32)`,
Wasm's `memory.copy`, which `duplicate` uses.

These are as raw as the per-type ones (`i32_load`, `v128_load`, …), which stay
for the allocator and the canonical ABI. They take any address, so they are not
the safe API. `LinearArray` is.

### Reading a file straight into linear memory

`fs::read_linear(path) -> Result<LinearArray<u8>, FsError>` sizes its array as
`fs::read` sizes its buffer, and hands each stream copy the array's own memory
at the offset already filled. No byte is lifted into the GC heap. A file of
`f32`s then becomes a `LinearArray<f32>` by `cast`, with no element copied.

## Roadmap

1. `AnyBits` and the four built-ins, written into the specification, each with
   an e2e fixture: the loads and stores at every `AnyBits` width, and the
   refusal of any other type argument.
2. `core:linear` with `LinearArray` and its tests. A fixture shows the hot loop
   of a product indexing two arrays losing every check in its fast version.
3. `fs::read_linear`.
4. Loam's `Cpu` backend keeps `Packed` in a `LinearArray<f32x4>` and loads
   weights through `read_linear`. `Backend` declares `affine type Rhs<K, N>`, so
   `Prepared<Cpu>` and `Model<Cpu>` become affine by instantiation, as
   `Model<WebGpuBackend>` already is. Done when Gemma 3's next token is
   measured against what it is now.

## Known gaps

- Whether `unchecked a[i]` may skip the index's contract is open. On a `List`
  it may, and a wrong vouch reads a stale slot or traps on the engine's check.
  Here a wrong vouch on a write corrupts the allocator or a canonical ABI
  buffer, and nothing traps.
- `AnyBits` and `FromLeBytes` admit nearly the same types, and nothing relates
  the two.
- A 32-bit memory addresses 4 GiB, shared with the canonical ABI's buffers, so
  the linear data of one instance stays below that. A larger model needs
  `memory64`, which nothing here considers.
- A struct of `AnyBits` fields is not `AnyBits`, so an array of records is
  several arrays.
- There is no view over part of a `LinearArray`. A kernel indexes the whole
  array with its own offset.
- The rules in
  [References into Affine Values](./spec-affine.md#references-into-affine-values)
  are what rule out a use after free. A gap in them is a use after free here,
  where for a GC value it is only a stale read.

## Related WEPs

- [Affine Types](./wep-2026-10-17-affine-types.md)
- [Contract Checks](./wep-2026-10-06-contract-checks.md)
- [Loam](./wep-2026-09-20-loam.md)
