grammar T;

<ImportListener("T")>
<LRListener("T")>

s
@after {
<ContextMember("$ctx", "r"):ToStringTree():writeln()>
<ContextMember("$ctx", "r"):WalkListener()>
}
   : r=e ;
e : e op='*' e
   | e op='+' e
   | INT
   ;
MULT: '*' ;
ADD : '+' ;
INT : [0-9]+ ;
ID  : [a-z]+ ;
WS : [ \t\n]+ -> skip ;
