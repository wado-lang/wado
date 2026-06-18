# Token-Stream SoA Decomposition — Design

Design for the Gale-side token-stream rework called for in
[`perf.md`](./perf.md) §1 ("Token-stream construction"). Scope: the
**generated parser runtime** and the generator that emits it. The
Wado-side alternative (extending `container_sroa`) is out of scope; it
stays tracked in `perf.md` §1 as the other path.

**No API-compatibility constraint.** Gale has zero external users, so this
design optimizes purely for parser speed and long-term maintainability. A
flatter, less convenient token API is an acceptable — preferred — trade
when it removes allocation. We are not preserving the generated-parser
`Token` field-access syntax.

**Status: implemented.** The generated-parser path stores tokens in a
`TokenStream` (SoA) and hands terminals to the CST as a two-word `Tok`
view (`&TokenStream` + `i32` index). The lexer allocates no per-token
aggregate and every scan/dispatch read is a single `array.get i32`; the
front end keeps the value-typed `Token`. The fully-flat variant — bare
`i32` in the CST with the stream threaded through the walk — is recorded
below as a possible follow-up, deferred because the `Tok` view keeps the
CST self-contained (no walk/`to_tree`/`to_string_tree` signature churn)
while capturing the dominant win.

## Problem

Tokens are stored array-of-structs (AoS):

```wado
// src/runtime/lex.wado
pub struct Token {
    pub kind: i32,
    pub text: LexerSlice,            // { chars: &List<char>, start: i32, end: i32 }
    pub span: Span,                  // { start: i32, end: i32 }
    pub leading_trivia: List<Token>,
    pub channel: i32,
}
```

`Parser.tokens` is `Array<Token>` (`parser_gen.wado:162`), and the CST
stores tokens by value (`CstChild::Token(Token)`, `cst.wado:12`). This
drives two top profile costs (`perf.md` live profile):

- **Per-token `struct.new Token` in `tokenize`** — each token allocates a
  `Token` + a `LexerSlice` + a `Span` + a `leading_trivia` `List`
  (`lexer_gen.wado:2108`). The ~25% token-stream-construction bucket;
  `grow` is already pre-sized away, so what remains is the allocation
  itself.
- **Pointer-chasing reads** for fields that are all `i32`:
  - `peek_kind` → `tokens[pos].kind`: `array.get (ref Token)` +
    `struct.get` (`parser_gen.wado:246`).
  - `last_end` → `tokens[pos-1].span.end`: the four-step
    `Parser→Array→Token→Span→end` chain (`parser_gen.wado:276`).
  - every generated `scan_*` / dispatch site reading `tokens[pos].kind`
    (~1100 sites).

Only `kind`, `span.start`, `span.end` (all `i32`) are touched in
scan/predict/dispatch. `text`, `leading_trivia`, `channel` are read only
on the error / trace / CST-stringify / `to_lexer_string` paths.

## Goal

A token is an **`i32` index** into a struct-of-arrays. No `Token`
aggregate is constructed on any parse path — not in the lexer, not in the
parser, not in the CST. Concretely:

- The lexer pushes primitives into parallel `i32` arrays; zero per-token
  allocation.
- `peek_kind` / `last_end` / scan dispatch become a single `array.get
  i32` — no reference load, no `struct.get`, no load chain.
- CST terminals store an `i32` token index, not a copied struct.
- `text` / `span` / `channel` / trivia are reconstructed on demand from
  the stream, only on the cold diagnostic and round-trip paths.

## Design

### 1. `TokenStream`: the only token representation

`Array<Token>` is replaced by a `TokenStream` of parallel primitive
arrays plus one shared char buffer. All parser-visible arrays are indexed
by the token position `pos`.

```wado
pub struct TokenStream {
    // Hot, parallel, indexed by pos. Default-channel tokens only, so pos
    // keeps today's meaning and every hot read is a single direct index.
    kinds:  Array<i32>,   // kind_at(pos)
    starts: Array<i32>,   // start_at(pos)  (== text/span start)
    ends:   Array<i32>,   // end_at(pos)    (== text/span end)

    // Shared once for the whole stream (was copied into every LexerSlice).
    chars: List<char>,

    // Cold side SoA: hidden/skip "trivia" tokens, flat in source order.
    // Each parser token owns the half-open range [triv_lo, triv_hi).
    triv_lo:     Array<i32>,   // indexed by pos
    triv_hi:     Array<i32>,   // indexed by pos
    triv_kinds:  Array<i32>,   // flat
    triv_starts: Array<i32>,
    triv_ends:   Array<i32>,
    triv_chans:  Array<i32>,
}
```

Why this shape:

- `text.start/end` and `span.start/end` are always equal in the current
  lexer (`lexer_gen.wado:2108` passes the same `tok_start`/`best_end` to
  both `slice` and `Span::new`), so one `starts`/`ends` pair serves text
  and span. `LexerSlice` and per-token `Span` storage disappear.
- `starts` and `ends` stay **separate** arrays (not interleaved, not a
  `Span` array): `last_end` touches only `ends`, `peek_kind` only
  `kinds`, so each hot read stays on its own dense cache line.
- The parser stream holds **default-channel tokens only**, so `pos`
  indexes the hot arrays directly — no filter map, no skip loop on the
  hot path. Hidden/skip tokens (today's `leading_trivia`) move to the
  flat trivia side SoA, which only `to_lexer_string` reads. This is why
  there is no `channels` array on the main stream: every main-stream
  entry is channel 0; the channel id lives on the trivia entries
  (`triv_chans`).

### 2. Terminals are a `Tok` view; `Token` stays only for the front end

The generated parser never builds a `Token` aggregate. Hot reads go
through the stream's `i32` arrays. A terminal that is committed into the
CST is wrapped in a two-word **`Tok` view** — `{ stream: &TokenStream,
idx: i32 }` — so the CST stays self-contained (text/span/trivia render
from the view without threading the stream through the walk). `Token` /
`LexerSlice` remain the value-typed vocabulary for Gale's own `.g4`
front end (which has its own hand-written lexer and no `TokenStream`),
untouched by this change.

`TokenStream`'s accessors are the **single definition of the layout** —
the generator and `Tok` read through them (hot reads touch the `pub`
`kinds`/`starts`/`ends` arrays directly for a guaranteed single
`array.get i32`), so adding or moving a field touches one file:

`chars` is a `&List<char>` borrow of the lexer's source list (the
`LexerSlice` pattern), so the stream costs no char copy.

```wado
impl TokenStream {
    pub fn len(&self) -> i32 { return self.kinds.len(); }
    pub fn kind_at(&self, i: i32) -> i32 { return self.kinds[i]; }

    // Cold: materialize only for diagnostics / stringify.
    pub fn span_at(&self, i: i32) -> Span { return Span::new(self.starts[i], self.ends[i]); }
    pub fn is_empty_text(&self, i: i32) -> bool { return self.starts[i] >= self.ends[i]; }
    pub fn push_text(&self, out: &mut String, start: i32, end: i32) {   // no alloc
        for let mut j = start; j < end; j += 1 { out.push(self.chars[j]); }
    }
    pub fn token_text(&self, i: i32) -> String { /* cold, allocates */ }
    // trivia: the flat triv_* arrays, sliced by [triv_lo[i], triv_hi[i])
}
```

Writes go through one pair of methods so the parallel arrays can never
desync (`trivia_mark()` returns the next trivia index, which `tokenize`
records as a token's `triv_lo`/`triv_hi`):

```wado
fn push_token(&mut self, kind, start, end, triv_lo, triv_hi) { /* all five, in lockstep */ }
fn push_trivia(&mut self, kind, start, end, channel) { /* the flat triv_* arrays */ }
```

The CST terminal handle is a two-word **view** over the stream:

```wado
pub struct Tok { pub stream: &TokenStream, pub idx: i32 }
impl Tok {
    pub fn new(stream: &TokenStream, idx: i32) -> Tok with stores[stream] { ... }
    pub fn kind(&self) -> i32 { return self.stream.kinds[self.idx]; }
    pub fn span(&self) -> Span { return self.stream.span_at(self.idx); }
    pub fn text(&self) -> String { return self.stream.token_text(self.idx); }
    pub fn push_text(&self, out: &mut String) { ... }
    // is_empty_text / triv_lo / triv_hi / push_leading_trivia_text
}
```

`Span` is kept (built per CST node and per error — not per token read).
`LexerSlice` stays for the front end; the generated path renders text via
`TokenStream::push_text` / `Tok::text`.

### 3. `Parser` and its hot methods

```wado
pub struct Parser {
    pub tokens: TokenStream,           // was Array<Token>
    pub pos: i32,
    pub pending: Option<ParseError>,
    // trace_depth / atn_stack / atn_ret_pending unchanged
    // NOTE: the separate `chars: List<char>` field is dropped; error
    // line:col resolves against `tokens.chars`.
}
```

```wado
fn peek_kind(&self) -> i32 { return self.tokens.kinds[self.pos]; }      // array.get i32

fn peek_at(&self, offset: i32) -> i32 {
    let idx = self.pos + offset;
    if idx >= self.tokens.len() { return TK_EOF; }
    return self.tokens.kinds[idx];                                      // array.get i32
}

fn last_end(&self) -> i32 {
    if self.pos == 0 { return 0; }
    return self.tokens.ends[self.pos - 1];                              // chain gone
}

// `advance` / `expect` build a `Tok` view only at the commit boundary —
// never on the speculative scan path, which reads `kinds[pos]` directly.
fn advance(&mut self) -> Tok { let i = self.pos; self.pos += 1; return Tok::new(&self.tokens, i); }

fn expect(&mut self, kind: i32) -> Result<Tok, ParseError> {
    if self.tokens.kinds[self.pos] == kind {                           // hot: array.get i32
        if kind == TK_EOF { return Result::Ok(Tok::new(&self.tokens, self.pos)); }
        return Result::Ok(self.advance());
    }
    // cold error path
    let name = token_kind_name(kind);
    return Result::Err(self.error(
        `expected {name}, got \"{self.tokens.token_text(self.pos)}\"`,
        self.tokens.span_at(self.pos), [name]));
}
```

`peek` (which returned `&Token`) is replaced by `peek_start` / `peek_span`
(i32 / Span) plus `peek_kind`. `expect` / `advance` / `match_*` return a
`Tok` view, so each leaf emit site (`let f = p.expect(kind)?;`) and node
field is unchanged apart from the field's type (`Token` → `Tok`).

### 4. `tokenize`: emit primitives into a `TokenStream`

`tokenize` (`lexer_gen.wado:1871`) builds a `TokenStream` directly. The
pre-size moves onto the parallel arrays:

```wado
let cap = lexer.chars.len() / 4 + 1;
// kinds / starts / ends / triv_lo / triv_hi : List<i32> with_capacity(cap)
// triv_kinds / triv_starts / triv_ends / triv_chans : List<i32> (flat)
```

Per accepted default-channel token: record the current trivia range, then
push `kind`, `start`, `end`, `triv_lo`, `triv_hi` — five `array.set i32`,
no write barrier, replacing
`Token::with_trivia(kind, lexer.slice(...), Span::new(...), trivia)` +
`tokens.push(tok)`. Skip / hidden-channel tokens append to the flat
trivia arrays instead of building a `Token` and a `leading_trivia` `List`.
EOF pushes one sentinel with `start == end == lexer.pos`. The `List<i32>`
arrays stay as-is; the stream is returned by value.

### 5. CST stores a `Tok` view (self-contained)

```wado
pub variant CstChild {
    Token(Tok),       // was Token(Token) — now a two-word view
    Node(CstNode),
}
```

`CstNode` is unchanged (`name`, `span`, `children`). The terminal commit
path stores a `Tok` (idx + `&stream`) instead of copying a full `Token`,
so the lexer + parse path allocate no per-token aggregate. Because the
view carries the stream, `to_string_tree` / the `Visitor` /
`TreeRecorder` / generated walker / `to_tree` keep their existing
signatures — **no stream threading, no driver-test churn**:

```wado
fn cst_to_string_tree_impl(out: &mut String, node: &CstNode) {
    // for Token(t) child: if !t.is_empty_text() { out.push(' '); t.push_text(out); }
}
pub fn to_string_tree(&self) -> String { ... }      // unchanged signature
pub trait Visitor { fn visit_token(&mut self, token: &Tok) { } }
```

The CST's `Tok`s reference the stream, so the tree is meaningful as long
as the stream is reachable; GC keeps it alive (the `Parser` owns it, and
each `Tok` references it). This mirrors the existing `LexerSlice` holding
`&List<char>`.

### 6. Generated `scan_*` / dispatch surface

All ~1100 `scan_*` sites and their `tokens[pos].kind` reads are
generator-emitted (`parser_gen.wado`), so this is a generator change:

- Signature: `&Array<Token>` → `&TokenStream` at the
  `add_param("tokens", "&Array<Token>")` sites (and `atn.wado` /
  `follow.wado` scan helpers).
- Reads: every emitted `tokens[pos].kind` / `tokens[pos + d].kind` /
  excluded-set compare becomes `tokens.kinds[<i>]`.

Cold consumers (`tools.wado` `to_lexer_string`, `highlight.wado`, trace
`trace_tok`, error formatting) switch to `Tok` accessors / the stream's
`token_text` / `push_text` / `span_at` / flat trivia ranges.

Gale's own front end (`g4/lexer`, `g4/parser`, `g4/token`) keeps the
value-typed `Token` (its own hand-written `.g4` lexer has no
`TokenStream`); it is off the runtime hot path and untouched.

## Expected payoff

- `tokenize`: 3–4 allocations/token → five `array.set i32`. Removes the
  bulk of the ~25% token-stream-construction bucket.
- `peek_kind` / scan dispatch: `array.get (ref) + struct.get` →
  `array.get i32`, the most frequent read in the parser.
- `last_end`: four-step chain → one `array.get i32` (`perf.md` §3 lever).
- `_gale_new_parser`: drops the redundant `input.chars().collect()`
  (`perf.md` §5) — the stream borrows the lexer's chars.
- CST terminals: deep `Token` copy → two-word `Tok` view; no per-token
  aggregate is built in the lexer or on the scan path.

### Possible follow-up: bare `i32` in the CST

Storing a bare `i32` (no `Tok` view) would also remove the per-terminal
two-word view, at the cost of threading `&TokenStream` through the
walker / `to_tree` / `to_string_tree` / `Visitor` (the CST is no longer
self-contained) and updating the driver-test call sites. Deferred: the
view captures the dominant win (lexer allocation + i32 hot reads) with a
self-contained CST, and the residual two-word commit-path alloc is far
below the eliminated per-token aggregates.

## Maintainability

The standing risk of parallel-array SoA is array desync and
field-addition friction. Contained by discipline, not by the type system:

- **One writer**: `push_token` / `push_trivia` are the only mutators;
  never push to an individual array. With every parallel array advanced
  in lockstep by a single method, the arrays cannot desync by
  construction — no length invariant needs runtime enforcement. (Wado has
  only always-on `assert`, not a debug-only tier, so a per-`tokenize`
  length check would be permanent cost for an invariant the writer already
  guarantees; cover it once in a unit test instead.)
- **One reader surface**: the `pub` hot arrays plus `span_at` /
  `is_empty_text` / `push_text` / `token_text` / flat trivia ranges (and
  the `Tok` accessors that wrap them) are the only access points, so the
  physical layout lives in `TokenStream` alone. Adding a per-token field =
  one new array + one accessor + one `push_token` parameter.
- **One generated-path representation**: a `TokenStream` index, wrapped in
  a `Tok` view only where the CST needs a self-contained handle. The front
  end's value `Token` is a separate, unchanged type.

## Risks / open questions

- **Stream lifetime.** Each CST `Tok` references its `TokenStream`. The
  `Parser` owns the stream; GC keeps it alive while the tree references it
  (same as `LexerSlice` holding `&List<char>` today). A consumer wanting a
  tree detached from its stream is not supported by the view.
- **Trivia round-trip.** `to_lexer_string` channel interleaving and
  `<EOF>` stay byte-identical after flattening trivia — verified by the
  `runtime_test` and antlr4-compat `_tokens_test` oracles.

## Validation (done, TDD)

1. `TokenStream` (storage + `push_token`/`push_trivia` + accessors) unit
   tested in `runtime_test.wado`.
2. `tokenize` emits primitives; `Parser` / scan / dispatch read
   `tokens.kinds[…]`; terminals are `Tok`; `cst`/`tools`/`highlight`/`atn`/
   `follow` and the hand-written + generated consumers updated. Calculator,
   runtime, and the driver suite (json/html/css3/sqlite/typescript/
   highlight/antlr4) pass; `runtime_test` 36/36.
3. Re-profile `benchmark/sqlite_parse` (`perf.md` reproduce steps) and
   update `perf.md` §1/§3 with the measured delta. *(pending)*
