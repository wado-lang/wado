//! The format specifier of a template-string interpolation — the text after the
//! top-level `:` in `${expr:SPEC}`.
//!
//! ```text
//! SPEC := [[fill] align] ['+'] ['#'] ['0'] [width] ['.' precision] [kind]
//! align := '<' | '^' | '>'
//! kind  := 'b' | 'o' | 'x' | 'X' | 'e' | 'E' | 'f' | '?'
//! ```
//!
//! One grammar, one parser: the parser validates the spec where it can point at
//! the offending characters, and reify re-parses the same text into the
//! [`TemplateFormatSpec`] the template synthesiser reads. Anything the grammar
//! does not accept is an error — a spec is silently ignorable otherwise, and a
//! mistyped one then formats nothing with no way to notice.
//!
//! `fill` is any character the interpolation scanner does not read as structure
//! when it splits `${…}` — so not `'`, `"`, `` ` ``, `{` or `}`, none of which
//! is a sensible thing to pad with.

use std::fmt;

/// Alignment of the rendered value within `width`.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum Align {
    Left,
    Center,
    Right,
}

impl Align {
    fn from_char(c: char) -> Option<Self> {
        match c {
            '<' => Some(Self::Left),
            '^' => Some(Self::Center),
            '>' => Some(Self::Right),
            _ => None,
        }
    }

    #[must_use]
    pub const fn as_char(self) -> char {
        match self {
            Self::Left => '<',
            Self::Center => '^',
            Self::Right => '>',
        }
    }
}

/// Which format trait renders the interpolated value.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum FormatKind {
    /// No kind character.
    Display,
    Inspect,
    Binary,
    Octal,
    LowerHex,
    UpperHex,
    LowerExp,
    UpperExp,
    /// `f`. Renders through `Display`, which already honours `precision`; the
    /// character exists so `${x:.2f}` reads as a float format.
    Fixed,
}

impl FormatKind {
    fn from_char(c: char) -> Option<Self> {
        match c {
            '?' => Some(Self::Inspect),
            'b' => Some(Self::Binary),
            'o' => Some(Self::Octal),
            'x' => Some(Self::LowerHex),
            'X' => Some(Self::UpperHex),
            'e' => Some(Self::LowerExp),
            'E' => Some(Self::UpperExp),
            'f' => Some(Self::Fixed),
            _ => None,
        }
    }

    /// The character that selects this kind; `None` for the default `Display`.
    #[must_use]
    pub const fn as_char(self) -> Option<char> {
        match self {
            Self::Display => None,
            Self::Inspect => Some('?'),
            Self::Binary => Some('b'),
            Self::Octal => Some('o'),
            Self::LowerHex => Some('x'),
            Self::UpperHex => Some('X'),
            Self::LowerExp => Some('e'),
            Self::UpperExp => Some('E'),
            Self::Fixed => Some('f'),
        }
    }
}

/// A parsed `${expr:SPEC}` specifier.
#[derive(Debug, Clone)]
pub struct TemplateFormatSpec {
    pub fill: Option<char>,
    pub align: Option<Align>,
    pub sign_plus: bool,
    pub alternate: bool,
    pub zero_pad: bool,
    pub width: Option<i32>,
    pub precision: Option<i32>,
    pub kind: FormatKind,
}

impl TemplateFormatSpec {
    /// A spec that only selects `kind`, as `${x:?}` does.
    #[must_use]
    pub const fn of_kind(kind: FormatKind) -> Self {
        Self {
            fill: None,
            align: None,
            sign_plus: false,
            alternate: false,
            zero_pad: false,
            width: None,
            precision: None,
            kind,
        }
    }

    /// Whether the spec asks for anything a bare `Formatter::new` cannot
    /// express, and so needs a full `Formatter` literal.
    #[must_use]
    pub fn needs_formatter_fields(&self) -> bool {
        self.fill.is_some()
            || self.align.is_some()
            || self.sign_plus
            || self.zero_pad
            || self.width.is_some()
            || self.precision.is_some()
    }
}

/// Renders back to the source syntax, so a spec survives a round trip through
/// the IR dumps.
impl fmt::Display for TemplateFormatSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(fill) = self.fill {
            write!(f, "{fill}")?;
        }
        if let Some(align) = self.align {
            write!(f, "{}", align.as_char())?;
        }
        for (present, flag) in [
            (self.sign_plus, '+'),
            (self.alternate, '#'),
            (self.zero_pad, '0'),
        ] {
            if present {
                write!(f, "{flag}")?;
            }
        }
        if let Some(width) = self.width {
            write!(f, "{width}")?;
        }
        if let Some(precision) = self.precision {
            write!(f, ".{precision}")?;
        }
        if let Some(kind) = self.kind.as_char() {
            write!(f, "{kind}")?;
        }
        Ok(())
    }
}

/// Why a spec was rejected, and where — so a caller holding a span can point at
/// the offending character.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct FormatSpecError {
    pub message: String,
    /// Byte offset into the spec text passed to [`parse`], after trimming.
    pub offset: usize,
}

impl fmt::Display for FormatSpecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

/// Parse a format specifier. Surrounding whitespace is trimmed, mirroring the
/// trim on the interpolation expression, so `${ x : 5 }` reads as `${x:5}`. A
/// space fill survives it by being the default fill anyway.
///
/// # Errors
///
/// Returns the first violation of the grammar above, with its offset in the
/// trimmed spec.
pub fn parse(spec: &str) -> Result<TemplateFormatSpec, FormatSpecError> {
    Parser::new(spec).parse()
}

struct Parser {
    chars: Vec<char>,
    /// Byte offsets of `chars`, plus the total length as a final entry, so a
    /// character index maps to the byte offset an error should point at.
    offsets: Vec<usize>,
    index: usize,
}

impl Parser {
    fn new(spec: &str) -> Self {
        let trimmed = spec.trim();
        let lead = spec.len() - spec.trim_start().len();
        let mut offsets: Vec<usize> = trimmed
            .char_indices()
            .map(|(byte, _)| lead + byte)
            .collect();
        offsets.push(lead + trimmed.len());
        Self {
            chars: trimmed.chars().collect(),
            offsets,
            index: 0,
        }
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.index).copied()
    }

    fn peek_second(&self) -> Option<char> {
        self.chars.get(self.index + 1).copied()
    }

    fn eat(&mut self, c: char) -> bool {
        if self.peek() == Some(c) {
            self.index += 1;
            return true;
        }
        false
    }

    fn error(&self, message: impl Into<String>) -> FormatSpecError {
        FormatSpecError {
            message: message.into(),
            offset: self.offsets[self.index.min(self.offsets.len() - 1)],
        }
    }

    fn parse(mut self) -> Result<TemplateFormatSpec, FormatSpecError> {
        if self.chars.is_empty() {
            return Err(self.error("empty format specifier"));
        }

        // [[fill] align] — a fill character is only a fill when an alignment
        // follows it, so `x` alone stays an (invalid) kind character.
        let (fill, align) = match (self.peek(), self.peek_second()) {
            (Some(f), Some(a)) if Align::from_char(a).is_some() => {
                self.index += 2;
                (Some(f), Align::from_char(a))
            }
            (Some(a), _) if Align::from_char(a).is_some() => {
                self.index += 1;
                (None, Align::from_char(a))
            }
            _ => (None, None),
        };

        let sign_plus = self.eat('+');
        let alternate = self.eat('#');
        let zero_pad = self.eat('0');
        let width = self.parse_number("width")?;
        let precision = if self.eat('.') {
            match self.parse_number("precision")? {
                Some(p) => Some(p),
                None => return Err(self.error("expected digits after `.` in format specifier")),
            }
        } else {
            None
        };

        let kind = match self.peek() {
            None => FormatKind::Display,
            Some(c) => match FormatKind::from_char(c) {
                Some(kind) => {
                    self.index += 1;
                    kind
                }
                None => {
                    return Err(self.error(format!(
                        "unknown format specifier `{c}`; expected one of `b`, `o`, `x`, `X`, `e`, `E`, `f`, `?`"
                    )));
                }
            },
        };

        if let Some(c) = self.peek() {
            return Err(self.error(format!("unexpected `{c}` after the format specifier")));
        }

        Ok(TemplateFormatSpec {
            fill,
            align,
            sign_plus,
            alternate,
            zero_pad,
            width,
            precision,
            kind,
        })
    }

    fn parse_number(&mut self, what: &str) -> Result<Option<i32>, FormatSpecError> {
        let start = self.index;
        while self.peek().is_some_and(|c| c.is_ascii_digit()) {
            self.index += 1;
        }
        if start == self.index {
            return Ok(None);
        }
        let digits: String = self.chars[start..self.index].iter().collect();
        digits
            .parse::<i32>()
            .map(Some)
            .map_err(|_| FormatSpecError {
                message: format!("format specifier {what} `{digits}` is too large"),
                offset: self.offsets[start],
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(spec: &str) -> TemplateFormatSpec {
        parse(spec).unwrap_or_else(|e| panic!("`{spec}` should parse: {e}"))
    }

    fn err(spec: &str) -> FormatSpecError {
        parse(spec).unwrap_err()
    }

    #[test]
    fn parses_alignment_and_fill() {
        let s = ok("*^10");
        assert_eq!(s.fill, Some('*'));
        assert_eq!(s.align, Some(Align::Center));
        assert_eq!(s.width, Some(10));

        let bare = ok("<5");
        assert_eq!(bare.fill, None);
        assert_eq!(bare.align, Some(Align::Left));
        assert_eq!(bare.width, Some(5));

        // `-` is a fill, not a sign: only `+` is a sign.
        let dash = ok("-<8");
        assert_eq!(dash.fill, Some('-'));
        assert_eq!(dash.align, Some(Align::Left));
    }

    #[test]
    fn parses_flags_width_precision_kind() {
        let s = ok("+#010.2x");
        assert!(s.sign_plus);
        assert!(s.alternate);
        assert!(s.zero_pad);
        assert_eq!(s.width, Some(10));
        assert_eq!(s.precision, Some(2));
        assert_eq!(s.kind, FormatKind::LowerHex);
    }

    #[test]
    fn zero_flag_is_zero_pad_even_without_width() {
        // `0.2f` is zero-pad + precision, matching the documented grammar —
        // not a width of zero.
        let s = ok("0.2f");
        assert!(s.zero_pad);
        assert_eq!(s.width, None);
        assert_eq!(s.precision, Some(2));
        assert_eq!(s.kind, FormatKind::Fixed);
    }

    #[test]
    fn surrounding_whitespace_is_trimmed() {
        let s = ok(" 5 ");
        assert_eq!(s.width, Some(5));
        // A leading space before an alignment is the default fill anyway.
        assert_eq!(ok(" >5").align, Some(Align::Right));
    }

    #[test]
    fn every_kind_character_is_accepted() {
        for (c, kind) in [
            ('b', FormatKind::Binary),
            ('o', FormatKind::Octal),
            ('x', FormatKind::LowerHex),
            ('X', FormatKind::UpperHex),
            ('e', FormatKind::LowerExp),
            ('E', FormatKind::UpperExp),
            ('f', FormatKind::Fixed),
            ('?', FormatKind::Inspect),
        ] {
            assert_eq!(ok(&c.to_string()).kind, kind, "kind `{c}`");
        }
        assert_eq!(ok("5").kind, FormatKind::Display);
    }

    #[test]
    fn rejects_garbage_instead_of_ignoring_it() {
        assert_eq!(err("").message, "empty format specifier");
        assert_eq!(err("   ").message, "empty format specifier");
        assert!(
            err("zz")
                .message
                .starts_with("unknown format specifier `z`")
        );
        assert!(err("%").message.starts_with("unknown format specifier `%`"));
        assert!(
            err("5:8")
                .message
                .starts_with("unknown format specifier `:`")
        );
        assert!(
            err(".2.3")
                .message
                .starts_with("unknown format specifier `.`")
        );
        assert!(err(".").message.contains("expected digits after `.`"));
        assert!(err("5x1").message.contains("unexpected `1`"));
        assert!(err("99999999999").message.contains("too large"));
    }

    #[test]
    fn error_offsets_point_at_the_offending_character() {
        assert_eq!(err("5zz").offset, 1);
        // The offset is measured in the untrimmed text the caller passed.
        assert_eq!(err(" 5zz").offset, 2);
        assert_eq!(err("10.2q").offset, 4);
    }
}
