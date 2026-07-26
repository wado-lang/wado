// Source: Gale test fixture (Stage C: lexer action placement)
// License: same as the Gale package
//
// A lexer action runs where it sits, not at the end of the match: each one
// below sees only the text matched so far. Only the winning group alternative's
// actions run — the losing arm is tried during matching but must stay silent.
lexer grammar JavaLexActionPos;

I : ( {System.out.println("lose:" + getText());} 'a'
    | {System.out.println("a0:" + getText());} 'a'
      {System.out.println("a1:" + getText());} 'b'
      {System.out.println("a2:" + getText());}
    ) {System.out.println("end:" + getText());} ;
WS : ' ' -> skip ;
J : . ;
