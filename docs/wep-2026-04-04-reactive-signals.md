# Reactive Signals

**Status: Not yet implemented**

## Context

Modern UI frameworks (SolidJS, Svelte 5, Preact Signals, Vue, Angular) have converged on fine-grained reactivity via signals — a push-pull reactive primitive that tracks dependencies and propagates changes efficiently. Wado integrates this pattern as a first-class language feature with compile-time dependency analysis, eliminating the need for runtime auto-tracking.

### Prior Art

Most signal implementations use **runtime auto-tracking**: a global stack records which computed/effect is currently executing, and signals register themselves as dependencies when read. This enables dynamic dependencies (dependencies that change based on control flow) but has costs:

- Runtime overhead for stack management and subscription bookkeeping
- Memory allocation for dynamic subscriber sets
- Complexity in cleanup of stale subscriptions

Wado takes a different approach: **static dependency analysis** at compile-time, trading dynamic dependency precision for zero-cost dependency tracking and compiler optimization opportunities.

## Decision

### The `reactive` Keyword

**Source** values are mutable reactive state:

```wado
let reactive mut count = 0;

count = 5;          // Mutation triggers updates
count += 1;         // Also triggers updates
```

**Derived** values are computed from other reactive values:

```wado
let reactive doubled = || count * 2;
let reactive quadrupled = || doubled * 2;

// Reading derived values
let x = doubled;    // Returns current computed value
```

Derived values are lazily recomputed when their dependencies change and the derived value is read. The compiler statically analyzes dependencies and builds a dependency graph at compile-time.

### The `observe` Function

The `observe` function (from `core:reactive`) executes side effects when reactive dependencies change. Dependencies are automatically tracked — any reactive value read within the closure becomes a dependency.

```wado
use {observe} from "core:reactive";

let reactive mut count = 0;
let reactive doubled = || count * 2;

observe(|| {
    println(`Count is now: {count}`);
    // Dependencies: count
});

observe(|| {
    println(`Doubled is now: {doubled}`);
    // Dependencies: doubled (and transitively, count)
});
```

#### Cleanup

Return a cleanup function to run when the observation is disposed or before re-running:

```wado
observe(|| {
    let subscription = external_api.subscribe(`event-{count}`);
    println(`Subscribed to event-{count}`);

    return || {
        subscription.unsubscribe();
        println(`Cleaned up subscription for event-{count}`);
    };
});
```

The cleanup function runs:

- Before the effect re-runs (when dependencies change)
- When the enclosing scope ends
- When the component unmounts (in UI contexts)

#### Manual Disposal

```wado
let dispose = observe(|| {
    println(`Count: {count}`);
});

// Later, stop observing
dispose();
```

### Reactive References

Reactive values can be passed by reference to functions:

```wado
fn increment(counter: &reactive mut i32) {
    *counter += 1;  // Triggers updates in caller's scope
}

let reactive mut count = 0;
let reactive doubled = || count * 2;

increment(&reactive count);  // count becomes 1, doubled becomes 2
```

### Execution Semantics

Reactive behavior differs between execution contexts:

#### CLI World (Synchronous)

In CLI programs, reactive updates are **synchronous and immediate**:

```wado
use {observe} from "core:reactive";

let reactive mut count = 0;
let reactive doubled = || count * 2;

observe(|| {
    println(`doubled = {doubled}`);
});

count = 5;
// observe() callback runs immediately here, before next line
// Output: "doubled = 10"

println("after mutation");
// Output: "after mutation"
```

- Updates propagate immediately when a source is mutated
- observe() callbacks run synchronously before execution continues
- Observations live for the duration of their enclosing scope

#### Event-Looped World (Browser/GUI)

In event-driven contexts, reactive updates are triggered by **external events**:

```wado
use {observe} from "core:reactive";

fn Counter() -> Element with Dom {
    let reactive mut count = 0;
    let reactive doubled = || count * 2;

    observe(|| {
        println(`Count changed to {count}`);
    });

    return <div>
        <p>{doubled}</p>
        <button onclick={|_| count += 1}>+1</button>
    </div>;
}
```

- Updates are triggered by events (clicks, timers, network responses)
- Multiple mutations within a single event handler are **batched** (see Batching below)
- observe() callbacks and UI bindings persist for the component's lifetime
- The event loop keeps the program alive to receive future events

#### Comparison

| Aspect             | CLI                       | Event-looped                    |
| ------------------ | ------------------------- | ------------------------------- |
| Trigger            | Direct assignment in code | External events                 |
| Propagation        | Synchronous, immediate    | May be batched per event        |
| observe() lifetime | Enclosing scope duration  | Component/subscription lifetime |
| Primary use case   | Computed dependencies     | UI binding, subscriptions       |

### JSX Integration

Reactive values integrate seamlessly with JSX:

```wado
fn Counter() -> Element with Dom {
    let reactive mut count = 0;

    return <button onclick={|_| count += 1}>
        {count}
    </button>;
}
```

The compiler tracks that `{count}` depends on the reactive value and generates code to update only that text node when `count` changes — no virtual DOM diffing required.

`Reactive` is built into the language; no `with` declaration required.

### Internal Algorithm: Push-Pull with Static Dependencies

Wado's reactive system uses a **push-pull** algorithm with **compile-time dependency analysis**.

#### Two-Phase Update

When a source value is mutated, the update proceeds in two phases:

1. **Push phase (invalidation):** The mutation immediately marks all direct and transitive dependents as **dirty**. This propagation is synchronous and traverses the entire downstream dependency graph. No derived values are recomputed during this phase — only dirty flags are set.
2. **Pull phase (lazy re-evaluation):** Derived values are recomputed **only when read**. When a dirty derived value is accessed (by an `observe` callback, a JSX binding, or user code), it re-executes its computation function, caches the result, and clears its dirty flag. If a derived value is never read after being invalidated, it is never recomputed.

```wado
let reactive mut a = 1;
let reactive b = || a * 2;        // derived, depends on a
let reactive c = || a * 3;        // derived, depends on a
let reactive sum = || b + c;      // derived, depends on b and c

a = 10;
// Push phase: b, c, sum are all marked dirty (no recomputation yet)

let x = sum;
// Pull phase: sum is dirty → reads b (dirty → recomputes to 20) and c (dirty → recomputes to 30) → sum recomputes to 50
// If c were never read by anyone, it would not be recomputed
```

#### Static Dependency Analysis

Dependencies are resolved entirely at **compile-time**. The compiler analyzes which reactive values appear in each derived expression and `observe` body, and generates the dependency graph statically. There is no runtime dependency tracking (no global stack, no auto-tracking).

This means dependencies are **conservative**: if a derived expression references a reactive value inside a conditional branch, the dependency is always registered regardless of which branch executes at runtime.

```wado
let reactive mut flag = true;
let reactive mut a = 1;
let reactive mut b = 2;

let reactive derived = || {
    if flag { return a * 2; }
    return b + 1;
};
// Static dependencies: flag, a, b (all three, always)
// Mutating b marks derived as dirty even when flag is true
```

This over-subscription is acceptable because:

- The push phase only sets dirty flags (cheap).
- The pull phase skips recomputation if the result is unchanged (see equality check below).
- Static analysis enables compiler optimizations impossible with dynamic tracking (inlining, dead-code elimination of unused derived values, allocation-free dependency graphs).

#### Glitch-Free Guarantee

A **glitch** is an inconsistent intermediate state observed by an effect. Wado guarantees glitch-free execution: `observe` callbacks and JSX bindings never see a partially-updated dependency graph.

This is achieved by separating push and pull phases:

1. The push phase completes first, marking **all** transitive dependents as dirty.
2. The pull phase runs only within `observe` callbacks (or JSX re-renders), which are scheduled **after** the push phase completes.
3. During the pull phase, each derived value reads its dependencies, which themselves pull from their dependencies recursively. Because all dirty flags were set in step 1, the pull always traverses to the true sources and recomputes in a bottom-up (topological) order.

Diamond dependency example:

```wado
let reactive mut count = 0;
let reactive doubled = || count * 2;
let reactive tripled = || count * 3;
let reactive sum = || doubled + tripled;

observe(|| {
    println(`sum = {sum}`);
    // Never sees doubled-updated + tripled-stale:
    // Pull reads sum → pulls doubled (recomputes) → pulls tripled (recomputes) → sum recomputes
});

count = 10;
// Push: doubled, tripled, sum all marked dirty
// Pull (in observe): sum → doubled (20) + tripled (30) = 50 ✓
```

Because the dependency graph is static and known at compile-time, the compiler can verify at compile-time that a valid topological ordering exists (no cycles).

#### Equality Check (Change Propagation Cut-Off)

When a derived value is recomputed during the pull phase, the new value is compared to the cached previous value using `Eq`. If they are equal, the derived value's downstream dependents are **not** pulled — their dirty flags remain but will be cleared without recomputation when eventually read.

This prevents unnecessary cascading recomputation:

```wado
let reactive mut x = 5;
let reactive clamped = || i32::min(x, 10);  // clamps to max 10
let reactive display = || `value: {clamped}`;

observe(|| { println(display); });

x = 7;     // clamped: 5 → 7, display recomputes → "value: 7"
x = 100;   // clamped: 7 → 10, display recomputes → "value: 10"
x = 200;   // clamped: 10 → 10 (equal!), display is NOT recomputed
```

Equality check requires the derived type to implement `Eq`. For types without `Eq`, the derived value always propagates (no cut-off).

#### Batching

In the **CLI world**, each mutation triggers the push-pull cycle immediately and synchronously: the push phase runs, then all active `observe` callbacks are pulled.

In the **event-looped world** (browser/GUI), mutations within a single event handler are **batched**:

1. All mutations within the event handler run the push phase (dirty propagation) immediately.
2. The pull phase (re-evaluation of `observe` callbacks and JSX bindings) is deferred until the event handler returns.
3. This means multiple mutations to different sources result in a single pull pass, avoiding redundant recomputations.

```wado
fn handle_click() with Dom {
    // Event handler — mutations are batched
    count = count + 1;   // push: marks dependents dirty
    name = "Alice";      // push: marks dependents dirty
    // pull phase runs here, after handler returns
    // All observe callbacks and JSX bindings update once
}
```

In CLI world, explicit batching is available via `batch` from `core:reactive`:

```wado
use {batch} from "core:reactive";

batch(|| {
    count = 10;      // push only (dirty flags)
    other = 20;      // push only (dirty flags)
});
// pull phase runs here, after batch returns
```

## Consequences

- The `reactive` keyword, `observe`, and `batch` are new language/library primitives.
- Static dependency analysis means the compiler must perform a dataflow pass over reactive closures to determine which reactive values are referenced.
- Over-subscription from static analysis is mitigated by cheap dirty-flag propagation and equality-based cut-off.
- The push-pull model enables lazy evaluation — unused derived values are never recomputed.
- Glitch-free guarantee comes from phase separation, not topological sorting at runtime.
- `Eq` is leveraged for change propagation cut-off; types without `Eq` always propagate.
- JSX integration relies on the same push-pull mechanism — the compiler generates `observe`-like subscriptions for each reactive expression in JSX templates.

### TODO

- [ ] Implement `reactive` keyword in the parser and type checker
- [ ] Implement static dependency analysis pass
- [ ] Implement push-pull codegen (dirty flags, lazy re-evaluation)
- [ ] Implement equality-based propagation cut-off
- [ ] Implement `observe` in `core:reactive`
- [ ] Implement `batch` in `core:reactive`
- [ ] Implement reactive references (`&reactive mut T`)
- [ ] Integrate with JSX codegen
- [ ] Determine batching strategy for event-looped world
