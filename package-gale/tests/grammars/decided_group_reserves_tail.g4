// A loop whose body is a group the lookahead decides outright takes a separate
// parse-side entry: it asks the decision which alternative to run and commits.
// That entry emitted no reserve, while the scan's ordinary iteration path did —
// so here the two disagree in the direction opposite to a token body's, with
// the scan measuring a loop the parse then runs greedily.
//
// `(A Z | B Y)* A Z` is the shape. The alternatives are token-led and disjoint,
// so the decision is taken; the loop's first alternative is the tail, so an
// iteration that runs takes exactly what the tail needs. On `a z a z` the loop
// must stop after one, and only the reserve tells it to — the group's own scan
// is happy to match either.
grammar DecidedGroupReservesTail;

start
    : body EOF
    ;

body
    : (A Z | B Y)* A Z
    ;

A    : 'a' ;
B    : 'b' ;
X    : 'x' ;
Y    : 'y' ;
Z    : 'z' ;
WS   : [ \t\r\n]+ -> skip ;
