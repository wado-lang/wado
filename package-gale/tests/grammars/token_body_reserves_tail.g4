// Source: Gale test fixture (a token-bodied loop reserving its alternative's tail)
// License: same as the Gale package
//
// `ll_loop_reserves_tail.g4` gives its loop a group body, which the scan walks
// through the ordinary iteration path. A body the loop's own guard already
// implies -- one token -- takes a shortcut instead: the scan advances by one and
// returns, skipping the reserve the parse side still applies.
//
// `A* A` is that shape. Without the reserve the scan eats both `a`s, finds no
// `A` left for the mandatory tail, and scores the alternative -1 -- for input
// the parse accepts.
grammar TokenBodyReservesTail;

start
    : pick EOF
    ;

// Two alternatives sharing every leading token, so `as` is measured by a scan
// rather than reached directly.
pick
    : as SEMI
    | as
    ;

as
    : A* A
    ;

A    : 'a' ;
SEMI : ';' ;
WS   : [ \t\r\n]+ -> skip ;
