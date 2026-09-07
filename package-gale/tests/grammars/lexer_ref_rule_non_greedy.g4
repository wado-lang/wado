// Source: hand-written for Gale's lexer emit tests.
// License: same as the Gale package.
//
// An ATN-class rule reaching a non-greedy repeat through a reference to
// another token rule, not a fragment: `A : ('a' | 'ab')+ B ;` with
// `B : 'x'+? ;`. The overlapping arms route `A` to the lexer simulator, and
// `B`'s `+?` is what makes `A`'s answer the earliest accept rather than the
// longest — reachable only by resolving `B` the way the simulator inlines it.
grammar LexerRefRuleNonGreedy;

start
    : A B EOF
    ;

A
    : ('a' | 'ab')+ B
    ;

B
    : 'x'+?
    ;

WS
    : [ \t\r\n]+ -> skip
    ;
