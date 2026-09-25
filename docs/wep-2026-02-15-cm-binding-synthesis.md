# WEP: TIR-Level CM Binding Synthesis

## Context

A Wado program crosses the Component Model (CM) boundary at two points: an
_import_ calls a function the host provides, and an _export_ hands a Wado
function to the host. Each crossing lowers Wado GC values to the CM flat ABI
(scalars and linear memory) and lifts CM values back into Wado types.

That logic once lived in three places: raw instruction emission in codegen,
per-type converter functions in `internal.wado`, and pattern-matching convention
structs. Each new WIT type shape needed hand-written support in all three.

The canonical operations on streams, futures and waitable sets are
[resource canonical attributes](./wep-2026-03-01-cm-resource-canonical-attrs.md).
[WASI HTTP Integration](./wep-2026-02-21-wasi-http.md) builds its handler on the
export bindings described here.

## Decision

### Bindings are TIR functions

One type-driven, recursive synthesizer generates every binding as a TIR
function. Synthesis runs after the effect check and before monomorphization. A
binding then goes through monomorphization, lowering, optimization, WIR and
codegen like any other function. So the optimizer can inline a small binding and
fold its branches, and codegen knows nothing about lifting or lowering.
`wado dump` shows the whole CM glue as TIR.

### Lifting and lowering follow the type

The synthesizer recurses into a type's structure, so no type has a converter of
its own.

| Type                                                                | Lowering                      | Lifting                             | Provider                                                     |
| ------------------------------------------------------------------- | ----------------------------- | ----------------------------------- | ------------------------------------------------------------ |
| i32, i64, f32, f64                                                  | identity / `builtin::*_store` | `builtin::*_load`                   | synthesizer inline                                           |
| bool                                                                | `value as i32`                | `i32_load8_u(addr) != 0`            | synthesizer inline                                           |
| char                                                                | `value as i32`                | `char::from_u32_unchecked`          | synthesizer inline                                           |
| String                                                              | alloc + copy → `(ptr, len)`   | copy from linear memory → GC string | `internal::cm_lower_string`, `internal::memory_to_gc_string` |
| List\<u8\>                                                          | alloc + copy → `(ptr, len)`   | recursive, as any list              | `internal::cm_lower_array_u8`                                |
| list\<T\>, option\<T\>, result\<T, E\>, map, tuple, record, variant | recursive                     | recursive                           | synthesizer generates TIR                                    |

`cm_abi.rs` computes the sizes, alignments, field offsets and flat type
sequences the Canonical ABI defines.

### What may cross the boundary

Every type in an import or export signature is checked before a binding is
built. A type the CM cannot represent is a compile error at that signature, never
an invalid component.

Some types have no CM representation in any world:

- an empty record, and the empty tuple `[]`
- `i128`, `u128`, `v128`, `f16` and `bf16`
- a recursive type, since WIT has none
- a function type, a reactive cell, and an unresolved type parameter
- a generic instance other than the CM shapes (`Option`, `Result`, `List`,
  `TreeMap`, a tuple, `Future`, `Stream`)

`()` stands only where the CM lets a type be absent: a function result, a
`Result` arm, a case payload, and a `future` or `stream` payload. Anywhere else,
a type must be there. So a `()` parameter and `Option<()>` are both errors, on
an import and on an export alike (`cm_import_user_unit_param_rejected.wado`,
`cm_export_unit_param_rejected.wado`, `cm_export_option_of_unit_rejected.wado`).

A reference crosses an import only as a borrowed resource handle
(`cm_import_user_ref_string_rejected.wado`). A `map` key is restricted as
[the CM `map` type](./wep-2026-08-25-cm-map-type.md) says.

### Import adapters

Each import the program references gets one adapter. The adapter takes
Wado-typed parameters and lowers them to flat values. It calls the lowered import
through a `CmRawCall`. A sync import's result is lifted back to a Wado value,
through an out-pointer when it flattens to more than one core value. An async
import returns an `AsyncCall<T>`, and a lift function of its own reads the
result once the call completes.

Each parameter reaches the flat call in one of four ways:

- `String` and `List<u8>` are one parameter. A stdlib helper copies the bytes
  into linear memory as `(ptr, len)`.
- Any other `List<T>` is one parameter. Its elements are lowered into a buffer
  passed as `(ptr, len)`.
- A record, variant, option, result, tuple or map is one GC value. The adapter
  flattens it itself.
- A scalar, enum, flags value or resource handle is forwarded unchanged.

An async call passes at most four flat parameters directly. More go through one
buffer.

Every call of an import is rewritten to call its adapter: a free function, a
method through a receiver, or a static method. The adapter is retyped to the
types at each call site, so a WASI-derived type takes the caller's.

A flat adapter parameter is authoritative. At a free or method call, each
argument whose core type differs from its own adapter parameter is cast to that
parameter. A sync import taking a `stream` or `future` returns its `i32` flat
result as it is. That decides the return alone, so a `u64` or `f64` beside the
stream still reaches the import whole (`cm_import_user_stream_with_wide_params.wado`).
A bare `null` passed where the adapter takes a GC value is typed as the
registry's `Option`, since the source `null` may not know its payload type.

### Export adapters

Each world export gets one binding, and the binding is what the component
exports. It lifts the flat CM parameters into Wado values, calls the user
function once, and delivers the result through one of four epilogues:

- An async export with no parameters and a unit result, such as a command's
  `run()` or a test, calls `task-return(0)`. That is the Ok discriminant of the
  `result<>` it lifts through.
- An async export returning `Result<T, E>`, such as a service's `handle()`,
  matches the result, lowers the active arm into the joined flat slots, and
  calls `task-return` once.
- An `export async fn` delivers through its own `task return` statements (see
  below). The binding only lifts and calls.
- A `--lib` export lifts synchronously. The binding returns the lowered value,
  directly when it is one core value and through an out-pointer otherwise.
  A [`post-return`](./wep-2026-07-28-cm-post-return.md) frees that area.

A parameter of any type that may cross is lifted, records, variants and results
included.

A `--lib` export can declare no result at all, as `export async fn ping()` does.
It lifts through no CM type, so it flattens to no slot and delivers with
`task-return()`, against a canon carrying no result type. Both the canon and
`canon lift` read the export's declared result, so they agree on whether one is
there.

They read it differently. `lib_task_return_valtype` takes the declared type,
while `emit_world_exports` keeps local newtypes. A `pub type Meters = i32` result
reaches the lift as the named `meters` and the canon as bare `s32`. Interning
makes those the same component type.

#### The `task-return` canon

One `canon task.return` carries one result type, so the import is keyed by the
export whose result it delivers: `task-return:handle`, `task-return:ping`. A
`--lib` world can then export `ping()` beside `label() -> String`, each with its
own signature. WIR translation types each canon from the flat arguments at its
call site, so no signature has to be agreed on ahead of the call.

The empty key is the one shared canon, for the deliveries whose result is
`result<>`: a test export, and a WASI export taking no parameters and returning
unit. That result is fixed rather than read off an export, so a test world
carrying hundreds of deliveries still needs only the one canon.

### The task entry of an `export async fn`

`task return` names the function's result, and its destination depends on who
entered the function. The CM runtime is the destination for the export binding's
entry; a Wado caller is the destination for its own call. The two cannot share
one body: `task.return` hands a resource result to the host, so the same value
cannot also be returned.

So the delivery lives in a copy. `split_task_entry` clones the user's function
as `$cm_task_entry__<name>`, `expand_task_returns_in_func` rewrites the copy's
`task return` statements into the canonical calls, and the export binding calls
the copy. DCE drops whichever copy nothing reaches.

The user's own function keeps its `task return` statements for
`reduce_unexpanded_task_returns`, which runs after export synthesis. There each
`task return value` becomes a binding into an `Option` slot, and the body ends
by returning what it bound. It traps if it reaches the end having delivered
nothing, which for the task entry is a CM protocol error either way. The
`Option` is what lets a delivery under a branch type-check without demanding a
default for the declared type. A unit result takes no slot: there is nothing to
bind, so only the operand is left behind.

No `task return` survives synthesis, and synthesis asserts it. Every phase after
it may treat one as unreachable.

Both rewrites stop at a closure boundary. A closure is a function of its own: its
`return` ends the closure, and `task return` is rejected there. So neither the
delivery nor the binding may cross into one.

The delivery point stays inside the body in both copies. A binding that waited
for the body to return before delivering would buffer a streamed response body
in full, which is the deadlock `task return` exists to avoid.

### Export validation

`validate_world_signature_compatibility` decides whether an export matches its
world. The arity has to match, and then every type has to lower to the same flat
CM values as the world's. That is the criterion the adapters read the boundary
by. So a program it rejects is one whose adapter would have read the boundary's
words against a layout that is not theirs.

Flat shapes alone are too coarse for one case: `i32` and `Result<(), ()>` both
flatten to a single `i32`. So a world declaring a `Result` needs the export to
return one as well. Unit stands in only where the world's `Ok` payload is itself
unit, which is all the `Ok(())` wrap fills. `wasi:cli/command`'s
`Result<(), ()>` takes it, and `wasi:http/service`'s
`Result<Response, ErrorCode>` does not. The rule holds for `async` exports as
well as sync ones.

An `export async fn` also has to carry a `task return`. The check sits beside
the missing-return one: both ask whether a body can produce the result its
signature promises, and both exempt a body that provably exits on every path
first.

What counts as an answer differs. Missing-return needs a `return` on every
path. This one needs a single `task return` anywhere, because delivering under a
branch is what `task return` is for. A path that misses it traps at the
boundary, and no static answer improves on that.

### `CmRawCall`

A raw CM call is a TIR node of its own:

```
TirExprKind::CmRawCall {
    local_name: "wasi:http/types/[method]fields.get",
    args: [self_handle, name_ptr, name_len, outptr],
}
```

Codegen resolves `local_name` to the imported function index. The same node
carries `task-return` and the other CM intrinsics an export binding calls.

## Roadmap

None. Every open item is a known gap below.

## Known gaps

### A list or map export parameter is lifted through linear memory

The lift of a `list` or `map` reads its `(ptr, len)` pair from linear memory. An
export parameter arrives as two flat values instead, so the binding allocates
eight bytes, stores the pair there, lifts from it and frees it. Each such
parameter costs an allocation and a free per call.
