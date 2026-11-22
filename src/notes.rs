//! Plain-text notes: editing functions on (text, cursor byte offset).

use std::path::PathBuf;

pub fn path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    let directory = std::env::var("XDG_DATA_HOME").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from(home).join(".local/share"));
    directory.join("oversway").join("notes.txt")
}

pub fn load() -> String {
    std::fs::read_to_string(path()).unwrap_or_default()
}

pub fn save(text: &str) {
    let path = path();
    let _ = std::fs::create_dir_all(path.parent().unwrap());
    let temporary = path.with_extension("txt.tmp");
    if std::fs::write(&temporary, text).is_ok() {
        let _ = std::fs::rename(temporary, path);
    }
}

pub fn previous_boundary(text: &str, cursor: usize) -> usize {
    text[..cursor].char_indices().next_back().map_or(0, |(index, _)| index)
}

pub fn next_boundary(text: &str, cursor: usize) -> usize {
    text[cursor..].chars().next().map_or(cursor, |character| cursor + character.len_utf8())
}

pub fn line_start(text: &str, cursor: usize) -> usize {
    text[..cursor].rfind('\n').map_or(0, |index| index + 1)
}

pub fn line_end(text: &str, cursor: usize) -> usize {
    text[cursor..].find('\n').map_or(text.len(), |index| cursor + index)
}

/// Move to the same column (in characters) on the previous or next logical line.
pub fn vertical(text: &str, cursor: usize, down: bool) -> usize {
    let start = line_start(text, cursor);
    let column = text[start..cursor].chars().count();
    let target_start = if down {
        let end = line_end(text, cursor);
        if end == text.len() {
            return text.len();
        }
        end + 1
    } else {
        if start == 0 {
            return 0;
        }
        line_start(text, start - 1)
    };
    let target_end = line_end(text, target_start);
    text[target_start..target_end].char_indices().nth(column).map_or(target_end, |(index, _)| target_start + index)
}

/// Delete the word before the cursor (Ctrl+Backspace / Ctrl+W).
pub fn previous_word(text: &str, cursor: usize) -> usize {
    let before = &text[..cursor];
    let trimmed = before.trim_end_matches(|character: char| character.is_whitespace());
    trimmed.rfind(|character: char| character.is_whitespace()).map_or(0, |index| index + 1)
}
