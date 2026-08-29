// Source: Gale test fixture (Stage C java2wado @members translation)
// License: same as the Gale package
//
// A `language = Java` grammar with two `@parser::members` blocks: their fields
// and methods become part of the generated `Parser`, and an action calls them.
// Java reaches a member bare or through `this`, and reaches across the blocks —
// ANTLR concatenates them — so every spelling here lands on one declaration.
grammar JavaMembers;

@parser::members {
    int calls = 0;
    void bump() { calls += 1; }
}

@parser::members {
    void bumpTwice() { bump(); this.bump(); }
}

r : ID {bumpTwice(); System.out.println(calls);} ;

ID : [a-z]+ ;
WS : [ \t\r\n]+ -> skip ;
