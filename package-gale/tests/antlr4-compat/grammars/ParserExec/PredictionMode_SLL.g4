grammar T;

r
   : (a b | a) EOF {<writeln("$text")>}
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
