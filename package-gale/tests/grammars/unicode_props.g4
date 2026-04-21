// Source: Hand-authored for Gale's Unicode property coverage.
// License: BSD-3-Clause (matches the Gale repo).
//
// ANTLR4 supports Unicode property escapes `\p{Name}` inside character
// sets. Common use cases: `\p{L}` (any letter), `\p{Nd}` (decimal
// digit), `\p{Zs}` (space separator). These expand into character
// ranges at grammar-parse time — see `expand_unicode_property` in
// `g4/parser.wado`.
//
// The existing `src/g4/parser_test.wado` verifies the ranges are
// emitted correctly, but no driver-level test exercises the match
// path with non-ASCII input. This grammar plugs that gap.

grammar unicode_props;

line : (WORD | NUM | SPACE)+ EOF ;

WORD  : [\p{L}]+ ;
NUM   : [\p{Nd}]+ ;
SPACE : [\p{Zs}] -> skip ;
