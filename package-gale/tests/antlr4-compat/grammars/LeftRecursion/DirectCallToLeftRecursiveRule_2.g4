grammar T;
a @after {System.out.println($ctx.toStringTree(this));} : a ID
  | ID
  ;
ID : 'a'..'z'+ ;
WS : (' '|'\n') -> skip ;
