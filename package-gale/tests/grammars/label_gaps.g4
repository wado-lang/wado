// Source: Hand-authored for Gale's parser-label coverage.
// License: BSD-3-Clause (matches the Gale repo).
//
// Minimal grammar that exercises scalar (single-valued) element labels at
// parser level. The list form `items += ITEM` is covered by parser_gaps.g4
// (A2); this fixture targets the distinct `name = TOK` and `name = rule`
// codegen path (LabelElement with `list = false`).
//
// Layout:
//   - `assign_stmt` uses a scalar TOKEN label (`key = ID`) and a scalar
//     literal label (`value = STRING`).
//   - `pair_stmt` uses a scalar rule-reference label (`k = key_rule`), mixed
//     with an unlabeled token.
//   - `mix_stmt` uses both a list label and a scalar label in the same
//     alternative, to verify that the generator does not confuse the two
//     when they appear together.

grammar label_gaps;

program
    : stmt* EOF
    ;

stmt
    : assign_stmt
    | pair_stmt
    | mix_stmt
    ;

assign_stmt
    : key = ID EQ value = STRING SEMI
    ;

pair_stmt
    : PAIR k = key_rule COLON ID SEMI
    ;

key_rule
    : ID
    ;

mix_stmt
    : LIST head = ID (COMMA rest += ID)* SEMI
    ;

PAIR   : 'pair' ;
LIST   : 'list' ;
EQ     : '=' ;
COLON  : ':' ;
COMMA  : ',' ;
SEMI   : ';' ;
ID     : [a-zA-Z_] [a-zA-Z_0-9]* ;
STRING : '"' (~["\\] | '\\' .)* '"' ;
WS     : [ \t\r\n]+ -> skip ;
