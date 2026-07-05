// Source: Gale test fixture (Stage C context-free predicate reading `p`)
// License: same as the Gale package
//
// A context-free (`$`-free) alt-initial predicate that actually reads the
// parser context API — `{ p.la(1) == TK_NUM }?` — rather than a bare
// `{true}?` / `{false}?` literal. It is extracted to a standalone
// `pred_<id>(p: &Parser) -> bool` and called from the token-led dispatch, so
// this pins that a `p`-using predicate body compiles under the `&Parser`
// receiver and evaluates at prediction time. `takes` gates the NUM branch on a
// condition that holds; `rejects` gates it on one that fails.
grammar WadoPredLa;

options { language = Wado; }

takes : ID { p.emit("id"); }
      | { p.la(1) == TK_NUM }? NUM { p.emit("num"); }
      ;

rejects : ID { p.emit("id"); }
        | { p.la(1) == TK_ID }? NUM { p.emit("num"); }
        ;

ID : [a-z]+ ;
NUM : [0-9]+ ;
WS : ' ' -> skip ;
