grammar T;




s
@after {
System.out.println($ctx.r.toStringTree(this));

}
  : r=a ;
a : INT INT
  | ID
  ;
MULT: '*' ;
ADD : '+' ;
INT : [0-9]+ ;
ID  : [a-z]+ ;
WS : [ \t\n]+ -> skip ;
