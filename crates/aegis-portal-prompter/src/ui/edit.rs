//! App-owned single-line editing surfaces. lens's textfield keeps its
//! caret in widget state with no host API to move it, so a programmatic
//! buffer change (pre-filled path, Tab completion) would strand the caret
//! at a stale offset; owning the string and caret sidesteps that. The
//! secret prompt established the pattern: the dialog renders the text and
//! caret itself and consumes text and editing keys from the per-frame
//! input snapshot.

use lens::{Color, LayoutOpts};

/// The 1.5 px caret bar drawn between the before/after text runs.
pub fn caret_bar(color: Color) -> LayoutOpts {
    LayoutOpts {
        width: 1.5,
        height: 18.0,
        bg: color,
        ..Default::default()
    }
}

/// Insert text at the caret, dropping control characters (single line).
pub fn insert(text: &mut String, caret: &mut usize, input: &str) {
    let clean: String = input.chars().filter(|c| !c.is_control()).collect();
    if clean.is_empty() {
        return;
    }
    text.insert_str(*caret, &clean);
    *caret += clean.len();
}

pub fn delete_backward(text: &mut String, caret: &mut usize) {
    let start = prev_boundary(text, *caret);
    if start < *caret {
        text.replace_range(start..*caret, "");
        *caret = start;
    }
}

pub fn delete_forward(text: &mut String, caret: &mut usize) {
    let end = next_boundary(text, *caret);
    if end > *caret {
        text.replace_range(*caret..end, "");
    }
}

pub fn prev_boundary(text: &str, index: usize) -> usize {
    text[..index]
        .char_indices()
        .next_back()
        .map_or(0, |(i, _)| i)
}

pub fn next_boundary(text: &str, index: usize) -> usize {
    text[index..]
        .chars()
        .next()
        .map_or(text.len(), |c| index + c.len_utf8())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_moves_the_caret_with_the_text() {
        let mut text = String::from("ab");
        let mut caret = 1;
        insert(&mut text, &mut caret, "xy\n");
        assert_eq!(text, "axyb");
        assert_eq!(caret, 3);
    }

    #[test]
    fn editing_is_char_boundary_safe() {
        let mut text = String::from("aé中");
        let mut caret = text.len();
        delete_backward(&mut text, &mut caret);
        assert_eq!(text, "aé");
        delete_backward(&mut text, &mut caret);
        assert_eq!(text, "a");
        delete_backward(&mut text, &mut caret);
        assert_eq!(text, "");
        delete_backward(&mut text, &mut caret);
        assert_eq!(text, "");

        insert(&mut text, &mut caret, "é中");
        let mut caret = 0;
        delete_forward(&mut text, &mut caret);
        assert_eq!(text, "中");
        delete_forward(&mut text, &mut caret);
        assert_eq!(text, "");
    }

    #[test]
    fn caret_movement_clamps_to_the_ends() {
        let text = String::from("aé");
        assert_eq!(prev_boundary(&text, 0), 0);
        assert_eq!(next_boundary(&text, text.len()), text.len());
        assert_eq!(next_boundary(&text, 1), 3);
        assert_eq!(prev_boundary(&text, 3), 1);
    }
}
