// Source: Gale test fixture (Stage C java2wado @members translation)
// License: same as the Gale package
//
// A `language = Java` grammar with a `@parser::members` block: the field and
// method become part of the generated `Parser`, and an action calls them.
grammar JavaMembers;

@parser::members {
    int calls = 0;
    void bump() { this.calls += 1; }
}

r : ID {this.bump(); System.out.println(this.calls);} ;

ID : [a-z]+ ;
WS : [ \t\r\n]+ -> skip ;
