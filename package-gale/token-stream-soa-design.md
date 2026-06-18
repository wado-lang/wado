# Token-Stream SoA Decomposition — Design

Design for the Gale-side token-stream rework called for in
[`perf.md`](./perf.md) §1 ("Token-stream construction"). Scope: the
**generated parser runtime** and the generator that emits it. The
Wado-side alternative (extending `container_sroa`) is out of scope for
this document; it remains tracked in `perf.md` §1 as the other path.

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

`Parser.tokens` is `Array<Token>` (`parser_gen.wado:162`). This AoS
layout drives two of the top profile costs (`perf.md` live profile):

- **Per-token `struct.new Token` in `tokenize`** — each token allocates a
  `Token` plus a `LexerSlice`, a `Span`, and a `leading_trivia` `List`
  (`lexer_gen.wado:2108`). This is the ~25% "token-stream construction"
  bucket; the pre-size already killed `List<Token>::grow`, so what's left
  is the per-token allocation itself, not reallocation.
- **Pointer-chasing reads.** The hot path touches only `kind` and
  `span.end`, but pays AoS indirection for both:
  - `peek_kind` → `tokens[pos].kind`: `array.get (ref Token)` + write
    barrier-free but reference-load, then `struct.get kind`
    (`parser_gen.wado:246`).
  - `last_end` → `tokens[pos-1].span.end`: the four-step
    `Parser→Array→Token→Span→end` chain (`parser_gen.wado:276`),
    high purely by call frequency (`perf.md` §3).
  - `peek_at`, and every generated `scan_*` / parse dispatch site reading
    `tokens[pos].kind`.

The hot fields (`kind`, `span.start`, `span.end`) are `i32`. The cold
fields (`text`, `leading_trivia`, `channel`) are read only on the error,
trace, CST-stringify, and `to_lexer_string` round-trip paths — never in
scan/predict/dispatch.

## Goal

Store the hot fields as parallel primitive arrays so that:

- `peek_kind` / scan dispatch become a single `array.get i32` — no
  reference load, no `struct.get`.
- `last_end` becomes a single `array.get i32` — the load chain collapses
  (`perf.md` §3 is the same lever, paying off in two places).
- The lex loop pushes `i32`s into parallel arrays instead of allocating a
  `Token` + `LexerSlice` + `Span` + trivia `List` per token. The
  per-token `struct.new` disappears; the array store is a barrier-free
  `array.set i32`.

Keep the public `Token` API working for the cold consumers (CST
terminals, errors, highlight, `to_lexer_string`) by materializing a
`Token` **only at those boundaries**, never on the hot path.

## Design

### 1. `TokenStream`: the SoA storage

Replace `Array<Token>` with a `TokenStream` struct holding parallel
primitive arrays plus a single shared char buffer. All arrays are
indexed by the same token position `pos`.

```wado
pub struct TokenStream {
    // Hot, parallel, indexed by pos. Default-channel tokens only,
    // so `pos` keeps today's meaning and stays single-indexed.
    kinds:  Array<i32>,   // tokens[pos].kind
    starts: Array<i32>,   // tokens[pos].span.start  (== text.start)
    ends:   Array<i32>,   // tokens[pos].span.end    (== text.end)

    // Shared once for the whole stream (was duplicated into every
    // LexerSlice as `chars: &List<char>`).
    chars: List<char>,

    // Cold sidecar, indexed by pos. Built during lex, read only on
    // the round-trip / highlight paths.
    channels:  Array<i32>,
    triv_lo:   Array<i32>,  // [triv_lo[pos], triv_hi[pos]) into the flat
    triv_hi:   Array<i32>,  //   trivia SoA below
    // Flat trivia SoA (hidden-channel tokens), append order:
    triv_kinds:  List<i32>,
    triv_starts: List<i32>,
    triv_ends:   List<i32>,
    triv_chans:  List<i32>,
}
```

Notes:

- `text.start`/`text.end` and `span.start`/`span.end` are always equal in
  the current lexer (`lexer_gen.wado:2108` passes the same `tok_start` /
  `best_end` to both `slice` and `Span::new`), so a single `starts`/`ends`
  pair serves both. The `LexerSlice` struct is removed from per-token
  storage; text is reconstructed from `chars[starts[pos]..ends[pos]]` on
  demand.
- `leading_trivia: List<Token>` per token is replaced by a flat trivia
  SoA plus a per-token `[triv_lo, triv_hi)` range. The lexer appends
  trivia primitives and records the range — no per-token `List`
  allocation. Trivia is itself only hidden-channel tokens, and only
  `to_lexer_string` (`tools.wado:86`) consumes it.
- `channels` stays for `to_lexer_string` channel filtering; it is not on
  the parser hot path.

`TokenStream` exposes barrier-free hot accessors returning `i32`:

```wado
impl TokenStream {
    pub fn len(&self) -> i32 { return self.kinds.len(); }
    pub fn kind_at(&self, i: i32)  -> i32 { return self.kinds[i]; }
    pub fn start_at(&self, i: i32) -> i32 { return self.starts[i]; }
    pub fn end_at(&self, i: i32)   -> i32 { return self.ends[i]; }
}
```

### 2. `Token` becomes a boundary view, not the storage

`Token` is kept for the cold consumers, but demoted from "the storage
element" to a **view** over the stream:

```wado
pub struct Token {
    stream: &TokenStream,
    idx: i32,
}

impl Token {
    pub fn kind(&self)  -> i32 { return self.stream.kinds[self.idx]; }
    pub fn span(&self)  -> Span { return Span::new(self.stream.starts[self.idx],
                                                   self.stream.ends[self.idx]); }
    pub fn text(&self)  -> LexerSlice {
        return LexerSlice::new(&self.stream.chars,
                               self.stream.starts[self.idx],
                               self.stream.ends[self.idx]);
    }
    pub fn channel(&self) -> i32 { return self.stream.channels[self.idx]; }
    pub fn leading_trivia(&self) -> List<Token> { /* reconstruct from triv_lo/hi */ }
}
```

Because the view's only owned data is `&TokenStream` (a reference) plus
an `i32`, copying a `Token` under value semantics copies the reference,
not the tokens — so `CstChild::Token(Token)` (`cst.wado:12`) stores a
cheap handle instead of a deep-copied struct, and the per-terminal
`struct.new Token` on the commit path shrinks to a two-word view.

This trades **field syntax for accessor syntax**: `tok.kind` →
`tok.kind()`, `tok.span` → `tok.span()`, `tok.text` → `tok.text()`. That
is the main blast radius (see §5). Almost all sites are generator-emitted
strings, so the change is centralized.

The CST terminal must outlive the parse only as long as the stream does.
The `Parser` owns the `TokenStream` and the CST is built during the parse
while the `Parser` is alive, so the `&TokenStream` in each terminal view
is valid for the tree's lifetime. (If a caller needs a CST that outlives
the parser, that is already a deep copy today; the view requires the
stream be retained — call out in the API.)

### 3. `Parser` and its hot methods

```wado
pub struct Parser {
    pub tokens: TokenStream,   // was Array<Token>
    pub pos: i32,
    pub chars: List<char>,     // unchanged (error line:col); may alias tokens.chars
    pub pending: Option<ParseError>,
    // trace_depth / atn_stack / atn_ret_pending unchanged
}
```

Hot methods (emitted by `gen_parser_struct`):

```wado
fn peek_kind(&self) -> i32 { return self.tokens.kinds[self.pos]; }       // array.get i32

fn peek_at(&self, offset: i32) -> i32 {
    let idx = self.pos + offset;
    if idx >= self.tokens.len() { return TK_EOF; }
    return self.tokens.kinds[idx];                                        // array.get i32
}

fn last_end(&self) -> i32 {
    if self.pos == 0 { return 0; }
    return self.tokens.ends[self.pos - 1];                               // array.get i32 — chain gone
}

fn peek(&self) -> Token { return Token { stream: &self.tokens, idx: self.pos }; }  // view, cold

fn advance(&mut self) -> Token {
    let tok = Token { stream: &self.tokens, idx: self.pos };
    self.pos += 1;
    return tok;
}

fn expect(&mut self, kind: i32) -> Result<Token, ParseError> {
    if self.tokens.kinds[self.pos] == kind {                            // hot: array.get i32
        if kind == TK_EOF { return Result::Ok(Token { stream: &self.tokens, idx: self.pos }); }
        return Result::Ok(self.advance());
    }
    // cold error path: view -> .text()/.span()
    let tok = Token { stream: &self.tokens, idx: self.pos };
    let name = token_kind_name(kind);
    return Result::Err(self.error(`expected {name}, got \"{tok.text()}\"`, tok.span(), [name]));
}
```

`peek` changes return type from `&Token` to `Token` (a view is cheap to
return by value); callers that took `&Token` adjust to value.

### 4. `tokenize`: emit primitives, not structs

`tokenize` (`lexer_gen.wado:1871`) builds a `TokenStream` directly. The
pre-size moves onto the parallel arrays:

```wado
let cap = lexer.chars.len() / 4 + 1;
let mut kinds:  List<i32> = List::with_capacity(cap);
let mut starts: List<i32> = List::with_capacity(cap);
let mut ends:   List<i32> = List::with_capacity(cap);
let mut channels: List<i32> = List::with_capacity(cap);
// trivia ranges + flat trivia SoA likewise
```

Per accepted default-channel token, push four `i32`s (`array.set i32`,
no write barrier) and record the trivia range — replacing
`Token::with_trivia(best_kind, lexer.slice(...), Span::new(...), trivia)`
+ `tokens.push(tok)`. Skip / hidden-channel tokens append to the flat
trivia SoA instead of constructing a `Token` and a `leading_trivia`
`List`. EOF pushes one sentinel entry with `start == end == lexer.pos`.

Return `TokenStream { kinds: kinds.to_array(), ... , chars: lexer.chars }`.

### 5. Generated `scan_*` / dispatch and the rest of the surface

All `scan_*` signatures take `&Array<Token>` today (`parser_gen.wado:540`
et al., ~1100 call sites) and read `tokens[pos].kind`. Both are
generator-emitted:

- Signature: `&Array<Token>` → `&TokenStream` at the four
  `add_param("tokens", "&Array<Token>")` emit sites.
- Body reads: `tokens[pos].kind` → `tokens.kinds[pos]` at every emitter
  (`kind_check_str` callers, group dispatch `tokens[pos].kind`,
  lookahead `tokens[pos + 1].kind`, excluded-set compares). Centralize by
  routing all of them through one helper that emits `tokens.kinds[<i>]`.

Cold/boundary consumers switch to the view accessors:

- `cst.wado`: `t.text` → `t.text()` in `cst_to_string_tree_impl`;
  `CstChild::Token` now holds a view.
- `tools.wado` `to_lexer_string` / `emit_lexer_token`: read `tok.text()`,
  `tok.channel()`, and walk the trivia range instead of
  `tok.leading_trivia`.
- `highlight.wado`, trace (`trace_tok`).
- Gale's own front end (`token`/`lexer`/`parser`/`ir` import `lex.wado`):
  these lex `.g4` files at Gale **compile time**, off the runtime hot
  path. They move to the same accessors. If the churn there is large, the
  front end can keep a thin local `Token` value type independent of the
  generated-parser `TokenStream`; prefer the shared view first and only
  split if it complicates the front end.

## Expected payoff

- `tokenize`: per-token `struct.new Token` + `LexerSlice` + `Span` +
  trivia `List` (3–4 allocations/token) → four `array.set i32`. Removes
  the bulk of the ~25% token-stream-construction bucket.
- `peek_kind` / scan dispatch: `array.get (ref) + struct.get` →
  `array.get i32`. Removes a reference load on the most frequent read in
  the parser.
- `last_end`: four-step chain → one `array.get i32` (`perf.md` §3, no code
  change needed beyond this).
- CST terminals: deep `Token` copy → two-word view copy.

Cold paths (errors, `to_lexer_string`, highlight) reconstruct
`Token`/`LexerSlice`/`Span` on demand; they already materialize strings,
so the extra reconstruction is in the noise.

## Risks / open questions

- **API churn `tok.field` → `tok.field()`.** Centralized in the
  generator, but touches `cst.wado` / `tools.wado` / `highlight.wado` and
  the front end. Mechanical; covered by the existing driver and g4 tests.
- **View lifetime.** A CST holding `&TokenStream` requires the stream be
  retained for the tree's lifetime. True during parse; document for any
  consumer that wants a detached tree.
- **Front-end split.** Decide whether Gale's own `.g4` front end shares
  the view `Token` or keeps a local value `Token`. Start shared.
- **Trivia flattening correctness.** `to_lexer_string` round-trip
  (channel interleaving, `<EOF>`) must stay byte-identical. The driver
  tests' `to_string_tree` / `to_lexer_string` outputs are the oracle.
- **`advance` still mints one view per consumed token.** Cheap (two
  words, no sub-allocations), but if even that shows up, terminals can
  store a bare `idx: i32` and reconstruct the view lazily.

## Validation plan (TDD)

1. Land `TokenStream` + view `Token` with accessors behind the existing
   `lex.wado`/`cst.wado` APIs; keep all driver and g4 tests green
   (red/green per `CLAUDE.md`).
2. Rewrite `tokenize` to emit the SoA; assert `to_lexer_string` round-trip
   byte-identical on the corpus.
3. Switch `gen_parser_struct` + scan/dispatch emitters to the SoA reads;
   regenerate and re-run Layer 1–3 tests (`package-gale/CLAUDE.md`).
4. Re-profile `benchmark/sqlite_parse` (`perf.md` reproduce steps) and
   update `perf.md` §1/§3 with the measured delta.
