// Source: hand-written regression grammar for Gale's group scan tournament.
// License: same as the Gale package.
//
// Mirrors SQLite's `( table_or_subquery (',' table_or_subquery)* | join_clause )`
// FROM dispatch: alt 1 is a list, alt 2 a bare-RuleRef catch-all whose rule can
// also consume the separator. On a scan-length tie (single item, or comma list)
// ANTLR4 resolves the ambiguity to the lowest alternative, so the catch-all
// wins only when it scans strictly further (the `J` operator).
grammar GroupTieCatchAll;

s : 'F' ( item ( ',' item )* | pair ) EOF ;

pair : item ( op item opt_tail )* ;

op : ',' | 'J' ;

opt_tail : ( 'T' )? ;

item : ID ;

ID : [a-z]+ ;
WS : [ \t]+ -> skip ;
