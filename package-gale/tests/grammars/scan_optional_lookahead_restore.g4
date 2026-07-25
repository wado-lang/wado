// Source: hand-written regression for the scan-side `OptionalLookahead`
// position restore (the SQLite `table_or_subquery` / `FROM f(1)` shape).
// License: BSD-3-Clause (matches the rest of the gale test corpus).
//
// Both `item` alts open with the same rule ref and a '(' chain deep enough to
// hide the disambiguating token, so the decision lands on the longest-match
// scan tournament. Scanning alt 0 on `f ( ( ( ( 1 ) ) ) )` enters the
// two-shape optional `( 'as'? alias )?` (its gate admits '(') and then fails
// inside `alias`; the trailing `( 'idx' name | 'no' 'idx' )?` reads
// tokens[pos] immediately after, so an unrestored scan position poisons it.
grammar ScanOptionalLookaheadRestore;

s : stmt EOF ;

// Both alts open with the same opaque rule ref, so picking one scans `item`
// to the end — this is the call site that reaches the poisoned position.
stmt
 : item
 | item 'z'
 ;

item
 : name ( 'as'? alias )? ( 'idx' name | 'no' 'idx' )?
 | name '(' inner ')'
 ;

alias
 : ID
 | '(' alias ')'
 ;

inner
 : NUMBER
 | '(' inner ')'
 ;

name : ID ;

ID : [a-z]+ ;
NUMBER : [0-9]+ ;
WS : [ \t\r\n]+ -> skip ;
