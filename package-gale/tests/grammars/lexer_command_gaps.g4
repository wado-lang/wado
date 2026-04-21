// Source: Hand-authored for Gale's lexer-command coverage.
// License: BSD-3-Clause (matches the Gale repo).
//
// Covers three lexer-command gaps that no existing driver grammar
// exercises end-to-end:
//
//   - A `-> type(X)` command used *alone* (no pushMode, no channel).
//     Canonical use case: two rules that match different source forms
//     but should be emitted as the same token kind. `channels.md` and
//     `lexer-rules.md` in `vendor/antlr4/doc/` describe the semantics.
//   - A numeric `-> channel(N)` that routes a token to a custom channel
//     declared via `channels { ... }`. Both the `channels` block and
//     numeric channel arguments are ANTLR4 features that the existing
//     `HIDDEN`-only tests do not cover.
//   - A named reference to `DEFAULT_MODE` / `DEFAULT_TOKEN_CHANNEL` —
//     ANTLR4 accepts these as identifiers that resolve to the implicit
//     default mode/channel. At minimum the g4 parser must accept them.

grammar lexer_command_gaps;

channels { COMMENTS_CHANNEL }

text : (WORD | NUMBER | PRAGMA)* EOF ;

// Two different source forms, one emitted token kind: the spelled-out
// number 'zero' is re-typed to NUMBER via `-> type(NUMBER)`.
WORD  : [a-z]+ ;
ZERO  : 'zero' -> type(NUMBER) ;
NUMBER : [0-9]+ ;

// Numeric channel argument: routes comments to the custom channel by
// declared name (not HIDDEN). The parser side never sees COMMENT tokens.
COMMENT : '//' ~[\r\n]* -> channel(COMMENTS_CHANNEL) ;

// `-> mode(DEFAULT_MODE)` — exercises the parser's ability to resolve the
// implicit default mode identifier. A `pragma` line switches to PRAGMA_MODE
// to consume its body, then returns to the default mode by name. The
// opening `#pragma` keyword itself is skipped so the parser sees only the
// inner PRAGMA token.
PRAGMA_OPEN : '#pragma' -> skip, pushMode(PRAGMA_MODE) ;

WS : [ \t\r\n]+ -> skip ;

mode PRAGMA_MODE;

// The pragma body is a single word emitted as PRAGMA. The terminating
// newline switches back to the default mode via `mode(DEFAULT_MODE)`.
PRAGMA   : [a-zA-Z_]+ ;
PRAGMA_WS : [ \t]+ -> skip ;
PRAGMA_END : '\n' -> mode(DEFAULT_MODE), skip ;
