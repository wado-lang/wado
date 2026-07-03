// Source: Gale test fixture (Stage C predicate on a non-interchangeable group)
// License: same as the Gale package
//
// The two alts share the prefix `A B` but differ in length (`A B C`), so they
// do NOT tie — the longest-match tournament, not a grammar-order predicate
// chain, must disambiguate. A predicate on one alt must not hijack the group
// and commit to the shorter alt: on `a b c` the parser must take `A B C`.
grammar WadoPredMixed;

options { language = Wado; }

s : {true}? A B { p.emit("ab"); }
  | A B C { p.emit("abc"); }
  ;

A : 'a' ;
B : 'b' ;
C : 'c' ;
WS : ' ' -> skip ;
