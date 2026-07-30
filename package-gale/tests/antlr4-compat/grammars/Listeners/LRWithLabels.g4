grammar T;




s
@after {
System.out.println($ctx.r.toStringTree(this));

}
  : r=e ;
e : e '(' eList ')' # Call
  | INT    # Int
  ;
eList : e (',' e)* ;
MULT: '*' ;
ADD : '+' ;
INT : [0-9]+ ;
ID  : [a-z]+ ;
WS : [ \t\n]+ -> skip ;
