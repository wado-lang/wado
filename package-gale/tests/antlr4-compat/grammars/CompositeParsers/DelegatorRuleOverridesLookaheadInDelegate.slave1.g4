parser grammar S;
type_ : 'int' ;
decl : type_ ID ';'
    | type_ ID init_ ';' {System.out.println("JavaDecl: " + $text);};
init_ : '=' INT;
