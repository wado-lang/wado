// The labelled form of `ll_wildcard_alt.g4`: a label binds a name to its
// element and changes nothing about what the element matches, so `w=.` must
// reach the same wildcard-aware dispatch `.` does. Recognising only the bare
// `Wildcard` leaves the labelled alt behind a kind-check that commits to
// `assign` whenever the lookahead is an `ID`.
//
// Source: derived from ANTLR4 runtime-testsuite descriptor
//   ParserExec/Wildcard.txt
//
// License: BSD 3-Clause (vendor/antlr4/LICENSE.txt) — derived test grammar.

grammar LlWildcardLabelAlt;

a : (assign | w=.)+ EOF ;
assign : ID '=' INT ';' ;

ID : 'a'..'z'+ ;
INT : '0'..'9'+ ;
WS : (' '|'\n') -> skip ;
