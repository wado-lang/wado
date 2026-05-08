grammar T;

<ImportListener("T")>
<LRWithLabelsListener("T")>

s
@after {
<ContextMember("$ctx", "r"):ToStringTree():writeln()>
<ContextMember("$ctx", "r"):WalkListener()>
}
  : r=e ;
e : e '(' eList ')' # Call
  | INT    # Int
  ;
eList : e (',' e)* ;
MULT: '*' ;
ADD : '+' ;
INT : [0-9]+ ;
ID  : [a-z]+ ;
WS : [ \t\n]+ -> skip ;
