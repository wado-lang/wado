// Source: Hand-authored for Gale's parser-level non-greedy coverage.
// License: BSD-3-Clause (matches the Gale repo).
//
// Minimal grammar that exercises PARSER-level non-greedy operators. The
// canonical fuzzy-parsing idiom from `vendor/antlr4/doc/wildcard.md`:
//
//   file : .*? (constant .*?)+ EOF ;
//
// Parser non-greedy `.*?` and `TOK*?` take a distinct code path in
// `parser_gen.wado` (`compute_non_greedy_condition`,
// `gen_repeat_non_greedy`) that no existing driver grammar exercises
// end-to-end — `ANTLRv4Parser.g4` uses `ARGUMENT_CONTENT*?` but the ANTLR4
// driver test only tokenizes, never runs the parser.

grammar non_greedy_gaps;

file : .*? (constant .*?)+ EOF ;

constant : 'public' 'static' 'final' 'int' ID ;

ID : [a-zA-Z_] [a-zA-Z_0-9]* ;
INT : [0-9]+ ;
OP : [+\-*/=] ;
SEMI : ';' ;
WS : [ \t\r\n]+ -> skip ;
