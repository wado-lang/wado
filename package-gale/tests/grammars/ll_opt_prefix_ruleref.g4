// Source: hand-written for Gale's LL prediction tests.
// License: same as the Gale package.
//
// A greedy optional whose fixed-token prefix runs out at a RuleRef, competing
// with a caller loop that opens on the same token. `seg : ID (SEP args)?` sits
// inside `path : seg (SEP seg)*`, so SEP alone cannot decide: `a::b` belongs to
// the loop and `a::<b>` to the optional. FIRST(args) = {LT} separates them at
// the second token, which is the shape RustParser's `pathExprSegment` has.
grammar LlOptPrefixRuleRef;

path
    : seg (SEP seg)* EOF
    ;

seg
    : ID (SEP args)?
    ;

args
    : LT ID GT
    ;

ID  : [a-z]+ ;
SEP : '::' ;
LT  : '<' ;
GT  : '>' ;
WS  : [ \t\r\n]+ -> skip ;
