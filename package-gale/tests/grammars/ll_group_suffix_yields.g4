// A tail-greedy loop whose yield is proved only PAST the group holding it.
// `path`'s `(SEP ID)*` sits inside `(path? SEP)?`, so the caller wants one SEP
// — but every iteration of the loop starts with one too, and only the token
// after that SEP says which. That token belongs to the group the call site
// sits beside, not to the alternative the call site is in.
//
// This is `RustParser.g4`'s `useTree : (simplePath? PATHSEP)? (STAR | ...)`,
// where it costs every `use a::b::*;` and `use a::b::{c};` — a path of two or
// more segments in front of a glob or a group.
grammar LlGroupSuffixYields;

start
    : tree EOF
    ;

tree
    : (path? SEP) (STAR | LBRACE ID RBRACE)
    | path
    ;

path
    : ID (SEP ID)*
    ;

ID     : [a-z]+ ;
SEP    : '::' ;
STAR   : '*' ;
LBRACE : '{' ;
RBRACE : '}' ;
WS     : [ \t\r\n]+ -> skip ;
