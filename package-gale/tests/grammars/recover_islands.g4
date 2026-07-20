// Source: hand-written Gale regression fixture for the recover_islands option.
// License: same terms as the Gale package (see package-gale/README.md).
//
// A start rule that only derives `set`-led items. `group` is reached inside an
// item, and its opening `[` (LB) is a delimiter token exclusive to `group`, so
// it is a unique recovery trigger: a fragment the start rule cannot derive but
// that contains a `[ ... ]` group still builds a real `group` subtree under the
// `recover_islands` option.

grammar RecoverIslands;

prog  : item* EOF ;
item  : 'set' ID group? ';' ;
group : LB ID RB ;

ID : [a-z]+ ;
LB : '[' ;
RB : ']' ;

WS : [ \t\r\n]+ -> skip ;
