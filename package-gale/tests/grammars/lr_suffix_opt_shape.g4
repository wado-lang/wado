// Source: hand-written for Gale's left-recursion tests.
// License: same as the Gale package.
//
// A shape-lookahead optional (`( 'a'? 'b' )?`) inside a left-recursive SUFFIX
// alternative. Which tokens may start the group depends on which nested
// optionals fire, so a one-token first-set check ({'a','b'}) enters on the bare
// `'a'` that belongs to the suffix's own trailing element — the op-only walker
// has to branch per shape, as the surface-element walker does.
grammar LrSuffixOptShape;

s : e EOF ;
e : ID
  | e 'in' ( 'a'? 'b' )? 'a'
  ;

ID : [a-z]+ ;
WS : [ \t\r\n]+ -> skip ;
