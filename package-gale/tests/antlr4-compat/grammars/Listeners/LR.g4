grammar T;




s
@after {
System.out.println($ctx.r.toStringTree(this));

}
   : r=e ;
e : e op='*' e
   | e op='+' e
   | INT
   ;
MULT: '*' ;
ADD : '+' ;
INT : [0-9]+ ;
ID  : [a-z]+ ;
WS : [ \t\n]+ -> skip ;
