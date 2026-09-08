// A group dispatch gives its nullable alternative the last arm, for the
// lookahead none of the others select. When that alternative is also gated, the
// arm that selects it on its own tokens folds the predicate into that arm's
// condition. The body then carries no guard of its own, so a bare last arm runs
// the alternative on the very tokens the first arm refused.
grammar GatedNullableAlt;

@parser::members {
    int on = 0;
}

start
    : ON {on = 1;} body
    | OFF body
    ;

// The first alternative is both gated and nullable: it has a branch of its own
// for `ID`, and it also matches empty.
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
