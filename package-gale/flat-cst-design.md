# Design: flat event-stream CST (SSOT)

Design for the [`perf.md`](./perf.md) direction *"flat event-stream CST
(SSOT) — measured ~5× on syntax_highlight"*. Read `perf.md` first for the
measurements and the two failed cursor spikes this design supersedes.

**Status: landed.** `benchmark-syntax-highlight` went 29.5 → 9.9 ms/iter
copying (~3.0× wall), GC portion 21.5 → 4.1 ms (~5.2×, no longer GC-bound);
`sqlite_parse` build-only did not regress. All 1845 package-gale tests pass.
One deviation from the sketch below: Wado has no by-value `self` / move
(methods are `&self` / `&mut self`), so `finish` cannot move the columns out of
the builder — it copies `tag`/`a`/`alt` into the store (exact-sized, transient,
free under `copying`) rather than the zero-copy hand-off the "no second arena"
note imagines. The event stream is still the sole representation (no node tree,
no `BuildEvent` log); only the finalize hand-off copies.

## Goal

`syntax_highlight` is GC-bound: the copying collector re-traces the whole live
CST every cycle, and the CST is thousands of small WasmGC objects
(`CstNode` + a per-node `List<CstChild>` + `CstChild` payload boxes, plus the
`List<BuildEvent>` it is built from). Replace all of that with a **flat
i32-column event stream that is itself the tree** — a handful of arrays, so the
live-object count the collector traces collapses from thousands to ~7.

### Measured baseline (this task, debug `wado`, `-O2` guest)

`benchmark-syntax-highlight`, 13366-byte SQLite fixture:

| collector           |     ms/iter | throughput | note                        |
| ------------------- | ----------: | ---------: | --------------------------- |
| `copying` (default) | ~29.5       | ~453 KB/s  | headline                    |
| `null` (no GC)      | ~8.0        | ~1.66 MB/s | pure compute + bump-alloc   |
| **GC portion**      | **~21.5 (~73%)** | —     | `copying − null`            |

Target: collapse the ~21.5 ms GC portion. The perf.md prototype (40× fixed
driver) took `copying` 20.3 → 3.9 ms (~5.2×), GC 15.2 → 0.6 ms, and `null`
also dropped (no tree build, no walk-time iterator box).

## Why the two prior cursor spikes lost, and how this avoids both

The failed *"flat green-tree + cursor"* spike (perf.md "Failed approaches")
lost on both benchmarks for two reasons, both structurally avoided here:

1. **Value-cursor re-boxing.** A `Cst` / `CstChild` value per visited node — and
   every Wado struct is a WasmGC object — moved allocation from build-time to
   walk-time instead of removing it. Here traversal threads `(&store, i: i32)`
   as **unbundled scalars** (columns by reference, index a bare `i32`); no
   node / cursor struct is ever constructed, so the walk allocates nothing.
2. **`array.new_default` zero-fill of an over-sized second arena.** A loose
   `List::with_capacity(n)` zero-fills via `array.new_default`; nine over-sized
   arrays dominated. Here the parser's event stream **is** the store — there is
   no second arena and no copy from event-log to tree — and the columns **grow
   by `push`** exactly as `List<BuildEvent>` does today (never
   `with_capacity`-oversized). This is the same grow-by-push shape the prototype
   measured at 3.9 ms.

## The store: three raw columns + finalized aggregates

`TreeBuilder` replaces its `List<BuildEvent>` with parallel `List<i32>`
columns. One **row** per event, in the same pre-order the parser already emits.

Raw columns, written during the parse:

| column | `E_OPEN`         | `E_CLOSE`  | `E_TOK` / `E_MISS` / `E_SKIP` |
| ------ | ---------------- | ---------- | ----------------------------- |
| `tag`  | `E_OPEN`         | `E_CLOSE`  | the row's tag                 |
| `a`    | node kind        | end offset | token index                   |
| `b`    | start offset     | (open kind, stamped in finalize) | — |

Finalized aggregate columns, meaningful on `E_OPEN` rows, filled by one linear
`finish()` pass (zero elsewhere):

| column  | meaning on an `E_OPEN` row                                             |
| ------- | --------------------------------------------------------------------- |
| `end`   | `span.end` (max of close offset and every child end)                  |
| `flags` | `NODE_ERROR` / `NODE_INCOMPLETE`                                       |
| `alt`   | labeled-alternative index (`-1` when unset)                           |
| `next`  | row index just past this node's whole subtree (O(1) sibling skip)     |

Seven parallel `List<i32>`. The struct that owns them:

```wado
pub struct CstStore {
    tag: List<i32>, a: List<i32>, b: List<i32>,
    end: List<i32>, flags: List<i32>, alt: List<i32>, next: List<i32>,
}
```

Tags (replacing the `BuildEvent` variant):

```wado
global E_OPEN: i32 = 0;   // a = kind, b = start
global E_CLOSE: i32 = 1;  // a = end offset
global E_TOK: i32 = 2;    // a = token index
global E_MISS: i32 = 3;   // a = synthetic token index
global E_SKIP: i32 = 4;   // a = skipped token index
```

A **node** is identified by the row index of its `E_OPEN`. The **root** is row
`0`. `K_ERROR`, `NODE_ERROR`, `NODE_INCOMPLETE` keep their current values.

## `TreeBuilder`: byte-identical driver

The generated parser calls exactly the same methods with the same signatures,
so **the emitted driver is byte-for-byte unchanged** — only `tree.wado`'s
implementation changes. The builder keeps the columns plus a small
`open: List<i32>` stack of currently-open row indices (needed by `set_alt`).

| method                          | new implementation                                             |
| ------------------------------- | -------------------------------------------------------------- |
| `start_node(kind, start)`       | push row `(E_OPEN, kind, start)`; `open.push(row)`             |
| `start_error(start)`            | push row `(E_OPEN, K_ERROR, start)`; `open.push(row)`          |
| `finish_node(end)`              | push row `(E_CLOSE, end, 0)`; `open.pop()`                     |
| `token(idx)`                    | push row `(E_TOK, idx, 0)`                                     |
| `missing(idx)`                  | push row `(E_MISS, idx, 0)`                                    |
| `skip(idx)`                     | push row `(E_SKIP, idx, 0)`                                    |
| `set_alt(alt)`                  | `alt[open.last()] = alt`                                       |
| `checkpoint()`                  | `tag.len()`                                                    |
| `start_node_at(cp, kind, start)`| `insert` an `E_OPEN` row into every column at `cp`; `open.push(cp)` |
| `truncate(cp)`                  | pop every column back to len `cp`; pop `open` entries `>= cp`  |
| `finish()`                      | run the finalize pass (below), return the `CstStore`          |

`start_node_at` is the LR left-associative wrap (precedence climbing). At the
call site the atom subtree is already closed, so every row in `[cp, len)` is
balanced and the live `open` stack holds only ancestors with index `< cp`.
Inserting at `cp` therefore never shifts an index that is on the stack, and the
new row at `cp` is greater than every ancestor, so `open.push(cp)` keeps the
stack monotonic. (This is why aggregation must be deferred — see below.)

## The finalize pass — the single aggregation point

**Why not patch incrementally in `finish_node`/`set_alt`.** `start_node_at`
retro-inserts a wrap node *after* its children have already closed. If
`NODE_ERROR` bubbled eagerly on each `finish_node`, the atom's error would have
already bubbled to the *ancestor* and the later-inserted wrap would start clean
— losing the error the node tree correctly inherits (the tree bubbles child
`NODE_ERROR` into every parent in `tree_build_node`). Deferring **all**
aggregation to one pass over the final row order sidesteps this entirely: the
inserted wrap row is in place, its subtree follows it, and the pass sees the
true structure. This pass is the flat analogue of today's `tree_build_node`,
minus the allocation — it only writes i32 columns.

```wado
fn finalize(store: &mut CstStore) {
    let mut stack: List<i32> = [];               // open row indices
    for let mut r = 0; r < store.tag.len(); r += 1 {
        let t = store.tag[r];
        if t == E_OPEN {
            store.flags[r] = if store.a[r] == K_ERROR { NODE_ERROR } else { 0 };
            store.alt[r] = store.alt[r];         // -1 unless set_alt wrote it
            store.end[r] = store.b[r];           // provisional = start
            stack.push(r);
        } else if t == E_MISS {
            let oi = stack[stack.len() - 1];
            store.flags[oi] = store.flags[oi] | NODE_INCOMPLETE | NODE_ERROR;
        } else if t == E_SKIP {
            let oi = stack[stack.len() - 1];
            store.flags[oi] = store.flags[oi] | NODE_ERROR;
        } else if t == E_CLOSE {
            let oi = stack.pop();
            if store.a[r] > store.end[oi] { store.end[oi] = store.a[r]; }
            store.b[r] = store.a[oi];            // stamp open kind for the walk
            store.next[oi] = r + 1;              // subtree extent
            if stack.len() > 0 {
                let pi = stack[stack.len() - 1];
                if store.end[oi] > store.end[pi] { store.end[pi] = store.end[oi]; }
                store.flags[pi] = store.flags[pi] | (store.flags[oi] & NODE_ERROR);
            }
        }
        // E_TOK: nothing — tokens never extend a node's end (matches tree_build_node)
    }
}
```

This reproduces `tree_build_node` exactly: `span.end` = max(close offset, child
ends); `NODE_ERROR` bubbles up (but `NODE_INCOMPLETE` does not — it stays on the
node with the `Missing` child); `alt` defaults to `-1`. The `start_node_at` unit
test (`1+2+3` → `(expr (expr (expr 1) + (expr 2)) + (expr 3))`) is the load-
bearing case for the deferred bubble and stays green.

`next` and the close-row kind stamp (`b`) are the only *new* information; both
are free by-products of a pass we already run, and both are pure O(1) query
enablers, so **no information and no O(1) query is lost** versus the node tree.

## Consumers: functions over `(&store, i)`

Every consumer becomes a free function over the shared store and a row index —
no owned node, no deep copy. A subtree is a **view** (an index into the shared
store), strictly better for the read-only consumers that exist today.

**O(1) queries** (replacing `CstNode` methods):

```wado
pub fn cst_kind(s: &CstStore, i: i32) -> NodeKind { return s.a[i]; }
pub fn cst_span(s: &CstStore, i: i32) -> Span { return Span::new(s.b[i], s.end[i]); }
pub fn cst_alt(s: &CstStore, i: i32) -> i32 { return s.alt[i]; }
pub fn cst_is_error(s: &CstStore, i: i32) -> bool { return s.flags[i] & NODE_ERROR != 0; }
pub fn cst_is_incomplete(s: &CstStore, i: i32) -> bool { return s.flags[i] & NODE_INCOMPLETE != 0; }
```

**`highlight_walk`** (the ~29% frame) — a flat forward scan, no recursion, no
per-node struct, allocates nothing. `hl_exit` fires on close only when the
matching open was a rule (kind stamped in `b` by finalize):

```wado
pub fn highlight_walk(v: &mut HighlightVisitor, toks: &TokenStream, s: &CstStore) {
    for let mut r = 0; r < s.tag.len(); r += 1 {
        let t = s.tag[r];
        if t == E_OPEN {
            if s.a[r] != K_ERROR { v.hl_enter(s.a[r]); }
        } else if t == E_CLOSE {
            if s.b[r] != K_ERROR { v.hl_exit(); }
        } else {
            v.hl_visit_token(toks, s.a[r]);   // E_TOK / E_MISS / E_SKIP
        }
    }
}
```

**`to_string_tree` / `to_string_subtree`** — recursion over the row index using
`next` for child boundaries; still allocation-free (scalars + `&store`):

```wado
fn cst_to_string_tree_at(out, s, i, toks, rule_names, token_names) {
    out.push_str(&"(");
    if s.a[i] == K_ERROR { out.push_str(&"<error>"); }
    else { out.push_str(&rule_names[s.a[i]]); }
    let mut c = i + 1;
    while c < s.next[i] {                       // direct children only
        let t = s.tag[c];
        if t == E_OPEN { out.push_str(&" "); cst_to_string_tree_at(out, s, c, ...); c = s.next[c]; }
        else if t == E_TOK { /* text, if non-empty */ c += 1; }
        else if t == E_MISS { /* <missing NAME> */ c += 1; }
        else if t == E_SKIP { /* <skip text> */ c += 1; }
        else { c += 1; }                        // E_CLOSE of a child handled via next
    }
    out.push_str(&")");
}
```

**`find_child(s, i, kind)`** — skip-scan direct children via `next`, O(children):

```wado
pub fn cst_find_child(s: &CstStore, i: i32, kind: NodeKind) -> i32 {   // -1 = none
    let mut c = i + 1;
    while c < s.next[i] {
        if s.tag[c] == E_OPEN { if s.a[c] == kind { return c; } c = s.next[c]; }
        else { c += 1; }
    }
    return -1;
}
```

**`ParseResult`** owns the store; the root is row 0:

```wado
pub struct ParseResult {
    pub cst: CstStore,
    pub tokens: TokenStream,
    pub diagnostics: List<Diagnostic>,
    pub output: String = "",
}
```

`NodeKind` and its name-aware `Display` / `Inspect` (emitted by `cst_gen`) are
unchanged — they key off the kind `i32`, which is now `s.a[i]`.

## Codegen emitter changes

Small, mechanical — the driver body is untouched; only the tree type and the
consumer call shapes move.

- **`parser_gen.wado`** — `ParseResult { root: p.b.finish(), ... }` becomes
  `ParseResult { cst: p.b.finish(), ... }`. The `Parser.b: TreeBuilder` field and
  every `p.b.*` call site are unchanged. (`finish()` now returns `CstStore`.)
- **`cst_gen.wado`** — `gen_alt_enums` emits
  `pub fn <rule>_alt(cst: &CstStore, node: i32) -> Option<…>` reading
  `cst.alt[node]`. `gen_string_tree_helper` calls the index-based
  `to_string_tree_at(&result.cst, 0, …)` and the index-based `find_child`.
- **`highlight_gen.wado`** — `highlight_walk(&mut visitor, &result.tokens,
  &result.cst)`.
- **`tree.wado` / `highlight_walk.wado` / `highlight.wado`** — rewritten as
  above. `tree.wado` still always-emitted; the `highlight*` fragments stay
  gated on the `highlight` option.

Gale's own g4 front end does **not** use this runtime tree (it lowers to
`Grammar` IR), so it needs no change. No hand-written code constructs
`CstNode` / `CstChild`; every `tests/generated/*` occurrence is the inlined
runtime and regenerates.

## Migration & verification

Single cutover (no parallel "old tree + new flat" bridge — more code, more risk
than one representation), per perf.md's execution note. Retire `CstNode` /
`CstChild` / `BuildEvent` / `tree_build_node`.

1. Rewrite `src/runtime/tree.wado` (store + builder + finalize + cursor
   functions) and its unit tests in `tree_test.wado`, converting each
   `tree.children[k]` / `tree.is_error()` / `to_string_tree` assertion to the
   `(&store, index)` form. Keep every existing case — flat node, nested spans,
   `Missing`/`Skipped`/`K_ERROR` bubbling, and the `start_node_at` left-assoc
   wrap — they pin the finalize semantics. **Red/green TDD:** write the failing
   store test first, then implement.
2. Rewrite `highlight_walk.wado`, `highlight.wado` (only `highlight_walk`'s
   signature changes), and the three emitters.
3. Regenerate the `tests/generated/*` goldens and the driver-test / format
   fixtures (`mise run` golden + format tasks via `on-task-done`). Driver goldens
   (`to_string_tree` S-expressions) must be **byte-identical** — the render is
   unchanged, only its backing store is. Add driver coverage over
   error-recovery input exercising `E_MISS` / `E_SKIP` / `K_ERROR` columns.
4. Re-measure with `--collector null,copying` on **both** `syntax_highlight`
   (expect `copying` ≈ 29.5 → ~6 ms on this host, GC portion → near-zero) **and**
   `sqlite_parse` (build-then-`ok()`: it pays column zero-fill for less GC
   benefit — verify it does **not** regress; the store grows by `push`, so there
   is no oversized `array.new_default` fill).

## Risks

- **`sqlite_parse` (build-only) regression.** The build path now writes 7
  columns instead of one `List<BuildEvent>` and runs the finalize pass, but
  never allocates the node tree it used to. Grow-by-push keeps zero-fill bounded
  (the trap the earlier spike hit was `with_capacity`, not `push`). Gate the
  land on `sqlite_parse` staying at ~2.4 ms/iter (its perf.md baseline).
- **Finalize correctness under `start_node_at`.** Fully covered by the existing
  left-assoc wrap test plus new recovery-column driver tests; the deferred
  single-pass bubble is the reason this is correct where an eager patch is not.
- **Column count / zero-fill.** Four aggregate columns (`end`/`flags`/`alt`/
  `next`) are zero on non-`E_OPEN` rows. The prototype measured this shape at
  3.9 ms, so it is not a lever; if it ever shows, the aggregates can move to a
  compact side table keyed by open-row ordinal, but do not pre-optimize.
