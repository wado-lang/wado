// Source: hand-written for Gale's left-recursion tests.
// License: same as the Gale package.
//
// A shape-lookahead optional (`( 'a'? 'b' )?`) inside a left-recursive SUFFIX
// alternative. Which tokens may start the group depends on which nested
// optionals fire, so a one-token first-set check ({'a','b'}) enters on the
// bare `'a'` that belongs to the suffix's own trailing element. The op-only
// walker that drives LR-suffix bodies has to read the per-shape lookahead the
// same way the surface-element walker does.
grammar LrSuffixOptShape;

s : e EOF ;
e : ID
  | e 'in' ( 'a'? 'b' )? 'a'
  ;

ID : [a-z]+ ;
WS : [ \t\r\n]+ -> skip ;
