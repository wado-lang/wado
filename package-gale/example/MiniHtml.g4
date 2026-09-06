// Source: hand-written for Gale's tutorial — the host of `MiniCss.g4` and
// `MiniJs.g4`.
// License: same as the Gale package.
//
// `import` composes all three into one grammar: one lexer with three modes,
// one parser, one tree. `stylesheet` and `program` below are the delegates'
// start rules — undefined in this file, resolved once composition has run.
//
// This grammar owns the boundaries and nothing else. It declares `mode CSS`
// and `mode JS` for the closing tags that leave them; `MiniCss.g4` and
// `MiniJs.g4` declare the same mode names for their own rules, and composition
// unifies them by name. So the host knows where each language starts and ends,
// and neither delegate knows it has a host.
//
// Gale accepts `mode` in a combined grammar; ANTLR4 wants a separate `lexer
// grammar` for it. A combined grammar already bundles a lexer, so the
// desugaring is unambiguous — see the compatibility principle in AGENTS.md.
grammar MiniHtml;

import MiniCss, MiniJs;

document : node* EOF ;

node
    : STYLE_OPEN stylesheet STYLE_CLOSE
    | SCRIPT_OPEN program SCRIPT_CLOSE
    | TAG_OPEN
    | TAG_CLOSE
    | TEXT
    ;

STYLE_OPEN  : '<style>' -> pushMode(CSS) ;
SCRIPT_OPEN : '<script>' -> pushMode(JS) ;
TAG_CLOSE   : '</' NAME '>' ;
TAG_OPEN    : '<' NAME ~[<>]* '>' ;
TEXT        : ~[<]+ ;

fragment NAME : [a-zA-Z] [a-zA-Z0-9-]* ;

mode CSS;
STYLE_CLOSE : '</style>' -> popMode ;

mode JS;
SCRIPT_CLOSE : '</script>' -> popMode ;
