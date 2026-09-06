// Source: hand-written for Gale's LL prediction tests.
// License: same as the Gale package.
//
// A greedy optional at a rule's deep tail, against a caller loop that opens on
// the same token. `seg : ID SEP? (args | fn_)?` sits inside
// `path : seg (SEP seg)*`, where the loop is the caller's last element, so the
// callee is at the caller's tail and the runtime FOLLOW gate is what decides.
//
// SEP alone cannot: `a::b` belongs to the loop and `a::<b>` to the optional.
// Only the caller's continuation two positions deep — SEP then FIRST(seg) —
// separates them, and a one-token mask would yield both (soundness invariant 1).
// This is `RustParser.g4`'s `typePathSegment` inside `typePath`.
grammar LlOptTailCallerLoop;

start
    : path EOF
    ;

path
    : seg (SEP seg)*
    ;

seg
    : ID SEP? (args | fn_)?
    ;

args
    : LT ID GT
    ;

fn_
    : LPAREN RPAREN
    ;

ID     : [a-z]+ ;
SEP    : '::' ;
LT     : '<' ;
GT     : '>' ;
LPAREN : '(' ;
RPAREN : ')' ;
WS     : [ \t\r\n]+ -> skip ;
