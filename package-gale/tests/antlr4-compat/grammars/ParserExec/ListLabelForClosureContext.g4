grammar T;
ifStatement
@after {
java.util.List<?> __assertIsList = $ctx.elseIfStatement();
}
    : 'if' expression
      ( ( 'then'
 executableStatement*
 elseIfStatement*  // <--- problem is here; should yield a list not node
 elseStatement?
 'end' 'if'
        ) | executableStatement )
    ;

elseIfStatement
    : 'else' 'if' expression 'then' executableStatement*
    ;
expression : 'a' ;
executableStatement : 'a' ;
elseStatement : 'a' ;
