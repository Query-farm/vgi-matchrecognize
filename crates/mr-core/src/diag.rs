//! Pointing at the offending character of a user-supplied string.
//!
//! `pattern`, `define` and `measures` reach us as opaque strings nested inside a
//! SQL string literal, so "expected `)`" on its own leaves the user counting
//! parentheses by hand. Every parse error therefore carries the character
//! position it was raised at, and renders as the source plus a caret:
//!
//! ```text
//! match_recognize pattern error: expected ')', found end of pattern
//!     A (B | C D
//!               ^
//! ```
//!
//! Positions are **character** indices, not byte offsets — both lexers already
//! walk a `Vec<char>`, and a caret is aligned by counting characters.

/// Longest source window rendered around the caret. Predicates are usually far
/// shorter than this; the window exists so that a generated 4 KB expression does
/// not turn one error into a wall of text.
const WINDOW: usize = 72;

/// Indent shared by the source line and the caret line.
const INDENT: &str = "    ";

/// Render `src` with a caret under character `at`, as the two lines that follow
/// an error message.
///
/// `at` may be one past the end (the "found end of input" case), which puts the
/// caret just after the last character. Returns an empty string for empty
/// input, so a caller can append it unconditionally.
pub fn point_at(src: &str, at: usize) -> String {
    let chars: Vec<char> = src.chars().collect();
    if chars.is_empty() {
        return String::new();
    }
    let at = at.min(chars.len());

    // Window the source when it is too long to show whole, keeping the caret
    // inside it. The ellipses are counted in the caret offset, since they
    // occupy columns on the rendered line.
    let (start, end) = if chars.len() <= WINDOW {
        (0, chars.len())
    } else {
        let half = WINDOW / 2;
        let start = at.saturating_sub(half);
        let end = (start + WINDOW).min(chars.len());
        // Re-anchor when the caret sits near the end and the window ran short.
        (end.saturating_sub(WINDOW), end)
    };

    let mut line = String::new();
    if start > 0 {
        line.push('…');
    }
    line.extend(&chars[start..end]);
    if end < chars.len() {
        line.push('…');
    }

    // A control character (a newline inside a JSON-supplied predicate, say)
    // would desynchronise the caret from the text above it, so render it as a
    // single visible placeholder.
    let line: String = line
        .chars()
        .map(|c| if c.is_control() { '·' } else { c })
        .collect();

    let caret_col = (at - start) + usize::from(start > 0);
    format!(
        "\n{INDENT}{line}\n{INDENT}{:>width$}^",
        "",
        width = caret_col
    )
}

#[cfg(test)]
mod tests {
    use super::point_at;

    /// The caret lands under the character at `at`, on the line below it.
    #[test]
    fn caret_is_under_the_reported_character() {
        let rendered = point_at("A (B | C D", 3);
        let lines: Vec<&str> = rendered.lines().collect();
        // lines[0] is empty: the rendering starts with the newline that
        // separates it from the message.
        assert_eq!(lines[1], "    A (B | C D");
        assert_eq!(lines[2], "       ^");
        // The caret column indexes the same character in the source line.
        let col = lines[2].find('^').unwrap();
        assert_eq!(lines[1].as_bytes()[col], b'B');
    }

    /// One past the end is the "found end of input" case, and is not an error.
    #[test]
    fn caret_may_sit_one_past_the_end() {
        let rendered = point_at("A (B", 4);
        let lines: Vec<&str> = rendered.lines().collect();
        assert_eq!(lines[1], "    A (B");
        assert_eq!(lines[2], "        ^");
    }

    /// Nothing to point at, so nothing is rendered — the caller appends it
    /// unconditionally.
    #[test]
    fn empty_source_renders_nothing() {
        assert_eq!(point_at("", 0), "");
    }

    /// A long expression is windowed, and the caret stays aligned with the
    /// character it names despite the leading ellipsis.
    #[test]
    fn a_long_source_is_windowed_around_the_caret() {
        let src = format!("{}@{}", "a + ".repeat(40), "b + ".repeat(40));
        let at = src.chars().position(|c| c == '@').unwrap();
        let rendered = point_at(&src, at);
        let lines: Vec<&str> = rendered.lines().collect();
        let col = lines[2].find('^').unwrap();
        assert_eq!(lines[1].chars().nth(col), Some('@'));
        assert!(lines[1].starts_with("    …"), "expected a leading ellipsis");
    }

    /// A caret near the end of a long source still points at the right
    /// character, with the window anchored to the tail.
    #[test]
    fn a_caret_near_the_end_stays_aligned() {
        let src = format!("{}@", "a + ".repeat(60));
        let at = src.chars().count() - 1;
        let rendered = point_at(&src, at);
        let lines: Vec<&str> = rendered.lines().collect();
        let col = lines[2].find('^').unwrap();
        assert_eq!(lines[1].chars().nth(col), Some('@'));
    }

    /// An embedded newline would otherwise push the caret onto its own line.
    #[test]
    fn control_characters_do_not_desynchronise_the_caret() {
        let rendered = point_at("a\nb + @", 6);
        let lines: Vec<&str> = rendered.lines().collect();
        assert_eq!(lines.len(), 3, "the source must render as one line");
        let col = lines[2].find('^').unwrap();
        assert_eq!(lines[1].chars().nth(col), Some('@'));
    }
}
