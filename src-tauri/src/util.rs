//! Small text helpers shared by several modules.

/// Trims `text` and cuts it to `max` characters, appending `…` when shortened.
/// Meant for error messages and log previews.
pub fn snippet(text: &str, max: usize) -> String {
    let trimmed = text.trim();
    match trimmed.char_indices().nth(max) {
        None => trimmed.to_string(),
        Some((cut, _)) => format!("{}…", &trimmed[..cut]),
    }
}

/// Cuts `text` to `max` characters and marks the cut on its own line.
/// Meant for log details and prompts where the truncation must be visible.
pub fn truncate_chars(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        None => text.to_string(),
        Some((cut, _)) => format!("{}\n… (kısaltıldı)", &text[..cut]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snippet_respects_char_boundaries() {
        assert_eq!(snippet("  merhaba  ", 20), "merhaba");
        assert_eq!(snippet("çğüşiö", 3), "çğü…");
    }

    #[test]
    fn truncate_marks_the_cut() {
        assert_eq!(truncate_chars("abc", 3), "abc");
        assert_eq!(truncate_chars("abcd", 3), "abc\n… (kısaltıldı)");
    }
}
