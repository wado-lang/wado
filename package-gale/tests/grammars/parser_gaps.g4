// Source: Hand-authored for Gale's parser-gaps coverage.
// License: BSD-3-Clause (matches the Gale repo).
//
// A minimal combined grammar that exercises parser-level features that
// were uncovered by the existing driver-test grammars — see TODO.md
// "Driver-level verification (remaining gaps)":
//
//   - A1: parser-side `~TOK` and `~( block )` (set complement of a token).
//   - A2: list labels `items += ITEM` (vs. single-valued labels).
//   - A5: parser-level wildcard `.` (matches any single token).

grammar parser_gaps;

program
    : stmt* EOF
    ;

stmt
    : list_stmt
    | not_stmt
    | any_stmt
    ;

// A2: list labels — `items += ITEM` collects every matching token into the
// same list label. The parser must recognize the `+=` list form without
// confusing it with the scalar `name = ITEM` form.
list_stmt
    : LIST items += ITEM (COMMA items += ITEM)* SEMI
    ;

// A1: `~TOK` (token-complement) and `~( TOK | TOK )` (set-complement) at
// parser level. Both must match exactly one token that is NOT in the set.
not_stmt
    : NOT ~ITEM SEMI
    | SET ~( ITEM | NUMBER ) SEMI
    ;

// A5: parser-level wildcard `.` matches any single token.
any_stmt
    : ANY . SEMI
    ;

LIST   : 'list' ;
NOT    : 'not' ;
SET    : 'set' ;
ANY    : 'any' ;
ITEM   : [a-z_] [a-z_0-9]* ;
NUMBER : [0-9]+ ;
COMMA  : ',' ;
SEMI   : ';' ;
WS     : [ \t\r\n]+ -> skip ;
