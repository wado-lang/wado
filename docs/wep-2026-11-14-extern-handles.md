# WEP: An Unrestricted Resource Is a Host Reference (`externref`)

## Context

An unrestricted resource is the handle type the web bindings are built on: a
DOM object the program may copy freely
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
configuration, and on Node 26 after a jco 1.30.0 transpile, against jsdom. Its
main core module imports functions that take and return `externref`. Inside the
component a second core module satisfies those imports with functions that
trap. The component is not in the tree: the roadmap's collection test takes its
place.

| Check                                                                           | wasmtime | jco on Node |
| ------------------------------------------------------------------------------- | -------- | ----------- |
| The component validates and instantiates                                        | passes   | passes      |
| A call to a web import the host does not replace traps                          | passes   | —           |
| A guest GC struct is a handle: `extern.convert_any`, `ref.test`, `ref.eq`       | passes   | passes      |
| The host replaces the trapping module's functions with its own                  | —        | passes      |
| A host object crosses as an `externref`, in both directions                     | —        | passes      |
| The host calls a core export the component does not lift                        | —        | passes      |
| That export runs during an import call, and calls a `canon lower`ed sync import | —        | passes      |
| It does the same from the event loop, outside any export call                   | —        | passes      |
| Handles the guest dropped are collected                                         | —        | passes      |
| A handle held in a guest global survives, and is collected once cleared         | —        | passes      |
| A detached element and a closure that hold each other are collected             | —        | passes      |
| A guest GC struct keys a JavaScript `WeakMap`                                   | —        | passes      |

One fact decides how `==` lowers. On V8 no host value passes `ref.test eqref`
once internalized: not a DOM object, a plain object, a function or a string. A
guest struct does.

The host replaces the functions through jco's `instantiation` mode, which hands
the embedder each core module and its imports before instantiating it.

## Decision

### The handle is a nullable `externref`

An unrestricted resource is one Wasm value, `(ref null extern)`. Every type in
an `extends` tree is that one value, so an upcast and an inherited method's
receiver stay no-ops. `Option<R>` of an unrestricted resource `R` is the same
value, with `None` as the null reference.

The handle is no longer a number. `as` converts a handle only to a resource it
extends, and nothing converts one to or from `f64`.

A handle is either the host's object or a struct the guest minted (below). The
program cannot tell which, and no rule below depends on which host is running.

### The web imports leave the canonical ABI

An import whose signature holds an unrestricted resource is a core function
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

### A guest mints a handle from a struct

A handler such as `SurfaceDom` answers web calls in Wado, so it must make
handles. A minted handle is a GC struct holding a class and a reference to a
value of the handler's choosing, converted with `extern.convert_any`. The
handler reads the value back from a handle it minted, and gets `None` from one
it did not.

```wado
let node: Node = builtin::mint_handle(ELEMENT_CLASS, &element);
let back = builtin::minted::<SurfaceElement>(&node);   // Option<&mut SurfaceElement>
```

A node of `SurfaceDom` is then collected like any other value, and the handler
keeps no table either.

### Narrowing asks for the class once

The class numbering of
[Resource Inheritance](./wep-2026-04-28-resource-inheritance.md#host-runtime-contract)
stays: each resource declares `classes = "lo..=hi"`, and a narrowing is a range
test on a class. What changes is where the class comes from.

- A minted handle carries its class in the struct, so the guest reads it.
- A host object carries none. The guest calls the import `class-of`, which
  answers the class of the nearest ancestor the slice declares.

A `match` with any number of type-pattern arms reads the class once. Narrowing
the host's object costs one call where it cost none.

`Inspect` reads the class the same way and writes the name of the resource that
owns it. There is no index to write beside it.

### `==` is identity

Two handles are equal when they are one object.

- Where both pass `ref.test eqref`, the guest compares them with `ref.eq`.
- Where neither does, the guest calls the import `same`, which answers
  JavaScript's `===`.
- Where one does, they differ.

This is correct whatever an engine answers for a host object, since `ref.eq` on
two references that pass the test is identity too. On V8 the second case is
every pair of host objects.

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

- [ ] The representation: an unrestricted resource is `(ref null extern)` in
  the guest, `Option<R>` is the nullable reference, and the `f64` cast is gone
- [ ] Minted handles: `builtin::mint_handle` and `builtin::minted`, narrowing
  and `Inspect` reading a minted class, and `==` by `ref.eq`
- [ ] `SurfaceDom` mints its handles from its own nodes and keeps no table
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

- Only V8 was measured. No test runs in Firefox or Safari, and a host object's
  answer to `ref.test eqref` is known for V8 alone.
- A list of handles and a record holding one have no crossing form, since both
  spill to linear memory. `sequence<Node>` waits on this.
- `Promise<T>` has no crossing form. A `Future<T>` is a canonical ABI handle,
  and a web import no longer passes through the canonical ABI.
- A callback runs outside any Component Model task when the event loop calls
  it. A synchronously lowered import works from there. An asynchronously
  lowered one was not tried.
- `class-of` costs a call for each narrowing of a host object. It was not
  measured against the range test it replaces.
- The loader rests on jco's `instantiation` mode handing over each core
  module's imports. Nothing in jco's documentation promises the shape.
- No host other than a JavaScript engine and a Wado handler can answer a web
  import, since no other host has a reference to give.
- A component that imports another component's unrestricted resource has no
  path: the handle does not cross a component boundary.

## Supersedes

- [Resource Inheritance](./wep-2026-04-28-resource-inheritance.md): "Host
  runtime contract", "`Eq` is auto-derived as reference equality", "`Inspect`
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
  discipline an unrestricted resource is exempt from
- [Research: Component Model Pain Points](./research-cm-pain-points.md) — what
  the Component Model lacks for handles and callbacks
