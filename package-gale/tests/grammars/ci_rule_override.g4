// Source: Hand-authored for Gale's rule-level caseInsensitive override.
// License: BSD-3-Clause (matches the Gale repo).
//
// Grammar-level `caseInsensitive = true` enables CI matching for all
// lexer rules. Individual rules can *opt out* via a rule-level option
// `options { caseInsensitive = false; }`. This is the inverse of
// `ci_sql.g4` (which only tests the grammar-level true → rule-level
// inherits true path) and exercises the override codegen in
// `lexer_gen.wado`.

grammar ci_rule_override;

options { caseInsensitive = true; }

line : (KEYWORD | IDENT | EXACT)+ EOF ;

// Inherits grammar-level CI: matches `if`, `IF`, `If`, `iF`.
KEYWORD : 'if' ;

// Inherits grammar-level CI: matches identifiers in any case.
IDENT : [a-z][a-z0-9_]* ;

// Rule-level override: this keyword must match *exactly* `EXACT`
// regardless of the grammar-level CI setting. Lower- or mixed-case
// `exact` must NOT be accepted by this rule (it will fall through to
// IDENT instead).
EXACT options { caseInsensitive = false; } : 'EXACT' ;

WS : [ \t\r\n]+ -> skip ;
