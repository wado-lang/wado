// Two ways a prediction cascade is incomplete but does not look it.
//
// `n` — alt 0's remainder after the shared `X` is `Y?`, which can match empty.
// FIRST of a nullable remainder does not cover the alt: on `X` alone the alt
// matches with `Y?` empty and the continuation comes from the caller, so no
// branch derived from FIRST claims that token. A completeness check that reads
// FIRST as exact declares the cascade complete and suppresses the tournament
// the alt needs.
//
// `m` — the cascade misses `X X` (the `B*` repeat, as in ll_repeat_alt_gap),
// so the tournament fallback runs. On input the grammar does not accept at
// all, no alt's scan matches: committing to an alt anyway makes the parse
// report that alt's next token instead of the rule, so the fallback has to
// leave the rule's own error path in front.

grammar LlCascadeGaps;

n : X Y?
  | X Z
  ;

m : X W* V
  | X W U
  ;

X : 'X' ;
Y : 'Y' ;
Z : 'Z' ;
W : 'W' ;
V : 'V' ;
U : 'U' ;
WS : [ \r\n\t]+ -> skip ;
