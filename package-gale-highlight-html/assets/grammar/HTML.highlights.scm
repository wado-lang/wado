; Syntax-highlight query for HTMLLexer.g4 / HTMLParser.g4 (Gale highlights.scm
; subset). Captures use the tree-sitter standard vocabulary; each becomes a CSS
; class.
;
; A `<script>` / `<style>` body is one token to this grammar. `src/lib.wado`
; hands its text to the JavaScript or CSS parser, so nothing here classifies it.

; A `TAG_NAME` names an attribute inside `htmlAttribute` and the element
; everywhere else; an override is decided by the rule stack.
(htmlAttribute (TAG_NAME) @attribute)
(TAG_NAME) @tag
(ATTVALUE_VALUE) @string
(TAG_EQUALS) @operator

(TAG_OPEN) @punctuation.bracket
(TAG_CLOSE) @punctuation.bracket
(TAG_SLASH) @punctuation.bracket
(TAG_SLASH_CLOSE) @punctuation.bracket

(HTML_COMMENT) @comment
(HTML_CONDITIONAL_COMMENT) @comment
(CDATA) @string.special
(DTD) @keyword
(XML) @keyword
(SCRIPTLET) @embedded
