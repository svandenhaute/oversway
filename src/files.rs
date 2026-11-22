//! hjkl file browser: one directory listing, read synchronously (a normal directory
//! takes well under a millisecond). Directories first, then files, case-insensitive.

use std::path::{Path, PathBuf};

pub struct Entry {
    pub name: String,
    pub directory: bool,
}

pub struct Browser {
    pub directory: PathBuf,
    pub entries: Vec<Entry>,
    pub selected: usize,
    pub show_hidden: bool,
}

pub fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/".into()))
}

pub fn new() -> Browser {
    let mut browser = Browser { directory: home(), entries: Vec::new(), selected: 0, show_hidden: false };
    read(&mut browser);
    browser
}

/// Re-lists the directory, keeping the selection on the same name when it still exists.
pub fn read(browser: &mut Browser) {
    let previous = browser.entries.get(browser.selected).map(|entry| entry.name.clone());
    let mut entries: Vec<Entry> = std::fs::read_dir(&browser.directory)
        .map(|reader| {
            reader
                .flatten()
                .map(|item| {
                    let name = item.file_name().to_string_lossy().into_owned();
                    // Symlinks to directories count as directories.
                    let directory = item.file_type().is_ok_and(|kind| kind.is_dir()) || (item.file_type().is_ok_and(|kind| kind.is_symlink()) && item.path().is_dir());
                    Entry { name, directory }
                })
                .filter(|entry| browser.show_hidden || !entry.name.starts_with('.'))
                .collect()
        })
        .unwrap_or_default();
    entries.sort_by_cached_key(|entry| (!entry.directory, entry.name.to_lowercase()));
    browser.selected = previous.and_then(|name| entries.iter().position(|entry| entry.name == name)).unwrap_or(0);
    browser.entries = entries;
}

pub fn change_directory(browser: &mut Browser, directory: PathBuf, select: Option<&str>) {
    browser.directory = directory;
    browser.entries.clear();
    read(browser);
    browser.selected = select.and_then(|name| browser.entries.iter().position(|entry| entry.name == name)).unwrap_or(0);
}

/// Enters the selected directory, or returns the selected file's path for opening.
pub fn enter(browser: &mut Browser) -> Option<PathBuf> {
    let entry = browser.entries.get(browser.selected)?;
    let path = browser.directory.join(&entry.name);
    if entry.directory {
        change_directory(browser, path, None);
        None
    } else {
        Some(path)
    }
}

/// Goes up one level with the cursor on the directory we came from.
pub fn parent(browser: &mut Browser) {
    let Some(parent) = browser.directory.parent().map(Path::to_path_buf) else { return };
    let child = browser.directory.file_name().map(|name| name.to_string_lossy().into_owned());
    change_directory(browser, parent, child.as_deref());
}

pub fn move_selection(browser: &mut Browser, delta: isize) {
    let last = browser.entries.len().saturating_sub(1) as isize;
    browser.selected = (browser.selected as isize + delta).clamp(0, last) as usize;
}

/// Path with the home directory shown as ~.
pub fn display_path(path: &Path) -> String {
    let home = home();
    match path.strip_prefix(&home) {
        Ok(rest) if rest.as_os_str().is_empty() => "~".into(),
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => path.display().to_string(),
    }
}

/// Single-quoted for sh (sway's exec runs through sh -c).
pub fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}
