grammar T;

r
   : a b {<ToStringTree("$ctx"):writeln()>}
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
