// Source: distilled from ANTLR4 runtime-testsuite Performance/DropLoopEntryBranchInLRRule_4
// License: BSD-3-Clause (ANTLR4)
// Cross-rule scan-caller variant: `stat`'s two alts share the `expr` prefix,
// so a scan tournament measures `expr`'s extent. The first alt requires
// `'and' ID '!'` after `expr` — its mandatory `'and'` overlaps `expr`'s binary
// `'and'` operator. The scan caller stack must push stat's return state (not
// only `expr`'s own self-ref operand) so `scan_expr` yields the mandatory
// `'and'` to the caller; otherwise the greedy binary loop eats it and both
// alts fail.
grammar LrScanCaller;

stat : expr 'and' ID '!'
     | expr ';'
     ;

expr : ID
     | expr 'and' expr
     | 'between' expr 'and' expr
     ;

ID : [a-zA-Z_] [a-zA-Z_0-9]* ;
WS : [ \t\r\n]+ -> skip ;
