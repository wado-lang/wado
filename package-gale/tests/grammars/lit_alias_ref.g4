// Source: Gale test fixture (issue #1752 repro).
// License: same as the Gale package
//
// `'kw'` and `K` are two spellings of one token. Minting a separate token for
// `'kw'` makes the `K 'x'` alternative unreachable and leaves the trailing `x`
// unconsumed with no error.
grammar LitAliasRef;

s : 'kw' | K 'x' ;

K  : 'kw' ;
X  : 'x' ;
WS : ' ' -> skip ;
