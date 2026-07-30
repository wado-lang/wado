grammar T;




s
@after {
System.out.println($ctx.r.toStringTree(this));

}
  : r=a ;
a : b b        // forces list
  | b      // a list still
  ;
b : ID | INT;
MULT: '*' ;
ADD : '+' ;
INT : [0-9]+ ;
ID  : [a-z]+ ;
WS : [ \t\n]+ -> skip ;
