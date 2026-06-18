# Token-Stream SoA Decomposition — Design

Gale-side rework called for in [`perf.md`](./perf.md) §1 ("Token-stream
construction"). Scope: the **generated-parser path** and the generator
that emits it. The Wado-side alternative (extending `container_sroa`)
stays in `perf.md` §1 as the other path.

**No API-compatibility constraint.** Gale has zero external users, so this
optimizes purely for parser speed and long-term maintainability. A
flatter, less convenient token API is the preferred trade when it removes
allocation.

**Status: implemented (ideal form).** A token is a bare `i32` index into a
`TokenStream` (struct-of-arrays). No per-token aggregate is allocated
anywhere — not in the lexer, not on the scan path, not in the typed AST,
not in the generic CST. `parse()` returns `ParseResult<T> { root, tokens }`
bundling the typed tree with the stream its `i32` terminals index. The
front end keeps the value-typed `Token`.

## Problem

Tokens were stored array-of-structs (AoS): `Parser.tokens: Array<Token>`
with `Token { kind, text: LexerSlice, span: Span, leading_trivia:
List<Token>, channel }`, and the CST/typed nodes stored `Token` by value.
This drove two top profile costs (`perf.md`):

- **Per-token `struct.new Token` in `tokenize`** — each token allocated a
  `Token` + `LexerSlice` + `Span` + a `leading_trivia` `List` (~25%
  bucket).
- **Pointer-chasing reads** of fields that are all `i32`: `peek_kind` →
  `array.get (ref) + struct.get`; `last_end` → the four-step
  `Parser→Array→Token→Span→end` chain; every `scan_*` site reading
  `tokens[pos].kind` (~1100 sites).

Only `kind` / `span.start` / `span.end` (all `i32`) are read in
scan/predict/dispatch. `text` / `leading_trivia` / `channel` are read only
on the error / trace / stringify / `to_lexer_string` paths.

## Design

### 1. `TokenStream`: struct-of-arrays storage

```wado
pub struct TokenStream {
    // Hot, parallel, indexed by token position. Default-channel tokens
    // only, so `pos` stays single-indexed and every hot read is one get.
    pub kinds: List<i32>,
    pub starts: List<i32>,
    pub ends: List<i32>,
    // Per-token leading-trivia range into the flat triv_* arrays.
    pub triv_lo: List<i32>,
    pub triv_hi: List<i32>,
    // Borrow of the lexer's source chars (the LexerSlice pattern; no copy).
    pub chars: &List<char>,
    // Flat trivia SoA (hidden / skip tokens), source order.
    pub triv_kinds: List<i32>,
    pub triv_starts: List<i32>,
    pub triv_ends: List<i32>,
    pub triv_chans: List<i32>,
}
```

- `text.start/end` equal `span.start/end` in the lexer, so one
  `starts`/`ends` pair serves both; `LexerSlice` and per-token `Span`
  storage disappear.
- `starts`/`ends` stay **separate** arrays (not a `Span` array): `last_end`
  touches only `ends`, `peek_kind` only `kinds` — each hot read on its own
  dense cache line.
- The stream holds **default-channel tokens only**; `leading_trivia` moves
  to the flat `triv_*` arrays addressed per token by `[triv_lo, triv_hi)`.
  Only `to_lexer_string` / highlight read trivia.

Writes go through one pair of methods, so the parallel arrays cannot
desync by construction (`trivia_mark()` is the next trivia index, recorded
as a token's `triv_lo`/`triv_hi`):

```wado
fn push_token(&mut self, kind, start, end, triv_lo, triv_hi) { /* all five, lockstep */ }
fn push_trivia(&mut self, kind, start, end, channel) { /* the flat triv_* arrays */ }
```

Cold accessors materialize only for diagnostics / stringify: `span_at`,
`is_empty_text`, `push_text(out, start, end)`, `token_text(i)`,
`push_token_text(out, i)`, `push_leading_trivia_text(out, i)`.

### 2. A token is an `i32`; `ParseResult` bundles tree + stream

`Token` / `LexerSlice` remain the value-typed vocabulary for Gale's own
`.g4` front end (its hand-written lexer has no `TokenStream`). The
generated path never builds a `Token` aggregate — a token is a bare `i32`
index. The typed tree and the stream it indexes travel together:

```wado
pub struct ParseResult<T> {
    pub root: T,            // the typed start-rule node; terminals are i32
    pub tokens: TokenStream,
}
```

`parse(input) -> Result<ParseResult<StartNode>, ParseError>`. Because a
terminal is a bare index, the tree is meaningful only alongside its
stream; bundling them keeps every consumer self-contained with no
per-terminal allocation.

### 3. `Parser` and its hot methods

```wado
pub struct Parser { pub tokens: TokenStream, pub pos: i32, pub pending: Option<ParseError>, ... }

fn peek_kind(&self) -> i32 { return self.tokens.kinds[self.pos]; }        // array.get i32
fn last_end(&self)  -> i32 { if self.pos == 0 { return 0; } return self.tokens.ends[self.pos - 1]; }
fn peek_at(&self, off: i32) -> i32 { let i = self.pos + off; if i >= self.tokens.len() { return TK_EOF; } return self.tokens.kinds[i]; }
fn peek_start(&self) -> i32 { return self.tokens.starts[self.pos]; }
fn peek_span(&self)  -> Span { return self.tokens.span_at(self.pos); }

// commit methods deal in i32 indices
fn advance(&mut self) -> i32 { let i = self.pos; self.pos += 1; return i; }
fn expect(&mut self, kind: i32) -> Result<i32, ParseError> {
    if self.tokens.kinds[self.pos] == kind {
        if kind == TK_EOF { return Result::Ok(self.pos); }
        return Result::Ok(self.advance());
    }
    let name = token_kind_name(kind);
    return Result::Err(self.error(
        `expected {name}, got \"{self.tokens.token_text(self.pos)}\"`,
        self.tokens.span_at(self.pos), [name]));
}
```

`match_any` / `match_not` / `match_set` / `fail` likewise return `i32`. The
separate `Parser.chars` field is dropped; error `line:col` resolves against
`tokens.chars`, which also removes the redundant `input.chars().collect()`
in `_gale_new_parser` (`perf.md` §5). `_gale_run` wraps the typed root and
the parser's stream into a `ParseResult`.

### 4. `tokenize`: emit primitives

`tokenize` builds a `TokenStream` (`TokenStream::new(&lexer.chars)`,
pre-sized to `chars.len()/4 + 1`). Per accepted default-channel token it
calls `push_token`; skip / hidden-channel tokens call `push_trivia` — flat
`i32` writes, no aggregate. EOF is one sentinel with `start == end`. The
trivia accumulator (`leading_trivia: List<Token>` reset per token) is gone.

### 5. CST: bare `i32` terminals, stream threaded through the walk

```wado
pub variant CstChild { Token(i32), Node(CstNode) }

pub struct CstNode { pub name: String, pub span: Span, pub children: List<CstChild>, pub toks: &TokenStream }

pub trait Visitor {
    fn enter_rule(&mut self, rule_id: i32, name: &String, span: &Span) {}
    fn exit_rule(&mut self, rule_id: i32, name: &String, span: &Span) {}
    fn visit_token(&mut self, toks: &TokenStream, idx: i32) {}
}
```

The generated walker threads the stream: `walk_X(v, toks, node)`, emitting
`v.visit_token(toks, <idx>)` and `walk_Y(v, toks, …)`. `to_tree(result:
&ParseResult<Start>)` runs the walk over `&result.tokens` / `&result.root`
and builds a `CstNode` tree; each `CstNode` carries `toks` (a borrow), so
`to_string_tree(&self)` and `unparse_xml(&CstNode)` stay self-contained —
they read terminal text/span from `node.toks` by index. `CstNode` storing
a `&TokenStream` mirrors `LexerSlice` holding `&List<char>`;
`tree_build_node` / `TreeRecorder::new` carry `with stores[toks]`.

### 6. Generated `scan_*` / dispatch surface

Generator-emitted (`parser_gen.wado` + `atn.wado` / `follow.wado` scan
helpers): `&Array<Token>` → `&TokenStream`; every `tokens[pos].kind` /
`tokens[pos + d].kind` becomes `tokens.kinds[<i>]`. Typed-node terminal
fields and the single-token node's `token` field are `i32`. Cold consumers
(`tools.wado` `to_lexer_string`, `highlight.wado`, trace, error
formatting) read via the stream's flat arrays.

## Payoff

- `tokenize`: 3–4 allocations/token → flat `i32` writes. Removes the bulk
  of the ~25% token-stream-construction bucket.
- `peek_kind` / scan dispatch: `array.get (ref) + struct.get` →
  `array.get i32`, the most frequent read in the parser.
- `last_end`: four-step chain → one `array.get i32` (`perf.md` §3).
- `_gale_new_parser`: drops the redundant `input.chars().collect()`
  (`perf.md` §5).
- Typed AST + CST terminals: bare `i32` — **zero per-terminal allocation**
  on any parse.

## Maintainability

Parallel-array SoA risks array desync and field-addition friction —
contained by discipline:

- **One writer**: `push_token` / `push_trivia` advance every array in
  lockstep, so the arrays cannot desync by construction; no runtime length
  invariant needed (Wado has only always-on `assert`, not a debug tier — a
  unit test covers it).
- **One reader surface**: the `pub` hot arrays plus `span_at` /
  `is_empty_text` / `push_text` / `token_text` / flat trivia ranges; the
  physical layout lives in `TokenStream` alone. Adding a per-token field =
  one array + one accessor + one `push_token` parameter.
- **One generated-path representation**: an `i32` index, everywhere
  (parser, typed AST, CST, diagnostics). The front end's value `Token` is a
  separate, unchanged type.

## Risks / open questions

- **Stream lifetime.** `ParseResult` and each `CstNode` reference the
  `TokenStream`; GC keeps it alive while the tree references it (same as
  `LexerSlice` holding `&List<char>`). A token index is meaningless without
  its stream — they are bundled so this is not exposed.
- **Trivia round-trip.** `to_lexer_string` channel interleaving and `<EOF>`
  stay byte-identical after flattening trivia — verified by `runtime_test`
  and the antlr4-compat `_tokens_test` oracles.

## Validation (done, TDD)

1. `TokenStream` (storage + `push_token`/`push_trivia` + accessors)
   unit-tested in `runtime_test.wado`.
2. `tokenize` emits primitives; `Parser` / scan / dispatch read
   `tokens.kinds[…]`; terminals are `i32`; `cst`/`tools`/`highlight`/`atn`/
   `follow` + walker + `to_tree` + `ParseResult` threaded; hand-written and
   generated consumers updated. Green: `runtime_test` (36), `atn_sim_test`
   (11), and the driver suite (calculator/json/html/sqlite/typescript/
   css3/highlight/antlr4/non-greedy typed-AST/error-recovery).
3. Re-profile `benchmark/sqlite_parse` (`perf.md` reproduce steps) and
   update `perf.md` §1/§3 with the measured delta. *(pending)*
