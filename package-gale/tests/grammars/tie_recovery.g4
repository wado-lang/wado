// The repeated `item` and the `trailer` after it share their first token 'x'.
// On `x x` both fail at the second 'x', `item` expecting ID and `trailer`
// expecting '='. The tie must go to the malformed element.
grammar TieRecovery;
prog    : item* trailer EOF ;
item    : 'x' ID ;
trailer : 'x' '=' ;
ID : [a-z]+ ;
WS : [ ]+ -> skip ;
