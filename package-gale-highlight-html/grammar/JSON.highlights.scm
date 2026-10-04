; Syntax-highlight query for JSON.g4 (Gale highlights.scm subset), for a
; `<script>` whose type holds JSON: an import map or `application/ld+json`.
;
; A key stays a string: an override fires while its rule is anywhere on the
; rule stack, and a nested object's key has the enclosing value on it.

(STRING) @string
(NUMBER) @number

"true" @constant.builtin
"false" @constant.builtin
"null" @constant.builtin

"{" @punctuation.bracket
"}" @punctuation.bracket
"[" @punctuation.bracket
"]" @punctuation.bracket
"," @punctuation.delimiter
":" @punctuation.delimiter
