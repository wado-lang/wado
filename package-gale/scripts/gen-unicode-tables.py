#!/usr/bin/env python3
"""Regenerate `src/g4/unicode_tables.wado` from the Unicode Character Database.

Gale expands `\\p{...}` / `\\P{...}` in a lexer char set into code-point ranges
at grammar-parse time. `\\P` is the complement of `\\p`, so an approximate table
does not just miss characters — it admits them. The table therefore has to be
the real general-category assignment, generated rather than hand-maintained.

Usage:
    curl -O https://www.unicode.org/Public/UCD/latest/ucd/UnicodeData.txt
    python3 scripts/gen-unicode-tables.py UnicodeData.txt <version> > src/g4/unicode_tables.wado
"""

import sys

CODE_POINT_MAX = 0x10FFFF

# General-category long aliases (PropertyValueAliases.txt). The group letters
# (L, M, N, P, S, Z, C) are unions computed by the consumer, not stored here.
LONG_ALIASES = {
    "Lu": "Uppercase_Letter",
    "Ll": "Lowercase_Letter",
    "Lt": "Titlecase_Letter",
    "Lm": "Modifier_Letter",
    "Lo": "Other_Letter",
    "Mn": "Nonspacing_Mark",
    "Mc": "Spacing_Mark",
    "Me": "Enclosing_Mark",
    "Nd": "Decimal_Number",
    "Nl": "Letter_Number",
    "No": "Other_Number",
    "Pc": "Connector_Punctuation",
    "Pd": "Dash_Punctuation",
    "Ps": "Open_Punctuation",
    "Pe": "Close_Punctuation",
    "Pi": "Initial_Punctuation",
    "Pf": "Final_Punctuation",
    "Po": "Other_Punctuation",
    "Sm": "Math_Symbol",
    "Sc": "Currency_Symbol",
    "Sk": "Modifier_Symbol",
    "So": "Other_Symbol",
    "Zs": "Space_Separator",
    "Zl": "Line_Separator",
    "Zp": "Paragraph_Separator",
    "Cc": "Control",
    "Cf": "Format",
    "Cs": "Surrogate",
    "Co": "Private_Use",
    "Cn": "Unassigned",
}

GROUP_LONG = {
    "L": "Letter",
    "M": "Mark",
    "N": "Number",
    "P": "Punctuation",
    "S": "Symbol",
    "Z": "Separator",
    "C": "Other",
}


def read_categories(path):
    """Code-point -> category runs, expanding the `First>`/`Last>` range pairs."""
    runs = []  # (start, end, category)
    pending_first = None
    with open(path, encoding="utf-8") as f:
        for line in f:
            fields = line.rstrip("\n").split(";")
            if len(fields) < 3:
                continue
            code = int(fields[0], 16)
            name = fields[1]
            cat = fields[2]
            if name.endswith(", First>"):
                pending_first = (code, cat)
                continue
            if name.endswith(", Last>"):
                start, start_cat = pending_first
                assert start_cat == cat, f"range halves disagree at {code:04X}"
                runs.append((start, code, cat))
                pending_first = None
                continue
            runs.append((code, code, cat))
    assert pending_first is None, "unterminated First> range"
    return runs


def category_ranges(runs):
    """Merge each category's runs into maximal ranges. Unassigned code points
    are Cn, which is exactly what is left over."""
    by_cat = {}
    for start, end, cat in runs:
        by_cat.setdefault(cat, []).append((start, end))

    assigned = []
    for start, end, _ in runs:
        assigned.append((start, end))
    assigned.sort()
    cn = []
    next_cp = 0
    for start, end in assigned:
        if start > next_cp:
            cn.append((next_cp, start - 1))
        next_cp = max(next_cp, end + 1)
    if next_cp <= CODE_POINT_MAX:
        cn.append((next_cp, CODE_POINT_MAX))
    by_cat["Cn"] = cn

    out = {}
    for cat, ranges in by_cat.items():
        ranges.sort()
        merged = []
        for start, end in ranges:
            if merged and start <= merged[-1][1] + 1:
                merged[-1] = (merged[-1][0], max(merged[-1][1], end))
            else:
                merged.append((start, end))
        out[cat] = merged
    return out


def encode(ranges):
    """`start[-end]` pairs in hex, comma separated. A single code point drops
    its `-end` half, which is most of the table."""
    parts = []
    for start, end in ranges:
        if start == end:
            parts.append(f"{start:X}")
        else:
            parts.append(f"{start:X}-{end:X}")
    return ",".join(parts)


def main():
    path = sys.argv[1]
    version = sys.argv[2]
    cats = category_ranges(read_categories(path))

    out = []
    out.append('#![generated(by = "scripts/regen-unicode-tables.sh", sources = [])]')
    out.append("//! Unicode general-category ranges for `\\p{...}` / `\\P{...}` char sets.")
    out.append("//!")
    out.append(f"//! Do not edit by hand — regenerate from UCD {version} with")
    out.append("//! `scripts/regen-unicode-tables.sh`.")
    out.append("//!")
    out.append("//! A category's ranges are `START[-END]` in hex, comma separated. `\\P`")
    out.append("//! complements what `\\p` selects, so an approximate table would admit real")
    out.append("//! members of the property, not merely miss some.")
    out.append("")
    out.append(f'pub global UNICODE_VERSION: String = "{version}";')
    out.append("")
    out.append("/// Ranges of one general category (`Lu`, `Nd`, …), or an empty string when")
    out.append("/// the name is not a general category. Group letters (`L`, `N`, …) are")
    out.append("/// unions of their subcategories and are resolved by the caller.")
    out.append("pub fn general_category_ranges(name: &String) -> String {")
    out.append("    return match *name {")
    for cat in sorted(cats):
        long = LONG_ALIASES[cat]
        out.append(f'        "{cat}" | "{long}" => "{encode(cats[cat])}",')
    out.append('        _ => "",')
    out.append("    };")
    out.append("}")
    out.append("")
    out.append("/// Subcategories of a general-category group letter (`L` -> `Lu`, `Ll`, …),")
    out.append("/// or an empty list when the name is not a group.")
    out.append("pub fn general_category_group(name: &String) -> List<String> {")
    out.append("    return match *name {")
    for group in "LMNPSZC":
        subs = sorted(c for c in cats if c.startswith(group))
        joined = ", ".join(f'"{c}"' for c in subs)
        out.append(f'        "{group}" | "{GROUP_LONG[group]}" => [{joined}],')
    out.append("        _ => [],")
    out.append("    };")
    out.append("}")
    out.append("")
    print("\n".join(out))


if __name__ == "__main__":
    main()
