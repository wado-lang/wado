// Source: Gale test fixture (Stage C superClass operation with arguments)
// License: same as the Gale package
//
// A base predicate that takes an argument, the shape `TypeScriptParser` uses
// (`{this.p("of")}?`). The call sites are the operation's only signature
// source, so the literal they pass is what the generated interface declares.
grammar WadoSuperArg;

options { superClass = ArgBase; language = Wado; }

kw : { this.word("of") }? ID { p.emit("of") }
   | { this.word("in") }? ID { p.emit("in") }
   | ID { p.emit("other") }
   ;

ID : [a-z]+ ;
WS : [ \t\r\n]+ -> skip ;
