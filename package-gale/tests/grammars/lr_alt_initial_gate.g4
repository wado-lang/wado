// An LR alternative whose gate action sits before the leading self-ref.
//
// The parse never runs it. `emit_lr_branch_body` interleaves an LR
// alternative's actions with its suffix ops from `before == 1`, because the
// alt-initial position is the loop-entry decision rather than a suffix action.
// The generator warns about the action as unreachable.
//
// The suffix scan walks `elements[1..]` and used to carry that action anyway,
// pinned to the body's start. `noStruct` was then 1 for the scan and 0 for the
// parse, so the inner atom refused itself and the precedence loop declined a
// suffix the parse would have taken. `x + x` stopped at `x`.
//
// `lr_suffix_gate_action.g4` covers the positions that are run. This covers the
// one that is not.
grammar LrAltInitialGate;

options {
    language = Java;
}

@parser::members {
    int noStruct = 0;
}

start
    : stmt EOF
    ;

stmt
    : e SEMI
    | e
    ;

e
    : {noStruct = 1;} e OP e
    | {noStruct == 0}? ID
    | LB e RB
    ;

OP   : '+' ;
ID   : 'x' ;
LB   : '(' ;
RB   : ')' ;
SEMI : ';' ;
WS   : [ \t\r\n]+ -> skip ;
