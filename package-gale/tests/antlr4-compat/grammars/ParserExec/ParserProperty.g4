grammar T;
<ParserPropertyMember()>
a : {<ParserPropertyCall({$parser}, "Property()")>}? ID {<writeln("\"valid\"")>}
  ;
ID : 'a'..'z'+ ;
WS : (' '|'\n') -> skip ;
