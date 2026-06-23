// Source: hand-written for Gale's left-recursion tests (extends lr_between.g4).
// License: same as the Gale package.
//
// ATN-class LR rule (the shared `'and'` delimiter forces the runtime
// loop-entry) that also carries a postfix call alt whose args are self-refs
// nested in a Group/Repeat. Pins that the whole-grammar ATN builds those nested
// self-refs at prec 0 (bounded by `,` / `)`), matching the static
// `stamp_lr_self_ref_min_prec` nested branch and the published ANTLR4 jar.
// Previously rejected at codegen by the `build_lr_suffix` nested-self-ref
// assert.
grammar LrBetweenCall;
prog : expr EOF ;
expr : INT
     | expr 'and' expr
     | expr 'or' expr
     | 'between' expr 'and' expr
     | expr '(' (expr (',' expr)*)? ')'
     ;
INT : [0-9]+ ;
WS  : [ \t\r\n]+ -> skip ;
