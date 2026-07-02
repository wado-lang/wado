grammar T;
s @after {System.out.println($ctx.toStringTree(this));} : a ;
a : a ID {false}?<fail='custom message'>
  | ID
  ;
ID : 'a'..'z'+ ;
WS : (' '|'\n') -> skip ;
