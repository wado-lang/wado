// Source: Gale test fixture (a literal-deduped lexer rule referenced by another)
// License: same as the Gale package
//
// `K` has the same fixed text as the parser literal `'kw'`, so the dispatch
// path reaches it through the shared literal matcher. `Z` still references it,
// which needs `K`'s own matcher to exist.
grammar LexerLitDupRef;

s : 'kw' | Z ;

K  : 'kw' ;
Z  : 'z'+ K ;
WS : ' ' -> skip ;
