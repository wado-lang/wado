// Source: hand-written regression grammar for Gale's `+` first iteration.
// License: same as the Gale package.
//
// A `+`'s mandatory first iteration is a required position, and what decides
// it is prediction, not validation. Measured against the published jar:
//
//   ( A t | B t )+  on `a`    → the lookahead alone picks `A t`, so ANTLR4
//                               commits and reports what the body could not
//                               match: `missing ';' at '<EOF>'`.
//   ( A B | A C )+  on `a a`  → both alternatives start with `A` and neither
//                               can match, so prediction itself fails and
//                               ANTLR4 reports a no-viable-alternative.
//
// The scan that guards the iterations after the first answers a different
// question — "may I go round again" — and must not decline the first one.
grammar PlusFirstCommit;

s : 'p' ( A t | B t )+ EOF
  | 'q' ( A B | A C )+ EOF
  ;

t : SEMI ;

A : 'a' ;
B : 'b' ;
C : 'c' ;
SEMI : ';' ;
WS : [ \t\r\n]+ -> skip ;
