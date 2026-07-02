grammar T;
@parser::members {int i = 0;}
s : ({this.i += 1;
System.out.print("i=");
System.out.println(this.i);} a)+ ;
a : {this.i % 2 == 0}? ID {System.out.println("alt 1");}
  | {this.i % 2 != 0}? ID {System.out.println("alt 2");}
  ;
ID : 'a'..'z'+ ;
INT : '0'..'9'+;
WS : (' '|'\n') -> skip ;
