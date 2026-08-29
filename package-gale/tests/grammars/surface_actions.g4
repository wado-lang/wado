// Source: Gale test fixture (Stage C surface matrix)
// License: same as the Gale package
//
// One grammar reaching as much of the action pipeline as a single grammar can,
// driven by a test whose real assertion is that it compiles at all. Every
// defect this fixture was added for reached the user the same way — generation
// reported nothing and the generated module did not compile — which the
// substring assertions in `src/*_test.wado` cannot see and only a fixture
// someone had already written would have caught.
//
// Covered here: `@parser::members` across two blocks, reached bare and through
// `this`; a `@lexer::members` method per receiver (reader, writer, printer),
// reached from a lexer action and a lexer predicate; rule `returns` / `locals`;
// `@init` / `@after`; and a string concatenation over a member field.
grammar SurfaceActions;

@parser::members {
    int calls = 0;
    void bump() { calls += 1; }
}

@parser::members {
    String label() { return "n=" + this.calls; }
}

@lexer::members {
    int depth = 0;
    boolean deep() { return this.depth > 0; }
    void enter() { this.depth = this.depth + 1; }
    void note(String s) { System.out.println(s); }
}

prog
locals [int seen]
@init { $seen = 0; }
@after { System.out.println(label()); }
    : item+
    ;

item returns [int n]
    : OPEN { bump(); $n = 1; }
    | ID { $n = 2; }
    ;

OPEN : '(' { enter(); note("open"); } ;
DEEP : 'd' { this.deep() }? ;
ID : [a-z]+ ;
WS : [ \t\r\n]+ -> skip ;
