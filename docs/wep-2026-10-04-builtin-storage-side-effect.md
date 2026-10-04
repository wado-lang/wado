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
- Neither takes new syntax. The one grammar change is that an attribute array
  may hold identifiers as well as strings.

`#[immediate(...)]` says how an argument is encoded, not what the call does. It
is not part of this design.

### `#[storage(...)]`

The first argument is one of five values:

| Value          | Meaning                                                          | Example                                        |
| -------------- | ---------------------------------------------------------------- | ---------------------------------------------- |
| `none`         | The call shares no storage and keeps none                        | `i32_and`, `i32_load`                          |
| `fresh`        | The result is new storage and holds nothing it was handed        | `array_new`                                    |
| `part_of_args` | The result is an argument's storage, or part of it               | `array_get_ref`, `select`                      |
| `holds_args`   | The result is new storage that holds what the arguments hold     | `variant_case_construct`, `array_clone_prefix` |
| `stores_args`  | The call stores what the arguments hold into the `&mut` argument | `array_set`, `array_copy`                      |

The attribute names no parameter. The types say which ones it means:

- Only a parameter whose type can carry storage counts. `select`'s `cond: bool`
  and `array_get_ref`'s `idx: i32` do not.
- The destination of `stores_args` is the `&mut` parameter.
- A by-value parameter contributes itself. A reference parameter contributes
  what it points to, so `array_copy` stores the elements of `src`.

`len = p` says the returned array holds `p` elements. It goes with `fresh` and
`holds_args`, the two values whose result is a new array.

### `#[side_effect(...)]`

The bare identifiers are:

| Identifier  | Meaning                                                          |
| ----------- | ---------------------------------------------------------------- |
| `none`      | No effect at all; it stands alone                                |
| `trap`      | The call may trap                                                |
| `read`      | The call reads linear memory                                     |
| `write`     | The call writes linear memory                                    |
| `host`      | The call reaches code the compiler cannot see                    |
| `hint`      | The call computes nothing, but its position is what it means     |
| `black_box` | The optimizer may assume nothing about the operand or the result |

A write through a `&mut` parameter is not listed, since the type states it.

`black_box` exists for `builtin::black_box` alone. A test or a benchmark uses
that call to keep the work it measures from being folded away, which no other
identifier says.

### Trap Conditions

`trap` alone means the call may trap at any time. Condition keys narrow it: with
any of them, the conditions listed are the only ones under which the call
traps. A condition key is an error without `trap`.

| Key        | Form     | Traps when                                     |
| ---------- | -------- | ---------------------------------------------- |
| `outside`  | `[a, …]` | The range does not lie within array `a`        |
| `at`       | `[p, …]` | Paired with `outside`: the range starts at `p` |
| `len`      | `p`      | Every range in `outside` has `p` elements      |
| `unset`    | `a`      | The element read from array `a` holds no value |
| `negative` | `p`      | `p` is below zero                              |

`outside` and `at` are arrays of the same length, and the i-th entries pair up.
Without `at`, every range starts at 0. Without `len`, every range has 1
element. One key per condition and arrays for the ranges let `array_copy` state
its two ranges without repeating a key. A single range is written as an array
too, so a key has one form.

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

### Core Wasm Imports

A function imported from a core `.wasm` / `.wat` asset is opaque: the compiler
sees none of its body. The declaration the compiler writes for it carries both
attributes, with values that assume the worst: `#[storage(none)]` and
`#[side_effect(read, write, trap)]`.

### Component Model Imports

A declaration carrying `#[cm(...)]` takes neither attribute: `#[cm]` implies
both, and the same values hold for every Component Model import. That one rule
is why the compiler needs no table to answer for them.

- The boundary copies every value, so the result is new storage and the call
  keeps nothing it was handed: `#[storage(fresh)]`.
- The callee is opaque and may trap: `#[side_effect(trap, host)]`.
- Lowering a value reads and writes linear memory, but no program can observe
  it, so `read` and `write` are not implied.

### Validation

Each of these is an error:

- A body-less `core:builtin` declaration missing either attribute.
- Either attribute on a function with a body, or on a `trait` or `interface`
  method requirement.
- Either attribute on a declaration carrying `#[cm(...)]`.
- A second `#[storage]` or `#[side_effect]` on one declaration.
- A repeated key, an unknown identifier or key, or `none` beside anything else.
- A name in a condition key that is not a parameter.
- `outside` and `at` arrays of different lengths.

## Roadmap

- [ ] Accept identifiers in an attribute array.
- [ ] Parse `#[storage]` and `#[side_effect]` into one record per declaration,
  and validate them as Decision says.
- [ ] Point every reader of `#[result]`, `#[retain]`, `#[trap]`,
  `#[linear_memory]`, and of host calls recognized by `#[canonical]`, at
  that record.
- [ ] Write both attributes on every `core:builtin` declaration, and remove the
  four old attributes.
- [ ] Write both attributes on the declaration of every core Wasm import.
- [ ] Derive both from `#[cm(...)]` for every Component Model import.
- [ ] Move the rules into `spec-attributes.md`, replacing the four sections.

## Known Gaps

- Whether a core Wasm import also carries `host` is not decided.
