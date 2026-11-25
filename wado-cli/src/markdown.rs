//! The `CommonMark` rules `wado spec` reads Markdown by.

/// `CommonMark` fenced code block delimiter — three or more `` ` `` or `~`.
#[derive(Clone, Copy)]
pub struct FenceMarker {
    /// Fence character (`` ` `` or `~`).
    ch: char,
    /// Number of fence characters in the opening run.
    len: usize,
}

/// The fence `s`, a line with its indentation trimmed, opens, if any.
pub fn fence_open(s: &str) -> Option<FenceMarker> {
    let ch = s.chars().next()?;
    if ch != '`' && ch != '~' {
        return None;
    }
    let len = s.chars().take_while(|&c| c == ch).count();
    if len < 3 {
        return None;
    }
    // Backtick fences forbid backticks in the info string per CommonMark; the
    // remaining content after a tilde fence has no such restriction.
    if ch == '`' && s[len..].contains('`') {
        return None;
    }
    Some(FenceMarker { ch, len })
}

/// Whether `s`, a line with its indentation trimmed, closes `open`.
pub fn matches_fence_close(s: &str, open: &FenceMarker) -> bool {
    let count = s.chars().take_while(|&c| c == open.ch).count();
    if count < open.len {
        return false;
    }
    s[count..].chars().all(|c| c == ' ' || c == '\t')
}
