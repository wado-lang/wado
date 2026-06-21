// Source: Hand-authored for Gale's labeled-alternative accessor coverage.
// License: BSD-3-Clause (matches the Gale repo).
//
// Minimal labeled-alternative grammar for the `<rule>_alt` accessor. `expr`
// has two labeled alts with disjoint first tokens (INT vs '('), so a clean
// parse stamps each `expr` node's alternative. An input like `()` drives the
// inner `expr` to a position whose lookahead (`)`) matches no alternative: the
// parser opens the `expr` node, picks no alt, and closes it with `alt` unset,
// exercising the accessor's `None` path.

grammar AltOpt;

prog : expr EOF ;

expr : INT          # Num
     | '(' expr ')' # Paren
     ;

INT : [0-9]+ ;
WS  : [ \t\r\n]+ -> skip ;
