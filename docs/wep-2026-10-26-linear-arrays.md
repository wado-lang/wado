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
    pub fn is_empty(&self) -> bool;
    pub fn at(&self, index: i32) -> T
        contract 0 <= index < self.len();
    pub fn set(&mut self, index: i32, value: T)
        contract 0 <= index < self.len();
    pub fn duplicate(&self) -> LinearArray<T>;
    pub fn to_list(&self) -> List<T>;
    pub fn cast<U: AnyBits>(self) -> LinearArray<U>
        contract self.len() * builtin::size_of::<T>() % builtin::size_of::<U>() == 0;
}

impl LinearArray<u8> {
    pub fn to_list_of<U: AnyBits>(&self, start: i32, end: i32) -> List<U>
        contract 0 <= start <= end <= self.len()
        contract (end - start) % builtin::size_of::<U>() == 0;
}
```

`to_list_of` decodes one part of a byte array, such as one tensor of a
checkpoint file, as little-endian `U`s at any alignment. A `cast` cannot: it
takes the whole array, and a part may start off an element boundary.

It implements `IndexValue<i32>` and `IndexAssign<i32>`, so `a[i]` and
`a[i] = v` read as they do on a `List`. It implements neither `IndexRef` nor
`IndexRefMut`: an element is read and written by value, and no reference points
into linear memory. An affine type implements only an `affine trait`, so
`IndexValue` is one: an affine container hands out copyable elements by value
as a `List` does. `at` is the contracted read, named as on the `Sequence`
family, where `get` returns an `Option`.

`LinearArray` is not a `Sequence`, since that family is not affine.

The block's address is private, so no API hands out a pointer. Every byte is
written before anything can read it: there is no uninitialized constructor.

Every block is aligned to 16 bytes, whatever `T` is. The free-list allocator
aligns every block that way already. A `cast` keeps the block, so the elements
of the new type are aligned too.

`cast` moves the array and reinterprets its bytes in place, little-endian, as
Wasm reads them. Moving it leaves no second view, so no element is ever seen as
two types at once. The block's `Drop` belongs to a private `Block` the array
holds, not to the array: a `Drop` type is never partially moved, and `cast`
moves the block out of the array it takes.

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
  in a loop that keeps the index in range, so a loop rarely needs more.
  `unchecked a[i]` skips it outside a build that checks contracts, as it does
  on a `List`, and is written only where the index is proven in range and the
  check is left standing. A wrong vouch here writes past the block and nothing
  traps, so the test world, which checks every contract, is where it shows.
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
- a newtype over any of these, since a newtype has no invariant of its own;
- a `flags` type, since `as` makes one of any `u32`, undeclared bits included.

The rest are left out, because some byte pattern is no value of theirs:

- `bool`, whose bytes other than 0 and 1 are no `bool`.
- `char`, where a surrogate or a number past `0x10FFFF` is no `char`.
- An `enum`, where a discriminant may be out of range.
- A struct, a variant or a reference, any of which may hold a GC reference.
- `i128` and `u128`, which are GC objects.
- `()`, which has no width.

### It crosses into an import, and nowhere else

A `LinearArray` names a block in this instance's memory, so it may not appear in
an `export` signature. A value that needs to cross there is converted with
`to_list()`.

A synchronous import reads a `list<T>` argument out of the caller's memory
during the call, and a `LinearArray<T>` already lies there in the layout the
Canonical ABI gives a `list<T>`. So such an import takes the array's own block,
with no copy:

```wado
use { ListArg } from "core:linear";

// What the bindings generator writes for `write-buffer-with-copy`.
fn write_buffer_with_copy<affine L: ListArg<Elem = u8>>(
    &self,
    buffer: &GpuBuffer,
    buffer_offset: GpuSize64,
    data: L,
    data_offset: Option<GpuSize64>,
    size: Option<GpuSize64>,
) -> Result<(), WriteBufferError>;

queue.write_buffer_with_copy(&buffer, 0, &weights, null, null);   // no copy
queue.write_buffer_with_copy(&buffer, 0, bytes, null, null);      // copied, as before
```

`ListArg` is what a `list<T>` argument is, for `T` a Component Model number
type. `List<T>`, and so `ByteList`, implement it by copying the elements into a
block for the call, as every list argument is lowered today. `&LinearArray<T>`
implements it by handing over its own block, which the borrow keeps alive for
the whole call. Both generators write such a parameter this way, the WIT
bindings of the standard library and the declarations a component import
produces, so a call site never chooses between two functions. A list literal
no longer coerces to such a parameter, since nothing tells it which `ListArg`
to be: `write_buffer_with_copy(&buffer, 0, [1, 2] as ByteList, null, null)`
names it.

The generators write it on a resource method and on a world function, but not
on an interface's operation. An operation declares no type parameters, because
a handler installed for it serves it at one type, so an operation keeps its
`List<T>`.

`ListArg::lowered` answers a `LoweredList`, whose fields only `core:*` can
write. An implementation outside it can only delegate to one of these two, so
no implementation hands the host a block it does not own or frees one it did
not allocate.

An asynchronous import is left as it is. Its call returns before the host has
read the argument, and nothing holds a borrow past the call that made it.

A result is left as it is too. Lifting a `list<T>` result into a `LinearArray`
would also save a copy, but a binding would then have to say which of the two
it answers, and no workload yet reads back enough for that to pay.

### Built-ins

The `core:linear` module needs three built-ins:

| Built-in                                          | Lowers to                      | Used by                                |
| ------------------------------------------------- | ------------------------------ | -------------------------------------- |
| `builtin::load<T: AnyBits>(addr: i32) -> T`       | the `load` of `T`'s Wasm type  | `at`, `IndexValue`, `to_list`          |
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

The stated size is a hint. A block it fills reads the next copy into a chunk of
its own, so a file as long as it says ends without growing the block. A longer
one doubles the block, which may then hold more bytes than the array uses;
`len` bounds every access, `cast` included.

## Roadmap

In order:

- [x] `AnyBits` and the four built-ins, written into the specification, each
  with an e2e fixture: the loads and stores at every `AnyBits` width, and
  the refusal of any other type argument.
- [x] `core:linear` with `LinearArray` and its tests. A fixture shows the hot
  loop of a product indexing two arrays losing every check in its fast
  version.
- [x] `fs::read_linear`.
- [x] Loam's `Cpu` backend keeps `Packed` in a `LinearArray<f32x4>` and loads
  weights through `read_linear`. `Backend` declares
  `affine type Rhs<K, N>`, so `Prepared<Cpu>` and `Model<Cpu>` become
  affine by instantiation, as `Model<WebGpuBackend>` already is. Done when
  Gemma 3's next token is measured against what it is now.

- [ ] A synchronous import's `list<T>` parameter takes any `ListArg<Elem = T>`,
  in the standard library's WIT bindings and in a component import's
  declarations. Done when Loam's WebGPU backend uploads weights from a
  `LinearArray` with no copy, and Gemma 3's load on WebGPU is measured against
  what it is now.

Gemma 3 (270M) before and after the `Cpu` item, both run by one `wado` from
`package-loam/example/gemma3-270m` with
`wado run -O2 --collector <collector> gemma3.wado`. The two figures in a cell
are the two prompts `generate.json` records.

| Collector | Measured          | `Packed` in an `Array<f32x4>` | `Packed` in a `LinearArray<f32x4>` |
| --------- | ----------------- | ----------------------------- | ---------------------------------- |
| `drc`     | ms a token        | 104.1 / 105.2                 | 80.7 / 83.6                        |
| `drc`     | ms for the prompt | 236 / 444                     | 278 / 549                          |
| `copying` | ms a token        | runs out of GC heap           | 73.4 / 74.3                        |
| `copying` | ms for the prompt | runs out of GC heap           | 171 / 315                          |

## Known gaps

- `AnyBits` and `FromLeBytes` admit nearly the same types, and nothing relates
  the two.
- A 32-bit memory addresses 4 GiB, shared with the canonical ABI's buffers, so
  the linear data of one instance stays below that. A larger model needs
  `memory64`, which nothing here considers.
- A struct of `AnyBits` fields is not `AnyBits`, so an array of records is
  several arrays.
- There is no view over part of a `LinearArray`. A kernel indexes the whole
  array with its own offset.
- Under `drc`, Gemma 3's prompt reads slower with `Packed` in a `LinearArray`
  than in an `Array`, while each token after it reads faster. Nothing explains
  it yet. A harness timing `next_token` alone on the same ids reads the
  `LinearArray` faster on the prompt too, the GC heap's initial size does not
  move it, and the model's code compiles the same under both harnesses.
- The rules in
  [References into Affine Values](./spec-affine.md#references-into-affine-values)
  are what rule out a use after free. A gap in them is a use after free here,
  where for a GC value it is only a stale read.

## Related WEPs

- [Affine Types](./wep-2026-10-17-affine-types.md)
- [Contract Checks](./wep-2026-10-06-contract-checks.md)
- [Loam](./wep-2026-09-20-loam.md)
