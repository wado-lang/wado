grammar T;
s : a {System.out.println("alt 1");}
  | b {System.out.println("alt 2");}
  ;
a : {false}? ID INT
  | ID INT
  ;
b : ID ID
  ;
ID : 'a'..'z'+ ;
INT : '0'..'9'+;
WS : (' '|'\n') -> skip ;
