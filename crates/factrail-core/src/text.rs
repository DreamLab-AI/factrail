//! Character-indexed string helpers.
//!
//! Every length and offset the rails reason about is counted in Unicode scalar
//! values (`char`s), as the calibrating engine (Python) counts them, so a
//! threshold such as "never cut an observation of 6000 chars or less" means the
//! same thing for an ASCII log and for one carrying CJK paths. Tool output is
//! overwhelmingly ASCII, so each helper takes an O(1) byte path when it can.

/// Length of `s` in `char`s.
pub fn clen(s: &str) -> usize {
    if s.is_ascii() {
        s.len()
    } else {
        s.chars().count()
    }
}

/// Byte offset of the `ci`-th `char` of `s`, or `s.len()` past the end.
pub fn byte_at(s: &str, ci: usize) -> usize {
    if s.is_ascii() {
        return ci.min(s.len());
    }
    s.char_indices().nth(ci).map_or(s.len(), |(b, _)| b)
}

/// `char` index of byte offset `b` (which must lie on a `char` boundary).
pub fn char_at(s: &str, b: usize) -> usize {
    if s.is_ascii() {
        b
    } else {
        s[..b].chars().count()
    }
}

/// The `char`s `[a, b)` of `s`, clamped to its length.
pub fn cslice(s: &str, a: usize, b: usize) -> &str {
    let start = byte_at(s, a);
    let end = byte_at(s, b.max(a));
    &s[start..end]
}

/// The first `n` `char`s of `s`.
pub fn head(s: &str, n: usize) -> &str {
    &s[..byte_at(s, n)]
}

/// The last `n` `char`s of `s`.
pub fn tail(s: &str, n: usize) -> &str {
    let len = clen(s);
    &s[byte_at(s, len.saturating_sub(n))..]
}

/// `char` index of the last `needle` starting at or before `char` index `from`
/// (JavaScript's `lastIndexOf(needle, from)` and Python's `rfind(needle, 0, from + 1)`).
pub fn rfind_char(s: &str, needle: char, from: usize) -> Option<usize> {
    let limit = byte_at(s, from.saturating_add(1));
    s[..limit].rfind(needle).map(|b| char_at(s, b))
}

/// `char` index of the first `needle` at or after `char` index `from`.
pub fn find_char(s: &str, needle: char, from: usize) -> Option<usize> {
    let start = byte_at(s, from);
    s[start..].find(needle).map(|b| char_at(s, start + b))
}

/// `char` index of the first occurrence of `needle` in `s`.
pub fn find_str(s: &str, needle: &str) -> Option<usize> {
    s.find(needle).map(|b| char_at(s, b))
}

/// `s` cut to `limit` `char`s with a trailing ellipsis when it was longer.
pub fn truncate(s: &str, limit: usize) -> String {
    if clen(s) <= limit {
        return s.to_owned();
    }
    let mut out = head(s, limit.saturating_sub(1)).to_owned();
    out.push('…');
    out
}

/// `s` with its middle replaced by an omission note, when longer than `head + tail + 40`.
pub fn abridge(s: &str, head_chars: usize, tail_chars: usize) -> String {
    let len = clen(s);
    if len <= head_chars + tail_chars + 40 {
        return s.to_owned();
    }
    format!(
        "{}\n[… {} chars omitted …]\n{}",
        head(s, head_chars),
        len - head_chars - tail_chars,
        tail(s, tail_chars)
    )
}

/// Lines of `s` split on `\n`, each without a trailing `\r`.
pub fn lines(s: &str) -> impl Iterator<Item = &str> {
    s.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn char_indexing_matches_on_ascii_and_unicode() {
        assert_eq!(clen("abc"), 3);
        assert_eq!(clen("aé✓"), 3);
        assert_eq!(cslice("aé✓z", 1, 3), "é✓");
        assert_eq!(head("aé✓z", 2), "aé");
        assert_eq!(tail("aé✓z", 2), "✓z");
        assert_eq!(rfind_char("a\nb\nc", '\n', 2), Some(1));
        assert_eq!(rfind_char("a\nb\nc", '\n', 3), Some(3));
        assert_eq!(find_char("é\nb\nc", '\n', 2), Some(3));
        assert_eq!(find_str("✓✓x", "x"), Some(2));
    }

    #[test]
    fn truncate_and_abridge() {
        assert_eq!(truncate("abcdef", 4), "abc…");
        assert_eq!(truncate("abc", 4), "abc");
        let long = "x".repeat(700);
        let a = abridge(&long, 400, 150);
        assert!(a.contains("[… 150 chars omitted …]"));
        assert_eq!(abridge("short", 400, 150), "short");
    }
}
