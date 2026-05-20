// LL prediction regression: a multi-alt group whose alts are a
// concrete RuleRef and the wildcard `.`. Without the wildcard-aware
// overlap merge introduced in `compute_overlap_groups_with_wildcard`,
// the parse-side dispatch commits to the RuleRef alt whenever the
// lookahead matches that alt's FIRST set, even when the RuleRef's
// deeper structure cannot succeed — leaving the wildcard alt
// unreachable.
//
// Source: derived from ANTLR4 runtime-testsuite descriptor
//   ParserExec/Wildcard.txt
//
// License: BSD 3-Clause (vendor/antlr4/LICENSE.txt) — derived test grammar.

grammar LLWildcard;

a : (assign | .)+ EOF ;
assign : ID '=' INT ';' ;

ID : 'a'..'z'+ ;
INT : '0'..'9'+ ;
WS : (' '|'\n') -> skip ;
