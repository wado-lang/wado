// `lr_nested_self_ref.g4`'s list operand in an ATN-class rule: the
// `'!' ae '>>' ae` atom hands its operand the loop's `'>>'`, so the runtime
// simulator decides the loop entry. The nested operand is `ae[0]` there too,
// and only full context says whether it may take a `'>>'`.
grammar LrNestedSelfRefAtn;

bang : ae EOF ;
ae : ae '*' ae
   | ae (',' ae)* '>>' ae
   | '!' ae '>>' ae
   | ID
   ;

ID : [a-z]+ ;
WS : [ \t\r\n]+ -> skip ;
