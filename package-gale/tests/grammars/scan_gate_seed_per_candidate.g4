// Every candidate of a longest-match tournament scans from the same position,
// so every one has to start from the parser's gate. A candidate that walks into
// a head and fails before walking out of it leaves the ban set; seeding the
// gate once for the tournament hands that to the next candidate, which then
// measures the same tokens under a ban the parser never set.
//
// Here the first candidate scanned raises the ban and fails on its last token,
// and the second needs the ban clear to match at all.
grammar ScanGateSeedPerCandidate;

@parser::members {
    int noBrace = 0;
}

start
    : pick EOF
    ;

pick
    : ID {noBrace = 1;} LB ID RB RB
    | expr SEMI
    ;

expr
    : expr PLUS expr
    | {noBrace == 0}? ID LB ID RB
    | ID
    ;

PLUS : '+' ;
SEMI : ';' ;
LB   : '{' ;
RB   : '}' ;
ID   : [a-z]+ ;
WS   : [ \t\r\n]+ -> skip ;
