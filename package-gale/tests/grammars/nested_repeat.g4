// Source: hand-written regression grammar for Gale's repeat canonicalisation.
// License: same as the Gale package.
//
// A repeat over a repeat is a shape lowering cannot name, and surface g4
// cannot spell one (`e**` is a syntax error) — but two things produce it:
// parentheses around an already-repeating body, and the fold of `( X | )`
// onto an optional. Both collapse, by the identities the jar agrees with:
//
//   (X*)?    → X*   `q b` and `q a a b` both parse
//   (X+)?    → X*   zero-or-one of one-or-more is zero-or-more
//   ( X* | ) → X*   the fold mints an optional too, so it normalises too
//   (X+)*    → X*   the outer one is not always an optional: a closure over a
//   (X+)+    → X+   non-nullable repeat body is legal, so both reach here
grammar NestedRepeat;

s : 'p' (A*)? B
  | 'q' (A+)? B
  | 'r' ( A* | ) B
  | 's' (A+)* B
  | 't' (A+)+ B
  ;

A : 'a' ;
B : 'b' ;
WS : [ \t\r\n]+ -> skip ;
