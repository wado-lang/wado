grammar GatedNullableAlt;

@parser::members {
    int on = 0;
}

start
    : ON {on = 1;} body
    | OFF body
    ;

// The group's first alternative is both gated and nullable: it has a branch of
// its own for `ID` (which folds the gate into that branch's condition and marks
// it answered, so the body carries no guard) and it also matches empty.
body
    : LB ({on == 1}? ID? | LP) RB
    ;

ON   : 'on' ;
OFF  : 'off' ;
LB   : '{' ;
RB   : '}' ;
LP   : '(' ;
ID   : [a-z]+ ;
WS   : [ \t\r\n]+ -> skip ;
