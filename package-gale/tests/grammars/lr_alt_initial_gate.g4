// An LR alternative whose gate action sits before the leading self-ref.
//
// `emit_lr_branch_body` interleaves an LR alternative's actions with its suffix
// ops from `before == 1`, and says so: the alt-initial position is the loop-entry
// decision, not a suffix action, and the parse never runs it. The generator warns
// about it too.
//
// The suffix *scan* walks `elements[1..]` and used to carry that action anyway,
// pinned to the body's start. So `no_struct` was 1 for the scan and 0 for the
// parse, the inner atom refused itself, and the precedence loop declined a
// suffix the parse would have taken: `x + x` stopped at `x`.
//
// `lr_suffix_gate_action.g4` covers the positions that *are* run; this covers the
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
