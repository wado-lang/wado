# Design: integer token identity in the Gale generator

## Problem

The generator identifies terminals by `String` name (`TK_FOO`, `TK_LIT_...`)
everywhere: FIRST / FOLLOW / kind / sync sets are `List<String>`, and hot paths
rebuild `` `TK_{name}` `` / `literal_const_name(text)` per call. On keyword-heavy
grammars this dominates `gale gen` time — the copying collector re-traces
thousands of small `String` objects (measured: css3 2.4s GC vs SQLite 24s GC;
GC scales with distinct-token count, not output size). Two naive fixes failed:

- **Lazy interner + cache** (`String→i32` `TreeMap`, id cached on nothing): a
  net _loss_ — per-call `intern` is a String tree-lookup that costs more than
  the compare it replaced, and it still rebuilds `` `TK_{name}` `` strings.
  SQLite copying 61s → 80s, null 38.5s → 48s.

The lesson: interning only pays when it is **not** in the hot path. The token's
integer identity must be resolved **once** and then carried — never looked up or
re-stringified during analysis.

## Enabler: the canonical id already exists

`lexer_gen.wado::token_slot_order(lexer_rules, lit_tokens) -> List<TokenSlot>`
is the canonical token numbering: **slot `i` has kind id `i`**, and that `i` is
exactly the value emitted for the `TK_*` global (`gen_token_constants`,
lexer_gen.wado:215). So every terminal already _has_ a dense integer identity —
the pipeline just discards it and carries the name instead. Nothing new to
number; we thread the id that already exists.

`TokenSlot` covers every terminal: `Rule(i)` (lexer rule), `Eof`, `Error`,
`Lit(i)` (literal token). FIRST sets only ever contain `` `TK_{tokenref.name}` ``,
`literal_const_name(literal.text)`, and `TK_EOF` — all of which are slots, so
every set element maps to a canonical id.

## Core model

**Token identity in all analysis (FIRST/FOLLOW/kind/sync/prediction) is the
`i32` kind id.** The `TK_*` name is materialised only at codegen emit, from a
single id→name table.

### 1. `TokenKinds` table (built once, in `codegen`)

Built from `token_slot_order` right after grammar assembly:

```
struct TokenKinds {
    names: List<String>,        // id -> "TK_*" name   (names[i] = slot i's name)
    by_name: TreeMap<String, i32>,  // "TK_*" name -> id  (for resolution only)
}
```

`names` is the _only_ place `TK_*` strings live long-term — one shared table,
replacing the thousands of copies now in the caches. Passed into `GenContext`.

### 2. The token _is_ an integer: `kind_id` on the terminal IR

Add a resolved id to the terminal elements (same pattern as the already-cached
`TokenRefElement.lower_name` / `field_name`):

```
TokenRefElement { …, kind_id: i32 = -1 }
LiteralElement  { …, kind_id: i32 = -1 }
```

Resolve in **one pass** over all rule elements (in `codegen`, after
`token_slot_order` is known, before lowering):

- `TokenRef(t)` → `by_name["TK_" + t.name]`
- `Literal(l)` → `by_name[literal_const_name(l.text)]`

After this pass `first_of_element(TokenRef)` is `[elem.kind_id]` — no string
built, no map lookup, no intern. This is the "token itself is an integer" step,
and the reason it beats caching: resolution is amortised to once per element at
build time, so the hot recursion is pure integer work.

### 3. Set representation

- **FIRST sets**: `List<i32>` (kind ids), **insertion order preserved** (dedup
  via a `seen` bitset, O(1)). Cache `first_cache: List<Option<List<i32>>>`.
- **FOLLOW**: a bitset over the canonical kind ids (`follow_env`; landed
  2026-08 — the private lazy interner is gone).
- **kind / sync / follow-mask registries**: store `List<i32>`.

## Byte-identity strategy

Output must stay byte-identical for every grammar. Three ordering rules:

1. **FIRST-set order = insertion order of ids** — identical to today's
   insertion order of names (ids ↔ names are 1:1), so emit through `names`
   reproduces the same sequence.
2. **Kind-set canonical order**: canonicalisation must still order **by name**
   (kind ids sort in declaration order, not name order). Landed 2026-08 as
   `GenContext::canonical_kind_ids`: a lazily rebuilt `kind_rank` table
   (`rank[id]` = lexicographic rank of the name) sorts the ids with integer
   compares, and the joined id list is the registry key. Rank order equals
   name order, so canonical order and helper ids match the former name sort.
3. **Emit boundary**: `kind_check_str` / `first_check_str` /
   `intern_kind_set` / `dump` take `List<i32>` and stringify via
   `TokenKinds.names`. `p.peek_kind() == TK_FOO` is reproduced because `TK_FOO`'s
   value _is_ `kind_id`, but we emit the **name** (not the number) for identity.

## Emit boundary — the only stringify sites

After the refactor, `TK_*` strings are produced only at:
`intern_kind_set` / `kind_check_str` / `kind_negate_check_str` (kind-set
helpers), the sync-set and follow-mask emit, and `dump`. Everything upstream is
integer.

## Phased plan (tree stays green; each phase byte-identity-checked on css3 +

SQLite md5; GC measured `--collector copying` vs `null` on SQLite)

- **P0 — table + ids, unused.** Build `TokenKinds` in codegen; add `kind_id` to
  the two elements; one resolution pass; pass table to `GenContext`. No analysis
  change yet → byte-identical by construction.
- **P1 — gen_context FIRST → `List<i32>`.** `first_of_*` use `elem.kind_id`;
  `first_cache` int. Keep thin `first_of_*` String wrappers (via `names`) so
  unconverted consumers still build; they are removed in P4.
- **P2 — prediction → int.** `sll_*`, `build_sll_node`, `try_expand_opaque`,
  `PredictionBranch.tokens` (stringify at the few construction sites via table).
- **P3 — lower + FOLLOW + kind/sync sets → int.** `compute_call_site_follow`,
  follow masks, `intern_kind_set`/`kind_check_str` accept `List<i32>`
  (canonicalise by name here); `follow_env` uses canonical ids.
- **P4 — alt_grouping + dump → int; drop the String wrappers.** Measure final GC.

Measure the win as the `copying − null` GC delta shrinking (not wall-time on a
small grammar). Expect the biggest effect on TS/Rust (keyword-dense, GC-bound).

## Reuse (don't reinvent)

- **`atn::build_token_kinds`** (atn.wado:244) already builds exactly the
  `name→id` `TreeMap` from `collect_literal_tokens` + `token_slot_order`. Promote
  it (plus an `id→name` `List`) into the shared `TokenKinds` table; the ATN
  builder then consumes the same table instead of rebuilding it.
- **`single_token_first` / `element_token_const`** (gen_util.wado:471-477)
  already centralise `TokenRef→"TK_name"` / `Literal→const_name`; the `kind_id`
  resolution pass routes through them, and open-coded `` `TK_{t.name}` `` sites
  (enumerated: lower, parser_gen, prediction, gen_context, atn, dump) migrate to
  reading `elem.kind_id`.
- **`follow_env`'s private `TokenInterner`** — deleted (2026-08); the FOLLOW
  bitsets index the canonical kind-id space directly.

## Risks / open points

- **Resolution completeness.** Every terminal shape must resolve: `TokenRef`,
  `Literal`, literals hoisted from `~'x'` (`Not` inner) into lit tokens, and
  `EOF`. `collect_lits_from_element` already enumerates the literal set, so the
  lit-token list is complete before resolution.
- **`kind_id = -1` guard.** Wildcard/`Not`/unresolved contribute the empty FIRST
  set (as today); assert no reachable terminal keeps `-1`.
- **Numeric `-> type(N)` tokens** (lexer_gen.wado:236) get extra `TK_<N>`
  globals but never appear in a parser FIRST set, so they need no id in the
  analysis table.
- **`GenContext` currently lacks `lexer_rules`.** The table is built in codegen
  (which has the `Grammar`) and injected, so `token_slot_order`'s inputs are
  available at construction.
