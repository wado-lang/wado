// Source: Gale test fixture (Stage C left-recursive alt actions)
// License: same as the Gale package
//
// Actions on a left-recursive rule: the atom alt (`INT`) and each LR suffix
// alt (`e '+' e`, `e '*' e`) run their action. The suffix action runs after
// the continuation's right operand, in left-associative precedence order, so
// the emitted stream reflects the parse's operator order.
grammar WadoLrAction;

options { language = Wado; }

e : e '*' e { p.emit("*"); }
  | e '+' e { p.emit("+"); }
  | INT { p.emit("i"); }
  ;

INT : [0-9]+ ;
WS : ' ' -> skip ;
