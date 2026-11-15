# WEP: Extern Handles — Resources the Collector Reclaims (`externref`)

## Context

The web bindings are built on one handle type: a DOM object the program may
copy freely. It is a resource declared `linearity = "unrestricted"`
([Resource Inheritance](./wep-2026-04-28-resource-inheritance.md)). Today it
crosses the component boundary as an `f64` holding a class and an index into a
table the host keeps.

### A handle is never released

The table keeps every object the program has received, for the life of the
instance. Every `Event` a dispatch creates stays, and so does every element
created and then removed. The callback registry does the same for every closure
the program registers.

Releasing by hand is not a way out. A DOM program holds handles in structs,
lists and closures, copies them, and upcasts them. An owner for each one is what
the affine discipline asks, and
[Resource Inheritance](./wep-2026-04-28-resource-inheritance.md) already found
that a handle hierarchy cannot meet it.

### Only a reference lets the collector do it

Wasm GC has no finalizers. When the guest drops a number, or a GC struct holding
one, nothing runs, so the host never learns that the number is dead. That rules
out every scheme that keeps the handle a number:

- Reference counting in the guest cannot decrement when a struct holding a
  handle is collected.
- A host table of weak references frees an object the guest still names once
  the page lets go of it.
- Releasing every handle minted during a callback when the callback returns
  needs the program to mark each one it keeps, which is releasing by hand.

A Wasm `externref` is a reference the engine's collector traces through both
heaps. A handle that is one needs no table and no release.

### The Component Model has no reference type

This is why the handle is a number today. A component value is never a
reference, `own` and `borrow` are the only handle types, and a host is reachable
only through `canon lower`. Compatibility with the Component Model and WASI is
no longer a constraint on the web bindings, so the handle may leave the
canonical ABI.

### What was measured

A hand-written component was run on wasmtime 50.0.1 under `wado`'s own engine
configuration, and after a jco 1.30.0 transpile on Node 26 (V8) and Bun 1.4.4
(JavaScriptCore, Safari's engine), against jsdom. The two JavaScript engines
agreed on every row. Its
main core module imports functions that take and return `externref`. Inside the
component a second core module satisfies those imports with functions that
trap. The component is not in the tree: the roadmap's collection test takes its
place.

| Check                                                                           | wasmtime | jco, Node and Bun |
| ------------------------------------------------------------------------------- | -------- | ----------------- |
| The component validates and instantiates                                        | passes   | passes            |
| A call to a web import the host does not replace traps                          | passes   | —                 |
| A guest GC struct is a handle: `extern.convert_any`, `ref.test`, `ref.eq`       | passes   | passes            |
| The host replaces the trapping module's functions with its own                  | —        | passes            |
| A host object crosses as an `externref`, in both directions                     | —        | passes            |
| The host calls a core export the component does not lift                        | —        | passes            |
| That export runs during an import call, and calls a `canon lower`ed sync import | —        | passes            |
| It does the same from the event loop, outside any export call                   | —        | passes            |
| Handles the guest dropped are collected                                         | —        | passes            |
| A handle held in a guest global survives, and is collected once cleared         | —        | passes            |
| A detached element and a closure that hold each other are collected             | —        | passes            |
| A guest GC struct keys a JavaScript `WeakMap`                                   | —        | passes            |

One fact decides how `==` lowers. On both engines no host value passes `ref.test eqref`
once internalized: not a DOM object, a plain object, a function or a string. A
guest struct does. The specification says the same of every engine. The JS API
turns a JavaScript object that is not an exported Wasm object into `ref.host`
(`ToWebAssemblyValue`), and the core specification types that as `ref any`, so
`ref.eq` never accepts one.

The host replaces the functions through jco's `instantiation` mode, which hands
the embedder each core module and its imports before instantiating it.

The handle table and `wait` were run on a core module alone, with no component
around it, on the same two engines:

| Check                                                                 | V8     | JavaScriptCore |
| --------------------------------------------------------------------- | ------ | -------------- |
| A list of 1000 handles crosses both ways through an exported table    | passes | passes         |
| Cleared slots keep nothing alive: every element is collected          | passes | passes         |
| A list copied into a guest GC array survives, and dies with the array | passes | passes         |
| Two promises in flight are waited on in the other order               | passes | passes         |
| The instance is re-entered while a `wait` is suspended                | passes | passes         |
| Entering through `WebAssembly.promising`, relative to an empty call   | 42     | 7              |

## Decision

### `repr = "externref"` declares an extern handle

A resource states its representation beside its linearity:

```wado
#[cm("wado-lang:web/node", linearity = "unrestricted", repr = "externref", classes = "1..=4")]
resource Node extends EventTarget { ... }
```

A resource so declared is an extern handle. The two fields are two axes.
`linearity` says whether a handle may be copied, which the compiler enforces.
`repr` says what the handle is in Wasm, which decides how it crosses a boundary.
Its value is the Wasm type's own name.
Everything below follows from `repr`, and nothing from `linearity`.

- `repr = "externref"` requires `linearity = "unrestricted"`. The collector
  reclaims an extern handle, so there is no drop for an owner to run.
- `extends` and `classes` require `repr = "externref"`, where they required
  `linearity = "unrestricted"` before. One Wasm value for a whole tree is what
  makes an upcast free.
- A resource without `repr` is represented as the Component Model represents
  it, which is an owned handle. `linearity = "unrestricted"` alone therefore
  names no representation and is an error, until a copyable canonical ABI
  token gives it one (see [Known gaps](#known-gaps)).

### An extern handle is an `externref`

An extern handle is one Wasm value, `(ref extern)`. Every type in an `extends`
tree is that one value, so an upcast and an inherited method's receiver stay
no-ops. `Option<R>` of an extern handle type `R` is `(ref null extern)`, with
`None` as the null reference: the niche every `Option` of a reference takes.

The handle is no longer a number. `as` converts a handle only to a resource it
extends, and nothing converts one to or from `f64`.

A handle is either the host's object or backed by a value of the guest (below). The
program cannot tell which, and no rule below depends on which host is running.

### The web imports leave the canonical ABI

An import whose signature holds an extern handle is a core function
import of the main module. Its flat signature is the canonical ABI's, with two
differences: a handle is one `externref`, and a closure is one `externref`
(below). A string or a list of scalars still crosses as a pointer and a length
in linear memory, and a result that does not fit in one value still returns
through a pointer. The host reads the memory and calls `cabi_realloc` through
the core instance, as the canonical ABI's lowering did for it before.

These imports are not component imports. The emitted WIT does not list them.

### A handle in memory is a slot in the handle table

Linear memory cannot hold a reference, and the canonical ABI puts a value there
in three cases: a list's elements, parameters past 16 flat values, and a result
past one. A handle in any of them is written as an `i32`, the index of a slot
in a table the guest keeps. A handle inside a record or a variant that stays
flat is that index too, so one type has one flat form.

- The side that sends writes each handle into a slot and the index into memory.
  The side that receives reads the slot.
- The guest allocates slots, as it allocates the memory beside them, and clears
  them once it has read or the call has returned. A cleared slot holds nothing
  alive.
- The table is a list in `core:rt`, not a Wasm table. The host reaches it
  through two exports of the main module: one puts an object in a free slot
  and answers its index, the other answers the object in a slot. A Wasm table
  the host reads and writes itself was the first design. It needs table
  declarations and `table.grow` in the compiler's IR, and it leaves allocation
  to an export all the same, for nothing a program can observe.
- Slots are not freed in the order they were taken: a call suspended in `wait`
  (below) keeps its slots while another call runs.

`List<Node>` therefore crosses both ways with no host call per element, and so
does a record with a handle in it, alone or in a list. No shape is left without
a form, and the flat limits stay the canonical ABI's.

Returning a handle beside other values as several Wasm results was the
alternative for results. It needs the limits lifted for these imports alone,
and it leaves a list of handles unanswered.

### One component runs on every host

The component satisfies the web imports itself, from a core module the compiler
synthesizes whose every function is `unreachable`. The result is a valid
component with no web import at the component level.

- On wasmtime the component runs as it is. A web call no handler answers traps
  when it is made, which is what `wado run`, `wado test` and `wado serve`
  already promise.
- In a browser, the loader replaces that module's functions with the glue
  while it instantiates the main module.

One artifact serves both, so the program a server renders is the one the
browser runs.

### A guest value backs a handle

A handler such as `SurfaceDom` answers web calls in Wado, so it must make
handles. Every extern handle type has two functions for it:

```wado
let node: Node = HtmlInputElement::backed_by(&input);
let back = node.backing::<SurfaceInput>();   // Option<&mut SurfaceInput>
```

- `R::backed_by(&value)` makes a handle of type `R` that the value stands
  behind. The handle's class is `R`'s own, so no class number is written, and
  the result upcasts like any `R`.
- `handle.backing::<T>()` gives the value back. It is `None` for the host's
  object, and for a handle backed by a value of another type.

A backed handle is a GC struct holding the class and a reference to the value,
converted with `extern.convert_any`. A node of `SurfaceDom` is then collected
like any other value, and the handler keeps no table either.

Calling `backed_by` on one value any number of times gives the same handle to
the program: `==` compares the backing values, not the structs around them. A
handler therefore has no duty to remember the handle it gave out, which it
could only break silently.

The backing type is bound by `RefMut`: a struct, a tuple, a list or a string,
which a reference reaches in place. A reference to any other value is a cell
holding a copy. A cell carries neither the value's identity nor its type: the
cells of two extern handle types are one struct, and so are those of two
function types, so `backing::<Element>()` would answer a handle a `Node` backs.

Any code may call either function. A backed handle that reaches a real host is
no object of that host's, and the host rejects it as it would any wrong value.

The two functions are the trait `Backing`, implemented once for every type the
compiler states `ExternHandle` of. A bound `R: ExternHandle` therefore offers
both.

### Narrowing asks for the class once

The class numbering of
[Resource Inheritance](./wep-2026-04-28-resource-inheritance.md#host-runtime-contract)
stays: each resource declares `classes = "lo..=hi"`, and a narrowing is a range
test on a class. What changes is where the class comes from.

- A backed handle carries its class in the struct, so the guest reads it.
- A host object carries none. The guest calls the import `class-of`, which
  answers the class of the nearest ancestor the slice declares.

A `match` with any number of type-pattern arms reads the class once. Narrowing
the host's object costs one call where it cost none. One import per target type
was the alternative: it needs no numbering, but a `match` then calls once per
arm.

The slice will grow to the whole web platform, well past a thousand classes, so
the cost of `class-of` must not grow with the slice. The glue keeps a map from
each declared interface's prototype to its class, and walks the object's
prototype chain until the map answers. The walk is as long as the object's
inheritance chain. It finds the nearest declared ancestor of an object whose own
interface the slice leaves out, as the `instanceof` scan of today's glue does.

Both were run over jsdom objects with the slice padded to 1507 classes, the
real ones last in the scan's order:

| `class-of` by                    | Relative time, V8 | Relative time, JavaScriptCore |
| -------------------------------- | ----------------- | ----------------------------- |
| `instanceof` scan, deepest first | 773               | 1884                          |
| Prototype chain into a map       | 1                 | 1                             |

They agreed on every object tried, an `HTMLDivElement` and a `Text` outside the
slice included.

`Inspect` reads the class the same way and writes the name of the resource that
owns it. There is no index to write beside it.

### `==` is identity

Two handles are equal when they are one object. The guest decides in this
order:

1. Two null references are equal, and a null differs from anything else. This
   is `None == None` for an `Option<R>`, and it calls no import.
2. Two backed handles are equal when `ref.eq` holds of their backing values.
3. Two handles neither of which is backed are the host's objects. The guest
   calls the import `same`, which answers JavaScript's `===`.
4. A backed handle and one that is not differ.

A handle is backed exactly when it passes `ref.test` for the backing struct's
type, which no host object does. Under a handler every handle is backed, so
`same` is never called where only a trap would answer it.

### A closure crosses as itself

A closure parameter of a web import crosses as an `externref` of the closure's
own GC struct. The registry in `core:rt` and its `u32` keys go away.

- The glue keeps one JavaScript function per closure in a `WeakMap` keyed by
  the closure, so `removeEventListener` receives the function
  `addEventListener` did.
- The function calls the closure back through a core export of the main module,
  one per argument shape. The component does not lift these exports, and the
  `wado:callback/callback` interface goes away.
- A listener the page drops takes its closure with it. A closure and an element
  that hold each other are collected together.

### A member returning a `Promise<T>` returns an `AsyncCall`

A WebIDL member that returns `Promise<T>` is declared as any async operation
is, `async fn … -> AsyncCall<Result<T, Rejection>>`. The program meets no new
type:

```wado
let call = window.fetch("/api");   // AsyncCall<Result<Response, Rejection>>
let response = call.wait()?;       // blocks until the promise settles
```

An `AsyncCall` then holds one of two things: the Component Model subtask it has
always held, or an extern handle to a JavaScript promise.

- The import returns the promise as one `externref`, whatever `T` is. The host
  has read every argument by then, so they cross as a synchronous call's do.
- `wait` on a promise blocks the current task until it settles. It calls the
  import `promise-wait(promise, buffer)`, which the glue wraps in
  `WebAssembly.Suspending`, so the Wasm stack is suspended and the page keeps
  running. The host writes the result at `buffer` in the canonical ABI's
  layout, and `wait` lifts it from there as it lifts a subtask's.
- A rejection is an `Err`, not a trap. A failed `fetch` or a refused permission
  is an ordinary outcome, unlike the DOM exceptions of a synchronous call.
  `Rejection` is an extern handle to the reason, answering `name()` and
  `message()`.
- `cancel`, and dropping the call unawaited, stop the waiting and nothing else.
  A promise cannot be cancelled, so the work behind it goes on.
- The call stays affine, as every `AsyncCall` is.
- A handler answers the operation with a call of its own making, so
  `SurfaceDom` serves these members as it serves the rest.

A real Component Model async import was the alternative, and it would need no
second kind of call. It was run under jco 1.30.0 with a listener that waits on
a `wasi:clocks` timer:

| The page calls the listener through        | The listener's `wait`           | Its body starts during the dispatch |
| ------------------------------------------ | ------------------------------- | ----------------------------------- |
| A synchronously lifted export, as today    | fails: the stack cannot suspend | yes                                 |
| The same export under jco's `asyncExports` | works, two at once included     | no                                  |
| The core export, with no lift              | fails: no task is current       | yes                                 |

A wait on a subtask needs a Component Model task, and jco starts a task's body
after the dispatch has returned. A listener that starts late cannot
`prevent_default` or `stop_propagation`, whether it waits or not. The promise
keeps the body inside the dispatch, at the cost of a second kind of call.

The late start is jco's, not the Component Model's. `canon_lift` resumes the
new task's thread before it returns to the caller, so the body runs up to its
first block inside the call. It defers the start only under backpressure, which
includes a call that arrives while another task of the instance holds its one
stack: a listener the program itself fires with `dispatch_event`.

### Every callback may wait

A stack can be suspended only when JavaScript entered it through
`WebAssembly.promising`. The glue enters every callback that way, so `wait`
works in a listener as it does in `run`.

A listener that waits returns to the page at its first `wait` and resumes
later. What it does after that comes too late for the event:
`prevent_default` after a `wait` has no effect, as in a JavaScript `async`
listener.

Whether a closure can reach `wait` is a static property of that closure, so the
entry stays free to differ per closure without a change to any program.

### A handle is not hashable yet

A handle implements no `Hash` and no `Ord`, so it keys no map. A host object has
no number of its own, and giving it one is the last step of the roadmap.

## Roadmap

Each step lands with `SurfaceDom`'s tests passing. The jsdom run fails from the
representation step until the loader step, since an `externref` does not pass
through the canonical ABI the present glue is called through. Closures come
before the glue: a listener is how the jsdom run reaches the program, so the
glue cannot be run without them.

- [x] `repr = "externref"` on `#[cm]`, validated beside `linearity`, `extends` and
  `classes`, and declared by `wado-from-idl` on every resource of the slice
- [x] The representation: an extern handle is `(ref extern)` in the guest,
  `Option<R>` is the nullable reference, and the `f64` cast is gone
- [x] Backed handles: `backed_by` and `backing` on every extern handle type,
  narrowing and `Inspect` reading a backed handle's class, and `==` by `ref.eq`
  of the backing values
- [x] `SurfaceDom` backs its handles with its own nodes and keeps no table, and
  `wado-from-idl` stops emitting the class globals
- [x] The boundary: a web import is a core import, the synthesized trapping
  module satisfies it, and the component validates and runs on wasmtime. An
  extern handle in an `export fn` signature is an error
- [x] `class-of` and `same` are imports, which narrowing, `Inspect` and `==`
  call for a host object
- [x] Closures cross as `externref`, called back through unlifted core exports,
  and the registry and `wado:callback/callback` are removed
- [x] The glue: `wado-from-idl` generates shims over core signatures, reading
  strings from memory, with no handle table, and keeps one function per closure
- [x] The loader: the jco transpile runs in `instantiation` mode and replaces
  the trapping module's functions with the glue. The programs that ran against
  a wasmtime host stub run against jsdom instead (`package-web/glue/probe`),
  since a Component Model host can neither supply a web import nor call an
  unlifted export: a two-level `extends`, narrowing by the host's class, `==`
  as the host interned, a closure called back after `run` returned, and one
  entered while `run` is suspended
- [x] The handle table: a handle in linear memory is a slot index, and
  `sequence<T>` and `FrozenArray<T>` of an interface lower to `List<T>`
- [x] An `AsyncCall` over a promise, and `Rejection`: `wait` through a
  suspending import, every callback entered through `WebAssembly.promising`,
  and `SurfaceDom` answering a `Promise<T>` member
- [x] A collection test in the jsdom run: a dropped handle and a cycle through
  both heaps, a listener's closure in it, are collected, and a handle a global
  holds is not until the global lets go
- [ ] `Hash` for a handle

## Known gaps

- No browser was measured, only V8 in Node and JavaScriptCore in Bun, both
  against jsdom. SpiderMonkey was not run at all. Collection across the two
  heaps with a browser's own DOM objects is unseen.
- Every callback pays for the `WebAssembly.promising` entry, whether it waits
  or not. Its cost was measured against an empty call only (below), not in a
  program.
- A callback that traps rejects the promise its `WebAssembly.promising` entry
  returns, which nothing handles. The DOM would report an exception thrown by a
  listener; an unhandled rejection reaches `unhandledrejection` instead.
- A rejection's reason that is `null` or `undefined` crosses as a new `Error`
  naming it, since no handle is null.
- An `AsyncCall` over a promise is no member of a `WaitableSet`, which waits on
  what the Component Model reports, and inserting one traps. Calls started
  together run together, and
  waiting on each in turn takes as long as the slowest. What is missing is the
  first of several to settle: a race of two `fetch`es, or a `fetch` against a
  `wasi:clocks` timer.
- A listener runs outside any Component Model task. A synchronously lowered
  import works from there. A wait on a Component Model async import, such as a
  `wasi:clocks` timer, does not: only a promise is waited on in a listener.
- A `Promise<T>` that is not a member's result has no form: one in a
  dictionary, a list or a callback's argument.
- A callback that resumes after a `wait` runs with the handlers installed at
  that moment, not those of its registration.
- `class-of` costs a call for each narrowing of a host object. It was not
  measured against the range test it replaces.
- A `match` reads the class once for each type-pattern arm, not once for the
  `match`.
- A scalar, a variant, a closure and an extern handle back no handle. A backed
  handle that carried its backing's type beside the class would admit them, at
  a word for each handle; nothing asked for one yet.
- A reference to an extern handle is a cell holding it, so a `&self` method on
  one allocates wherever the call is not inlined. The cost was not measured.
- A `&mut` to a place holding a handle asks `same` of the host before storing
  back, once for each call that takes one.
- The glue generator writes no shim for a member whose parameters are passed
  through memory, and stops on one. The slice has none.
- A list's element in the bindings is a scalar, a string or a handle. A
  `sequence` of anything else is skipped.
- Nothing measures the slots: a program that leaked one would keep its object
  alive unnoticed.
- The slots are a list in `core:rt` behind two exports, chosen for what it
  spares the compiler. A Wasm table of `externref` that the host reads and
  writes itself was not built, and neither was timed: what a list of handles
  costs per element, each way, is unknown.
- `class-of` reads prototypes of the glue's own realm. An object from another
  realm, such as an element of an `iframe`'s document, has other prototypes and
  matches none.
- The loader rests on jco's `instantiation` mode handing over each core
  module's imports. Nothing in jco's documentation promises the shape.
- No host other than a JavaScript engine and a Wado handler can answer a web
  import, since no other host has a reference to give.
- A component that imports another component's extern handle has no path: the
  handle does not cross a component boundary.
- `linearity = "unrestricted"` without `repr` is rejected: the web bindings were
  its only consumer. What a copyable canonical ABI token such as `Waitable`
  needs from it is undecided.

## Supersedes

- [Resource Inheritance](./wep-2026-04-28-resource-inheritance.md): the gate of
  "`resource extends` is gated on `unrestricted`", the single field of "Why the
  field names linearity, not representation", "Host runtime contract", "`Eq` is auto-derived as reference equality", "`Inspect`
  shows the dynamic type", "Lowering", "Lifecycle" and "Known gap: the handle is
  an index, not a reference". Its language rules stand: linearity, `extends`,
  subtyping, method resolution, and narrowing as a pattern.
- [The Web Interface for Wado](./wep-2026-04-01-web.md): "Callbacks", the
  `Promise<T>` row of "Compound Types", the handle
  table and the transpile steps of "Browser Glue", and "Premise: the handle is a
  value, not a CM resource". Its WebIDL mapping, effect design and handler-served
  DOM stand.

Until the roadmap lands, those sections describe what is built.

## Related WEPs

- [Migration to GC in Components](./wep-2026-03-28-gc-in-components.md) —
  CM-GC changes how `own` and `borrow` are represented, not what they mean
- [Resource Ownership](./wep-2026-05-21-resource-ownership.md) — the affine
  discipline an extern handle is exempt from
- [Research: Component Model Pain Points](./research-cm-pain-points.md) — what
  the Component Model lacks for handles and callbacks
