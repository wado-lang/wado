// Source: Gale test fixture (Stage C predicate on a token-led branch)
// License: same as the Gale package
//
// The alternatives have distinct first tokens, so dispatch is token-led
// (`Direct`), not a tournament. An alt-initial predicate must still guard its
// branch (ANTLR's "predicate tested even when unambiguous"): a false predicate
// on the uniquely-selected alt fails the parse into no-viable.
grammar WadoPredDirect;

options { language = Wado; }

ok  : ID { p.emit("id"); }
    | {true}?  NUM { p.emit("num"); }
    ;

bad : ID { p.emit("id"); }
    | {false}? NUM { p.emit("num"); }
    ;

ID : [a-z]+ ;
NUM : [0-9]+ ;
WS : ' ' -> skip ;
