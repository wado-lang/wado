// `~X` carries the same empty static FIRST set as `.` — neither can be
// enumerated — so an alternative opening on one is just as open-ended and must
// reach the same wildcard-aware dispatch. Recognising only `.` leaves the
// `~SEMI` alt behind a kind-check that commits to `assign` on any `ID`.
//
// The label + group wrapper on the second rule is the same claim for `x=(.)`:
// neither a label nor a `( … )` wrapper narrows what the element matches.
//
// Source: derived from ANTLR4 runtime-testsuite descriptor
//   ParserExec/Wildcard.txt
//
// License: BSD 3-Clause (vendor/antlr4/LICENSE.txt) — derived test grammar.

grammar LlNotAlt;

a : (assign | ~SEMI)+ EOF ;
b : (assign | x=(.))+ EOF ;
assign : ID '=' INT ';' ;

ID   : 'a'..'z'+ ;
INT  : '0'..'9'+ ;
SEMI : ';' ;
WS   : (' '|'\n') -> skip ;
