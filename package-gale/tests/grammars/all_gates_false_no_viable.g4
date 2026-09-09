// The pick after a longest-match tournament runs its last alternative with no
// test, because the one before it already returned and some alternative always
// matched. A gated candidate breaks that: it scores no-match for a reason the
// lookahead did not predict, so a tournament of gated candidates can end with
// nothing viable -- and the untested tail then parses an alternative the
// predicate refused.
grammar AllGatesFalseNoViable;

@parser::members {
    int on = 0;
}

start
    : stmt EOF
    ;

stmt
    : OFF pick
    | ON {on = 1;} pick
    ;

// Both gated on the same flag and sharing every leading token, so the choice is
// a tournament rather than a token-led branch.
pick
    : {on == 1}? ID LB ID RB
    | {on == 1}? ID LB ID RB SEMI
    ;

OFF  : 'off' ;
ON   : 'on' ;
SEMI : ';' ;
LB   : '{' ;
RB   : '}' ;
ID   : [a-z]+ ;
WS   : [ \t\r\n]+ -> skip ;
