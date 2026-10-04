# Builtin Storage and Side-Effect Attributes

## Context

A declaration with no body, such as a `core:builtin` primitive, has no body to
read its behavior from. Its attributes are the only source. The optimizer asks
the same few questions of every such call:

- Does the result share storage with an argument, and does the call keep an
  argument somewhere?
- Can the call trap, and if only under some conditions, which?
- Does it read or write linear memory?
- Does it reach code the compiler cannot see?
- Must it stay where it is although it computes nothing?

Today four attributes answer these: `#[result]`, `#[retain]`, `#[trap]` and
`#[linear_memory]`. Each was added for one analysis and has its own vocabulary.
Silence means something different in each, and some repeat. Some facts have no
attribute at all. Host calls are recognized by `#[canonical]`, and the position
of `cold_path` has no attribute. A declaration can therefore omit a fact that a
reader then guesses.

## Decision

Two attributes replace the four: `#[storage(...)]` and `#[side_effect(...)]`.

- Every body-less declaration in `core:builtin` carries both.
- Each appears once on a declaration, and a key appears once in an attribute.
- The one change to the syntax is that an attribute array may hold identifiers
  as well as strings.

`#[immediate(...)]` says how an argument is encoded, not what the call does. It
is not part of this design.

### `#[storage(...)]`

The first argument is one of five values:

| Value          | Meaning                                                          | Example                                        |
| -------------- | ---------------------------------------------------------------- | ---------------------------------------------- |
| `none`         | The call shares no storage and keeps none                        | `i32_and`, `i32_load`                          |
| `fresh`        | The result is new storage and holds nothing it was handed        | `array_new`                                    |
| `part_of_args` | The result is an argument's storage, or part of it               | `array_get_ref`, `select`, `black_box`         |
| `holds_args`   | The result is new storage that holds what the arguments hold     | `variant_case_construct`, `array_clone_prefix` |
| `stores_args`  | The call stores what the arguments hold into the `&mut` argument | `array_set`, `array_copy`                      |

The attribute names no parameter. The types say which ones it means:

- Only a parameter whose type can carry storage counts. `select`'s `cond: bool`
  and `array_get_ref`'s `idx: i32` do not.
- The destination of `stores_args` is the one `&mut` parameter.
- A by-value parameter contributes itself. `select` hands back one of its two
  operands, so its `part_of_args` result may be part of either.
- A reference parameter contributes what it points to, so `array_copy` stores
  the elements of `src`.

`len = p` says the returned array holds `p` elements. It goes with `fresh` and
`holds_args`, the two values whose result is new storage.

### `#[side_effect(...)]`

The bare identifiers are:

| Identifier  | Meaning                                                          |
| ----------- | ---------------------------------------------------------------- |
| `none`      | No effect at all                                                 |
| `trap`      | The call may trap                                                |
| `read`      | The call reads linear memory                                     |
| `write`     | The call writes linear memory                                    |
| `host`      | The call reaches code the compiler cannot see                    |
| `hint`      | The call computes nothing, but its position is what it means     |
| `black_box` | The optimizer may assume nothing about the operand or the result |

A write through a `&mut` parameter is not listed, since the type states it.

Each identifier states one fact, and none implies another. `host` says only that
the callee is out of sight. A call that also touches linear memory lists `read`
or `write` beside it.

`none` and `black_box` each stand alone. `black_box` exists for
`builtin::black_box`, which a test or a benchmark uses to keep the work it
measures from being folded away. The optimizer never deletes, moves or merges
such a call, and never computes its result from the operand. Its storage is
still stated, `#[storage(part_of_args)]`, because where a value is copied
depends on what the result shares, not on what the optimizer may assume about
it.

### Trap Conditions

`trap` alone means the call may trap at any time. Condition keys narrow it: with
any of them, the conditions listed are the only ones under which the call
traps.

| Key        | Form     | Traps when                                     |
| ---------- | -------- | ---------------------------------------------- |
| `outside`  | `[a, …]` | The range does not lie within array `a`        |
| `at`       | `[p, …]` | Paired with `outside`: the range starts at `p` |
| `len`      | `p`      | Every range in `outside` has `p` elements      |
| `unset`    | `a`      | The element of `a` at its `at` holds no value  |
| `negative` | `p`      | `p` is below zero                              |

`outside` and `at` are arrays of the same length, and the i-th entries pair up.
Without `at`, every range starts at 0. Without `len`, every range has 1
element. One key per condition and arrays for the ranges let `array_copy` state
its two ranges without repeating a key. A single range is written as an array
too, so a key has one form.

A range lies within `a` only when its start and its length are both
non-negative and they end at or before `a`'s length, as the Wasm instructions
read them unsigned.

`outside = [a]` also says the call does not replace `a`. Running out of memory
is not a trap any condition describes.

```wado
#[storage(fresh, len = len)]
#[side_effect(trap, negative = len)]
pub fn array_new<T>(len: i32) -> Array<T>;

#[storage(part_of_args)]
#[side_effect(trap, outside = [arr], at = [idx], unset = arr)]
pub fn array_get_value<T>(arr: &Array<T>, idx: i32) -> T;

#[storage(stores_args)]
#[side_effect(trap, outside = [dst, src], at = [dst_offset, src_offset], len = len)]
pub fn array_copy<T>(dst: &mut Array<T>, dst_offset: i32, src: &Array<T>, src_offset: i32, len: i32);

#[storage(holds_args, len = len)]
#[side_effect(trap, outside = [src], len = len)]
pub fn array_clone_prefix<T>(src: &Array<T>, len: i32) -> Array<T>;

#[storage(none)]
#[side_effect(read, trap)]
pub fn i32_load(addr: i32) -> i32;
```

### Where the Facts Live

A trap condition that follows from a Wasm instruction is still written in the
attribute. The alternative is a table inside the compiler, which holds the same
facts where no reader of the declaration sees them.

### Minted Builtins

A builtin the compiler calls without a source call, such as
`array_clone_shallow`, is declared in `core:builtin` like any other and carries
both attributes. A call to a builtin with no declaration is a compiler bug,
since no other place holds its facts.

### Canonical Builtins

A `#[canonical(...)]` builtin is a body-less `core:builtin` declaration like any
other and carries both attributes. Its name says what it is imported as, not
what the call does. A canonical that hands the host a buffer to fill later, as
`stream_read` does, lists `write` beside `host`. `realloc` is an export of the
`"mem"` core module, so it carries what a core Wasm import carries.

### Core Wasm Imports

A function imported from a core `.wasm` / `.wat` asset is opaque: the compiler
sees none of its body. The declaration the compiler writes for it carries both
attributes, with values that assume the worst: `#[storage(none)]` and
`#[side_effect(read, write, trap, host)]`.

### Component Model Imports

`#[cm(...)]` implies both attributes, and the same values hold for every
Component Model import. That one rule is why the compiler needs no table to
answer for them.

The facts belong to the raw import call inside the synthesized adapter, not to
the interface operation a program calls. An operation dispatches to a handler
when one is installed, and the handler's own body states what it does.

- The adapter lowers every argument to flat scalars and linear memory, and
  lifts the result back. The raw call shares no storage: `#[storage(none)]`.
- The adapter stores the arguments before the raw call and loads the result
  after it. A program cannot observe that traffic, but the optimizer sees
  both sides of the call, so the call reads and writes linear memory. The
  callee is opaque and may trap: `#[side_effect(read, write, trap, host)]`.

### Validation

Each of these is an error. A malformed attribute is never read as some other
fact.

- A body-less `core:builtin` declaration missing either attribute.
- Either attribute on a function with a body, on a `trait` or `interface`
  method requirement, or on a declaration carrying `#[cm(...)]`.
- A second `#[storage]` or `#[side_effect]` on one declaration.
- A repeated key, or an unknown value, identifier or key.
- `none` or `black_box` beside anything else in `#[side_effect]`.
- A condition key without `trap`.
- `at` or `len` in `#[side_effect]` without `outside`, or `outside` and `at`
  arrays of different lengths.
- A name in a key that is not a parameter of the declaration.
- `unset` naming an array that is not in `outside`.
- `outside` or `unset` naming a parameter that is not an array, or `negative`
  or `len` naming one that is not an integer.
- `len` in `#[storage]` beside a value other than `fresh` or `holds_args`.
- `stores_args` on a declaration without exactly one `&mut` parameter.
- `fresh`, `part_of_args` or `holds_args` on a declaration that returns `()`.
- `part_of_args`, `holds_args` or `stores_args` on a declaration with no
  parameter that can carry storage.

## Roadmap

The steps land as one change. Validation comes first, so its errors list every
declaration still to be written.

- [ ] Accept identifiers in an attribute array, print them back in the
  formatter, and test both in `tests/format.rs`.
- [ ] Parse `#[storage]` and `#[side_effect]` into one record per declaration,
  and validate them as Decision says.
- [ ] Write both attributes on every `core:builtin` declaration, declare every
  minted builtin there, and remove the four old attributes.
- [ ] Write both attributes on the declaration of every core Wasm import, and
  derive both from `#[cm(...)]` for the raw call of every Component Model
  import.
- [ ] Point every reader of `#[result]`, `#[retain]`, `#[trap]`,
  `#[linear_memory]`, and of host calls recognized by `#[canonical]`, at
  that record. The copy planner's special case for `select` goes with it,
  since `part_of_args` over every storage parameter states it.
- [ ] Move the rules into `spec-attributes.md`, replacing the four sections,
  and update what `spec-effects.md` and `spec-memory.md` say about
  `#[retain]` and `#[result]`.

## Known Gaps

- A bundled core Wasm asset such as the libm cannot state better than the worst
  case, so a call to it is never hoisted, merged or deleted.
