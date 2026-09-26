# Research: Component Model Pain Points

Where Wado's use of the Component Model (CM) meets a limit of the CM itself.
Wado targets the CM on purpose, so a limit it hits is feedback for the spec, not
a reason to leave it. Each entry names what Wado runs into, what the spec says
about it, and where a report belongs.

The spec is the target. An engine that deviates from it is reported to that
engine, as a separate matter. Wasmtime accepts reports only from people, not
from tools (`vendor/wasmtime/AGENTS.md`).

Checked against:

- `vendor/component-model` at `d1daf82` (2026-09-24)
- `vendor/wasm` at `6087111` (2026-09-22)
- wasmtime v49.0.0, the version `Cargo.lock` pins
- jco 1.30.0, the version `scripts/jco` installs

## A host object has no handle a GC guest can let go of

`wado-lang:web` binds the DOM. A page hands the program the same objects again
and again, and a program keeps them in lists, struct fields and closures. The
bindings therefore do not use CM resources for them. Each object crosses as an
`f64` index into a table the glue owns, and nothing ever releases an entry:
every object the program has seen stays alive for the life of the page
([Resource Inheritance](./wep-2026-04-28-resource-inheritance.md), "Lifecycle").

What the spec offers instead:

- The JS API, as the Explainer sketches it, maps a JS object to an `own` or
  `borrow` of an imported resource type (Explainer.md, "JS API"). Every `own`
  lowered into a component takes a
  fresh slot in its handle table (CanonicalABI.md, `lower_own`), and only
  `resource.drop` frees it.
- On the JS side, the same section drops an `own` held by a JS wrapper through
  a `FinalizationRegistry`, once the wrapper is unreachable. A Wasm GC guest has
  no counterpart. Core Wasm puts weak references and finalizers after the GC
  MVP, with no design yet (`proposals/gc/Post-MVP.md`, "Weak References").
- The GC ABI option
  ([component-model#525](https://github.com/WebAssembly/component-model/issues/525))
  is still undefined; CanonicalABI.md cites it only as the future home of
  `thread.new_ref`. A resource's `rep` is fixed to `i32` / `i64` "but will be
  generalized in the future" (CanonicalABI.md, "Resource State").

So a GC guest holding a host object must drop every handle by hand, including
the ones inside its own heap, or leak. The GC ABI option as scoped changes how
a handle is represented, not the drop obligation it carries, so it does not
lift this by itself.

What to report to the CM: a GC-managed guest needs a handle that is released
when the guest's collector finds it unreachable. That is the JS side's
`FinalizationRegistry` path, made available to the guest.

## A handle has no identity

A program asks whether two handles name the same object: `==` on two elements,
`removeEventListener` finding the listener `addEventListener` registered, a map
keyed by element.

The CM gives no way to ask. Lowering an `own` or a `borrow` creates a new table
index each time (CanonicalABI.md, `lower_own`, `lower_borrow`), so one object
crossing twice arrives as two unequal indices. `resource.rep` would reveal the
representation, but only to the instance that implements the resource
(CanonicalABI.md, "`canon resource.rep`"). An importer needs a host function per
resource type to compare two handles.

The web bindings avoid this only because their handles are not CM resources:
the glue interns each object, so one object always crosses as one number.

What to report to the CM: an importer cannot tell whether two handles name the
same resource.

## Resource types have no subtyping

WebIDL interfaces form single-inheritance chains
(`EventTarget → Node → Element → HTMLElement → HTMLInputElement`), and one
object is an instance of every type up its chain.

The CM has no relation between resource types:

- A resource type is generative, and equality is identity
  (CanonicalABI.md, "Resource State": "resource type equality is _not_ defined
  structurally"). Lifting a handle traps unless its type is exactly the one
  expected (`lift_own`, `lift_borrow`: `trap_if(h.rt is not t.rt)`).
- The only bound a type import may carry is `sub resource`, "any resource type"
  (Explainer.md, "Declarators").
- The JS API checks an incoming object against the imported resource type's
  constructor, dynamically (Explainer.md, "JS API"). An `HTMLInputElement`
  passes as `own $Element` and as `own $HTMLInputElement` alike, but becomes a
  different handle of a different type in each case.

An upcast is therefore a host call minting a second handle, and a downcast a
host call returning `option<own<Sub>>`, each with its own drop obligation.
[Resource Inheritance](./wep-2026-04-28-resource-inheritance.md) builds the
hierarchy in Wado instead and lowers the whole family to one universal handle.
That works only because the handle is not a CM resource.

What to report to the CM: a type import bounded by another resource type
(`sub $Parent`), so a handle lifts where its supertype is expected.

## A closure cannot cross

An event listener, `requestAnimationFrame` and `queueMicrotask` all take a
function. The CM has no function values. Concurrency.md lists them as future
work: "allow function closures to be passed as first-class values, supporting
the 'callback' pattern in many pre-existing APIs, including Web APIs".

[Web § Callbacks](./wep-2026-04-01-web.md#callbacks) encodes a closure as a
`u32` key and exports one trampoline per argument shape for the host to call
back through. What that costs:

- The registry never releases a key, as the handle table never releases an
  object.
- A key names a closure only within the instance that registered it, so a host
  serving several instances has to route each key back to its own. A function
  value would carry its instance with it.

What to report to the CM: nothing new. The need is already on the list; the
web bindings add a concrete consumer and the cost of the workaround.

## Checked and not a CM gap

### Recursive reentrance

`dispatch_event` runs its listeners during the import call, so the listener
reenters the component ([Web § Callbacks](./wep-2026-04-01-web.md#callbacks)).

The spec no longer forbids this. component-model#650 (2026-05-21) and #705
(2026-08-28) removed the `may_enter` flag and its trap. Concurrency.md,
"Reentrance", now describes a host reentering the caller's instance through a
recursive export call as possible, and leaves its hazards to the component's
documented API.

- wasmtime v49.0.0 follows. A host function for a synchronous import called
  the program's `wado:callback/callback` export back with `TypedFunc::call`;
  the listener ran, and `run` went on to complete. `may_enter` refuses entry
  only after a trap (`crates/wasmtime/src/runtime/component/store.rs`). One
  path panics instead, see "wasmtime" below.
- jco does not yet, see "jco" below.

The failure the web bindings record under jco is therefore jco's, not the
spec's.

### Exceptions from the host

A DOM operation that throws traps in the web bindings
([Web § Exceptions](./wep-2026-04-01-web.md#exceptions)).

The CM has a channel for it. The JS API turns an exception thrown by a function
whose WIT result is a `result` into that result's `error` case
(Explainer.md, "JS API"). What is missing is knowing which operations throw:
WebIDL does not declare it, so the generator cannot choose `result` for them.
That is a question for WebIDL, not the CM.

## Engine deviations

Found while checking the entries above. Each is the engine's to fix, not the
spec's.

### jco

A program that dispatches an event to its own listener, compiled by Wado and
transpiled by jco, run on Node against jsdom:

```wado
export fn run() with Dom {
    let body = Dom::document().body().unwrap();
    body.add_event_listener("ping", |_: Event| {
        Dom::document().body().unwrap().set_id("pinged");
    }, null);
    body.dispatch_event(Event::new("ping"));
}
```

- jco 1.30.0 runs the listener, then fails the outer `run` at its
  `task.return`:
  `TypeError: Cannot destructure property 'taskID' of '_getGlobalCurrentTaskMeta(...)' as it is undefined`.
  The nested export call clears the instance's current-task slot on exit
  instead of restoring the caller's.
- jco 1.35.0 restores the slot, but fails every Wado program once `run`
  returns, reentrant or not, `example/hello.wado` included:
  `task [1] is already resolved (did you forget to wait for an import?)`.
  For an `async` export lifted without a `callback`, it resolves the task again
  when the core function returns, although `task.return` already delivered the
  result. The spec has the core function return nothing there and ends the task
  (CanonicalABI.md, `canon_lift`, the stackful case). With that second resolve
  removed from the transpiled output, the program above runs its listener
  during the import call and returns.

`scripts/jco/package-lock.json` keeps 1.30.0 until the 1.35.0 defect is fixed.

### wasmtime

The same reentry, with the synchronous import implemented by
`func_wrap_async` and the export called back with `TypedFunc::call_async`,
panics inside wasmtime instead of returning an error. The call fails before the
listener runs, and the future's drop guard then unwraps a second error:
`` `subtask.cancel` called after terminal status delivered ``
(`src/runtime/component/concurrent/func.rs`, `SignalOnDrop::drop`). The first
error is lost. A library panic reachable from the public API is a defect
whatever the call's validity; a person has to file it.
