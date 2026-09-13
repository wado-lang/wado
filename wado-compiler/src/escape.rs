//! Escape decoding for string, byte, char, and template literals.
//!
//! One decoder for every phase that reads a literal's raw text: the parser
//! reads an attribute's arguments, the elaborator reads an expression's.

/// The source text a string literal needs between its quotes to denote `s`.
/// The inverse of [`unescape_string`].
pub(crate) fn escape_string(s: &str) -> String {
    let mut result = String::new();
    for c in s.chars() {
        match c {
            '"' => result.push_str("\\\""),
            '\\' => result.push_str("\\\\"),
            '\n' => result.push_str("\\n"),
            '\r' => result.push_str("\\r"),
            '\t' => result.push_str("\\t"),
            '\0' => result.push_str("\\0"),
            c if c.is_control() => {
                result.push_str(&format!("\\u{{{:04X}}}", c as u32));
            }
            c => result.push(c),
        }
    }
    result
}

/// Interpret the raw content of a string literal (between quotes, without the quotes).
///
/// Processes all escape sequences and handles surrogate pairs (`𐀀` style).
/// Returns the resulting Rust `String`, or an error message.
pub(crate) fn unescape_string(raw: &str) -> Result<String, String> {
    let mut result = String::new();
    let mut chars = raw.chars().peekable();
    let mut pending_high: Option<u16> = None;

    while let Some(ch) = chars.next() {
        if ch == '\\' {
            let escaped = unescape_one(&mut chars)?;
            if let Some(code_unit) = as_surrogate_code_unit(escaped) {
                if is_high_surrogate(code_unit) {
                    if pending_high.is_some() {
                        return Err(
                            "invalid surrogate pair: high surrogate not followed by low surrogate"
                                .to_string(),
                        );
                    }
                    pending_high = Some(code_unit);
                    continue;
                } else if is_low_surrogate(code_unit)
                    && let Some(high) = pending_high.take()
                {
                    let combined = decode_surrogate_pair(high, code_unit);
                    let c = char::from_u32(combined)
                        .ok_or_else(|| "invalid surrogate pair".to_string())?;
                    result.push(c);
                    continue;
                }
            }
            if pending_high.is_some() {
                return Err(
                    "invalid surrogate pair: high surrogate not followed by low surrogate"
                        .to_string(),
                );
            }
            result.push(escaped);
        } else {
            if pending_high.is_some() {
                return Err(
                    "invalid surrogate pair: high surrogate not followed by low surrogate"
                        .to_string(),
                );
            }
            result.push(ch);
        }
    }
    if pending_high.is_some() {
        return Err("invalid surrogate pair: high surrogate at end of string".to_string());
    }
    Ok(result)
}

/// Interpret the raw content of a byte-string literal `b"..."` (between quotes,
/// without the quotes) into the bytes it denotes.
///
/// `\xNN` (two hex digits) yields one raw byte; every other escape shares the
/// string decoder ([`unescape_one`]) and must resolve to an ASCII value.
/// Unicode escapes (`\u{...}` / `\uHHHH`), non-ASCII source characters, and
/// escapes above U+007F are rejected — a byte string is ASCII plus `\xNN`.
pub(crate) fn unescape_bytes(raw: &str) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    let mut chars = raw.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            if chars.peek() == Some(&'x') {
                chars.next();
                let hi = chars
                    .next()
                    .ok_or_else(|| "unterminated `\\x` escape".to_string())?;
                let lo = chars
                    .next()
                    .ok_or_else(|| "unterminated `\\x` escape".to_string())?;
                let byte = u8::from_str_radix(&format!("{hi}{lo}"), 16)
                    .map_err(|_| format!("invalid `\\x` escape: \\x{hi}{lo}"))?;
                out.push(byte);
            } else if chars.peek() == Some(&'u') {
                return Err(
                    "unicode escape `\\u` is not allowed in a byte literal; use `\\xNN`"
                        .to_string(),
                );
            } else {
                let decoded = unescape_one(&mut chars)? as u32;
                if decoded > 0x7f {
                    return Err(format!(
                        "escape resolves to U+{decoded:04X}, out of byte range; use `\\xNN`"
                    ));
                }
                out.push(decoded as u8);
            }
        } else if ch.is_ascii() {
            out.push(ch as u8);
        } else {
            return Err(format!(
                "non-ASCII character in byte string: '{ch}' (use a `\\xNN` escape)"
            ));
        }
    }
    Ok(out)
}

/// Decode a byte literal `b'x'` (raw content, without quotes) to its single
/// byte via [`unescape_bytes`], requiring exactly one byte.
pub(crate) fn unescape_byte(raw: &str) -> Result<u8, String> {
    match unescape_bytes(raw)?.as_slice() {
        [b] => Ok(*b),
        [] => Err("empty byte literal".to_string()),
        _ => Err("byte literal must be exactly one byte".to_string()),
    }
}

/// Interpret the raw content of a char literal (between quotes, without the quotes).
///
/// Returns the resulting `char`, or an error message.
pub(crate) fn unescape_char(raw: &str) -> Result<char, String> {
    let mut chars = raw.chars().peekable();
    let result = match chars.next() {
        Some('\\') => unescape_one(&mut chars)?,
        Some(c) => c,
        None => return Err("empty char literal".to_string()),
    };
    if chars.next().is_some() {
        return Err("char literal contains more than one character".to_string());
    }
    Ok(result)
}

/// Unescape a template string literal part (raw text between backticks).
///
/// Like `unescape_string` but also handles the escapes only a template needs:
/// the interpolation syntax (`\{`, `\}`, `\$`) and the delimiter (`` \` ``).
/// [`unescape_template_string`] past the body walk, which is what rejects a
/// malformed escape — no phase after it has a diagnostic channel for one.
pub(crate) fn unescape_template_segment(raw: &str) -> String {
    unescape_template_string(raw).expect("the body walk rejects a malformed template escape")
}

pub(crate) fn unescape_template_string(raw: &str) -> Result<String, String> {
    let mut result = String::new();
    let mut chars = raw.chars().peekable();
    let mut pending_high: Option<u16> = None;

    while let Some(ch) = chars.next() {
        if ch == '\\' {
            // Handle template-specific escapes first — the interpolation
            // syntax and the delimiter itself, none of which a plain string
            // literal has to escape.
            if let Some(&next) = chars.peek()
                && (next == '{' || next == '}' || next == '$' || next == '`')
            {
                if pending_high.is_some() {
                    return Err(
                        "invalid surrogate pair: high surrogate not followed by low surrogate"
                            .to_string(),
                    );
                }
                chars.next();
                result.push(next);
                continue;
            }
            let escaped = unescape_one(&mut chars)?;
            if let Some(code_unit) = as_surrogate_code_unit(escaped) {
                if is_high_surrogate(code_unit) {
                    if pending_high.is_some() {
                        return Err(
                            "invalid surrogate pair: high surrogate not followed by low surrogate"
                                .to_string(),
                        );
                    }
                    pending_high = Some(code_unit);
                    continue;
                } else if is_low_surrogate(code_unit)
                    && let Some(high) = pending_high.take()
                {
                    let combined = decode_surrogate_pair(high, code_unit);
                    let c = char::from_u32(combined)
                        .ok_or_else(|| "invalid surrogate pair".to_string())?;
                    result.push(c);
                    continue;
                }
            }
            if pending_high.is_some() {
                return Err(
                    "invalid surrogate pair: high surrogate not followed by low surrogate"
                        .to_string(),
                );
            }
            result.push(escaped);
        } else {
            if pending_high.is_some() {
                return Err(
                    "invalid surrogate pair: high surrogate not followed by low surrogate"
                        .to_string(),
                );
            }
            result.push(ch);
        }
    }
    if pending_high.is_some() {
        return Err("invalid surrogate pair: high surrogate at end of string".to_string());
    }
    Ok(result)
}

/// Parse one escape sequence (after the leading `\` has been consumed).
fn unescape_one<I: Iterator<Item = char>>(
    chars: &mut std::iter::Peekable<I>,
) -> Result<char, String> {
    match chars.next() {
        Some('n') => Ok('\n'),
        Some('t') => Ok('\t'),
        Some('r') => Ok('\r'),
        Some('\\') => Ok('\\'),
        Some('"') => Ok('"'),
        Some('\'') => Ok('\''),
        Some('/') => Ok('/'),
        Some('b') => Ok('\x08'),
        Some('f') => Ok('\x0C'),
        Some('0') => Ok('\0'),
        Some('u') => unescape_unicode(chars),
        Some(c) => Err(format!("invalid escape sequence: \\{c}")),
        None => Err("unterminated escape sequence".to_string()),
    }
}

fn unescape_unicode<I: Iterator<Item = char>>(
    chars: &mut std::iter::Peekable<I>,
) -> Result<char, String> {
    if chars.peek() == Some(&'{') {
        chars.next(); // consume '{'
        let mut hex = String::new();
        loop {
            match chars.next() {
                Some('}') => break,
                Some(c) if c.is_ascii_hexdigit() => hex.push(c),
                Some(c) => return Err(format!("invalid character in unicode escape: {c}")),
                None => return Err("unterminated unicode escape".to_string()),
            }
        }
        if hex.is_empty() {
            return Err("empty unicode escape".to_string());
        }
        let code_point = u32::from_str_radix(&hex, 16)
            .map_err(|_| format!("invalid unicode escape: \\u{{{hex}}}"))?;
        char::from_u32(code_point)
            .ok_or_else(|| format!("invalid unicode code point: U+{code_point:04X}"))
    } else {
        let mut hex = String::new();
        for _ in 0..4 {
            match chars.next() {
                Some(c) if c.is_ascii_hexdigit() => hex.push(c),
                _ => return Err("expected 4 hex digits after \\u".to_string()),
            }
        }
        let code_unit = u16::from_str_radix(&hex, 16)
            .map_err(|_| format!("invalid unicode escape: \\u{hex}"))?;
        if is_high_surrogate(code_unit) || is_low_surrogate(code_unit) {
            // Encode surrogate as PUA so the caller can detect and combine pairs
            let pua = if is_high_surrogate(code_unit) {
                u32::from(code_unit - 0xD800) + 0xE000
            } else {
                u32::from(code_unit - 0xDC00) + 0xE800
            };
            Ok(char::from_u32(pua).unwrap())
        } else {
            char::from_u32(u32::from(code_unit))
                .ok_or_else(|| format!("invalid unicode code point: U+{code_unit:04X}"))
        }
    }
}

fn as_surrogate_code_unit(c: char) -> Option<u16> {
    let code = c as u32;
    if (0xE000..=0xE7FF).contains(&code) {
        Some((code - 0xE000 + 0xD800) as u16)
    } else if (0xE800..=0xEBFF).contains(&code) {
        Some((code - 0xE800 + 0xDC00) as u16)
    } else {
        None
    }
}

fn is_high_surrogate(code_unit: u16) -> bool {
    (0xD800..=0xDBFF).contains(&code_unit)
}

fn is_low_surrogate(code_unit: u16) -> bool {
    (0xDC00..=0xDFFF).contains(&code_unit)
}

fn decode_surrogate_pair(high: u16, low: u16) -> u32 {
    let high = u32::from(high - 0xD800);
    let low = u32::from(low - 0xDC00);
    0x10000 + (high << 10) + low
}
