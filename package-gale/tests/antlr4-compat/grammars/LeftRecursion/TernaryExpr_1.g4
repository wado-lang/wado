grammar T;
s @after {System.out.println($ctx.toStringTree(this));} : e EOF ; // must indicate EOF can follow or 'a<EOF>' won't match
e : e '*' e
  | e '+' e
  |<assoc=right> e '?' e ':' e
  |<assoc=right> e '=' e
  | ID
  ;
ID : 'a'..'z'+ ;
WS : (' '|'\n') -> skip ;
