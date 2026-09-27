// ANTLR4 issue #4813: in `'[' [ \t]* [xX ] [ \t]* ']'` the loop's set overlaps
// the set after it, so on `[ ]` the loop must leave the space for `[xX ]`.
grammar LexerStarOverlapsNextSet;

checklistFile : itemLine EOF ;
itemLine : ITEM_LINE_CONTENT NEWLINE+ ;

WS : [ \t]+ -> channel(HIDDEN) ;
ITEM_LINE_CONTENT : '-' [ \t]* '[' [ \t]* [xX ] [ \t]* ']' ( [ \t]+ ~[\r\n]+ )? ;
NEWLINE : ( '\r'? '\n' | '\r' )+ ;
ENDOFLIST : '---ENDOFLIST---' ;
