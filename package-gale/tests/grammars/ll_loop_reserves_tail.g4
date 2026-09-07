// Source: hand-written for Gale's LL prediction tests.
// License: same as the Gale package.
//
// A greedy loop in front of a mandatory suffix that starts where the loop body
// does. `(arm SEMI)* arm` lets the loop take the only arm of `a;`, after which
// the mandatory `arm` has none — the loop has to leave one behind. Nothing at
// the loop's own decision point says so: `a` opens an iteration and the tail
// alike, and only what sits AFTER the arm tells them apart.
//
// This is `RustParser.g4`'s
// `matchArms : (matchArm FATARROW matchArmExpression)* matchArm FATARROW expression COMMA?`,
// where it costs every `match` whose last arm carries a trailing comma.
grammar LlLoopReservesTail;

start
    : arms EOF
    ;

arms
    : (arm SEMI)* arm SEMI?
    ;

arm
    : ID
    ;

ID   : [a-z]+ ;
SEMI : ';' ;
WS   : [ \t\r\n]+ -> skip ;
