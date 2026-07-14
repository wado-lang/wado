// Source: hand-written regression for the nested-list caller-FOLLOW gate.
// License: project-internal test fixture.
// A bracketed comma-list `[ item (',' item)* ]` as an element of a bare
// `item (',' item)*` list (targs): the array's inner loop must not yield its
// comma to the enclosing list's FOLLOW.
grammar NestedListFollow;
top   : items EOF ;
items : item (',' item)* ;
item  : '[' (item (',' item)*)? ']'
      | ID targs?
      ;
targs : '<' item (',' item)* '>' ;
ID : [a-z]+ ;
WS : [ \t\r\n]+ -> skip ;
