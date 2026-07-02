grammar T;
@parser::members {int i = 0;}
s : a+ ;
a : {this.i = 1;} ID {this.i == 1}? {System.out.println("alt 1");}
  | {this.i = 2;} ID {this.i == 2}? {System.out.println("alt 2");}
  ;
ID : 'a'..'z'+ ;
INT : '0'..'9'+;
WS : (' '|'\n') -> skip ;
