grammar T;

<ImportListener("T")>
<TokenGetterListener("T")>

s
@after {
<ContextMember("$ctx", "r"):ToStringTree():writeln()>
<ContextMember("$ctx", "r"):WalkListener()>
}
  : r=a ;
a : INT INT
  | ID
  ;
MULT: '*' ;
ADD : '+' ;
INT : [0-9]+ ;
ID  : [a-z]+ ;
WS : [ \t\n]+ -> skip ;
