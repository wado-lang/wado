// Source: distilled from ANTLR4 runtime-testsuite Performance/DropLoopEntryBranchInLRRule_4
// License: BSD-3-Clause (ANTLR4)
// The `'between' expr 'and' expr` atom shares the `'and'` delimiter with the
// binary `expr 'and' expr` LR alt: the static suffix-first dispatch greedily
// enters the `and`-loop and starves between's mandatory `'and'`. The runtime
// ATN simulator resolves the loop-entry with full context.
grammar LrBetween;

stat : expr ';' ;

expr : ID
     | expr 'and' expr
     | expr 'or' expr
     | 'between' expr 'and' expr
     ;

ID : [a-zA-Z_] [a-zA-Z_0-9]* ;
WS : [ \t\r\n]+ -> skip ;
