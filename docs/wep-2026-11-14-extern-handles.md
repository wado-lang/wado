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
  it, so `linearity = "unrestricted"` alone stays available to a copyable token
  that crosses as a canonical ABI value.

### An extern handle is a nullable `externref`

An extern handle is one Wasm value, `(ref null extern)`. Every type in an
`extends` tree is that one value, so an upcast and an inherited method's
receiver stay no-ops. `Option<R>` of an extern handle type `R` is the same
value, with `None` as the null reference.

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

A handle never spills to linear memory, which cannot hold a reference. A
signature that would spill one is rejected.

These imports are not component imports. The emitted WIT does not list them.

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

Any code may call either function. A backed handle that reaches a real host is
no object of that host's, and the host rejects it as it would any wrong value.

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

### A handle is not hashable yet

A handle implements no `Hash` and no `Ord`, so it keys no map. A host object has
no number of its own, and giving it one is the last step of the roadmap.

## Roadmap

Each step lands with `SurfaceDom`'s tests and the jsdom run passing.

- [ ] `repr = "externref"` on `#[cm]`, validated beside `linearity`, `extends` and
  `classes`, and declared by `wado-from-idl` on every resource of the slice
- [ ] The representation: an extern handle is `(ref null extern)` in
  the guest, `Option<R>` is the nullable reference, and the `f64` cast is gone
- [ ] Backed handles: `backed_by` and `backing` on every extern handle type,
  narrowing and `Inspect` reading a backed handle's class, and `==` by `ref.eq`
  of the backing values
- [ ] `SurfaceDom` backs its handles with its own nodes and keeps no table, and
  `wado-from-idl` stops emitting the class globals
- [ ] The boundary: a web import is a core import, the synthesized trapping
  module satisfies it, and the component validates and runs on wasmtime
- [ ] The glue: `wado-from-idl` generates shims over core signatures, reading
  strings from memory, with `class-of` and `same` and no handle table
- [ ] The loader: the jco transpile runs in `instantiation` mode and replaces
  the trapping module's functions with the glue
- [ ] Closures cross as `externref`, called back through unlifted core exports,
  and the registry and `wado:callback/callback` are removed
- [ ] A collection test in the jsdom run: dropped handles, dropped listeners,
  and a cycle through both heaps are collected
- [ ] `Hash` for a handle

## Known gaps

- No browser was measured, only V8 in Node and JavaScriptCore in Bun, both
  against jsdom. SpiderMonkey was not run at all. Collection across the two
  heaps with a browser's own DOM objects is unseen.
- A list of handles and a record holding one have no crossing form, since both
  spill to linear memory. `sequence<Node>` waits on this.
- `Promise<T>` has no crossing form. A `Future<T>` is a canonical ABI handle,
  and a web import no longer passes through the canonical ABI.
- A callback runs outside any Component Model task when the event loop calls
  it. A synchronously lowered import works from there. An asynchronously
  lowered one was not tried.
- `class-of` costs a call for each narrowing of a host object. It was not
  measured against the range test it replaces.
- `class-of` reads prototypes of the glue's own realm. An object from another
  realm, such as an element of an `iframe`'s document, has other prototypes and
  matches none.
- The loader rests on jco's `instantiation` mode handing over each core
  module's imports. Nothing in jco's documentation promises the shape.
- No host other than a JavaScript engine and a Wado handler can answer a web
  import, since no other host has a reference to give.
- A component that imports another component's extern handle has no path: the
  handle does not cross a component boundary.
- `linearity = "unrestricted"` without `repr` has no consumer once the web
  bindings declare `repr = "externref"`. The `f64` handle it means today is
  removed with the table, and what a copyable canonical ABI token such as
  `Waitable` needs from it is undecided.

## Supersedes

- [Resource Inheritance](./wep-2026-04-28-resource-inheritance.md): the gate of
  "`resource extends` is gated on `unrestricted`", the single field of "Why the
  field names linearity, not representation", "Host runtime contract", "`Eq` is auto-derived as reference equality", "`Inspect`
  shows the dynamic type", "Lowering", "Lifecycle" and "Known gap: the handle is
  an index, not a reference". Its language rules stand: linearity, `extends`,
  subtyping, method resolution, and narrowing as a pattern.
- [The Web Interface for Wado](./wep-2026-04-01-web.md): "Callbacks", the handle
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
