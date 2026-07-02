grammar T;

r
   : (a b | a) EOF {System.out.println($ctx.toStringTree(this));}
   ;
a
   : X Y?
   ;
b
   : Y
   ;
X: 'X';
Y: 'Y';
WS  : [ \r\n\t]+ -> skip ;
