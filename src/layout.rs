//! Fitting lines to the terminal width, ported from the HUD's `render.js` and
//! `string-width.js` so wrapping lands where the HUD's did.
//!
//! The width rules are the HUD's, not the terminal's: CJK is two columns,
//! combining and zero-width characters none, everything else (emoji included)
//! one.

const RESET: &str = "\x1b[0m";
const ELLIPSIS: &str = "...";

fn is_cjk(c: u32) -> bool {
    matches!(
        c,
        0x4e00..=0x9fff
            | 0x3400..=0x4dbf
            | 0x20000..=0x2ebef
            | 0xf900..=0xfaff
            | 0xac00..=0xd7af
            | 0x1100..=0x11ff
            | 0x3130..=0x318f
            | 0xa960..=0xa97f
            | 0xd7b0..=0xd7ff
            | 0x3040..=0x309f
            | 0x30a0..=0x30ff
            | 0x31f0..=0x31ff
            | 0xff01..=0xff60
            | 0xffe0..=0xffe6
            | 0x3000..=0x303f
            | 0x3200..=0x32ff
            | 0x3300..=0x33ff
            | 0xfe30..=0xfe4f
    )
}

fn is_zero_width(c: u32) -> bool {
    matches!(
        c,
        0x200b | 0x200c | 0x200d | 0xfeff
            | 0x0300..=0x036f
            | 0x1ab0..=0x1aff
            | 0x1dc0..=0x1dff
            | 0x20d0..=0x20ff
            | 0xfe20..=0xfe2f
    )
}

pub fn char_width(c: char) -> usize {
    let c = c as u32;
    if is_zero_width(c) {
        0
    } else if is_cjk(c) {
        2
    } else {
        1
    }
}

/// Length of the `ESC [ <digits and ;> <letter>` sequence at the start of `s`.
fn ansi_len(s: &str) -> Option<usize> {
    let rest = s.strip_prefix("\x1b[")?;
    let params = rest
        .bytes()
        .take_while(|b| b.is_ascii_digit() || *b == b';')
        .count();
    rest.as_bytes()
        .get(params)
        .filter(|b| b.is_ascii_alphabetic())
        .map(|_| 2 + params + 1)
}

/// Columns the line occupies, escape sequences not counted.
pub fn width(s: &str) -> usize {
    let mut total = 0;
    let mut i = 0;
    while i < s.len() {
        if let Some(n) = ansi_len(&s[i..]) {
            i += n;
            continue;
        }
        let c = s[i..].chars().next().unwrap_or(' ');
        total += char_width(c);
        i += c.len_utf8();
    }
    total
}

/// `s` cut to `max` columns with a trailing `...` when it did not fit. For
/// plain text (no escape sequences).
pub fn truncate_to_width(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if width(s) <= max {
        return s.to_string();
    }
    let Some(target) = max.checked_sub(ELLIPSIS.len()).filter(|t| *t > 0) else {
        return ELLIPSIS.chars().take(max).collect();
    };
    let mut out = String::new();
    let mut used = 0;
    for c in s.chars() {
        let w = char_width(c);
        if used + w > target {
            break;
        }
        out.push(c);
        used += w;
    }
    out + ELLIPSIS
}

/// One line cut to `max` columns, escape sequences passed through. A reset
/// goes before the ellipsis when any escape was kept, so color cannot bleed.
pub fn truncate_line(line: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if width(line) <= max {
        return line.to_string();
    }
    let target = max.saturating_sub(ELLIPSIS.len());
    let mut out = String::new();
    let mut used = 0;
    let mut ansi = false;
    let mut i = 0;
    while i < line.len() {
        if let Some(n) = ansi_len(&line[i..]) {
            out.push_str(&line[i..i + n]);
            ansi = true;
            i += n;
            continue;
        }
        let c = line[i..].chars().next().unwrap_or(' ');
        let w = char_width(c);
        if used + w > target {
            break;
        }
        out.push(c);
        used += w;
        i += c.len_utf8();
    }
    if ansi {
        out.push_str(RESET);
    }
    out + ELLIPSIS
}

/// Break `line` at `separator` boundaries so each piece fits `max` columns; a
/// piece that cannot fit alone is truncated. No separator in the line: truncate.
pub fn wrap_line(line: &str, max: usize, separators: [&str; 2]) -> Vec<String> {
    if max == 0 {
        return vec![String::new()];
    }
    if width(line) <= max {
        return vec![line.to_string()];
    }
    let Some(sep) = separators.into_iter().find(|s| line.contains(s)) else {
        return vec![truncate_line(line, max)];
    };
    let mut segments = line.split(sep);
    let mut current = segments.next().unwrap_or_default().to_string();
    let mut wrapped = Vec::new();
    let fit = |s: String| {
        if width(&s) > max {
            truncate_line(&s, max)
        } else {
            s
        }
    };
    for next in segments {
        let candidate = format!("{current}{sep}{next}");
        if width(&candidate) <= max {
            current = candidate;
        } else {
            wrapped.push(fit(std::mem::replace(&mut current, next.to_string())));
        }
    }
    wrapped.push(fit(current));
    wrapped
}

/// At most `limit` lines of `head` then `tail`: the first `limit - 1`, then a
/// note of how many were cut (the note itself takes the last slot). The tail
/// gives way first: when the head fills every slot exactly, the tail is left
/// out without a note, so the note never takes the place of a head line just
/// to count tail lines.
pub fn limit_lines(head: Vec<String>, tail: Vec<String>, limit: usize) -> Vec<String> {
    let limit = limit.max(1);
    if head.len() == limit {
        return head;
    }
    let mut lines = head;
    lines.extend(tail);
    if lines.len() <= limit {
        return lines;
    }
    let cut = lines.len() - limit + 1;
    lines.truncate(limit - 1);
    lines.push(format!("... (+{cut} lines)"));
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIM_SEP: &str = "\x1b[2m | \x1b[0m";
    const SEPS: [&str; 2] = [DIM_SEP, " | "];

    #[test]
    fn width_counts_like_the_hud() {
        assert_eq!(width("abc"), 3);
        assert_eq!(width("\x1b[31mabc\x1b[0m"), 3);
        assert_eq!(width("\u{4e2d}\u{6587}"), 4, "CJK is double width");
        assert_eq!(width("e\u{301}"), 1, "combining mark is free");
        assert_eq!(width("\u{200b}x"), 1);
        assert_eq!(
            width("\u{1f527}12 \u{1f916}3"),
            6,
            "the HUD counts emoji as one column"
        );
        assert_eq!(width(""), 0);
    }

    #[test]
    fn ansi_len_wants_a_complete_sequence() {
        assert_eq!(ansi_len("\x1b[0mx"), Some(4));
        assert_eq!(ansi_len("\x1b[38;5;1mx"), Some(9));
        assert_eq!(ansi_len("\x1b[31"), None);
        assert_eq!(ansi_len("\x1b[;"), None);
        assert_eq!(ansi_len("x"), None);
    }

    #[test]
    fn truncate_to_width_matches_the_hud() {
        assert_eq!(
            truncate_to_width("Detailed handoff of x", 15),
            "Detailed han..."
        );
        assert_eq!(truncate_to_width("short", 15), "short");
        assert_eq!(truncate_to_width("exactly15chars!", 15), "exactly15chars!");
        assert_eq!(truncate_to_width("abcdef", 0), "");
        assert_eq!(truncate_to_width("abcdef", 3), "...");
        assert_eq!(truncate_to_width("abcdef", 2), "..");
        // A double-width char that would straddle the cut is dropped whole.
        assert_eq!(truncate_to_width("ab\u{4e2d}\u{6587}cd", 6), "ab...");
        assert_eq!(
            truncate_to_width("ab\u{4e2d}\u{6587}cd", 7),
            "ab\u{4e2d}..."
        );
    }

    #[test]
    fn truncate_line_keeps_escapes_and_resets_before_the_ellipsis() {
        let line = "\x1b[31mabcdefghij\x1b[0m";
        assert_eq!(truncate_line(line, 6), "\x1b[31mabc\x1b[0m...");
        assert_eq!(truncate_line("abcdefghij", 6), "abc...");
        assert_eq!(truncate_line(line, 20), line);
        assert_eq!(truncate_line(line, 0), "");
        assert_eq!(truncate_line("abcdefghij", 2), "...");
    }

    #[test]
    fn wrap_breaks_at_separators_and_drops_them() {
        let line = format!("aaaa{DIM_SEP}bbbb{DIM_SEP}cccc{DIM_SEP}dd");
        assert_eq!(wrap_line(&line, 80, SEPS), std::slice::from_ref(&line));
        // "aaaa | bbbb" is 11 columns.
        assert_eq!(
            wrap_line(&line, 11, SEPS),
            [format!("aaaa{DIM_SEP}bbbb"), format!("cccc{DIM_SEP}dd")]
        );
        assert_eq!(
            wrap_line(&line, 5, SEPS),
            ["aaaa", "bbbb", "cccc", "dd"].map(String::from)
        );
    }

    #[test]
    fn wrap_uses_the_plain_separator_when_there_is_no_dim_one() {
        assert_eq!(
            wrap_line("aaaa | bbbb | cc", 9, SEPS),
            ["aaaa", "bbbb | cc"].map(String::from)
        );
    }

    #[test]
    fn wrap_truncates_a_piece_that_cannot_fit() {
        assert_eq!(
            wrap_line("a | bbbbbbbbbbbb", 8, SEPS),
            ["a", "bbbbb..."].map(String::from)
        );
        assert_eq!(wrap_line("nosep nosep nosep", 8, SEPS), ["nosep..."]);
        assert_eq!(wrap_line("abc", 0, SEPS), [""]);
    }

    #[test]
    fn limit_lines_keeps_the_head_and_counts_the_cut() {
        let v = |n: usize| (1..=n).map(|i| i.to_string()).collect::<Vec<_>>();
        let none = Vec::<String>::new;
        assert_eq!(limit_lines(v(4), none(), 4), v(4));
        assert_eq!(
            limit_lines(v(5), none(), 4),
            ["1", "2", "3", "... (+2 lines)"]
        );
        assert_eq!(
            limit_lines(v(7), none(), 4),
            ["1", "2", "3", "... (+4 lines)"]
        );
        assert_eq!(limit_lines(v(3), none(), 0), ["... (+3 lines)"]);
        assert_eq!(limit_lines(none(), none(), 4), none());
        // The tail fills free slots, and the note counts it when it overflows.
        let tail = || vec!["a".to_string(), "b".to_string()];
        assert_eq!(limit_lines(v(2), tail(), 4), ["1", "2", "a", "b"]);
        assert_eq!(
            limit_lines(v(3), tail(), 4),
            ["1", "2", "3", "... (+2 lines)"]
        );
        assert_eq!(
            limit_lines(v(5), tail(), 4),
            ["1", "2", "3", "... (+4 lines)"]
        );
    }

    #[test]
    fn limit_lines_drops_the_tail_before_cutting_a_full_head() {
        // A context notice under a main line that wraps to fill the cap: the
        // note would replace the main line's last row, hiding `ctx:`.
        let v = |n: usize| (1..=n).map(|i| i.to_string()).collect::<Vec<_>>();
        assert_eq!(limit_lines(v(4), vec!["notice".into()], 4), v(4));
        assert_eq!(limit_lines(v(1), vec!["notice".into()], 1), v(1));
    }
}
