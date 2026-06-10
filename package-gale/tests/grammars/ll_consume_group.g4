// LL regression: a Consume prediction node whose overlap group does NOT
// contain alternative 0.
//
// Source: hand-written minimal fixture (code-health.md, "Consume-node
// validation compares against alts[0]").
//
// License: same as Gale.
//
// Pattern: alts 1 and 2 share the terminal prefix ID, so their overlap
// group builds `Consume(ID) -> Dispatch{X, Y}`. Alternative 0 (`A`) is in
// a different overlap group. The Consume validation used to compare the
// consumed element against alts[0] ("A") instead of the group's own alts,
// spuriously failing, skipping the consume, and emitting the child
// Dispatch one token early — rejecting valid `i x i` / `i y i` input.
grammar LLConsumeGroup;

r : A | ID X ID | ID Y ID ;

A : 'a' ;
ID : 'i' ;
X : 'x' ;
Y : 'y' ;
WS : ' ' -> skip ;
