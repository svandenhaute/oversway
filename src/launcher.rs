//! Desktop entries, fuzzy matching and icon rasterization.

use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};
use resvg::{tiny_skia, usvg};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub const ICON_SIZE: u32 = 64;
pub const ATLAS_COLUMNS: u32 = 32;

#[derive(Clone, Debug)]
pub struct Entry {
    pub name: String,
    pub generic_name: String,
    pub keywords: String,
    pub exec: String,
    pub terminal: bool,
    pub icon: String,
    pub path: PathBuf,
}

pub fn application_directories() -> Vec<PathBuf> {
    let home = std::env::var("HOME").unwrap_or_default();
    let data_home = std::env::var("XDG_DATA_HOME").unwrap_or_else(|_| format!("{home}/.local/share"));
    let data_dirs = std::env::var("XDG_DATA_DIRS").unwrap_or_else(|_| "/usr/local/share:/usr/share".into());
    let mut directories: Vec<PathBuf> = std::iter::once(data_home.as_str())
        .chain(data_dirs.split(':'))
        .chain(["/var/lib/flatpak/exports/share"])
        .map(|directory| Path::new(directory).join("applications"))
        .filter(|directory| directory.is_dir())
        .collect();
    directories.dedup();
    directories
}

fn parse_desktop_file(path: &Path) -> Option<Entry> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut in_main_section = false;
    let mut fields: HashMap<&str, &str> = HashMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_main_section = line == "[Desktop Entry]";
            continue;
        }
        if !in_main_section {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            fields.entry(key.trim()).or_insert(value.trim());
        }
    }
    let flag = |key: &str| fields.get(key).is_some_and(|value| *value == "true");
    if fields.get("Type") != Some(&"Application") || flag("NoDisplay") || flag("Hidden") {
        return None;
    }
    let exec = fields.get("Exec")?;
    let name = fields.get("Name")?;
    // Expand %k (this file's path, e.g. Zotero locates its install dir from it) and %c (name),
    // drop the other field codes (%f, %U, ...), and unescape %%.
    let mut command = String::new();
    let mut characters = exec.chars().peekable();
    while let Some(character) = characters.next() {
        if character == '%' {
            match characters.next() {
                Some('%') => command.push('%'),
                Some('k') => command.push_str(&crate::files::shell_quote(&path.to_string_lossy())),
                Some('c') => command.push_str(&crate::files::shell_quote(name)),
                _ => {}
            }
        } else {
            command.push(character);
        }
    }
    Some(Entry {
        name: name.to_string(),
        generic_name: fields.get("GenericName").unwrap_or(&"").to_string(),
        keywords: fields.get("Keywords").unwrap_or(&"").replace(';', " "),
        exec: command.trim().to_string(),
        terminal: flag("Terminal"),
        icon: fields.get("Icon").unwrap_or(&"").to_string(),
        path: path.to_path_buf(),
    })
}

/// Earlier directories win on duplicate desktop file ids (XDG precedence).
pub fn scan() -> Vec<Entry> {
    let mut by_id: HashMap<String, Entry> = HashMap::new();
    for directory in application_directories() {
        let mut stack = vec![directory.clone()];
        while let Some(current) = stack.pop() {
            let Ok(reader) = std::fs::read_dir(&current) else { continue };
            for item in reader.flatten() {
                let path = item.path();
                if path.is_dir() {
                    stack.push(path);
                } else if path.extension().is_some_and(|extension| extension == "desktop") {
                    let id = path.strip_prefix(&directory).unwrap().to_string_lossy().replace('/', "-");
                    if by_id.contains_key(&id) {
                        continue;
                    }
                    if let Some(entry) = parse_desktop_file(&path) {
                        by_id.insert(id, entry);
                    }
                }
            }
        }
    }
    let mut entries: Vec<Entry> = by_id.into_values().collect();
    entries.sort_by_key(|entry| entry.name.to_lowercase());
    entries
}

/// Indices into `entries`, best first. An empty query orders by launch count.
pub fn filter(entries: &[Entry], query: &str, launch_counts: &HashMap<String, u32>, matcher: &mut Matcher) -> Vec<usize> {
    let count = |entry: &Entry| launch_counts.get(&entry.name).copied().unwrap_or(0);
    if query.trim().is_empty() {
        let mut indices: Vec<usize> = (0..entries.len()).collect();
        indices.sort_by_key(|&index| std::cmp::Reverse(count(&entries[index])));
        return indices;
    }
    let pattern = Pattern::parse(query, CaseMatching::Ignore, Normalization::Smart);
    let mut buffer = Vec::new();
    let mut scored: Vec<(u32, usize)> = entries
        .iter()
        .enumerate()
        .filter_map(|(index, entry)| {
            let name_score = pattern.score(Utf32Str::new(&entry.name, &mut buffer), matcher);
            let extra = format!("{} {}", entry.generic_name, entry.keywords);
            let extra_score = pattern.score(Utf32Str::new(&extra, &mut buffer), matcher).map(|score| score / 2);
            let score = name_score.max(extra_score)?;
            Some((score + count(entry).min(20) * 4, index))
        })
        .collect();
    scored.sort_by_key(|&(score, index)| (std::cmp::Reverse(score), index));
    scored.into_iter().map(|(_, index)| index).collect()
}

pub fn new_matcher() -> Matcher {
    Matcher::new(Config::DEFAULT)
}

pub fn launch(entry: &Entry) {
    let command = if entry.terminal {
        let terminal = std::env::var("TERMINAL").unwrap_or_else(|_| "alacritty".into());
        format!("{terminal} -e {}", entry.exec)
    } else {
        entry.exec.clone()
    };
    // sway spawns it: the process is not our child and lands on the current workspace.
    crate::sway::run_command(&format!("exec {command}"));
}

pub fn state_path(name: &str) -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    let directory = std::env::var("XDG_STATE_HOME").map(PathBuf::from).unwrap_or_else(|_| Path::new(&home).join(".local/state"));
    directory.join("oversway").join(name)
}

pub fn load_launch_counts() -> HashMap<String, u32> {
    std::fs::read_to_string(state_path("launches"))
        .unwrap_or_default()
        .lines()
        .filter_map(|line| {
            let (count, name) = line.split_once('\t')?;
            Some((name.to_string(), count.parse().ok()?))
        })
        .collect()
}

pub fn save_launch_counts(counts: &HashMap<String, u32>) {
    let path = state_path("launches");
    let _ = std::fs::create_dir_all(path.parent().unwrap());
    let text: String = counts.iter().map(|(name, count)| format!("{count}\t{name}\n")).collect();
    let _ = std::fs::write(path, text);
}

// ---------------------------------------------------------------- icons

pub const EYE_ICON_SIZE: u32 = 64;
const EYE_SVG_HEAD: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="white" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">"#;
/// Feather icons (MIT): eye and eye-off.
const EYE_OPEN: &str = r#"<path d="M1 12s4-8 11-8 11 8 11 8-4 8-11 8-11-8-11-8z"/><circle cx="12" cy="12" r="3"/></svg>"#;
const EYE_CLOSED: &str = r#"<path d="M17.94 17.94A10.07 10.07 0 0 1 12 20c-7 0-11-8-11-8a18.45 18.45 0 0 1 5.06-5.94M9.9 4.24A9.12 9.12 0 0 1 12 4c7 0 11 8 11 8a18.5 18.5 0 0 1-2.16 3.19m-6.72-1.07a3 3 0 1 1-4.24-4.24"/><line x1="1" y1="1" x2="23" y2="23"/></svg>"#;

/// Open eye then crossed-out eye, side by side: premultiplied RGBA, 2 * EYE_ICON_SIZE wide.
pub fn eye_icons() -> Vec<u8> {
    let size = EYE_ICON_SIZE as usize;
    let mut pixels = vec![0u8; 2 * size * size * 4];
    for (index, body) in [EYE_OPEN, EYE_CLOSED].iter().enumerate() {
        let svg = format!("{EYE_SVG_HEAD}{body}");
        let Ok(tree) = usvg::Tree::from_str(&svg, &usvg::Options::default()) else { continue };
        let Some(mut pixmap) = tiny_skia::Pixmap::new(EYE_ICON_SIZE, EYE_ICON_SIZE) else { continue };
        let scale = EYE_ICON_SIZE as f32 / 24.0;
        resvg::render(&tree, tiny_skia::Transform::from_scale(scale, scale), &mut pixmap.as_mut());
        for (row, line) in pixmap.data().chunks_exact(size * 4).enumerate() {
            let start = (row * 2 * size + index * size) * 4;
            pixels[start..start + size * 4].copy_from_slice(line);
        }
    }
    pixels
}

fn icon_theme_name() -> Option<String> {
    let home = std::env::var("HOME").ok()?;
    for file in ["/.config/gtk-4.0/settings.ini", "/.config/gtk-3.0/settings.ini"] {
        if let Ok(text) = std::fs::read_to_string(format!("{home}{file}")) {
            if let Some(line) = text.lines().find(|line| line.trim_start().starts_with("gtk-icon-theme-name")) {
                return line.split_once('=').map(|(_, value)| value.trim().trim_matches('"').to_string());
            }
        }
    }
    None
}

/// Map icon name -> best file. Themes earlier in the list win; within a theme prefer
/// scalable SVG, then the largest bitmap.
fn icon_index() -> HashMap<String, PathBuf> {
    let home = std::env::var("HOME").unwrap_or_default();
    let roots = [format!("{home}/.local/share/icons"), format!("{home}/.icons"), "/usr/share/icons".into(), "/var/lib/flatpak/exports/share/icons".into()];
    let mut themes: Vec<String> = icon_theme_name().into_iter().collect();
    themes.extend(["hicolor".into(), "Adwaita".into()]);
    let mut best: HashMap<String, (usize, u32, PathBuf)> = HashMap::new();
    let mut consider = |rank: usize, path: PathBuf| {
        let Some(stem) = path.file_stem().map(|stem| stem.to_string_lossy().to_string()) else { return };
        let extension = path.extension().map(|extension| extension.to_string_lossy().to_string()).unwrap_or_default();
        let quality = match extension.as_str() {
            "svg" => 10_000,
            "png" => path
                .components()
                .filter_map(|component| component.as_os_str().to_str()?.split('x').next()?.parse::<u32>().ok())
                .next()
                .unwrap_or(48)
                .min(ICON_SIZE * 2),
            _ => return,
        };
        let candidate = (rank, quality, path);
        match best.get(&stem) {
            Some((existing_rank, existing_quality, _)) if (*existing_rank, std::cmp::Reverse(*existing_quality)) <= (rank, std::cmp::Reverse(quality)) => {}
            _ => {
                best.insert(stem, candidate);
            }
        }
    };
    for (rank, theme) in themes.iter().enumerate() {
        for root in &roots {
            let mut stack = vec![Path::new(root).join(theme)];
            while let Some(current) = stack.pop() {
                let Ok(reader) = std::fs::read_dir(&current) else { continue };
                for item in reader.flatten() {
                    let path = item.path();
                    if item.file_type().is_ok_and(|kind| kind.is_dir()) {
                        let name = path.file_name().unwrap().to_string_lossy();
                        // Only application-like contexts, and skip symbolic variants.
                        if !name.contains("symbolic") && !["actions", "status", "emblems", "emotes", "cursors"].contains(&name.as_ref()) {
                            stack.push(path);
                        }
                    } else {
                        consider(rank, path);
                    }
                }
            }
        }
    }
    if let Ok(reader) = std::fs::read_dir("/usr/share/pixmaps") {
        for item in reader.flatten() {
            consider(themes.len(), item.path());
        }
    }
    best.into_iter().map(|(name, (_, _, path))| (name, path)).collect()
}

fn rasterize(path: &Path) -> Option<tiny_skia::Pixmap> {
    let size = ICON_SIZE as f32;
    let mut pixmap = tiny_skia::Pixmap::new(ICON_SIZE, ICON_SIZE)?;
    if path.extension().is_some_and(|extension| extension == "svg") {
        let data = std::fs::read(path).ok()?;
        let tree = usvg::Tree::from_data(&data, &usvg::Options::default()).ok()?;
        let source = tree.size();
        let scale = size / source.width().max(source.height());
        let offset_x = (size - source.width() * scale) / 2.0;
        let offset_y = (size - source.height() * scale) / 2.0;
        resvg::render(&tree, tiny_skia::Transform::from_scale(scale, scale).post_translate(offset_x, offset_y), &mut pixmap.as_mut());
    } else {
        let image = tiny_skia::Pixmap::load_png(path).ok()?;
        let scale = size / image.width().max(image.height()) as f32;
        let offset_x = (size - image.width() as f32 * scale) / 2.0;
        let offset_y = (size - image.height() as f32 * scale) / 2.0;
        let paint = tiny_skia::PixmapPaint { quality: tiny_skia::FilterQuality::Bicubic, ..Default::default() };
        pixmap.draw_pixmap(0, 0, image.as_ref(), &paint, tiny_skia::Transform::from_scale(scale, scale).post_translate(offset_x, offset_y), None);
    }
    Some(pixmap)
}

/// Premultiplied RGBA atlas, ATLAS_COLUMNS wide. Returns the atlas, its row count and
/// the slot for each icon name (missing icons get no slot).
pub struct IconAtlas {
    pub pixels: Vec<u8>,
    pub rows: u32,
    pub slots: HashMap<String, u32>,
}

pub fn build_icon_atlas(entries: &[Entry]) -> IconAtlas {
    let index = icon_index();
    let mut slots = HashMap::new();
    let mut images = Vec::new();
    for entry in entries {
        if entry.icon.is_empty() || slots.contains_key(&entry.icon) {
            continue;
        }
        let path = if entry.icon.starts_with('/') { Some(PathBuf::from(&entry.icon)) } else { index.get(&entry.icon).cloned() };
        if let Some(pixmap) = path.as_deref().and_then(rasterize) {
            slots.insert(entry.icon.clone(), images.len() as u32);
            images.push(pixmap);
        }
    }
    let rows = (images.len() as u32).div_ceil(ATLAS_COLUMNS).max(1);
    let width = ATLAS_COLUMNS * ICON_SIZE;
    let mut pixels = vec![0u8; (width * rows * ICON_SIZE * 4) as usize];
    for (slot, image) in images.iter().enumerate() {
        let column = slot as u32 % ATLAS_COLUMNS;
        let row = slot as u32 / ATLAS_COLUMNS;
        for y in 0..ICON_SIZE {
            let source = &image.data()[(y * ICON_SIZE * 4) as usize..((y + 1) * ICON_SIZE * 4) as usize];
            let start = (((row * ICON_SIZE + y) * width + column * ICON_SIZE) * 4) as usize;
            pixels[start..start + source.len()].copy_from_slice(source);
        }
    }
    IconAtlas { pixels, rows, slots }
}
