# WEP: Linear Arrays — Plain Data in Linear Memory

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
vectors, runs as follows. Each row is one program with the storage changed,
best of six, on an optimized `wado`:

| Storage                                   | GB/s |
| ----------------------------------------- | ---- |
| GC array                                  | 10.9 |
| GC array, `unroll_array_loops`            | 14.4 |
| Linear memory, a check per access left in | 15.4 |
| Linear memory, no check                   | 24.0 |

A check that survives costs nearly all of the gain. A check the optimizer
removes costs nothing. Gemma 3 spends 84% of a token in such products, so its
next token waits on this table.

### A large live array taxes every collection

The copying collector copies every object that survives a collection. A model's
weights survive all of them, so each collection copies the whole model.
Nothing in linear memory is traced or copied.

### Loading copies every byte twice more than it must

`fs::read` lifts each byte from linear memory into a GC array, and
`List::from_le_bytes` then decodes each element out of it. Wasm is
little-endian, so a file of little-endian `f32`s already has the layout of
the `f32` array in linear memory. The host's copy into linear memory could be
the last one.

### Linear memory has no guard between allocations

The allocator, the canonical ABI's buffers and every linear allocation share
one memory. A read past an allocation returns another one's bytes. A write past
it corrupts the allocator or a buffer the host is about to read, and nothing
traps. A GC array cannot be overrun this way. So the design must keep every
access in bounds without the engine's help.

## Decision

### `LinearArray<T>`: an owned, fixed-length array in linear memory

`core:linear` provides `LinearArray<T: Plain>`. It is affine and owns one
allocation from the program's allocator. Its `Drop` frees it. Its length is
fixed, as an `Array<T>`'s is.

```text
pub affine struct LinearArray<T: Plain> { … }

impl<T: Plain> LinearArray<T> {
    pub fn filled(len: i32, value: T) -> LinearArray<T>
        contract 0 <= len <= i32::MAX / builtin::size_of::<T>();
    pub fn from_slice<S: AsSlice<T>>(source: &S) -> LinearArray<T>;
    pub fn len(&self) -> i32;
    pub fn get(&self, index: i32) -> T
        contract 0 <= index < self.len();
    pub fn set(&mut self, index: i32, value: T)
        contract 0 <= index < self.len();
    pub fn duplicate(&self) -> LinearArray<T>;
    pub fn to_list(&self) -> List<T>;
}
```

It also implements `IndexValue<i32>` and `IndexAssign<i32>`, so `a[i]`,
`a[i] = v` and `unchecked a[i]` read as they do on a `List`.

Its address is private, so no API hands out a pointer. Every byte is written
before anything can read it: there is no uninitialized constructor.

### Why it is safe

Each way linear memory goes wrong maps to a rule the language already enforces:

- Use after free. The array is freed where its owner drops it. A read through a
  reference after the owner moved or dropped is refused, by
  [References into Affine Values](./spec-affine.md#references-into-affine-values).
- Double free. An affine value moves and is never copied, so one allocation has
  one owner.
- Out of bounds. Every element access states its range as a `contract`. An
  unmarked call checks it in every build. `loop_version_bce` deletes the check
  in a loop that keeps the index in range.
- Overlap. The allocator hands out disjoint blocks.
- Reading garbage. Every constructor writes all of the elements, and `Plain`
  admits only types for which every bit pattern is a value.
- A size past the address space. The constructor's contract bounds
  `len * size_of::<T>()` below `i32::MAX`.

A trap does not unwind, so it runs no `Drop`, and the allocation leaks with
the instance. The test world's `debug` allocator never reuses a freed block and
fills it with `0xFF`, so a read that escaped these rules shows in a test.

### `Plain`: the element types every bit pattern belongs to

`Plain` is a compiler-known marker that no program implements. It holds the
fixed-width integers `i8` to `i64` and `u8` to `u64`, the floats `f16`, `bf16`,
`f32` and `f64`, `v128` and its `core:simd` types, and a newtype over any of
these.

The rest are left out, because a byte pattern can be no value of theirs:

- `bool`, whose bytes other than 0 and 1 are no `bool`.
- `char`, where a surrogate or a number past `0x10FFFF` is no `char`.
- An `enum` or a `flags`, where a discriminant may be out of range or bits
  undeclared.
- A struct, a variant or a reference, any of which may hold a GC reference.
- `i128` and `u128`, which are GC objects.

### `freeze`: data that lives as long as the program

A value that holds a `LinearArray` must itself be affine. That spreads to
every value holding it, as far up as a model. Weights that are loaded once and
then only read do not need an owner at all.

`LinearArray::freeze(self) -> FrozenArray<T>` gives the allocation up for
good. A `FrozenArray<T>` offers `len`, `get`, `IndexValue` and `to_list`. It
can be read but not written, it is never freed, and it is not affine. Copying
it shares the allocation, which no program can observe, because nothing writes
to it. A value holding one stays copyable.

### Neither crosses a component boundary

A `LinearArray` or a `FrozenArray` names an address in this instance's memory,
so neither type may appear in an `export` signature. A value that needs to
cross is converted with `to_list()`.

### Built-ins

The `core:linear` module needs three built-ins:

| Built-in                                        | Lowers to                      | Used by                                |
| ----------------------------------------------- | ------------------------------ | -------------------------------------- |
| `builtin::load<T: Plain>(addr: i32) -> T`       | the `load` of `T`'s Wasm type  | `get`, `IndexValue`                    |
| `builtin::store<T: Plain>(addr: i32, value: T)` | the `store` of `T`'s Wasm type | `set`, `IndexAssign`, the constructors |
| `builtin::size_of<T: Plain>() -> i32`           | a constant                     | addresses, the constructors' contracts |

A narrow type loads and stores at its own width. An `i8`, for example, lowers
to `i32.load8_s` and `i32.store8`. The per-type built-ins (`i32_load`,
`v128_load`, …) stay, for the allocator and the canonical ABI.

A fourth is general-purpose: `builtin::memory_copy(dst: i32, src: i32, len: i32)`,
Wasm's `memory.copy`, which `duplicate` uses.

The allocation is aligned to `size_of::<T>()`, at most 16.

### Reading a file straight into linear memory

`fs::read_linear(path) -> Result<LinearArray<u8>, FsError>` hands each stream
copy the array's own memory, at the offset already filled. No byte is lifted
into the GC heap.

`LinearArray<u8>::cast<U: Plain>(self) -> LinearArray<U>` then reinterprets
the bytes in place:

- Its contract requires the length to be a multiple of `size_of::<U>()`.
- It keeps the allocation, so no element is copied.
- It reads little-endian, as Wasm does, which is what `from_le_bytes` decodes
  today.

## Roadmap

1. `Plain` and the four built-ins, each with an e2e fixture: the loads and
   stores of every `Plain` width, and the refusal of a non-`Plain` type
   argument.
2. `core:linear` with `LinearArray` and its tests. A fixture shows the hot loop
   of a product indexing two arrays losing every check in its fast version.
3. `FrozenArray` and `freeze`.
4. `fs::read_linear` and `cast`.
5. Loam's `Cpu` backend keeps `Packed` in a `FrozenArray<f32x4>` and loads
   weights through `read_linear`. Done when Gemma 3's next token is measured
   against what it is now.

## Known gaps

- A 32-bit memory addresses 4 GiB, shared with the canonical ABI's buffers, so
  the linear data of one instance stays below that. A larger model needs
  `memory64`, which nothing here considers.
- A struct of `Plain` fields is not `Plain`, so an array of records is several
  arrays.
- No view over part of a `LinearArray`. A kernel indexes the whole array with
  its own offset.
- `freeze` never frees. A program that loads one model after another keeps
  every one of them.
- The rules in
  [References into Affine Values](./spec-affine.md#references-into-affine-values)
  are what rule out a use after free. A gap in them is a use after free here,
  where for a GC value it is only a stale read.

## Related WEPs

- [Affine Types](./wep-2026-10-17-affine-types.md)
- [Contract Checks](./wep-2026-10-06-contract-checks.md)
- [Loam](./wep-2026-09-20-loam.md)
