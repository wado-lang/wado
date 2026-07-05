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

// Context-dependent predicate (`$mode`, seeded by @init) on the same ATN-routed
// shape: it too must be honoured at the ATN decision, where the value channel is
// in scope. `ctxkeep` keeps the longer alt (`$mode == 1` holds), `ctxdrop`
// excludes it (`$mode == 0` is false), so the shorter alt runs.
ctxkeep : ck 'c' ;
ck returns [i32 mode] @init { $mode = 1; }
   : 'a' 'b' { p.emit("ab"); }
   | { $mode == 1 }? 'a' 'b' 'c' { p.emit("abc"); }
   ;

ctxdrop : cd 'c' ;
cd returns [i32 mode] @init { $mode = 1; }
   : 'a' 'b' { p.emit("ab"); }
   | { $mode == 0 }? 'a' 'b' 'c' { p.emit("abc"); }
   ;

// Every alt of an ATN-routed decision gated false: no alternative is viable, so
// the decision must fail (no viable alternative), not fall through to the last
// alt whose own predicate rejected it.
allfalse : af 'c' ;
af : { false }? 'a' 'b' { p.emit("ab"); }
   | { false }? 'a' 'b' 'c' { p.emit("abc"); }
   ;

WS : ' ' -> skip ;
