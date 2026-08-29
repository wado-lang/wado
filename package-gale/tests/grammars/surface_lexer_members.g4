// Source: Gale test fixture (Stage C surface matrix)
// License: same as the Gale package
//
// A lexer whose only action content is its `@members` block. The machinery a
// member reaches is not always the machinery an action turns on — the output
// sink here — so a members-only grammar is its own surface, and one that
// generated a method calling an `emit` the `Lexer` did not carry.
lexer grammar SurfaceLexerMembers;

@members {
    int seen = 0;
    void note(String s) { System.out.println(s); }
    boolean any() { return this.seen > 0; }
}

A : 'a' ;
B : 'b' ;
WS : [ \t\r\n]+ -> skip ;
