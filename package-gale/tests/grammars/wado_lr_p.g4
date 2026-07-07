// Source: Gale test fixture (Stage C `$_p` on a left-recursive rule)
// License: same as the Gale package
//
// `$_p` is the precedence threshold the current invocation was entered with
// (ANTLR's `_p`), which Gale threads as the `min_prec` parameter every rule fn
// carries. The entry invocation runs at `min_prec == 0`; a right operand is a
// recursive invocation entered at the operator's precedence. The atom alt emits
// `$_p` so the threaded value is observable end to end.
grammar WadoLrP;

options { language = Wado; }

s : x=e { p.emit(`v={$x.v} `); } ;

e returns [i32 v]
    : l=e '+' r=e { $v = $l.v + $r.v; }
    | n=INT { $v = $n.int; p.emit(`_p={$_p} `); }
    ;

INT : [0-9]+ ;
WS : ' ' -> skip ;
