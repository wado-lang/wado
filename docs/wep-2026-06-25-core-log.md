# Structured Logging and Tracing Standard Library (`core:log`)

Status: Draft

## Context

Wado has best-effort ambient output (`log_stdout` / `log_stderr`, see
[Ambient Logging](./wep-2026-01-12-ambient-logging.md)) and strict effectful I/O
(`println` / `eprintln`), but no logging facility: no levels, structured fields,
spans, pluggable sink, source location, or filtering. This WEP designs the
full-set logger and tracer.

### What modern logging converged on

Rust `tracing` and Go `slog` agree on:

- Structured key–value fields, not values concatenated into the message.
- A facade decoupled from a backend (`tracing` `Subscriber` / `slog` `Handler`).
- Spans: named, timed scopes forming a tree; events attach to the current span
  (`tracing`; `slog` approximates with `Logger.With`).
- Composable layers (`tracing`'s `Layer` stack): filter + format + telemetry.
- Two-axis filtering: compile-time call-site stripping (`max_level_*`) and runtime
  directives (`RUST_LOG` / `EnvFilter`).
- Zero work when disabled; JSON for prod, text for dev; emit-time timestamps.

Both rely on what Wado lacks by design (Rust macros; Go dynamic dispatch and
variadic `...any`). Wado reproduces each with its own tools.

### Mapping onto Wado

| Concern                   | `tracing` / `slog`             | Wado mechanism                                                         |
| ------------------------- | ------------------------------ | ---------------------------------------------------------------------- |
| Backend abstraction       | `Subscriber` / `Handler` (dyn) | the `Log` **ambient effect**; sinks are effect handlers (static)       |
| Layer composition         | `Layer` stack                  | **nested effect handlers** (`with Log => &layer do`), forward to outer |
| Spans / scoped context    | spans, `Logger.With`           | first-class `Span` values entered for a scope                          |
| Compile-time level filter | `max_level_*`                  | **`#[param]` compile-time global** + constant-fold + DCE               |
| Runtime filter            | `EnvFilter` / min level        | a filter layer via `enabled(meta)`                                     |
| Source location           | `file!()` / `#[track_caller]`  | default args + call-site `#file` / `#line` / `#function`               |
| Structured fields         | macros / `...any`              | `List<Field>` built with `field<T: Serialize>(…)`                      |
| Field/event encoding      | `slog` serializer / `Visit`    | **`core:serde`**                                                       |
| Timestamp                 | subscriber adds it             | sink config (default on), via `wasi:clocks` `SystemClock`              |

Thesis: **a subscriber is an effect, layers and spans are nested handlers, the
level threshold is a compile-time parameter, and the default sink is the existing
`log_stderr`.** The rest is library code; the required language extension is
ambient effects (span scoping already works via a closure, `with span do` is
optional sugar).

## Decision

### Two layers: ambient default + rich subscriber

With no subscriber installed, the ambient `Log` effect falls back to a
best-effort default reusing existing ambient output (stderr, leaving stdout for
program output):

```wado
fn default_handler_event(event: &Event) {
    core:cli::log_stderr(render_plain(event));
}
// span lifecycle ops default to no-ops.
```

So `info(...)` works with zero setup; a real subscriber is opt-in.

### Types

```wado
pub enum Level { Trace, Debug, Info, Warn, Error }   // ordered by `level as i32`

pub type SpanId = u64;

pub struct Metadata {
    pub level: Level,
    pub target: String,   // category; default = caller #function
    pub name: String,     // span name; "" for events
    pub file: String,     // call-site #file
    pub line: i32,        // call-site #line
}

pub struct Field { pub key: String, pub value: Value }   // Value from core:value
pub struct Event {
    pub meta: Metadata,
    pub message: String,
    pub fields: List<Field> = [],
    pub parent: Option<SpanId> = null,   // null = current span
}
pub struct SpanAttrs { pub meta: Metadata, pub fields: List<Field> }

pub fn field<T: Serialize>(key: String, value: T) -> Field {
    let v = match value::to_value(&value) { Ok(v) => v, Err(_) => Value::Null };
    return Field { key, value: v };
}
```

`core:value::to_value` (a direct, data-model-preserving `Value` builder) is
implemented. Serde types carry the empty derive `impl Serialize for T;` (Wado's
derive form — there is no `#[derive(...)]`); bound-driven derivation
([Trait Derivation Policy](./wep-2026-06-25-trait-derivation.md)) would make
those implicit.

### The `Log` effect (subscriber)

An ambient effect (see Language Extensions) mirroring `tracing`'s `Subscriber`,
simplified by GC (no span refcounting). All ops return `()` — logging is
best-effort and never fails the program; sinks swallow their own write/serialize
errors.

```wado
#[ambient(default = default_handler)]
pub interface Log {
    fn enabled(meta: &Metadata) -> bool;
    fn current_span() -> Option<SpanId>;
    fn new_span(attrs: &SpanAttrs) -> SpanId;
    fn record_fields(span: SpanId, fields: List<Field>);   // fields onto an open span
    fn follows_from(span: SpanId, cause: SpanId);          // non-parent causal link
    fn enter(span: SpanId);
    fn exit(span: SpanId);
    fn close(span: SpanId);
    fn event(event: &Event);
}
```

### Events

Free functions; ambient, so no `with Log` and callable anywhere. Location
defaults resolve at the caller. The message is an eager `String` (the optimizer
drops it when the level is statically off; `enabled()` guards the runtime-off hot
path).

```wado
#[inline(always)]
pub fn event(level: Level, message: String, fields: List<Field> = [],
             target: String = #function, file: String = #file, line: i32 = #line) {
    if (level as i32) < LOG_MAX_LEVEL { return; }   // compile-time gate (folded)
    let meta = Metadata { level, target, name: "", file, line };
    if !Log::enabled(&meta) { return; }              // runtime filter
    Log::event(&Event { meta, message, fields, parent: null });
}
// info / debug / warn / error wrap `event` with their level.

pub fn enabled(level: Level, target: String = #function) -> bool { ... }   // expensive-field guard
```

```wado
info(`user logged in`, [field("user_id", id), field("ip", ip)]);
if enabled(Level::Debug) { debug(`state`, [field("snapshot", expensive_snapshot())]); }
```

### Spans

A span is a first-class, re-enterable value, entered for a scope:

```wado
pub struct Span { id: SpanId }
pub fn span(level: Level, name: String, fields: List<Field> = [], ...) -> Span { ... }   // calls new_span

impl Span {
    pub fn id(&self) -> SpanId { ... }
    pub fn record(&self, fields: List<Field>) { Log::record_fields(self.id, fields); }
    pub fn follows_from(&self, cause: &Span) { Log::follows_from(self.id, cause.id); }
}
pub fn current() -> Option<Span> { ... }
```

Entry emits `enter` on entry and `exit` on every exit path. v1 uses the closure
`in_span` (Span scoping, below); the illustrative native sugar:

```wado
let s = span(Level::Info, "request", [field("route", route)]);
with s do { info(`received`); handle(); }   // events parent to s; s exits on every path
```

The subscriber tracks the current-span stack from `enter`/`exit`, so `current()`
and parenting need no separate global. Entry is lexical (no free-floating RAII
guard, so no "guard held across an await" footgun). `close` fires on GC
unreachability or an explicit `span.close()`; fmt sinks usually ignore it.

### Subscribers and layers

A sink/layer is `impl Log`; layers nest, each handling what it cares about and
forwarding the rest (effect forwarding,
[Effect Handler](./wep-2026-04-11-effect-handler.md)).

```wado
pub struct TextSink { pub timestamp: bool = true, pub seq: bool = true, pub location: bool = false }
impl Log for TextSink {
    fn enabled(&self, meta: &Metadata) -> bool { resume true }
    fn event(&self, event: &Event) with Stderr, SystemClock { eprintln(render_text(event, self)); resume () }
    ..   // span ops: no-op or render
}

pub struct JsonSink { pub timestamp: bool = true, pub seq: bool = true }   // JSONL via json::to_string
pub struct NopSink;
pub struct CaptureSink { events: List<Event> }   // test sink

// Field-context layer (slog `With`): prepend fixed fields, forward.
pub struct Context { fields: List<Field> }
impl Log for Context {
    fn event(&self, event: &Event) {
        let mut merged = self.fields;
        merged.extend(event.fields);   // list concat (serde flatten not needed yet)
        Log::event(&Event { meta: event.meta, message: event.message, fields: merged, parent: event.parent });
        resume ()
    }
    ..
}

pub struct Filter { directives: List<Directive> }   // runtime EnvFilter-style layer
```

Install by nesting `with`:

```wado
export fn run() with Stdout, Stderr, SystemClock {
    with Log => &Filter { directives: parse_env() }, Log => &TextSink { location: true } do { app(); }
}
```

### Timestamp

Owned by the sink (not `Event`), configurable (`timestamp`, default on).
Container/collector stamps record ingestion time, not event time, and drift under
buffering — and not every target has a collector. With `timestamp: false` (the
container stamps) the sink needs no `SystemClock`. A monotonic `seq` counter
(default on) preserves intra-process order without a clock.

### Filtering (two axes)

Compile-time max level, constant-folded:

```wado
#[param(name = "log.level", from_env = "WADO_LOG")]
global LOG_LEVEL: String = "info";
global LOG_MAX_LEVEL: i32 = level_from_str(&LOG_LEVEL);
```

Below `LOG_MAX_LEVEL` is stripped everywhere (Zero-cost). A `Filter` layer adds
runtime `EnvFilter`-style directives (`target=level`, `mod::path=debug`,
span/field predicates) via `enabled(meta)`. This mirrors `tracing`'s
`max_level_*` plus `EnvFilter`.

### Error handling and reentrancy

Ops return `()`; sinks swallow errors; logging never aborts. Reentrancy (a sink,
or a `Serialize` it calls, logs again) doesn't loop: handler bodies run in the
outer scope, so a re-entrant log forwards outward and terminates at the
non-logging default. No panic (it would break the never-fail rule and punish
legitimate composition); a depth limit drops a pathological self-reinstalling
handler.

### Zero-cost when disabled

`LOG_MAX_LEVEL` is constant, so the gate folds; with `#[inline(always)]` a
statically-off call inlines to `if true { return }` and DCE removes the body and
the pure argument expressions (message, `field(...)`) over the
[Live ValueGraph](./wep-2026-06-15-live-value-graph.md). Side-effecting arguments
are not eliminable — guard those with `enabled()`.

### Async semantics

The current-span stack rides the effect-dispatch state, process-global today —
exact within one synchronous scope, wrong across concurrent tasks (HTTP
requests). So:

- v1 auto current-span propagation is single-scope only.
- Cross-task is explicit: carry the `Span` value and re-enter it (`tracing`'s
  `Instrument`, by hand) — first-class spans make this expressible now.
- Auto cross-task propagation (per-task dispatch state) is deferred to when WASI
  threads / Wasm stack switching stabilize; it adds no API surface.

## Language Extensions

### Ambient effects (required)

An ordinary `Log` effect would force `with Log` through every caller up to `run`
— the infectiousness [Ambient Logging](./wep-2026-01-12-ambient-logging.md)
avoided for `log`/`panic`. An ambient effect keeps that while staying overridable:

```wado
#[ambient(default = default_handler)]
pub interface Log { /* … */ }
```

- not added to a function's required-effect set — `Log` ops impose no `with Log`,
  so the facade is callable anywhere;
- still installable with `with Log => h do` — layers, spans, and test sinks work
  as normal;
- a default handler runs when no subscriber is installed (dispatch global null →
  call `default` instead of trapping / a CM adapter); it uses the ambient I/O
  bypass, needing no effects.

A thin addition to existing dispatch
([Effect Handler](./wep-2026-04-11-effect-handler.md) § Dispatch): the per-op
dispatch already branches "global null → default path" (the CM adapter for world
imports; here the declared default). Effect-check skips ambient effects; an
installed handler's own effects (e.g. `TextSink`'s `Stderr`/`SystemClock`) are
still checked at the `with` site. Ambient effects are general (debug/trace/metrics
sinks, feature-flag oracles), not logging-specific.

### Span scoping — no language change required

The bundled handler form (`with &h do`) and value form already work, with scoped
install and early-exit restore, so layers and sinks need no new syntax. Span
entry needs `enter`/`exit` at scope boundaries, and the dispatch desugar has no
install/uninstall hook — covered by a closure:

```wado
pub fn in_span<T, effect E>(s: &Span, body: fn() -> T with E) -> T with E, Log {
    Log::enter(s.id());
    let r = body();        // the closure cannot skip the exit
    Log::exit(s.id());
    return r;
}
```

A native `with span do { … }` (desugaring to `enter; B; exit`, with `exit` on
every exit path via the restore injector) is an optional ergonomic upgrade so
control flow can escape directly to the enclosing function. Not required for v1.

### Deferred (performance-gated): efficient field passing

`List<Field>` boxes each value into a `core:value::Value`. For fixed call-site
fields, pass an anonymous struct bounded by `Serialize` instead:

```wado
info(`user logged in`, { user_id: id, ip: ip });   // anonymous struct, no Value boxing
```

This needs further extensions and would retire `Field`/`Value`:

- Anonymous (structural) structs auto-deriving `Serialize`/`Inspect` — nameless,
  so they also need bound-driven derivation
  ([Trait Derivation Policy](./wep-2026-06-25-trait-derivation.md)).
- Erased serde: a `dyn Serialize` payload (reference + monomorphized serialize
  funcref) across the non-generic `Log` op, bridged through `dyn Serializer` (the
  `erased_serde` pattern). Context merge then uses a serde flatten combinator,
  not yet supported.

Ship `List<Field>` first, measure, adopt erased serde only if the per-call boxing
shows up. Only the field argument changes, so it is an internal swap.
(Caller-resolved `#file`/`#line`/`#function` is already provided.)

## Consequences

### Benefits

- A full-set logger and tracer (events, spans, layered subscribers, two-axis
  filtering) from existing features plus ambient effects.
- Layer composition, scoped context, and auto context-restore from effect-handler
  nesting — static dispatch, no dynamic dispatch.
- Zero setup via the ambient default; a real subscriber is opt-in. Caller source
  location without macros; testable with a capturing sink.

### Trade-offs

- Ambient effects deliberately hide a sink's I/O from signatures — the
  ambient-logging trade-off, generalized.
- Eager message; the optimizer (not a macro) drops it when statically off,
  `enabled()` for runtime-off.
- Baseline fields box through `core:value`, removed by the deferred path.
- Auto span propagation is single-scope in v1; cross-task is explicit until the
  async story settles.

### Prerequisites

- [ ] Language: ambient effects with a default handler.
- [x] `core:value::to_value` (direct Value-building serializer).
- [x] Span scoping via the `in_span` closure (no language change).
- [ ] Optional: native `with <span> do { … }` sugar.
- [ ] `core:log`: the types and `Log` effect above, the facade
      (`trace`/`debug`/`info`/`warn`/`error`/`event`/`enabled`), `span`/`current`,
      sinks (`TextSink`, `JsonSink`, `NopSink`, `CaptureSink`), layers (`Context`,
      `Filter`), the `#[param]` level globals, and `default_handler`.
- [ ] Optional (ergonomics): bound-driven serde derivation
      ([Trait Derivation Policy](./wep-2026-06-25-trait-derivation.md)).
- [ ] Deferred (perf): anonymous structs, erased serde, serde flatten.
- [ ] Deferred (async): automatic cross-task current-span propagation.

## Alternatives Considered

### Ordinary (non-ambient) effect

A normal `Log` effect needs no new feature but is infectious across the whole
call graph — rejected for the reasons the ambient-logging WEP rejected effectful
`panic`.

### Global mutable subscriber

A `global mut` subscriber works without ambient effects, but scoped context
(spans, layers) then needs a hand-managed global stack with no automatic restore
on early exit. Ambient effects get scoped, auto-restored context from `with`.

### Panic on reentrancy

Rejected: it contradicts the never-fail policy and penalizes legitimate
composition; forwarding already terminates re-entrancy at the non-logging
default, and a depth-limited drop is the backstop.

### Field passing: `Value` vs token buffer vs erased serde

The ladder is `Value` (tagged-union tree, allocating) < flat token buffer (one
allocation, replayable) < erased serde (zero boxing/copy until bytes). The
baseline uses `Value`; the deferred path jumps to erased serde once profiling
justifies it. A generic `Log` op (`event<F: Serialize>`) was rejected as the
erasure route: the handler vtable is installed at the `with` site, which cannot
enumerate the `F` shapes used across the block's dynamic extent.

## References

- [Ambient Logging Functions](./wep-2026-01-12-ambient-logging.md)
- [Compile-Time Location Literals](./wep-2026-01-23-compile-time-location-literals.md)
- [Default Arguments](./wep-2026-04-11-default-arguments.md)
- [Effect System Design](./wep-2026-01-27-effect-system-design.md)
- [Effect Handler](./wep-2026-04-11-effect-handler.md)
- [Serialization and Deserialization (Serde)](./wep-2026-02-28-serde.md)
- [Compile-Time Parameters](./wep-2026-04-26-compile-time-params.md)
- [The Live ValueGraph](./wep-2026-06-15-live-value-graph.md)
- Rust [`tracing`](https://docs.rs/tracing), [`log`](https://docs.rs/log)
- Go [`log/slog`](https://pkg.go.dev/log/slog)
