// Lexer regression: a greedy `~(X)+` whose inner can also consume the
// suffix's first character. ANTLR4's lexer uses NFA→DFA longest-match
// (`vendor/antlr4/doc/wildcard.md`); a naive single-pass greedy loop
// would over-shoot and strand the suffix.
// `gen_lexer_repeat_lookahead_aware` implements the standard
// longest-match algorithm in single-pass forward scan with explicit
// accept-state tracking (no try-fail-retry backtracking).
//
// Source: derived from ANTLR4 runtime-testsuite descriptors
//   Sets/UnicodeNegatedBMPSetIncludesSMPCodePoints.txt
//   Sets/UnicodeNegatedSMPSetIncludesBMPCodePoints.txt
//
// License: BSD 3-Clause (vendor/antlr4/LICENSE.txt) — derived test grammar.

grammar LexerGreedySuffix;

text : LETTERS+ ;

// `~('b' | ' ')+ 'c'` — inner excludes 'b' and whitespace so adjacent
// LETTERS tokens do not collapse into one; suffix 'c' is still in the
// inner's complement so a purely greedy `+` would consume it.
LETTERS : 'a' ~('b' | ' ')+ 'c' ;

WS : ' ' -> skip ;
