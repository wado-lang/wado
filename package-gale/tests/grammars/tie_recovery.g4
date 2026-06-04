// Synthetic regression fixture for the error-recovery span tie-break.
// The repeated element `item` and the construct after the repetition
// `trailer` share their first token 'x'. On input "x", `item` is
// malformed (expects ID after 'x') so the repetition records a deep
// recovery error ("expected ID") at EOF; `trailer` then also consumes
// the 'x' and fails at the SAME EOF position ("expected ="). The two
// errors tie on span.start, and the input-entry must prefer the
// malformed element ("expected ID") over the next construct ("expected =").
// Source: hand-authored for Gale (no upstream grammar).
// License: same as the Gale project (see repository root).
grammar TieRecovery;
prog    : item* trailer EOF ;
item    : 'x' ID ;
trailer : 'x' '=' ;
ID : [a-z]+ ;
WS : [ ]+ -> skip ;
