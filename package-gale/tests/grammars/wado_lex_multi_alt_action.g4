// Source: Gale test fixture (Stage C multi-alt lexer action)
// License: same as the Gale package
//
// A multi-alternative lexer rule where each alternative carries its own
// `{ ... }` action. `T` matches 'a' or 'b'; the winning alternative's action
// retypes the token (`lx.set_type`), so 'a' emits an `AA` and 'b' a `BB`. The
// replay re-derives the winning alternative (`winning_alt_T`) and runs only
// that alt's action — proving per-alt dispatch, not a blanket run.
grammar WadoLexMultiAltAction;

options { language = Wado; }

tokens { AA, BB }

prog : (item)* EOF ;
item : AA { p.emit("A") }
     | BB { p.emit("B") }
     ;

T : 'a' { lx.set_type(TK_AA) }
  | 'b' { lx.set_type(TK_BB) }
  ;
WS : ' ' -> skip ;
