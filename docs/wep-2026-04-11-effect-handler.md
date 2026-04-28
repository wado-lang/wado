# Effect Handler

Status: Draft

## Implementation Status

- [x] Front-end (lexer / AST / parser / unparser): `with E => h do { ... }`,
      `resume value`, and `..` rest in `impl Effect for Type` blocks.
- [x] Resolver / effect-check: track installed handlers in scope and skip
      handled effects when checking caller requirements.
      `TirExprKind::WithHandler { bindings, body, result_type }` and
      `TirExprKind::Resume { value }` carry the structure through TIR.
      `TirHandlerBinding` records `(effect, handler, handler_type)` so
      later phases can pick the correct `impl E for T` methods.
      Resolver validates that each binding's effect points at a real
      effect declaration, that the handler value's underlying type
      implements that effect, and that `resume` only appears inside a
      handler method body.
      Effect-check augments `current_effects` with the handled effects
      while walking the body so the body's calls do not propagate as
      caller requirements.
- [x] Effect-dispatch synthesis pass. Phase 3 began as static
      devirtualisation of direct `<E>::<op>` calls inside the
      do-block body and has been replaced by Phase 4's full dispatch
      protocol — see the cross-function-boundary entry below for the
      shape that ships today.
- [x] `effect_handler_with_do.wado` runs end-to-end (`mark: 12345`).
- [x] `effect_handler_resume.wado` runs end-to-end in the WEP example
      shape (`&mut self` + struct-owned `value`). Required two compiler
      fixes:
      1. Resolver: `lookup_function_return_type` now consults user-
      defined effect declarations via `TraitEnv::effect_decl_index`
      for `is_effect_like` callees the WASI registry doesn't know
      about. Before, `Counter::next()` was typed as `Unit` and
      downstream Lets / template formatters disagreed with the
      synthesised `MethodCall`'s actual `i32` return.
      2. Lowering: `lower/boxing.rs` had a "Box deref shortcut" that
      collapsed `&local.value` to `local` keyed only on the bare
      field name `value`, so any user struct with a `value` field
      (`Counter` here) had its field access silently dropped.
      Scoped the shortcut to locals whose post-boxing type is an
      actual `Box<T>` struct (`box_type_ids.contains(...)`).
      Regression covered by `effect_handler_mut_self_alias.wado`.
- [x] Cross-function-boundary dispatch (Phase 4). The dispatch
      synthesis pass now emits a per-effect `__Dispatch_<E>` Wasm GC
      struct (recursive `outer: Option<&Self>` + one
      `fn(args) -> ret` closure field per declared operation), a
      `__effect_<E>: Option<&__Dispatch_<E>>` mut global initialised
      to `null`, and a `__effect_dispatch__<E>__<op>` wrapper per
      operation. `WithHandler` lowers to a desugared block that saves
      the global, builds closures capturing the handler, populates the
      dispatch struct, installs the global, runs the body, and
      restores the global on exit. Every `<E>::<op>` call site in the
      package is rewritten to call the wrapper, including calls in
      helper functions invoked from inside the `with` body — so the
      installed handler is observed regardless of call-stack depth.
      The wrapper restores `outer` before invoking the closure, so
      handler methods can self-delegate (`Counter::next()` from inside
      `impl Counter::next`) without recursing through themselves; the
      recursive call reaches the outer handler chain. Operations the
      installed handler does not implement (the `..` rest pattern) get
      a trapping stub closure populated into the dispatch struct.
      Foundations landed alongside the wrapper synthesis:
      - `value_copy::insert::needs_value_copy` no longer wraps
      variant-templated generic instances (`Option<&T>`,
      `Result<T,E>`, ...) — the wrap was identity at the
      `synthesize` side and produced a trapping `(ref X) / nullref`
      signature mismatch when the source was a nullable global.
      - `lower::globals` marks `null`-initialised reference globals
      as `is_nullable` (so the Wasm slot accepts the `ref.null`
      initializer) but leaves a new `lazy_init` flag false, which
      codegen consults to decide whether `global.get` results
      should be narrowed with `ref.as_non_null`. Together these
      let `global mut x: Option<&T> = null` round-trip through the
      full pipeline.
      - The WIR `nullable_ref` representation pass (`Option<&T>` →
      `(ref null T)`) now runs at every opt level, since it picks
      a representation that the storage and the consumers must
      agree on for correctness — not just for `-O1+` perf.
      - `cm_binding::generate_adapters` now walks into `WithHandler`
      bodies in both its effect-call collector and its call-site
      rewriter, so the WASI fallback path of the dispatch wrapper
      always finds an adapter to call.

      End-to-end fixtures: `effect_handler_cross_function.wado` (call
      via helper function), `effect_handler_nested_same.wado` (two
      handlers for the same effect with proper outer chaining +
      restore), `effect_handler_self_delegation.wado` (handler method
      delegates to outer chain via recursive `<E>::<op>` call). All
      pass under `-O0`, `-O1`, `-O2`, `-O3`, `-Os`.
- [x] Early-exit restore. `return v` / `break L: v` / `continue` /
      label-less `break` from inside a `with` body now splice the
      per-`with` restore sequence in front of the jump, so the
      dispatch global is restored before the function leaves the
      do-block scope. Implemented as a TIR walker
      (`RestoreInjector`) that runs inside `desugar_with_handler`
      after the inner body has been desugared. Value-carrying jumps
      are rewritten to evaluate the value into a fresh temp local
      first (so the value evaluates under the do-block's
      still-installed handler), then run the restore sequence, then
      jump with the temp. Jumps targeting labels / loops declared
      _inside_ the do-block body (e.g. `break inner_label;` to a
      label in the body) are detected and skipped — they don't exit
      the `with`, so no restore is needed. Closures inside the body
      are not descended into; their `return`/`break`/`continue`
      target the closure itself.
      End-to-end fixtures: `effect_handler_early_return.wado`
      (return crosses the do-block),
      `effect_handler_break_label_cross.wado` (`break L` whose `L`
      is outside the do-block),
      `effect_handler_continue_cross.wado` (`continue` of an outer
      `while`), `effect_handler_inner_break.wado` (negative — break
      to an inner label must not splice). All pass under `-O0`,
      `-O1`, `-O2`, `-O3`, `-Os`.
- [x] Bundled handlers (`with &mut h do`). The resolver expands a
      bundled binding into one `TirHandlerBinding` per effect that
      the handler's underlying type implements (walking
      `trait_env.impl_index` for the type, filtered by
      `effect_decl_index`). The expanded bindings flow into the
      Phase 4 dispatch synthesis unchanged. Bindings install in
      source order, so a later binding wins when the same effect
      appears more than once on a `with` line — e.g.
      `with &mut h, Counter => &mut alt do { ... }` makes `alt` the
      inner `Counter` handler with `h` as its outer (delegated to
      via the dispatch global's `outer` chain).

      All bindings expanded from one bundled clause share a
      synthesised `__h_<bundle>` local in the dispatch desugaring
      (`TirHandlerBinding.bundle_group`). The synthesis emits the
      handler-binding `Let` once per bundle and reuses that local
      across every per-effect closure, so the handler expression
      is evaluated exactly once and mutations through any
      installed effect are observed by the rest. This is what
      makes value-form `with h do { ... }` work: without sharing,
      each effect would capture an independent value-copy.

      Diagnostics:

      - `BundledHandlerImplementsNoEffect` if the handler type
        implements zero effects (the user almost certainly meant
        `with E => h do`).
      - `BundledHandlerUnsupportedHandlerType` if the handler
        type cannot be indexed by name — type parameters,
        associated-type projections, function types, nested
        references, reactive / builtin-array / type-pack /
        unit / never. Reachable from user code (e.g. a generic
        function `fn f<T>(t: T) { with t do { ... } }`), so the
        resolver emits a proper diagnostic instead of panicking;
        the explicit `with E => h do` form is the documented
        workaround.

      End-to-end fixtures:

      - `effect_handler_bundled.wado` — one handler, two effects.
      - `effect_handler_bundled_value.wado` — value-form
        `with h do`, locking the once-evaluation +
        cross-effect-mutation contract via a side-effect counter
        and a shared `count` field.
      - `effect_handler_bundled_mixed.wado` — bundled + explicit
        on the same `with` line, locking source-order install
        with a self-delegation chain (later binding wins, outer
        chain reaches the earlier one).
      - `effect_handler_bundled_no_effect.wado` — negative; type
        with no effect impls is rejected.
      - `effect_handler_bundled_type_param_rejected.wado` —
        negative; bundled on a generic type parameter is rejected
        with the new diagnostic.
- [x] Resource handler dispatch. Resources participate in the
      dispatch protocol identically to effects: `with R => h do`
      installs a handler whose `impl R for T` methods are
      one-shot handler bodies (`resume` is valid),
      `R::<op>(args)` and `r.<op>(args)` call sites are
      rewritten through the dispatch wrapper, and the handler
      body sees the outer scope so `R::<op>` from inside an
      `impl R for T` method delegates to the outer
      (wasmtime-provided) implementation.

      Implementation:

      - `TraitEnv::resource_decl_index` mirrors
        `effect_decl_index`. The resolver accepts either index
        in `with X => h do` and `impl X for T` — the
        `NotAnEffect` diagnostic message names both kinds, and
        `in_handler_method` is set for resource impls so
        `resume` is gated correctly.
      - `synthesis::effect_dispatch::build_effect_index` walks
        both `module.effects` and `module.resources` into the
        same `EffectKey -> EffectMeta` index. `EffectMeta`
        carries an `is_resource` flag; the dispatch-wrapper
        synthesis uses it to leave the wrapper's `effects: vec![]`
        for resources (matching the cm_binding adapter — resources
        are not effects in Wado's effect system).
      - `lower_resume_in_handler_methods` recognises both effect
        and resource impl methods.
      - The bundled-handler form (`with &mut h do`) walks
        `trait_env.impl_index` for the type and accepts impls
        whose trait name is in either the effect or resource
        index; existing call-site rewriting picks up the
        wrappers unchanged because cm_binding already rewrites
        resource static / instance method calls to plain
        `Call { __cm_binding__<R>_<op> }` shape.
      Adjacent fixes that the resource path forced into view:

      - `TypeTable::retain` now closes the kept set under
        `redirects`. After `erase_newtypes_and_flags`, `get(id)`
        follows redirects to a canonical TypeId; a kept Newtype
        whose post-erasure target was DCE'd would panic with
        "TypeId not found". Resources' Newtype aliases
        (`FieldName = String`, `FieldValue = Array<u8>`) are the
        first user of this code path that pulls the Newtype's
        ID into the reachable set without having a separate
        reference to its base, so the latent retain-vs-redirect
        gap surfaced here.
      - `wir_build` now disambiguates duplicate parameter names
        before producing `WirFunction::param_names` /
        `FunctionTranslator::local_name`. Codegen looks up locals
        by name (`current_locals` is keyed by name in
        `codegen::emit::resolve_local`), so two parameters
        sharing a name would clobber each other's entry and
        silently mis-resolve. Resource methods declared as
        `fn op(self: &R, ...)` push that case into view: the
        synthesised closure's `__call` ends up with a
        `self: &__Closure` env at index 0 and a `self: &R`
        explicit param at index 1, both named `self`. The fix
        suffixes every colliding param with its local index so
        the names round-trip uniquely; non-param locals that
        merely shadow a param keep the original suffix-the-Let
        behaviour.

      End-to-end fixture:
      `effect_handler_resource_fields.wado` — installs a
      counting handler for `wasi:http`'s `Fields`, intercepts
      `Fields::new()` and `f.has(name)` inside `with`, and
      delegates each call back to the real WASI implementation
      from inside the handler body. Passes under `-O0`, `-O1`,
      `-O2`, `-O3`, `-Os`.

- [ ] `MockCM` and handler bundling helpers in `core:test`.
      Resource dispatch (above) gives `impl Stream<u8> for
      MockCM` the routing infrastructure it needs; what remains
      is the buffered Stream/Future implementation in
      `core:test::MockCM` itself and the helper APIs around it
      (see `Buffered CM Handlers (MockCM)` below). Generic
      resources (`Stream<T>`, `Future<T>`) require the dispatch
      wrapper to be synthesised per monomorphisation, which is
      not yet done — the current implementation handles
      non-generic resources only.

## Phase 3 implementation plan

Phase 3 is the codegen layer that makes `WithHandler` / `Resume` actually
run. Its scope:

1. Per-effect Wasm GC struct `$Dispatch_<Effect>` with fields:
   - `outer: ref null $Dispatch_<Effect>` — chains to the previous handler
   - `handler: ref null struct` — type-erased handler value (the same
     trick canonical closures use for `env`)
   - One funcref field per operation: `op_<n>: ref $sig_<n>` where
     `$sig_<n> = (ref null struct, op_args...) -> ret`
2. Per-effect `(mut (ref null $Dispatch_<Effect>))` global, initialised
   to `ref.null none`.
3. One dispatch wrapper Wasm function per operation. Body:
   ```
   let d = global.get $__effect_<E>
   if (ref.is_null d): return <fallback>(args)
   let outer = struct.get d.$outer
   global.set $__effect_<E> outer        ;; handler body sees outer scope
   let result = call_ref struct.get(d, $op_n) (struct.get d.$handler, args)
   global.set $__effect_<E> d            ;; restore
   return result
   ```
   `<fallback>` is the existing `__cm_binding__<E>_<op>` adapter for
   WASI effects, or `unreachable` for user-defined effects (well-typed
   programs reach the wrapper only when a handler is installed,
   enforced by effect-check).
4. One handler wrapper Wasm function per `(impl_type, effect, op)`
   triple. Body:
   ```
   let h = ref.cast<$T> handler_param
   return $T_<op>(h, args)   ;; original method body, with `resume` lowered to `Return`
   ```
5. `WithHandler { bindings, body, result_type }` lowers to:
   - For each binding: register the dispatch struct type + global +
     dispatch wrappers (lazy) and the handler wrappers for the chosen
     `impl E for T`.
   - Emit (per binding):
     ```
     let __save_<E> = global.get $__effect_<E>
     let __dispatch_<E> = struct.new $Dispatch_<E>(
         __save_<E>,
         h_as_anystruct,
         ref.func $__handler_<T>__<E>__<op_1>,
         ref.func $__handler_<T>__<E>__<op_2>,
         ...
     )
     global.set $__effect_<E> __dispatch_<E>
     ```
   - Emit body.
   - Emit (per binding, in reverse install order):
     ```
     global.set $__effect_<E> __save_<E>
     ```
   Early-exit jumps from `body` — `return v`, `break L: v` /
   `break L` whose `L` is declared outside the body, and bare
   `break` / `continue` exiting an outer loop — are handled by
   `RestoreInjector` (see Implementation Status). The walker
   splices the restore sequence in front of the jump and, for
   value-carrying jumps, binds the value to a fresh temp first so
   the value evaluates under the do-block's still-installed
   handler.
6. `Resume { value }` lowers to `Return { value: Some(value) }`. No
   post-resume / Stack Switching support in the MVP (per WEP).
7. Call site rewriting: every call to an effect operation routes
   through the dispatch wrapper, not directly to the CM binding adapter
   or a non-existent user function. For WASI, this means rewriting
   calls to `__cm_binding__<E>_<op>` to call `__effect_dispatch__<E>__<op>`
   instead. For user-defined effects, the resolver currently lands on
   an unresolved `Call { name: "<op>" }` (see Implementation Status);
   Phase 3 must update either the resolver to emit
   `Call { name: "__effect_dispatch__<E>__<op>" }` or have the
   synthesis pass detect and rewrite the unresolved-but-effect-named
   calls.

### Where the new code lives

- `wado-compiler/src/synthesis/effect_dispatch.rs` (new): TIR-level
  synthesis pass that runs after `cm_binding::generate_adapters` and
  after `effect-check`/`stores-check` (so it sees `WithHandler`).
  Generates the per-effect / per-impl wrappers, replaces `WithHandler`
  with desugared blocks, replaces `Resume` with `TirStmtKind::Return`,
  and rewrites call sites.
- Pipeline order in `wado-compiler/src/lib.rs`: insert the new pass
  between `check_stores` and `link::link`. Existing `synthesis::synthesize`
  stays as-is; the new pass is invoked separately so `WithHandler`
  survives effect-check.
- `wir_build` / `codegen` need no new variants — the desugared TIR uses
  `StructLiteral` / `GlobalVarGet` / `GlobalVarSet` / `IndirectCall` /
  `Block` / `Return` that already round-trip cleanly through WIR build
  and codegen.

### Wasm/Wado typing notes

- `ref null struct` (the type-erased handler) does not have a Wado
  surface form. Phase 3 introduces a synthetic TIR struct
  `__AnyHandler` (or reuses `()`) with no fields and stores
  `Option<&__AnyHandler>` in the dispatch struct's `handler` slot. The
  handler wrapper does `ref.cast` to the concrete `&T` before calling
  the user method.
- Operation funcrefs use Wado-level `fn(&__AnyHandler, args...) -> ret`
  function types, which lower to canonical closure func types in WIR.

### Test plan

- `effect_handler_with_do.wado` (existing fixture): drop `#![TODO]`,
  expect `mark: 12345` in stdout.
- `effect_handler_resume.wado` (existing fixture): drop `#![TODO]`,
  expect `1+2=3, final=2` in stdout.
- New negative fixtures (already covered by Phase 2 diagnostics, just
  verify they reject):
  - `resume` outside a handler method body
  - `with E => h do` where `h: T` does not `impl E for T`
  - `with NotAnEffect => h do` (e.g. `with i32 => 0 do`)
  - bundled handler form (`with &mut h do` without `Effect =>`), pending
    full implementation

## Context

[WEP: Effect System Design](./wep-2026-01-27-effect-system-design.md) defines how Wado tracks side effects. This WEP defines how effect handlers provide implementations for effects, enabling dependency injection, testing, and middleware patterns.

Effects are capabilities required by functions. The runtime (wasmtime) provides real implementations for world-imported effects. Effect handlers let user code substitute custom implementations, analogous to how wasmtime's `add_to_linker` registers host implementations for WIT imports.

### Design Goals

- Effects are traits: handlers are ordinary values with `impl Effect for Type`
- No `handler` keyword; handlers are struct instances
- Mutable state is natural via `&self` / `&mut self`
- `resume` is control flow (like `return`), not a special built-in
- CM stream/future unbuffered semantics are addressed by `MockCM`
- Handler bundling reduces boilerplate for types implementing multiple effects

## Decision

### Effect as Trait

An effect declaration defines an interface (like a trait). Any struct that implements the effect's operations can serve as a handler:

```wado
effect Stdin {
    fn read_line() -> String;
}

struct MockStdin {
    responses: Array<String>,
    mut index: i32,
}

impl Stdin for MockStdin {
    fn read_line(&mut self) -> String {
        let result = self.responses[self.index];
        self.index += 1;
        resume result
    }
}
```

Handler implementations add `&self` or `&mut self` to access the handler's state. The effect declaration itself has no `self` parameter (effect operations are free functions from the caller's perspective).

### Using Handlers

The `with Effect => value do { ... }` block installs a handler for the scope of the `do` block. The `=>` arrow reads as a dispatch binding ("calls to `Effect` go to `value`"); it mirrors the match-arm arrow and is deliberately not `=`, since the operation pushes a handler onto a per-effect stack rather than performing assignment.

```wado
fn test_input() {
    let mut mock = MockStdin { responses: ["hello", "world"], index: 0 };
    with Stdin => &mut mock do {
        let a = Stdin::read_line();  // "hello"
        let b = Stdin::read_line();  // "world"
    }
    assert mock.index == 2;
}
```

Multiple handlers:

```wado
with Stdin => &mut mock_stdin, Stdout => &mut mock_stdout do {
    // ...
}
```

Only the effects actually needed are required on the calling function:

- The handled effect itself: not required (handler satisfies it)
- Effects used by handler methods: required on the caller

### Handler Methods with Effects

Handler methods can have their own effect requirements:

```wado
struct LoggingStdin {
    response: String,
}

impl Stdin for LoggingStdin {
    fn read_line(&self) -> String with Stdout {
        println("reading...");
        resume self.response
    }
}

// Caller must have Stdout (handler method's effect), but not Stdin (handled)
fn test_logging() with Stdout {
    let mock = LoggingStdin { response: "mocked" };
    with Stdin => &mock do {
        let line = Stdin::read_line();
    }
}
```

### Handling Granularity and Wildcard

By default, `impl Effect for Type` must implement all operations of the effect (like a complete trait impl). Use `..` (rest pattern) to opt in to trapping on unimplemented operations:

```wado
struct MinimalTcp;

impl TcpSocket for MinimalTcp {
    fn create(&self, family: IpAddressFamily) -> Result<TcpSocket, ErrorCode> {
        resume Result::Ok(mock_socket())
    }
    fn connect(&self, self_: &TcpSocket, addr: IpSocketAddress) -> Result<(), ErrorCode> {
        resume Result::Ok(())
    }
    ..  // bind, listen, send, receive, etc. — trap if called
}
```

`..` is consistent with struct rest patterns (`let { name, .. } = person`).

### Resume Keyword

`resume` is a control flow expression similar to `return`. It passes a value to the computation and transfers control. The expression `resume` itself evaluates to `()`.

```wado
impl Stdin for MockStdin {
    fn read_line(&self) -> String {
        resume "value"
    }
}
```

For post-processing (one-shot continuations):

```wado
impl FileSystem for ManagedFs {
    fn open_file(&self, path: String) -> Handle {
        let handle = real_open(path);
        resume handle;
        real_close(handle);  // runs after do block completes
    }
}
```

### Continuation Semantics and Execution Model

One-shot only. Each `resume` executes at most once. Multi-shot continuations are a future consideration pending Wasm Stack Switching support.

Execution model depends on whether post-resume code exists:

| Pattern        | Example                                | Implementation                |
| -------------- | -------------------------------------- | ----------------------------- |
| No post-resume | `fn op() { resume value }`             | `resume` compiles to `return` |
| Post-resume    | `fn op() { resume value; cleanup(); }` | Wasm Stack Switching          |

Most handlers (test mocks, DI) have no post-resume code and use the `return` optimization. Post-resume handlers (resource cleanup, generators) require Wasm Stack Switching, which is available on amd64 in wasmtime.

### Effect Forwarding

Handlers only handle the effects they declare. All other effects forward to the outer scope. This follows the universal pattern in algebraic effect systems (Koka, Eff, OCaml 5, Effekt).

```wado
let mock = MockClient;
with Client => &mock do {
    let headers = Fields::new();    // Fields is not handled → forwards to outer scope
    let req = Request::new(...);    // Request is not handled → forwards to outer scope
    let resp = Client::send(req);   // Client IS handled → goes to MockClient
}
```

Handler method bodies execute in the outer effect scope. This means:

- A handler for effect E can call E's operations in its body to delegate to the outer implementation (no infinite recursion).
- A handler for effect E can use other effects that are available in the outer scope.

```wado
struct CachingClient {
    cache: &mut TreeMap<String, Response>,
}

impl Client for CachingClient {
    fn send(&mut self, request: Request) -> Result<Response, ErrorCode> {
        let key = request.get_path_with_query();
        if let Some(cached) = self.cache.get(key) {
            resume Result::Ok(cached)
        }
        let resp = Client::send(request);  // Client — outer scope (real impl)
        self.cache[key] = resp;
        resume resp
    }
    ..
}

export fn run() with Stdout, Client {
    let mut cache = TreeMap::<String, Response>::new();
    with Client => &mut CachingClient { cache: &mut cache } do {
        app();
    }
}
```

### Handler Nesting

Handlers nest naturally. Inner handlers override specific effects; unhandled effects forward through the chain to the outermost scope:

```wado
let mut mock_stdout = MockStdout { captured: [] };
let mock_client = MockClient;
with Stdout => &mut mock_stdout do {
    with Client => &mock_client do {
        println("sending...");   // Stdout → MockStdout (outer handler)
        Client::send(req);       // Client → MockClient (inner handler)
    }
}
```

### World Imports as the Outermost Handler

A world's imports define the outermost handler scope. The runtime (wasmtime) provides the real implementations for all imported effects. A `with ... do` block creates a nested handler that overrides specific effects within its scope.

Conceptually, compiling for `wasi:cli/command`:

```
wasmtime (outermost handler)
  ├─ Stdout     = WASI stdout implementation
  ├─ Stderr     = WASI stderr implementation
  ├─ TcpSocket  = WASI socket implementation
  ├─ Stream     = CM canonical runtime
  ├─ Future     = CM canonical runtime
  └─ ...all world imports...

  do {
      run()   ← user's export fn
  }
```

For the test world, the runtime provides a minimal set (Stdout, Stderr, CM builtins). Effects not imported by the test world (e.g., Client, Fields, Request from `wasi:http`) must be provided by user handlers.

### CM Streams and Futures Are Unbuffered

CM streams and futures are semantically unidirectional **unbuffered** channels (see [CM Concurrency spec](../vendor/component-model/design/mvp/Concurrency.md)). `stream.write` blocks until a concurrent reader consumes the data; `future.write` blocks until a concurrent reader reads the value. The CM runtime does not buffer data between the readable and writable ends.

This means synchronous effect handlers cannot directly use CM streams for data transfer. Consider `println`:

```wado
pub fn println(message: String) with Stdout {
    let [rx, tx] = Stream::<u8>::new();           // 1. stream pair
    let handle = Stdout::write_via_stream(rx);    // 2. handler intercepts here
    write_to_stream(tx, message, true);           // 3. tx.write() — blocks if no reader on rx
    drop_cli_write_future(handle);                // 4. future.drop() — blocks if no writer
}
```

If the handler at step 2 simply stores `rx` and resumes, the caller's `tx.write()` at step 3 blocks waiting for a reader on `rx`. In a synchronous handler, there is no concurrent reader — deadlock.

The real WASI runtime avoids this because `write_via_stream` starts an async task that reads `rx` concurrently. Synchronous mock handlers need a different approach: replace CM's unbuffered streams and futures with buffered in-memory implementations.

### Buffered CM Handlers (MockCM)

- [ ] Not yet implemented.

`core:test` will provide `MockCM` — a handler that implements `Stream<u8>`, `StreamWritable<u8>`, `Future<T>`, and `FutureWritable<T>` with buffered in-memory semantics. Writes append to a buffer without blocking; reads return buffered data immediately.

```wado
struct StreamBuffer {
    mut data: Array<u8>,
    mut read_pos: i32,
    mut write_closed: bool,
}

struct MockCM {
    mut stream_buffers: Array<StreamBuffer>,
    mut future_count: i32,
}

impl Stream<u8> for MockCM {
    fn new(&mut self) -> [Stream<u8>, StreamWritable<u8>] {
        let id = self.stream_buffers.len();
        self.stream_buffers.push(StreamBuffer { data: [], read_pos: 0, write_closed: false });
        resume [id as Stream<u8>, id as StreamWritable<u8>]
    }

    fn read(&mut self, stream: &Stream<u8>, max: i32) -> Array<u8> {
        let id = *stream as i32;
        let buf = &mut self.stream_buffers[id];
        let available = buf.data.len() - buf.read_pos;
        if available == 0 { resume [] }
        let count = i32::min(max, available);
        let mut result: Array<u8> = [];
        for let mut i = 0; i < count; i += 1 {
            result.push(buf.data[buf.read_pos + i]);
        }
        buf.read_pos += count;
        resume result
    }

    fn drop(&self, stream: &Stream<u8>) { resume () }
    fn cancel_read(&self, stream: &Stream<u8>) { resume () }
}

impl StreamWritable<u8> for MockCM {
    fn write(&mut self, writable: &StreamWritable<u8>, data: Array<u8>) {
        let id = *writable as i32;
        self.stream_buffers[id].data.extend(data);
        resume ()  // buffered — never blocks
    }

    fn write_raw(&mut self, writable: &StreamWritable<u8>, data: builtin::array<u8>, len: i32) {
        let id = *writable as i32;
        let buf = &mut self.stream_buffers[id];
        for let mut i = 0; i < len; i += 1 {
            buf.data.push(builtin::array_get_u8(data, i));
        }
        resume ()
    }

    fn drop(&mut self, writable: &StreamWritable<u8>) {
        let id = *writable as i32;
        self.stream_buffers[id].write_closed = true;
        resume ()
    }

    fn cancel_write(&self, writable: &StreamWritable<u8>) { resume () }
}
```

Future and FutureWritable use `&T` references for type-erased storage (GC keeps values alive):

```wado
struct MockCMFutureSlot {
    mut value: Option<&()>,  // type-erased: &T cast to &()
}

// MockCM also has:
//   mut future_slots: Array<MockCMFutureSlot>,

impl<T> Future<T> for MockCM {
    fn new(&mut self) -> [Future<T>, FutureWritable<T>] {
        let id = self.future_slots.len();
        self.future_slots.push(MockCMFutureSlot { value: null });
        resume [id as Future<T>, id as FutureWritable<T>]
    }

    fn read(&self, f: &Future<T>) -> Option<T> {
        let id = *f as i32;
        if let Some(erased) = self.future_slots[id].value {
            let ref = erased as &T;  // cast back to original type
            resume Option::Some(*ref)
        }
        resume null
    }

    fn drop(&self, f: &Future<T>) { resume () }
    fn cancel_read(&self, f: &Future<T>) { resume () }
}

impl<T> FutureWritable<T> for MockCM {
    fn write(&mut self, fw: &FutureWritable<T>, value: T) {
        let id = *fw as i32;
        self.future_slots[id].value = Option::Some(&value as &());  // type-erase via &()
        resume ()
    }

    fn drop(&self, fw: &FutureWritable<T>) { resume () }
    fn cancel_write(&self, fw: &FutureWritable<T>) { resume () }
}
```

### Handler Bundling

When a type implements multiple effects, listing each one in `with` is verbose. If the effect name is omitted, the `with` block handles all effects the type implements:

```wado
// Explicit: list each effect separately
with Stream<u8> => &mut cm, StreamWritable<u8> => &mut cm,
     Future<T> => &mut cm, FutureWritable<T> => &mut cm do { ... }

// Bundled: handle all effects MockCM implements
with &mut cm do { ... }
```

Multiple handlers compose naturally:

```wado
with &mut cm, Stdout => &mut stdout, Client => &mut client do {
    run();
}
```

This follows wasmtime's pattern where a single `WasiState` struct implements multiple `*View` traits and is registered with one `add_to_linker` call.

### Handlers for Testing

Effect handlers enable testing code that uses WASI effects without a real WASI runtime. Test functions implicitly have all effects, so handlers can provide any effect. `core:test::MockCM` provides buffered CM canonical handlers as a foundation.

#### Stdout Handler Example

MockStdout stores stream handles from each `write_via_stream` call. Because streams go through `MockCM` (buffered), the caller's `tx.write()` succeeds without blocking. After the `do` block, `drain()` reads buffered data from the stored stream handles:

```wado
struct MockStdout {
    mut streams: Array<Stream<u8>>,
}

impl Stdout for MockStdout {
    fn write_via_stream(&mut self, data: Stream<u8>) -> Future<Result<(), ErrorCode>> {
        self.streams.push(data);
        let [f, ftx] = Future::<Result<(), ErrorCode>>::new();
        ftx.write(Result::<(), ErrorCode>::Ok(()));
        ftx.drop();
        resume f  // no post-resume → compiles to return
    }
}

impl MockStdout {
    fn drain(&mut self) -> String {
        let mut result = String::with_capacity(256);
        for let stream of self.streams {
            loop {
                let chunk = stream.read(4096);
                if chunk.is_empty() { break; }
                result.push_str(String::from_utf8(chunk));
            }
            stream.drop();
        }
        self.streams = [];
        return result;
    }
}

test "println captures output" {
    let mut cm = MockCM::new();
    let mut stdout = MockStdout { streams: [] };
    with &mut cm, Stdout => &mut stdout do {
        println("hello");
        println("world");
        // drain() must be called inside MockCM scope (fake handles are only valid here)
        let output = stdout.drain();
        assert output == "hello\nworld\n";
    }
}
```

Execution flow:

```
println("hello"):
  Stream::<u8>::new()            → MockCM: creates buffer #0, returns fake handles
  Stdout::write_via_stream(rx)  → MockStdout: stores rx, creates fake Future, resumes
  tx.write_raw(bytes, len)      → MockCM: appends to buffer #0 (no block)
  tx.drop()                     → MockCM: marks buffer #0 as write-closed
  future.drop()                 → MockCM: no-op

stdout.drain():
  stream.read(4096)             → MockCM: reads from buffer #0 (immediate)
  stream.drop()                 → MockCM: no-op
```

#### HTTP Client Handler Example

Testing code that calls `Client::send` (e.g., `example/http-get.wado`). The mock constructs a Response with body data pre-written to a buffered stream — this is safe because `MockCM` streams are buffered, so `body_tx.write()` succeeds immediately without a concurrent reader:

```wado
struct MockClient {
    mut requests: Array<String>,
    response_body: String,
    status: StatusCode,
}

impl Client for MockClient {
    fn send(&mut self, request: Request) -> Result<Response, ErrorCode> {
        if let Some(path) = request.get_path_with_query() {
            self.requests.push(path);
        }

        let headers = Fields::new();  // forwards to outer scope
        let [trailers_rx, trailers_tx] = Future::<Result<Option<Trailers>, ErrorCode>>::new();
        let [body_rx, body_tx] = Stream::<u8>::new();  // → MockCM (buffered)

        body_tx.write(self.response_body.bytes().collect());  // buffered — no block
        body_tx.drop();
        trailers_tx.write(Result::<Option<Trailers>, ErrorCode>::Ok(null));
        trailers_tx.drop();

        let [resp, _] = Response::new(headers, Option::Some(body_rx), trailers_rx);
        resp.set_status_code(self.status);

        resume Result::<Response, ErrorCode>::Ok(resp)  // no post-resume
    }
    ..
}

test "http-get fetches and prints" {
    let mut cm = MockCM::new();
    let mut stdout = MockStdout { streams: [] };
    let mut client = MockClient {
        requests: [],
        response_body: `{"origin": "127.0.0.1"}`,
        status: 200,
    };
    with &mut cm, Stdout => &mut stdout, Client => &mut client do {
        run();  // example/http-get.wado's export fn run()
        assert client.requests[0] == "/get";
        let output = stdout.drain();
        assert output contains "Status: 200";
    }
}
```

Note: `Fields::new()`, `Response::new()` etc. are HTTP resource operations that forward to the outer scope. This test requires a world that imports `wasi:http` types (e.g., `wasi:http/service`), or additional handlers for those resources.

#### HTTP Server Middleware Example (Post-Resume)

A timing middleware uses post-resume to measure request processing time. The handler delegates to the outer `Handler` implementation via effect forwarding, resumes the response to the caller, then records metrics:

```wado
struct TimingMiddleware {
    mut log: Array<[String, u64]>,
}

impl Handler for TimingMiddleware {
    fn handle(&mut self, request: Request) -> Result<Response, ErrorCode> {
        let path = request.get_path_with_query().unwrap_or("?");
        let start = MonotonicClock::now();
        let resp = Handler::handle(request);  // delegates to outer scope
        resume resp;
        // Post-resume (Stack Switching): runs after do block completes
        let elapsed = MonotonicClock::now() - start;
        self.log.push([path, elapsed]);
    }
    ..
}
```

Testing with MockHandler as the downstream:

```wado
struct MockHandler {
    status: StatusCode,
    body: String,
}

impl Handler for MockHandler {
    fn handle(&self, request: Request) -> Result<Response, ErrorCode> {
        let headers = Fields::new();
        let [trailers_rx, trailers_tx] = Future::<Result<Option<Trailers>, ErrorCode>>::new();
        let [body_rx, body_tx] = Stream::<u8>::new();
        body_tx.write(self.body.bytes().collect());
        body_tx.drop();
        trailers_tx.write(Result::<Option<Trailers>, ErrorCode>::Ok(null));
        trailers_tx.drop();
        let [resp, _] = Response::new(headers, Option::Some(body_rx), trailers_rx);
        resp.set_status_code(self.status);
        resume Result::<Response, ErrorCode>::Ok(resp)
    }
    ..
}

test "timing middleware records elapsed time" {
    let mut cm = MockCM::new();
    let downstream = MockHandler { status: 200, body: "ok" };
    let mut timing = TimingMiddleware { log: [] };
    with &mut cm do {
        with Handler => &downstream do {
            with Handler => &mut timing do {
                let req = create_test_request("/api");
                let resp = Handler::handle(req);
                assert resp matches { Ok(_) };
            }
        }
    }
    assert timing.log.len() == 1;
    assert timing.log[0].0 == "/api";
}
```

Handler nesting: inner `TimingMiddleware` intercepts `Handler::handle`, delegates to the outer `MockHandler` via effect forwarding, and records timing in post-resume.

## Implementation Notes

### Front-End Grammar Notes

#### `do` and `resume` are contextual keywords

The lexer never emits dedicated `Do` or `Resume` token kinds; both
words are returned as ordinary identifiers and the parser only treats
them as keywords in unambiguous positions:

- `do` is recognised in the trailing position of a `with ... do { ... }`
  clause, immediately after the handler binding list.
- `resume` is recognised only in expression position. In statement /
  pattern positions (e.g. `let resume = ...;`) it remains an ordinary
  identifier.

This keeps both words available as variable names and avoids breaking
generated Wado source (e.g. ANTLR4 driver output that uses `let do = …`
for a TypeScript token of that name).

#### Handler expressions are restricted to unary expressions

Inside `with E1 => handler do { ... }`, the `handler` slot is parsed
with `parse_unary_expr`, which covers references (`&h`, `&mut h`),
prefix-`*` deref, `!`/`~`/`-`, identifiers, calls, method calls, and
field/index access. It deliberately stops short of:

- `as` casts
- `if` / `match` / `matches` / `do` expressions
- assignment / compound assignment

This keeps the grammar unambiguous: stopping at unary level prevents the
handler expression from greedily eating the trailing `,` or `do` token
that closes the binding list. Cases that need the excluded forms must
wrap the handler in parentheses, e.g.
`with E => (h as &mut MockE) do { ... }`.

### Dispatch Mechanism: funcref vtable + Wasm Global

Each effect gets a Wasm global holding a nullable reference to a dispatch record. The dispatch record is a Wasm GC struct containing a funcref per operation, a reference to the handler instance, and a reference to the outer (previous) dispatch record.

```wat
;; One dispatch record type per effect
(type $Dispatch_Stdout (struct
  (field $outer   (ref null $Dispatch_Stdout))   ;; previous handler
  (field $handler (ref any))                      ;; handler instance
  (field $op_write_via_stream (ref $sig_wvs))     ;; funcref per operation
))

;; One global per effect
(global $__effect_Stdout (mut (ref null $Dispatch_Stdout)) (ref.null $Dispatch_Stdout))
```

When no handler is installed (the common production path), the global is null and effect operations call the CM adapter directly. The null check is branch-predictor-friendly and adds negligible overhead relative to the CM boundary crossing.

### Dispatch Function

One dispatch function is generated per effect operation. It checks the global, and either calls the CM adapter (default) or calls through the dispatch record's funcref:

```wat
;; __dispatch_Stdout_write_via_stream
(func $dispatch_stdout_wvs (param ...) (result ...)
  (local $dispatch (ref null $Dispatch_Stdout))
  (local.set $dispatch (global.get $__effect_Stdout))
  (if (ref.is_null (local.get $dispatch))
    (then
      ;; Default: call existing CM adapter
      (return (call $__cm_adapter_stdout_wvs ...)))
    (else
      ;; Handler: restore outer scope, call handler, re-install
      (global.set $__effect_Stdout
        (struct.get $Dispatch_Stdout $outer (local.get $dispatch)))
      (local.set $result
        (call_ref $sig_wvs
          (struct.get $Dispatch_Stdout $handler (local.get $dispatch))
          ...
          (struct.get $Dispatch_Stdout $op_write_via_stream (local.get $dispatch))))
      (global.set $__effect_Stdout (local.get $dispatch))
      (return (local.get $result)))))
```

The outer-scope restoration before `call_ref` ensures that handler method bodies execute in the outer effect scope. This makes effect forwarding and self-delegation (e.g., `CachingClient` calling `Client::send` to reach the real implementation) work correctly without infinite recursion.

### Compilation of `with ... do`

```wado
with Stdout => &mut mock do { body }
```

Compiles to:

1. Construct a dispatch record: `struct.new $Dispatch_Stdout (global.get $__effect_Stdout, mock_ref, funcref_for_each_op)`
2. `global.set $__effect_Stdout` with the new dispatch record
3. Execute body — every control-flow exit from the body (`return`, `break L`/`continue` to a target outside the body) gets the restore step (4) spliced in front of it; value-carrying jumps bind the value to a temp local first so it evaluates under the still-installed handler
4. `global.set $__effect_Stdout` with the dispatch record's `outer` field (restore)

Nesting composes naturally — each `with` block links to the previous dispatch record via `outer`.

### Compilation of `resume`

In the MVP, only `resume` without post-resume code is supported. `resume value` compiles to `return value`. The handler method is a normal function; the dispatch function receives and propagates the return value. Post-resume (e.g., cleanup after the `do` block) requires Wasm Stack Switching and is deferred.

### Wildcard `..`

Unimplemented operations get a trap stub funcref in the dispatch record: `(func $trap (...) (unreachable))`.

### Binary Size

| Element          | Cost                               |
| ---------------- | ---------------------------------- |
| Per effect       | 1 global + 1 GC struct type        |
| Per operation    | 1 dispatch function (10-20 instr)  |
| Per `with` block | 1 struct.new + global save/restore |
| Per handler impl | 1 wrapper per implemented op       |

Growth is O(operations), independent of the number of call sites or handler types. Function signatures are unchanged — no hidden parameters.

### Design Alternatives Considered

- Hidden parameter threading: passes dispatch record as an extra function parameter. Enables static devirtualization at `with` sites, but changes every function signature in the effect chain and increases binary size proportional to call-chain depth times number of effects. Could be added later as an optimization pass on top of the global-based mechanism.
- Flat globals (one funcref global per operation, no struct): avoids GC allocation but requires O(operations) save/restore per nesting level and cannot represent the outer chain cleanly.
- Switch/br_table with integer discriminant: enables direct calls but duplicates dispatch code at every call site, growing O(call_sites × handler_types).

## Consequences

- Effects are traits: any type implementing `impl Effect for Type` can serve as a handler
- No `handler` keyword needed; handlers are ordinary values with effect implementations
- Mutable state in handlers is natural: struct fields accessed via `&self` / `&mut self`
- Handlers satisfy effects locally; unhandled effects forward to the outer scope
- Handler bodies execute in the outer effect scope, enabling delegation to real implementations
- World imports are the outermost handler scope; user handlers nest inside
- `..` wildcard enables partial handling with runtime trap for unimplemented operations
- `resume` without post-processing compiles to `return`; post-processing requires Stack Switching
- One-shot semantics ensure resource safety
- CM streams and futures are unbuffered — synchronous handlers need `MockCM` (buffered CM handlers) for data transfer
- Handler bundling (`with &mut value do`) reduces boilerplate when a type implements multiple effects
- `core:test::MockCM` provides standard buffered Stream/Future handlers as a foundation for all test mocks
