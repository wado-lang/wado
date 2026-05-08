grammar T;

r
   : (a b | a) EOF {<ToStringTree("$ctx"):writeln()>}
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
