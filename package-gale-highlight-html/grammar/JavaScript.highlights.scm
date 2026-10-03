; Syntax-highlight query for JavaScriptLexer.g4 / JavaScriptParser.g4 (Gale
; highlights.scm subset). Captures use the tree-sitter standard vocabulary; each
; becomes a CSS class.
;
; An override fires while its rule is anywhere on the rule stack, so only a rule
; whose whole subtree is a name can carry one. `identifierName` is: the name
; after `.`, an object key and an import alias. A declaration's name is not,
; since its body sits under the same rule, so a plain identifier stays
; uncoloured: telling a function from a variable takes name resolution.

; An import or export list names bindings rather than properties, though each
; name is an `identifierName` too.
(importModuleItems (Identifier) @variable)
(exportModuleItems (Identifier) @variable)
(importNamespace (Identifier) @variable)
(identifierName (Identifier) @property)
(privateIdentifier "#" @property)

(SingleLineComment) @comment
(MultiLineComment) @comment
(HtmlComment) @comment
(CDataComment) @comment
(HashBangLine) @comment

(StringLiteral) @string
(BackTick) @string
(TemplateStringAtom) @string
(TemplateStringStartExpression) @punctuation.special
(TemplateCloseBrace) @punctuation.special
(RegularExpressionLiteral) @string.regexp

(DecimalLiteral) @number
(HexIntegerLiteral) @number
(OctalIntegerLiteral) @number
(OctalIntegerLiteral2) @number
(BinaryIntegerLiteral) @number
(BigHexIntegerLiteral) @number
(BigOctalIntegerLiteral) @number
(BigBinaryIntegerLiteral) @number
(BigDecimalIntegerLiteral) @number

(NullLiteral) @constant.builtin
(BooleanLiteral) @constant.builtin
(This) @variable.builtin
(Super) @variable.builtin

(Break) @keyword
(Do) @keyword
(Instanceof) @keyword
(Typeof) @keyword
(Case) @keyword
(Else) @keyword
(New) @keyword
(Var) @keyword
(Catch) @keyword
(Finally) @keyword
(Return) @keyword
(Void) @keyword
(Continue) @keyword
(For) @keyword
(Switch) @keyword
(While) @keyword
(Debugger) @keyword
(Function_) @keyword
(With) @keyword
(Default) @keyword
(If) @keyword
(Throw) @keyword
(Delete) @keyword
(In) @keyword
(Try) @keyword
(As) @keyword
(From) @keyword
(Of) @keyword
(Yield) @keyword
(YieldStar) @keyword
(Class) @keyword
(Enum) @keyword
(Extends) @keyword
(Const) @keyword
(Export) @keyword
(Import) @keyword
(Async) @keyword
(Await) @keyword
(Implements) @keyword
(StrictLet) @keyword
(NonStrictLet) @keyword
(Private) @keyword
(Public) @keyword
(Interface) @keyword
(Package) @keyword
(Protected) @keyword
(Static) @keyword

(Assign) @operator
(QuestionMark) @operator
(QuestionMarkDot) @operator
(Ellipsis) @operator
(PlusPlus) @operator
(MinusMinus) @operator
(Plus) @operator
(Minus) @operator
(BitNot) @operator
(Not) @operator
(Multiply) @operator
(Divide) @operator
(Modulus) @operator
(Power) @operator
(NullCoalesce) @operator
(RightShiftArithmetic) @operator
(LeftShiftArithmetic) @operator
(RightShiftLogical) @operator
(LessThan) @operator
(MoreThan) @operator
(LessThanEquals) @operator
(GreaterThanEquals) @operator
(Equals_) @operator
(NotEquals) @operator
(IdentityEquals) @operator
(IdentityNotEquals) @operator
(BitAnd) @operator
(BitXOr) @operator
(BitOr) @operator
(And) @operator
(Or) @operator
(MultiplyAssign) @operator
(DivideAssign) @operator
(ModulusAssign) @operator
(PlusAssign) @operator
(MinusAssign) @operator
(LeftShiftArithmeticAssign) @operator
(RightShiftArithmeticAssign) @operator
(RightShiftLogicalAssign) @operator
(BitAndAssign) @operator
(BitXorAssign) @operator
(BitOrAssign) @operator
(PowerAssign) @operator
(NullishCoalescingAssign) @operator
(ARROW) @operator

(OpenBracket) @punctuation.bracket
(CloseBracket) @punctuation.bracket
(OpenParen) @punctuation.bracket
(CloseParen) @punctuation.bracket
(OpenBrace) @punctuation.bracket
(CloseBrace) @punctuation.bracket
(SemiColon) @punctuation.delimiter
(Comma) @punctuation.delimiter
(Colon) @punctuation.delimiter
(Dot) @punctuation.delimiter
