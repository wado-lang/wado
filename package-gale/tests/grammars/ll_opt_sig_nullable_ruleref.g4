// Source: hand-written for Gale's LL prediction tests.
// License: same as the Gale package.
//
// An optional whose fixed-token prefix ends at a rule that derives epsilon:
// `field : ID (COLON mods)? ;` with `mods : PUB? ;`. The entry signature walks
// past COLON, and `mods` matching nothing means the position after COLON holds
// whatever follows the optional instead. Naming FIRST(mods) there rejects the
// optional on every input that takes the empty `mods`.
grammar LlOptSigNullableRuleref;

start
    : field EOF
    ;

field
    : ID (COLON mods)?
    ;

mods
    : PUB?
    ;

PUB   : 'pub' ;
ID    : [a-z]+ ;
COLON : ':' ;
WS    : [ \t\r\n]+ -> skip ;
