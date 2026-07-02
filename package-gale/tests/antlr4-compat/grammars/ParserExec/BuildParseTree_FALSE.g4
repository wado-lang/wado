grammar T;

r
   : a b {System.out.println($ctx.toStringTree(this));}
   ;
a
   : A
   ;
b
   : B
   ;
A  : 'A';
B  : 'B';
WS  : [ \r\n\t]+ -> skip ;
