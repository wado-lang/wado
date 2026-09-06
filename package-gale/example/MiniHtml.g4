// Source: hand-written for Gale's tutorial — the host document for
// `MiniCss.g4` and `MiniJs.g4`.
// License: same as the Gale package.
//
// The lexer modes keep a `<style>` / `<script>` body out of the HTML token
// space and hand it back as one token, so `highlight.wado` can cut it out and
// send it to the grammar that owns it.
//
// Gale accepts `mode` in a combined grammar; ANTLR4 wants a separate `lexer
// grammar` for it. A combined grammar already bundles a lexer, so the
// desugaring is unambiguous — see the compatibility principle in AGENTS.md.
grammar MiniHtml;

document : node* EOF ;

node
    : STYLE_OPEN STYLE_BODY? STYLE_CLOSE
    | SCRIPT_OPEN SCRIPT_BODY? SCRIPT_CLOSE
    | TAG_OPEN
    | TAG_CLOSE
    | TEXT
    ;

STYLE_OPEN  : '<style>' -> pushMode(STYLE) ;
SCRIPT_OPEN : '<script>' -> pushMode(SCRIPT) ;
TAG_CLOSE   : '</' NAME '>' ;
TAG_OPEN    : '<' NAME ~[<>]* '>' ;
TEXT        : ~[<]+ ;

fragment NAME : [a-zA-Z] [a-zA-Z0-9-]* ;

mode STYLE;
STYLE_CLOSE : '</style>' -> popMode ;
STYLE_BODY  : ~[<]+ ;

mode SCRIPT;
SCRIPT_CLOSE : '</script>' -> popMode ;
SCRIPT_BODY  : ~[<]+ ;
