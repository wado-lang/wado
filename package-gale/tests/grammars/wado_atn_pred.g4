// Source: Gale test fixture (Stage C predicate on an ATN-routed decision)
// License: same as the Gale package
//
// `xt` / `xr` have the `AtEndConflict` shape (`'a' 'b'` is a prefix of
// `'a' 'b' 'c'`), so the decision cannot be made statically and routes through
// the runtime ATN simulator. An alt-initial predicate on the longer alt must
// still be honoured: `{true}?` leaves it selectable, `{false}?` excludes it
// from the prediction (the `disabled_alts` seed prune). For input `abcc` the
// full-context ATN otherwise picks the longer alt; `{false}?` forces the
// shorter one, which cannot complete, so the parse fails.
grammar WadoAtnPred;

options { language = Wado; }

takes : xt 'c' ;
xt : 'a' 'b' { p.emit("ab"); }
   | { true }? 'a' 'b' 'c' { p.emit("abc"); }
   ;

rejects : xr 'c' ;
xr : 'a' 'b' { p.emit("ab"); }
   | { false }? 'a' 'b' 'c' { p.emit("abc"); }
   ;

WS : ' ' -> skip ;
