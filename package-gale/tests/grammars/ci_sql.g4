// Source: Hand-authored for Gale's case-insensitive codegen coverage.
// License: BSD-3-Clause (matches the Gale repo).
//
// Minimal grammar that exercises the grammar-level `caseInsensitive = true`
// option across a keyword (literal fold) and an identifier rule (range fold).

grammar ci_sql;

options { caseInsensitive = true; }

stmt : IF IDENT EOF ;

IF    : 'if' ;
IDENT : [a-z_]+ ;
WS    : [ \t]+ -> skip ;
