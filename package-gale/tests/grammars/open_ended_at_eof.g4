// Source: hand-written regression grammar for an open-ended body at EOF.
// License: same as the Gale package.
//
// `.` and `~X` admit every *token*, and EOF is not one, so an optional or
// loop over such a body declines at EOF — on the parse side as on the scan
// side, whose bounds check and wildcard EOF test both fail there.
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
