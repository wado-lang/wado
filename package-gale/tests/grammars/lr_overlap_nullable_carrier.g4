// The shared first token sits inside an *optional* whose body is two tokens
// wide, so offset 1 is the optional's own second token, not what follows it.
grammar LrOverlapNullableCarrier;

e   : e ('>' '>')? '=' e
    | e cmp e
    | INT
    ;

cmp : '>' | '<' ;

INT : [0-9]+ ;
WS  : [ \t\r\n]+ -> skip ;
