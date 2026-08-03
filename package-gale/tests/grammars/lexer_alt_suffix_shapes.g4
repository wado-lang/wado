// Source: Gale test fixture (lexer alternation whose suffix is not a plain
// char sequence)
// License: same as the Gale package
//
// The arm of an alternation followed by a suffix must be chosen by how far
// the WHOLE rule reaches (`lexer_alt_suffix_longest.g4`). That choice used to
// need a suffix built from single-char-consuming elements; a suffix holding a
// repeat, a fragment reference, or an alternation with a multi-element arm
// fell back to first-match and mis-lexed input ANTLR4 accepts.
//
// Each rule below is a shape outside the old window, on input where the first
// arm reaches less far than the second.
lexer grammar LexerAltSuffixShapes;

// Repeat suffix: `'a'` + no `'c'` reaches 1 char, `'ab'` + `'c'` reaches 3.
A : ('a' | 'ab') 'c'? ;

// Fragment-reference suffix: only the `'pq'` arm leaves an `'r'` behind.
B : ('p' | 'pq') R ;

// Alternation suffix with a multi-element arm: `'mn'` + `'q'` matches where
// `'m'` + (`'n' 'o'` | `'q'`) does not.
C : ('m' | 'mn') ('n' 'o' | 'q') ;

// The same window drives the greedy repeat's per-iteration peek: the loop
// must stop at the last position where `'e'? 'd'` still matches, which a
// plain greedy loop cannot do (it eats the final `'d'`).
D : 'd'+ 'e'? 'd' ;

fragment R : 'r' ;

J : . ;
