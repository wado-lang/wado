grammar T;
@parser::members {int i = 0;}
s : a[2] a[1];
a[int i]
  : {$i == 1}? ID
  | {$i == 2}? ID
  ;
ID : 'a'..'z'+ ;
INT : '0'..'9'+;
WS : (' '|'\n') -> skip ;
