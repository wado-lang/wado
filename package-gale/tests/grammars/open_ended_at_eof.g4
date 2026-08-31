// Source: hand-written regression grammar for an open-ended body at EOF.
// License: same as the Gale package.
//
// `.` and `~X` admit every *token*, and EOF is not one. An optional or loop
// over such a body must therefore decline at EOF — which the scan side has
// always done (its bounds check and the wildcard's own EOF test both fail
// there) and the parse side did not, because `Admits::Everything` rendered as
// the literal `true`.
//
//   'k' x?  EOF   on `k`   → the optional declines; `x` never runs
//   'k' x?  EOF   on `k a` → `x` takes the `a`
//   'm' .?  EOF   on `m`   → written in place, same answer
grammar OpenEndedAtEof;

s : 'k' x? EOF
  | 'm' .? EOF
  ;

x : . ;

A : 'a' ;
WS : [ \t\r\n]+ -> skip ;
