; Syntax-highlight query for css3Lexer.g4 / css3Parser.g4 (Gale highlights.scm
; subset). Captures use the tree-sitter standard vocabulary; each becomes a CSS
; class.
;
; One `Ident` token is a tag, a class, a property or a value by where the parser
; put it. An override fires while its rule is anywhere on the rule stack and the
; first one written wins, so a rule nested inside another is listed above it.

(elementName (Ident) @tag)
(className (Ident) @type)
(className "." @type)
(attrib (Ident) @attribute)
(pseudo (Ident) @attribute)
(pseudo ":" @attribute)
(hexcolor (Hash) @number)
(Hash) @type
(property_ (Ident) @property)
(Variable) @variable
(keyframeSelector (From) @keyword)
(keyframeSelector (To) @keyword)
(mediaType (Ident) @type)
(mediaFeature (Ident) @property)
(term (Ident) @constant)

(Comment) @comment
(String_) @string
(Url) @string.special
(UnicodeRange) @number
(Number) @number
(Percentage) @number
(Dimension) @number
(UnknownDimension) @number

(Function_) @function
(Url_) @function
(Var) @function
(Calc) @function
(PseudoNot) @attribute

(AtKeyword) @keyword
(Import) @keyword
(Page) @keyword
(Media) @keyword
(Namespace) @keyword
(Charset) @keyword
(FontFace) @keyword
(Supports) @keyword
(Keyframes) @keyword
(Viewport) @keyword
(CounterStyle) @keyword
(FontFeatureValues) @keyword
(Important) @keyword
(MediaOnly) @keyword
(Not) @keyword
(And) @keyword
(Or) @keyword

(Plus) @operator
(Minus) @operator
(Greater) @operator
(Tilde) @operator
(Includes) @operator
(DashMatch) @operator
(PrefixMatch) @operator
(SuffixMatch) @operator
(SubstringMatch) @operator
(Equal) @operator
(Multiply) @operator
(Divide) @operator

(OpenBrace) @punctuation.bracket
(CloseBrace) @punctuation.bracket
(OpenParen) @punctuation.bracket
(CloseParen) @punctuation.bracket
(OpenBracket) @punctuation.bracket
(CloseBracket) @punctuation.bracket
(SemiColon) @punctuation.delimiter
(Colon) @punctuation.delimiter
(Comma) @punctuation.delimiter
