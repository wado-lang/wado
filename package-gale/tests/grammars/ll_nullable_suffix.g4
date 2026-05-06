// LL(*) gap: tail-greedy Optional with a NULLABLE next sibling.
//
// Pattern: like `ll_basic.g4` but the next sibling is itself optional.
// At the call site of `a` in alt 0 (`a b?`), `b?` is nullable. Static
// analysis cannot prove `Y` is required, so v1 declines to emit a
// variant — and Gale falls back to SLL/greedy, picking `a` alone and
// leaving `Y` unconsumed (then EOF expects nothing but sees `Y`).
//
// ANTLR4 LL would consider full context: alt 0's `b?` plus the outer
// `EOF` show that consuming `Y` here means the whole parse succeeds via
// alt 0, while leaving `Y` for the outer `EOF` fails. So LL picks alt 0
// with `b?` firing.
//
//   Input "X Y":
//     ANTLR4 LL  → (r (a X) (b Y))   (alt 0, b? fires)
//     Gale v1    → parse error       (alt 0 / alt 1 both fail)
//
// Gap: `gen_alt_elements`'s `is_suffix_nullable` guard prevents
// `intern_follow_variant` from registering the variant. Closing this
// gap requires either:
//   - propagating CTX_FOLLOW (outer rule's follow) into the analysis,
//     so the suffix-nullable case can still be variant-emitted; or
//   - a 2-token lookahead variant that distinguishes `Y EOF` (alt 0
//     wants `b?` to fire) from `Y` followed by something else.

grammar LLNullableSuffix;

r : (a b? | a) EOF ;
a : X Y? ;
b : Y ;

X : 'X' ;
Y : 'Y' ;
WS : [ \r\n\t]+ -> skip ;
