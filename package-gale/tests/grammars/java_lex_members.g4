// Source: Gale test fixture (Stage C @lexer::members under language = Java)
// License: same as the Gale package
//
// A Java `@lexer::members` block. The field is written by an action — which runs
// only for the rule that won the tournament — and read by a predicate, which is
// evaluated for every candidate, losers included. `deep()` therefore has to take
// `&self`: the predicate holds `&Lexer`. `push()` writes, so it takes
// `&mut self` and is reachable only from the action.
grammar JavaLexMembers;

@lexer::members {
    int depth = 0;
    boolean deep() { return this.depth > 1; }
    void push() { this.depth = this.depth + 1; }
}

s : t+ ;

t : A {System.out.print("a");}
  | B {System.out.print("B");}
  | C {System.out.print("c");}
  ;

A : 'a' {this.push();} ;
B : 'b' {this.deep()}? ;
C : 'b' ;
WS : [ \t\r\n]+ -> skip ;
