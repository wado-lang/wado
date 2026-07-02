lexer grammar L;
I : ({System.out.println("stuff fail: " + getText());} 'a'
| {System.out.println("stuff0:" + getText());}
       'a' {System.out.println("stuff1: " + getText());}
       'b' {System.out.println("stuff2: " + getText());})
       {System.out.println(getText());} ;
WS : (' '|'\n') -> skip ;
J : .;
