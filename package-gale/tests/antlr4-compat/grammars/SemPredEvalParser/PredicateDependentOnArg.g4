grammar T;
@parser::members {int i = 0;}
s : a[2] a[1];
a[int i]
  : {$i == 1}? ID {System.out.println("alt 1");}
  | {$i == 2}? ID {System.out.println("alt 2");}
  ;
ID : 'a'..'z'+ ;
INT : '0'..'9'+;
WS : (' '|'\n') -> skip ;
