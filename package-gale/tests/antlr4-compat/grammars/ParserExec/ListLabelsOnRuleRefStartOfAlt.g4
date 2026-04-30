grammar Test;

expression
@after {
<AssertIsList("$args")>
}
    : op=NOT args+=expression
    | args+=expression (op=AND args+=expression)+
    | args+=expression (op=OR args+=expression)+
    | IDENTIFIER
    ;

AND : 'and' ;
OR : 'or' ;
NOT : 'not' ;
IDENTIFIER : [a-zA-Z_][a-zA-Z0-9_]* ;
WS : [ \t\r\n]+ -> skip ;
