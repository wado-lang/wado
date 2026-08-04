// Source: Gale test fixture (predicate after a greedy repeat)
// License: same as the Gale package
//
// A predicate positioned after a greedy repeat has nowhere to go in the
// peek/commit emit, so the repeat keeps the plain greedy loop. `A`'s suffix
// (`B`, a rule reference) is peekable and `[a-b]` can eat a `'b'` that `B`
// would take, so without the predicate the lookahead-aware emit fires here.
grammar LexerPredAfterRepeat;

options { language = Wado; }

s : A { p.emit("a"); }
  | J { p.emit("j"); }
  ;

A : [a-b]+ { pos > start }? B ;
B : 'b'* 'z' ;
J : . ;
