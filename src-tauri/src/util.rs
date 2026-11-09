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

#[derive(Copy, Clone, PartialEq, Eq)]
enum ParserState {
    None,
    InError,
    InWarning,
}

fn strip_ansi(s: &str) -> String {
    if !s.contains('\x1b') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for next_c in chars.by_ref() {
                    if next_c.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn is_error_header(line: &str) -> bool {
    let trimmed = line.trim_start();
    if trimmed.starts_with("error: aborting") || trimmed.starts_with("error: could not compile") {
        return false;
    }
    trimmed.starts_with("error[") || trimmed.starts_with("error:")
}

fn is_warning_header(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.starts_with("warning:") || trimmed.starts_with("warning[")
}

fn is_noise_header(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.starts_with("error: aborting")
        || trimmed.starts_with("error: could not compile")
        || trimmed.starts_with("For more information about")
        || trimmed.starts_with("Some errors have detailed explanations")
        || trimmed.starts_with("Checking ")
        || trimmed.starts_with("Compiling ")
        || trimmed.starts_with("Finished ")
        || trimmed.starts_with("Downloaded ")
        || trimmed.starts_with("Fetching ")
        || trimmed.starts_with("Fresh ")
}

/// Filters `cargo check` output by removing warnings and compiler chatter,
/// returning only `error[...]` blocks with their locations and source lines.
pub fn condense_cargo_errors(output: &str) -> String {
    let cleaned_input = strip_ansi(output);
    let mut error_blocks: Vec<String> = Vec::new();
    let mut current_block: Vec<&str> = Vec::new();
    let mut state = ParserState::None;

    for line in cleaned_input.lines() {
        if is_error_header(line) {
            if state == ParserState::InError && !current_block.is_empty() {
                let block = current_block.join("\n").trim().to_string();
                if !block.is_empty() {
                    error_blocks.push(block);
                }
                current_block.clear();
            }
            state = ParserState::InError;
            current_block.push(line);
        } else if is_warning_header(line) {
            if state == ParserState::InError && !current_block.is_empty() {
                let block = current_block.join("\n").trim().to_string();
                if !block.is_empty() {
                    error_blocks.push(block);
                }
                current_block.clear();
            }
            state = ParserState::InWarning;
        } else if is_noise_header(line) {
            if state == ParserState::InError && !current_block.is_empty() {
                let block = current_block.join("\n").trim().to_string();
                if !block.is_empty() {
                    error_blocks.push(block);
                }
                current_block.clear();
            }
            state = ParserState::None;
        } else {
            match state {
                ParserState::InError => {
                    current_block.push(line);
                }
                ParserState::InWarning | ParserState::None => {}
            }
        }
    }

    if state == ParserState::InError && !current_block.is_empty() {
        let block = current_block.join("\n").trim().to_string();
        if !block.is_empty() {
            error_blocks.push(block);
        }
    }

    error_blocks.join("\n\n")
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

    #[test]
    fn condense_cargo_errors_strips_warnings_and_keeps_errors() {
        let raw = r#"
   Compiling demo v0.1.0 (/path/to/demo)
warning: unused variable: `x`
 --> src/main.rs:2:9
  |
2 |     let x = 5;
  |         ^ help: if this is intentional, prefix it with an underscore: `_x`
  |
  = note: `#[warn(unused_variables)]` on by default

error[E0425]: cannot find value `foo` in this scope
  --> src/main.rs:15:5
   |
15 |     foo();
   |     ^^^ not found in this scope

warning: `demo` (bin "demo") generated 1 warning
error: could not compile `demo` (bin "demo") due to 1 previous error; 1 warning emitted
"#;
        let condensed = condense_cargo_errors(raw);
        assert!(!condensed.contains("warning: unused variable"));
        assert!(!condensed.contains("`#[warn(unused_variables)]`"));
        assert!(!condensed.contains("generated 1 warning"));
        assert!(!condensed.contains("could not compile"));
        assert!(condensed.contains("error[E0425]: cannot find value `foo` in this scope"));
        assert!(condensed.contains("--> src/main.rs:15:5"));
        assert!(condensed.contains("15 |     foo();"));
    }

    #[test]
    fn condense_cargo_errors_empty_on_warnings_only() {
        let raw = r#"
warning: unused import: `std::fs`
 --> src/lib.rs:1:5
  |
1 | use std::fs;
  |     ^^^^^^^

warning: `demo` (lib) generated 1 warning
"#;
        let condensed = condense_cargo_errors(raw);
        assert!(condensed.is_empty());
    }

    #[test]
    fn condense_cargo_errors_multiple_errors() {
        let raw = r#"
error[E0425]: cannot find value `bar` in this scope
 --> src/main.rs:10:5
  |
10 |     bar();
  |     ^^^ not found in this scope

warning: unused variable: `y`
 --> src/main.rs:3:9

error[E0308]: mismatched types
 --> src/main.rs:20:18
  |
20 |     let z: u32 = "str";
  |            ---   ^^^^^ expected `u32`, found `&str`
"#;
        let condensed = condense_cargo_errors(raw);
        assert!(!condensed.contains("unused variable"));
        assert!(condensed.contains("error[E0425]"));
        assert!(condensed.contains("src/main.rs:10:5"));
        assert!(condensed.contains("error[E0308]"));
        assert!(condensed.contains("src/main.rs:20:18"));
    }

    #[test]
    fn condense_cargo_errors_handles_ansi_sequences() {
        let raw = "\x1b[31merror[E0425]\x1b[0m: cannot find value `a`\n --> src/main.rs:5:1";
        let condensed = condense_cargo_errors(raw);
        assert!(condensed.contains("error[E0425]: cannot find value `a`"));
        assert!(condensed.contains("src/main.rs:5:1"));
    }
}
