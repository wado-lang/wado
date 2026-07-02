grammar T;
s @after {System.out.println($ctx.toStringTree(this));} : e ;
e : a=e op=('*'|'/') b=e  {}{}
  | INT {}{}
  | '(' x=e ')' {}{}
  ;
INT : '0'..'9'+ ;
WS : (' '|'\n') -> skip ;
