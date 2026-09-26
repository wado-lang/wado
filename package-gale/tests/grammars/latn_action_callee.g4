// An ATN-class rule with an action is matched by `latn_match`, but its action
// replay re-walks the body with generated matchers. `ESC` and `INNER` are
// called only from such replays, so their `frag_` / `try_` matchers must still
// be emitted.
grammar LatnActionCallee;

options { language = Wado; }

@lexer::members {
    count: i32 = 0
}

start
    : (STR | NEST | ID)+ EOF
    ;

STR
    : '"' (~["] | ESC)* '"' { lx.count += 1; }
    ;

fragment ESC
    : '\\' '"'
    | '\\' '\\'
    ;

NEST
    : '<' (~[>] | INNER)* '>' { lx.count += 1; }
    ;

fragment INNER
    : '<' INNER* '>'
    ;

ID
    : [c-z]+
    ;

WS
    : [ \t\r\n]+ -> skip
    ;
