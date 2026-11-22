# Re — Linear-Time Regular Expressions for Wado

`wado-lang:re` matches ECMA-262 patterns, read as the `u` flag reads them, in
time linear in the input for every pattern. Backreferences and lookaround are
refused, since no linear-time engine can run them.
[WEP: Re](../docs/wep-2026-11-02-re.md) holds the design and the work ahead.

## Writing a Pattern

`re` takes a pattern as written, and `rex` is Perl's `/x`: outside a character
class it ignores whitespace, and `#` starts a comment to the end of the line.
Write either as a raw template, ``re#`…`#``, so each backslash reaches the
pattern as written. `Regex::new` builds one at run time and returns a malformed
pattern as an `Err`; the tags panic on one.

```wado
use { re, rex, Regex } from "wado-lang:re";

global DATE: Regex = re#`(?<y>\d{4})-(?<m>\d{2})-(?<d>\d{2})`#;

global FLOAT: Regex = rex#`
    ^ [+-]?
    ( \d+ (\.\d*)? | \.\d+ )   # mantissa
    ( [eE] [+-]? \d+ )?        # exponent
    $
`#;

fn year(text: String) -> Option<String> {
    let caps = DATE.captures(text)?;
    return Some(caps["y"]?.as_str().to_string());
}
```

A hole holding text matches that text literally, so text from a user cannot
inject syntax. A hole holding a `Regex` is spliced in as a non-capturing group,
and its own groups keep capturing.

```wado
let word = re#`\w+`#;
let greeting = re#`^${prefix}: ${word}$`#;   // `prefix` matches as plain text
```

The modifiers `(?ims-ims:…)` set `i`, `m` and `s` for a group, since a tag
takes no flag argument. `\p{…}` reads its properties from `core:icu`.

## Searching

The text is any `AsStrSlice`, and a match is a `StrSlice` over it, so neither
copies. Offsets count bytes.

- `is_match`, `find` and `captures` answer the leftmost match. `find_at` and
  `captures_at` start at a byte offset, with the text before it still read as
  context.
- `find_iter` and `captures_iter` find each match left to right, and skip an
  empty match where the previous one ended.
- `replace` and `replace_all` take a `Replacer`: a `String` or `StrSlice` that
  reads `$1`, `$<name>`, `$&` and `$$`, or a `fn(&Captures) -> String`.
- `split` yields one more piece than there are matches, so the pieces and the
  matches between them spell the text again. `splitn` stops at `n` pieces, the
  last holding the rest.

`validate_ecma262_syntax` accepts every pattern the `u` flag reads, including
those Re cannot match, as JSON Schema's `regex` format asks.
