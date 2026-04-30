parser grammar S;
type_ : 'int' ;
decl : type_ ID ';'
    | type_ ID init_ ';' {<AppendStr("\"JavaDecl: \"","$text"):writeln()>};
init_ : '=' INT;
