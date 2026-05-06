// LL(*) gap: tail-greedy callee that is a LEFT-RECURSIVE rule.
//
// Pattern: same shape as `ll_basic.g4` but `a` is rewritten via ANTLR4's
// direct left recursion algorithm. `intern_follow_variant` rejects LR
// rules in v1 because the variant emission would need to mirror
// `gen_scan_lr_functions` (atom + per-LR-alt suffix helpers + the
// precedence-climbing dispatcher) — none of which is plumbed today.
//
//   Input "X Y":
//     ANTLR4 LL → (r (a (a X)) (b Y))
//     Gale v1   → variant skipped → atom `a` greedily consumes X Y → wrong
//
// Gap: extend variant emission to emit `scan_<rule>__follow_<id>_atom`
// and `parse_<rule>__follow_<id>_atom` plus follow-aware LR-suffix
// helpers. The atom path is what carries tail-greedy here (the LR
// suffix is itself tail-greedy too, but that is a different sub-gap).

grammar LLLRAtom;

r : (a b | a) EOF ;

// `a` is left-recursive: the second alt's first element is `a` itself.
// ANTLR4 rewrites this into a precedence-climbing helper.
a
   : X Y?         // atom (non-LR) — tail-greedy on Y?
   | a '+' a      // LR-binary — its suffix `'+' a` is unrelated to Y
   ;

b : Y ;

X : 'X' ;
Y : 'Y' ;
WS : [ \r\n\t]+ -> skip ;
