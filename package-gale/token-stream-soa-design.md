# Token-Stream SoA Decomposition — Design

Design for the Gale-side token-stream rework called for in
[`perf.md`](./perf.md) §1 ("Token-stream construction"). Scope: the
**generated parser runtime** and the generator that emits it. The
Wado-side alternative (extending `container_sroa`) is out of scope; it
stays tracked in `perf.md` §1 as the other path.

**No API-compatibility constraint.** Gale has zero external users, so this
design optimizes purely for parser speed and long-term maintainability. A
flatter, less convenient token API is an acceptable — preferred — trade
when it removes allocation. We are not preserving the `Token` struct or
field-access syntax.

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

### 2. There is no `Token` type on any path

`Token`, `LexerSlice`, and per-token `Span` are removed from storage. A
"token" is an `i32` index. Everything reads through `TokenStream`
methods, which are the **single definition of the layout** — the
generator emits calls to these, never raw array indexing, so adding or
moving a field touches one file:

```wado
impl TokenStream {
    pub fn len(&self)        -> i32 { return self.kinds.len(); }
    pub fn kind_at(&self, i: i32)  -> i32 { return self.kinds[i]; }
    pub fn start_at(&self, i: i32) -> i32 { return self.starts[i]; }
    pub fn end_at(&self, i: i32)   -> i32 { return self.ends[i]; }

    // Cold: materialize only for diagnostics / stringify.
    pub fn span_at(&self, i: i32) -> Span { return Span::new(self.starts[i], self.ends[i]); }
    pub fn push_text(&self, out: &mut String, i: i32) {           // no alloc; appends
        for let mut j = self.starts[i]; j < self.ends[i]; j += 1 { out.push(self.chars[j]); }
    }
    pub fn text_string(&self, i: i32) -> String {                 // cold, allocates
        let mut s = String::with_capacity(self.ends[i] - self.starts[i]);
        self.push_text(&mut s, i);
        return s;
    }
    pub fn is_empty_text(&self, i: i32) -> bool { return self.starts[i] >= self.ends[i]; }
    // trivia iteration over [triv_lo[i], triv_hi[i]) for to_lexer_string
}
```

Writes go through one method so the parallel arrays can never desync:

```wado
fn push_token(&mut self, kind: i32, start: i32, end: i32,
              triv_lo: i32, triv_hi: i32) { /* push to all five, in lockstep */ }
```

`Span` is kept (it is built per CST node and per error — not per token
read, so not hot) for `CstNode.span` and `ParseError.span`. `LexerSlice`
is deleted; its only consumers were token text, now served by
`push_text` / `text_string`.

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

// `advance` / `expect` deal in i32 token indices, not Token values.
fn advance(&mut self) -> i32 { let i = self.pos; self.pos += 1; return i; }

fn expect(&mut self, kind: i32) -> Result<i32, ParseError> {
    if self.tokens.kinds[self.pos] == kind {                           // hot: array.get i32
        if kind == TK_EOF { return Result::Ok(self.pos); }
        return Result::Ok(self.advance());
    }
    // cold error path
    let i = self.pos;
    let name = token_kind_name(kind);
    return Result::Err(self.error(
        `expected {name}, got \"{self.tokens.text_string(i)}\"`,
        self.tokens.span_at(i), [name]));
}
```

`peek` (which returned `&Token`) is removed; callers want either the kind
(`peek_kind`) or the index (`pos`). No method returns a `Token`.

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
EOF pushes one sentinel with `start == end == lexer.pos`. Freeze the
`List`s to `Array` (`to_array`) and move `lexer.chars` into the stream.

### 5. CST stores `i32` token indices

```wado
pub variant CstChild {
    Token(i32),       // was Token(Token) — now the token index
    Node(CstNode),
}
```

`CstNode` is otherwise unchanged (`name: String`, `span: Span`,
`children`). The terminal commit path stores an `i32`, eliminating the
per-terminal token copy. Functions that render text take the stream
explicitly — acceptable given no API constraint:

```wado
fn cst_to_string_tree_impl(out: &mut String, node: &CstNode, toks: &TokenStream) {
    // ... for Token(i) child: if !toks.is_empty_text(i) { out.push(' '); toks.push_text(out, i); }
}
pub fn to_string_tree(&self, toks: &TokenStream) -> String { ... }
```

The CST holds indices, so it is only meaningful alongside the
`TokenStream` it was built from. Callers keep the stream (the `Parser`
owns it) for as long as they use the tree. Generated **typed** nodes
likewise store `i32` token fields instead of `Token` (the
`gen_parse_fn_single_token` emitter stores the index from `expect`).

### 6. Generated `scan_*` / dispatch surface

All ~1100 `scan_*` sites and their `tokens[pos].kind` reads are
generator-emitted (`parser_gen.wado`), so this is a generator change:

- Signature: `&Array<Token>` → `&TokenStream` at the four
  `add_param("tokens", "&Array<Token>")` sites.
- Reads: route every emitted `tokens[pos].kind` / `tokens[pos+1].kind` /
  excluded-set compare through one helper that emits `tokens.kinds[<i>]`.

Cold consumers (`tools.wado` `to_lexer_string`/`emit_lexer_token`,
`highlight.wado`, trace `trace_tok`, error formatting) switch to
`text_string` / `push_text` / `span_at` / trivia iteration.

Gale's own front end (`token`/`lexer`/`parser`/`ir` import `lex.wado`)
lexes `.g4` at Gale **compile time**, off the runtime hot path. It moves
to the same `TokenStream` API. If that complicates the front end, give it
a small local value `Token` independent of the generated-parser stream —
but try the shared `TokenStream` first; one token representation is the
more maintainable end state.

## Expected payoff

- `tokenize`: 3–4 allocations/token → five `array.set i32`. Removes the
  bulk of the ~25% token-stream-construction bucket.
- `peek_kind` / scan dispatch: `array.get (ref) + struct.get` →
  `array.get i32`, the most frequent read in the parser.
- `last_end`: four-step chain → one `array.get i32` (`perf.md` §3 lever).
- CST terminals: deep `Token` copy → bare `i32`. No `Token` aggregate is
  allocated anywhere on a parse.

## Maintainability

The standing risk of parallel-array SoA is array desync and
field-addition friction. Contained by discipline, not by the type system:

- **One writer**: `push_token` / `push_trivia` are the only mutators;
  never push to an individual array. A post-`tokenize` debug assertion
  checks all parser arrays share a length.
- **One reader surface**: `kind_at` / `start_at` / `end_at` / `span_at` /
  `push_text` / trivia iteration are the only access points; the
  generator emits these, so the physical layout lives in `TokenStream`
  alone. Adding a per-token field = one new array + one accessor + one
  `push_token` parameter.
- **One token representation**: an `i32` index, everywhere (parser, CST,
  diagnostics, ideally the front end). No view type, no value/handle
  duality to keep in sync.

## Risks / open questions

- **Stream lifetime.** The CST holds `i32` indices and is meaningful only
  with its `TokenStream`. The `Parser` owns the stream; a consumer that
  wants a detached tree must keep the stream (or materialize text into the
  node, a deliberate cold copy). Document at the API.
- **Trivia round-trip.** `to_lexer_string` channel interleaving and
  `<EOF>` must stay byte-identical after flattening trivia. The driver
  tests' `to_lexer_string` / `to_string_tree` outputs are the oracle.
- **Front-end split.** Decide whether Gale's `.g4` front end shares
  `TokenStream` or keeps a local value `Token`. Prefer shared.

## Validation plan (TDD)

1. Land `TokenStream` (storage + accessors + `push_token`) and switch
   `tokenize` to emit primitives; assert `to_lexer_string` byte-identical
   on the corpus (red/green per `CLAUDE.md`).
2. Switch `Parser`, `gen_parser_struct`, and the scan/dispatch emitters to
   `i32`-index access; make `CstChild::Token` an `i32`; regenerate and run
   Layer 1–3 tests (`package-gale/CLAUDE.md`).
3. Re-profile `benchmark/sqlite_parse` (`perf.md` reproduce steps) and
   update `perf.md` §1/§3 with the measured delta.
