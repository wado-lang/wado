# Builtin Storage and Side-Effect Attributes

## Context

A declaration with no body, such as a `core:builtin` primitive, has no body to
read its behavior from. Its attributes and its signature are the only sources.
The optimizer asks
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

- Every body-less declaration carries both, unless it carries `#[cm(...)]`.
- Each appears once on a declaration, and a key appears once in an attribute.
- The one change to the syntax is that an attribute array may hold identifiers
  as well as strings.

`#[immediate(...)]` says how an argument is encoded, not what the call does. It
is not part of this design.

### `#[storage(...)]`

The first argument is one of seven values:

| Value          | Meaning                                                           | Example                                         |
| -------------- | ----------------------------------------------------------------- | ----------------------------------------------- |
| `none`         | The call shares no storage and keeps none                         | `i32_and`, `i32_load`                           |
| `fresh`        | The result is new storage and holds nothing it was handed         | `array_new`                                     |
| `part_of_args` | The result is an argument's storage, or part of it                | `array_get_ref`, `select`, `black_box`          |
| `holds_args`   | The result is new storage that holds what the arguments hold      | `variant_case_construct`, `array_clone_shallow` |
| `copies_args`  | The result is new storage that holds copies of what the args hold | `array_clone`, `array_clone_prefix`             |
| `stores_args`  | The call stores what the arguments hold into the `&mut` argument  | `array_set`, `array_copy`                       |
| `opaque`       | The optimizer assumes nothing about what the call shares or keeps |                                                 |

`holds_args` and `copies_args` differ in what making the result reads. A
`holds_args` result holds the very objects it was handed, so a write through
one is seen through the other. A `copies_args` result holds copies made as a
value copy makes them, so making it reads everything the arguments reach. A
reference among them still names the object it named, so the result holds what
the arguments hold, as a `holds_args` one does.

The attribute names no parameter. The types say which ones it means:

- Only a parameter whose type can carry storage counts. `select`'s `cond: bool`
  and `array_get_ref`'s `idx: i32` do not. A type parameter counts, since some
  instantiation carries storage. The generic declaration is what states and
  validates the facts, so an instantiation over `i32` changes neither.
- The destination of `stores_args` is the one `&mut` parameter.
- A by-value parameter contributes itself. `select` hands back one of its two
  operands, so its `part_of_args` result may be part of either.
- A reference parameter contributes what it points to, so `array_copy` stores
  the elements of `src`.

`opaque` is for a call whose storage the other values cannot state, such as
one that keeps an argument where neither the result nor a `&mut` parameter
reaches. `opaque` in either attribute lists nothing, so it stays true whatever
the call does. A call into code the compiler cannot see is `opaque` in
`#[side_effect]`.

`len = p` says the returned array holds `p` elements. It goes with `fresh`,
`holds_args` and `copies_args`, the values whose result is new storage.

### `#[side_effect(...)]`

The bare identifiers are:

| Identifier  | Meaning                                                          |
| ----------- | ---------------------------------------------------------------- |
| `none`      | No effect beyond what the signature states                       |
| `trap`      | The call may trap                                                |
| `read`      | The call reads linear memory                                     |
| `write`     | The call writes linear memory                                    |
| `opaque`    | The optimizer assumes nothing the signature does not state       |
| `hint`      | The call computes nothing, but its position is what it means     |
| `black_box` | The optimizer may assume nothing about the operand or the result |
| `suspend`   | Other tasks may run before the call returns                      |

The signature states two facts, so no identifier lists them. A `&mut` parameter
is written through. A declaration returning `!` never returns: every call ends
in a trap. That is not `trap`, which says a call that returns may instead trap.

A write through a `&mut` parameter may reach everything the referent reaches.
When the call also states `outside = [a]` for that parameter, the write reaches
only the elements of `a` in that range.

A `hint` call is never deleted, moved or merged on its own. It goes when the
code that contains it goes, and it never keeps that code alive.

A fact the attribute leaves out is a fact the call does not have. A call with no
`trap` never traps. A wrong attribute miscompiles as wrong code does, so
validation checks the form of an attribute and never second-guesses its facts.

`none`, `opaque` and `black_box` each stand alone. `black_box` exists for
`builtin::black_box`, which a test or a benchmark uses to keep the work it
measures from being folded away. The optimizer never deletes, moves or merges
such a call, and never computes its result from the operand. Its storage is
still stated, `#[storage(part_of_args)]`, because where a value is copied
depends on what the result shares, not on what the optimizer may assume about
it.

### Suspension

`suspend` is a fact of its own, beside `opaque` or the listing identifiers. The
tasks that run while a call is suspended may read and write anything held
elsewhere, so a field read through a reference is loaded again after it, and a
parameter split into scalars is written back before it. A call without
`suspend`, `opaque` included, reaches only what its arguments reach, linear
memory and the host.

`opaque` alone used to imply suspension, and so cost those loads and
write-backs at every call into the host, though few such calls suspend. A
canonical builtin suspends only where the Canonical ABI blocks: `waitable-set.wait`,
and the cancel builtins, which Wado lowers synchronously. The copy builtins are
lowered with `async`, and hand `BLOCKED` back rather than suspend. A core Wasm
asset imports nothing but its memory, so it cannot suspend. A Component Model
import may.

The name is the Component Model's own word for a task that stops until an event
resumes it. A positive fact keeps the rule that what the attribute leaves out is
a fact the call does not have.

### Trap Conditions

`trap` alone means the call may trap at any time. Condition keys narrow it: with
any of them, the conditions listed are the only ones under which the call
traps.

| Key        | Form     | Meaning                                          |
| ---------- | -------- | ------------------------------------------------ |
| `outside`  | `[a, …]` | Traps unless the range lies within array `a`     |
| `at`       | `[p, …]` | Paired with `outside`: the range starts at `p`   |
| `count`    | `p`      | Every range in `outside` has `p` elements        |
| `unset`    | `a`      | Traps if the element of `a` at its `at` is unset |
| `negative` | `p`      | Traps if `p` is below zero                       |

`outside` and `at` are arrays of the same length, and the i-th entries pair up.
Without `at`, every range starts at 0. Without `count`, every range has 1
element. One key per condition and arrays for the ranges let `array_copy` state
its two ranges without repeating a key. A single range is written as an array
too, so a key has one form.

A range lies within `a` only when its start and its count are both
non-negative and they end at or before `a`'s length, as the Wasm instructions
read them unsigned. `outside = [a]` also says the call does not replace `a`.

A call that states `outside = [a], at = [i]` without `count` and returns a value
is an element accessor: it reaches the one element of `a` at `i`. It writes
that element when it returns `&mut`, and reads it otherwise.

The attributes state what a call does when the host behaves as specified.
Running out of memory is assumed never to happen, and no attribute states it.
The same holds for any other failure of the host: the attributes do not list
what a broken host might do.

```wado
#[storage(fresh, len = len)]
#[side_effect(trap, negative = len)]
pub fn array_new<T>(len: i32) -> Array<T>;

#[storage(part_of_args)]
#[side_effect(trap, outside = [arr], at = [idx], unset = arr)]
pub fn array_get_value<T>(arr: &Array<T>, idx: i32) -> T;

#[storage(stores_args)]
#[side_effect(trap, outside = [dst, src], at = [dst_offset, src_offset], count = len)]
pub fn array_copy<T>(dst: &mut Array<T>, dst_offset: i32, src: &Array<T>, src_offset: i32, len: i32);

#[storage(copies_args, len = len)]
#[side_effect(trap, outside = [src], count = len)]
pub fn array_clone_prefix<T>(src: &Array<T>, len: i32) -> Array<T>;

#[storage(none)]
#[side_effect(read, trap)]
pub fn i32_load(addr: i32) -> i32;
```

### Where the Facts Live

The compiler learns a builtin's storage and side effects from its attributes
and its signature. A table hardcoded inside the compiler, keyed by a builtin's
name or its module, is forbidden: it holds the same facts where no reader of the
declaration sees them. So a trap condition that follows from a Wasm instruction
is still written in the attribute.

Besides the two facts under `#[side_effect(...)]`, the signature states:

- An `Array<T>` parameter, by value or by reference, is reached as the array:
  its length and its element slots. The objects its elements reach are not
  read or written, unless the declaration is `copies_args`, which reads
  everything they reach, or `opaque` in either attribute.

A pass may still name a builtin for what it computes or means: lowering it to
an instruction, rewriting a call to it, or reading `cold_path` as a branch
hint. It never names one to learn what the call shares, keeps, reads, writes or
traps on.

This WEP states the vocabulary and the rules. The value each declaration takes
is written on the declaration, where validation and the tests check it.

### Minted Builtins

A builtin the compiler calls without a source call, such as
`array_clone_shallow`, is declared in `core:builtin` like any other and carries
both attributes. A call to a builtin with no declaration is a compiler bug,
since no other place holds its facts.

### Canonical Builtins

A `#[canonical(...)]` declaration in `core:builtin` is a body-less builtin like
any other and carries both attributes. A `#[canonical("wasm:<path>", …)]`
declaration the compiler writes for a core Wasm asset follows the next section. Its name says what it is imported as, not
what the call does.

### Core Wasm Imports

A function imported from a core `.wasm` / `.wat` asset is opaque: the compiler
sees none of its body. The declaration the compiler writes for it carries both
attributes: `#[storage(none)]`, since it exchanges only scalars, and
`#[side_effect(opaque)]`. `realloc`, an export of the `"mem"` core module,
carries the same.

### Component Model Imports

A Component Model import is declared by `#[cm(...)]` on an interface operation,
a resource method or a world function. The adapter the compiler synthesizes for
it makes a raw import call, and that raw call carries both attributes, with the
same values for every import. That one rule is why the compiler needs no table
to answer for them.

The facts belong to the raw call, never to the operation a program calls. An operation dispatches to a handler
when one is installed, and the handler's own body states what it does.

- The adapter lowers every argument to flat scalars and linear memory, and
  lifts the result back. The raw call shares no storage: `#[storage(none)]`.
- The callee is opaque and may suspend: `#[side_effect(opaque, suspend)]`. That
  covers the adapter's stores before the raw call and loads after it, which the
  optimizer sees on both sides.

### Validation

Each of these is an error. A malformed attribute is never read as some other
fact.

- A body-less declaration missing either attribute, unless it carries
  `#[cm(...)]` or is `#[unavailable]`, which is never called and so has no
  facts to state.
- Either attribute on a function with a body, on a `trait` or `interface`
  method requirement, or on a declaration carrying `#[cm(...)]`.
- A second `#[storage]` or `#[side_effect]` on one declaration.
- A repeated key, or an unknown value, identifier or key.
- `none`, `opaque` or `black_box` beside anything else in `#[side_effect]`,
  but for `opaque` beside `suspend`.
- `suspend` beside `none`, `hint` or `black_box`.
- A condition key without `trap`.
- `at` or `count` without `outside`, or `outside` and `at`
  arrays of different lengths.
- A name in a key that is not a parameter of the declaration.
- `unset` naming an array that is not in `outside`.
- `outside` or `unset` naming a parameter that is not an array, or `at`,
  `count` or `negative` naming one that is not an integer.
- `len` in `#[storage]` beside a value other than `fresh`, `holds_args` or
  `copies_args`, on a declaration that does not return an array, or naming a
  parameter that is not an integer.
- `stores_args` on a declaration without exactly one `&mut` parameter.
- `fresh`, `part_of_args`, `holds_args` or `copies_args` on a declaration that
  returns `()` or `!`.
- `part_of_args`, `holds_args`, `copies_args` or `stores_args` on a declaration
  with no parameter that can carry storage. The `&mut` parameter `stores_args`
  stores into does not count.
- `none` on a declaration whose result can carry storage.

## Roadmap

The steps land as one change. Validation comes first, so its errors list every
declaration still to be written.

- [x] Accept identifiers in an attribute array, print them back in the
  formatter, and test both in `tests/format.rs`.
- [x] Parse `#[storage]` and `#[side_effect]` into one record per declaration,
  and validate them as Decision says.
- [x] Write both attributes on every `core:builtin` declaration, declare every
  minted builtin there, and remove the four old attributes.
- [x] Write both attributes on the declaration of every core Wasm import, and
  derive both from `#[cm(...)]` for the raw call of every Component Model
  import.
- [x] Point every reader of `#[result]`, `#[retain]`, `#[trap]` and
  `#[linear_memory]` at that record, and replace each place that decides a
  builtin's storage or side effects by its name or its module:
  - [x] `mod_ref::leaf_effect`: a `#[canonical]` builtin is opaque, and a
    minted builtin falls back to may-trap and writes-shared-heap.
  - [x] `NirPackage::pure_builtin_callee_ids`: every builtin writes no field
    slot.
  - [x] `value_copy::ownership`: a builtin that hands out no storage is owned.
  - [x] `value_copy::analyze`: `select` is a projection of its operands.
  - [x] `value_copy::place`: the `array_get_*` results are part of the array.
  - [x] `nir::FunctionRef::array_element_access`: which builtins read or write
    an element.
  - [x] `heap_effect::classify_callee`: an `array_` builtin writes only its
    array.
  - [x] `dce`: `cold_path` is inert.
- [x] Read the facts the signature states (Where the Facts Live): an
  `Array<T>` parameter reaches only the array unless the declaration is
  `copies_args` or `opaque`. Declare `array_clone` and `array_clone_prefix`
  `copies_args`.
- [x] Move the rules into `spec-attributes.md`, replacing the four sections,
  and update what `spec-effects.md` and `spec-memory.md` say about
  `#[retain]` and `#[result]`.
- [x] Split `suspend` out of `opaque` (Suspension), and declare it on the
  builtins that block.

## Known gaps

- A Component Model import that passes GC references at the boundary, as
  [WEP: Migration to GC in Components](./wep-2026-03-28-gc-in-components.md)
  plans, shares storage with what it is handed. The raw call of every import
  carries `#[storage(none)]`, which is then false.
- A `&mut` parameter always counts as a write, so `array_get_ref_mut`, which
  only hands out a reference, invalidates what the optimizer knew of the array.
- A bundled core Wasm asset such as the libm is declared `opaque`, so a call to
  it is never hoisted, merged or deleted, and no field version forwards across
  it.
