// Source: Gale test fixture (shapes the suffix peek must not claim)
// License: same as the Gale package
//
// The arm / iteration scoring peeks the suffix by emitting it a second time
// (`lexer_alt_suffix_shapes.g4`), so it may only claim a suffix the peek
// reproduces exactly, and it must know what the suffix can start with.
//
// `A` / `B`: the peek emitter has no non-greedy form — it lowers every repeat
// greedily — so a `.*?` under the suffix would be peeked as `.*` and swallow
// the terminator the commit still needs.
//
// `C`: `D` resolves its own `caseInsensitive`, so `D`'s first char set is
// `X` *and* `x`, which `[a-z]` overlaps. Reading it without the fold makes the
// two look disjoint and drops the greedy loop's lookahead.
lexer grammar LexerSuffixPeekLimits;

A : 'a'+ .*? 'b' ;
B : ('-' | '--') .*? 'z' ;
C : [a-z]+ D ;

D options { caseInsensitive = true; } : 'X' ;

J : . ;
