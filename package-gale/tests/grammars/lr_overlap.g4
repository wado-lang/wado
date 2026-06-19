// Source: hand-written for Gale's resilient-parser overlapping-LR tests.
// License: same as the Gale package.
//
// Two left-recursive alternatives share a first suffix token (`'['`), so the
// loop-entry dispatch is an overlap group: `'[' e ']'` (indexed) versus
// `'[' ']'` (empty index), disambiguated by the second token.
grammar LrOverlap;

e   : e '[' e ']'
    | e '[' ']'
    | INT
    ;

INT : [0-9]+ ;
WS  : [ \t\r\n]+ -> skip ;
