grammar T;

<ImportListener("T")>
<RuleGetterListener("T")>

s
@after {
<ContextMember("$ctx", "r"):ToStringTree():writeln()>
<ContextMember("$ctx", "r"):WalkListener()>
}
  : r=a ;
a : b b        // forces list
  | b      // a list still
  ;
b : ID | INT;
MULT: '*' ;
ADD : '+' ;
INT : [0-9]+ ;
ID  : [a-z]+ ;
WS : [ \t\n]+ -> skip ;
