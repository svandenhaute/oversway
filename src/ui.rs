//! Immediate-mode layout: every frame is drawn from `App` from scratch.

use crate::render::{rgba, text_width, DrawList, Gpu};
use crate::{files, launcher, notes, Action, App, Focus, OutputView, BLUR_FADE};
use glyphon::{Buffer, Weight};
use std::time::Instant;

pub const VISIBLE_RESULTS: usize = 9;

const TEXT: u32 = 0xcdd6f4;
const SUBTLE: u32 = 0x9399b2;
const PANEL: u32 = 0x1e1e2e;
const SURFACE: u32 = 0x313244;
const ACCENT: u32 = 0x89b4fa;
const GREEN: u32 = 0xa6e3a1;
const YELLOW: u32 = 0xf9e2af;

fn text_color(hex: u32, alpha: f32) -> [u8; 4] {
    [(hex >> 16) as u8, (hex >> 8) as u8, hex as u8, (alpha * 255.0) as u8]
}

type Hits = Vec<([f32; 4], Action)>;

struct Painter<'a> {
    gpu: &'a mut Gpu,
    list: DrawList,
    hits: Hits,
    scale: f32,
    width: f32,
    height: f32,
}

impl Painter<'_> {
    /// Logical size to physical pixels.
    fn s(&self, logical: f32) -> f32 {
        (logical * self.scale).round()
    }

    fn shape(&mut self, text: &str, size: f32, wrap: Option<f32>, weight: Weight) -> Buffer {
        let size = self.s(size);
        self.gpu.text(text, size, wrap, weight, false)
    }

    /// Draws text with its top-left at (x, y); returns its width.
    fn label(&mut self, text: &str, x: f32, y: f32, size: f32, weight: Weight, color: [u8; 4], clip: [f32; 4]) -> f32 {
        let buffer = self.shape(text, size, None, weight);
        let width = text_width(&buffer);
        self.list.text(buffer, x, y, clip, color);
        width
    }

    fn centered(&mut self, text: &str, center_x: f32, y: f32, size: f32, weight: Weight, color: [u8; 4]) {
        let buffer = self.shape(text, size, None, weight);
        let width = text_width(&buffer);
        let clip = [0.0, 0.0, self.width, self.height];
        self.list.text(buffer, center_x - width / 2.0, y, clip, color);
    }
}

pub fn draw(gpu: &mut Gpu, app: &App, view: &OutputView, size: (u32, u32), scale: f32, now: Instant) -> (DrawList, Hits, bool) {
    let mut painter = Painter { gpu, list: DrawList::default(), hits: Vec::new(), scale, width: size.0 as f32, height: size.1 as f32 };
    let (width, height) = (painter.width, painter.height);

    // Background: the dim surface is underneath; fade the blurred capture in over it.
    let fade = (now.saturating_duration_since(app.shown_at).as_secs_f32() / BLUR_FADE.as_secs_f32()).clamp(0.0, 1.0);
    let mut animating = false;
    if let Some((blurred, _)) = &view.blurred {
        painter.list.image(blurred, 0.0, 0.0, width, height, [0.0, 0.0, 1.0, 1.0], [fade; 4], 0.0);
        painter.list.rect(0.0, 0.0, width, height, rgba(0x000000, 0.45 * fade), 0.0);
        animating = fade < 1.0;
    }

    // Clock, date, stats.
    let clock = chrono::Local::now();
    let top = painter.s(56.0);
    painter.centered(&clock.format("%H:%M").to_string(), width / 2.0, top, 84.0, Weight::BOLD, text_color(TEXT, 1.0));
    painter.centered(&clock.format("%A %-d %B %Y").to_string(), width / 2.0, top + painter.s(112.0), 22.0, Weight::NORMAL, text_color(SUBTLE, 1.0));
    draw_idle_button(&mut painter, app, top);
    let stats_top = top + painter.s(160.0);
    draw_stats(&mut painter, app, stats_top);

    // Workspaces of this output.
    let workspaces: Vec<&crate::sway::Workspace> = app.snapshot.workspaces.iter().filter(|workspace| workspace.output == view.name).collect();
    let workspaces_top = stats_top + painter.s(146.0);
    let wanted_height = painter.s(170.0);
    let workspaces_bottom = draw_workspaces(&mut painter, app, view, &workspaces, workspaces_top, wanted_height);

    // Same content in every mode.
    {
        let panels_top = workspaces_bottom + painter.s(48.0);
        let panel_height = (height - panels_top - painter.s(48.0)).min(painter.s(600.0));
        let gap = painter.s(32.0);
        let panel_width = painter.s(560.0).min((width - painter.s(96.0) - 2.0 * gap) / 3.0);
        let left = (width - 3.0 * panel_width - 2.0 * gap) / 2.0;
        if panel_height > painter.s(120.0) {
            let column = |index: f32| [left + index * (panel_width + gap), panels_top, panel_width, panel_height];
            draw_files(&mut painter, app, column(0.0));
            draw_launcher(&mut painter, app, column(1.0));
            draw_notes(&mut painter, app, column(2.0));
        }
    }

    (painter.list, painter.hits, animating)
}

/// Top-right pill: an open eye while swaylock is disabled (screen stays awake), a crossed-out eye otherwise.
fn draw_idle_button(painter: &mut Painter, app: &App, top: f32) {
    let (color, label, column) = if app.idle_inhibited { (ACCENT, "Swaylock off", 0.0) } else { (SUBTLE, "Swaylock on", 0.5) };
    let label_buffer = painter.shape(label, 14.0, None, Weight::BOLD);
    let (padding, icon, gap, height) = (painter.s(14.0), painter.s(22.0), painter.s(10.0), painter.s(44.0));
    let width = padding + icon + gap + text_width(&label_buffer) + padding;
    let (x, y) = (painter.width - painter.s(48.0) - width, top);
    painter.list.rect(x, y, width, height, rgba(PANEL, 0.82), height / 2.0);
    if app.idle_inhibited {
        painter.list.outline(x, y, width, height, rgba(ACCENT, 0.9), height / 2.0, painter.s(1.5));
    }
    painter.list.image(&app.eye_icons, x + padding, y + (height - icon) / 2.0, icon, icon, [column, 0.0, 0.5, 1.0], rgba(color, 1.0), 0.0);
    let text_y = y + (height - painter.s(19.0)) / 2.0;
    let clip = [0.0, 0.0, painter.width, painter.height];
    painter.list.text(label_buffer, x + padding + icon + gap, text_y, clip, text_color(color, 1.0));
    painter.hits.push(([x, y, width, height], Action::ToggleIdleInhibit));
}

const GIBIBYTE: f64 = 1024.0 * 1024.0 * 1024.0;

fn gibibytes(used: u64, total: u64) -> String {
    format!("{:.1} / {:.1} GiB", used as f64 / GIBIBYTE, total as f64 / GIBIBYTE)
}

fn fraction(used: u64, total: u64) -> f32 {
    if total > 0 { used as f32 / total as f32 } else { 0.0 }
}

/// Card background with a title on the left and a value on the right; returns the content rect.
fn card(painter: &mut Painter, rect: [f32; 4], title: &str, value: &str) -> [f32; 4] {
    let padding = painter.s(14.0);
    let clip = [0.0, 0.0, painter.width, painter.height];
    painter.list.rect(rect[0], rect[1], rect[2], rect[3], rgba(PANEL, 0.82), painter.s(12.0));
    painter.label(title, rect[0] + padding, rect[1] + padding, 13.0, Weight::BOLD, text_color(SUBTLE, 1.0), clip);
    let buffer = painter.shape(value, 15.0, None, Weight::BOLD);
    let value_x = rect[0] + rect[2] - padding - text_width(&buffer);
    painter.list.text(buffer, value_x, rect[1] + padding - painter.s(1.0), clip, text_color(TEXT, 1.0));
    let header = painter.s(40.0);
    [rect[0] + padding, rect[1] + header, rect[2] - 2.0 * padding, rect[3] - header - padding]
}

/// Small label, right-aligned value, and a fill bar under them. Returns the next y.
fn meter(painter: &mut Painter, x: f32, y: f32, width: f32, label: &str, value: &str, filled: f32, color: u32) -> f32 {
    let clip = [0.0, 0.0, painter.width, painter.height];
    painter.label(label, x, y, 12.0, Weight::NORMAL, text_color(SUBTLE, 1.0), clip);
    let buffer = painter.shape(value, 12.0, None, Weight::NORMAL);
    let value_x = x + width - text_width(&buffer);
    painter.list.text(buffer, value_x, y, clip, text_color(TEXT, 0.9));
    let bar_y = y + painter.s(19.0);
    let bar_height = painter.s(8.0);
    painter.list.rect(x, bar_y, width, bar_height, rgba(SURFACE, 1.0), bar_height / 2.0);
    painter.list.rect(x, bar_y, (width * filled.clamp(0.0, 1.0)).max(bar_height), bar_height, rgba(color, 0.9), bar_height / 2.0);
    bar_y + bar_height + painter.s(8.0)
}

fn draw_stats(painter: &mut Painter, app: &App, top: f32) {
    let stats = &app.stats;
    let card_width = painter.s(380.0);
    let card_height = painter.s(112.0);
    let gap = painter.s(20.0);
    let left = (painter.width - 3.0 * card_width - 2.0 * gap) / 2.0;
    let watts = |watts: Option<f32>| watts.map_or(String::new(), |watts| format!(" · {watts:.0} W"));

    // CPU: total, package power, one bar per core.
    let content = card(painter, [left, top, card_width, card_height], "CPU", &format!("{:.0}%{}", stats.cpu * 100.0, watts(stats.cpu_watts)));
    let count = stats.cores.len().max(1) as f32;
    let slot = content[2] / count;
    for (index, load) in stats.cores.iter().enumerate() {
        let x = content[0] + index as f32 * slot;
        let bar_width = (slot - painter.s(2.0)).max(1.0);
        painter.list.rect(x, content[1], bar_width, content[3], rgba(SURFACE, 1.0), painter.s(2.0));
        let height = (load * content[3]).max(painter.s(1.0));
        painter.list.rect(x, content[1] + content[3] - height, bar_width, height, rgba(GREEN, 0.9), painter.s(2.0));
    }

    // Memory and swap.
    let x = left + card_width + gap;
    let content = card(painter, [x, top, card_width, card_height], "MEMORY", &gibibytes(stats.memory_used, stats.memory_total));
    let next = meter(painter, content[0], content[1], content[2], "RAM", &format!("{:.0}%", fraction(stats.memory_used, stats.memory_total) * 100.0), fraction(stats.memory_used, stats.memory_total), YELLOW);
    meter(painter, content[0], next, content[2], "Swap", &gibibytes(stats.swap_used, stats.swap_total), fraction(stats.swap_used, stats.swap_total), YELLOW);

    // GPU: power, utilization, memory (NVML, every 2 s).
    let x = left + 2.0 * (card_width + gap);
    match &stats.gpu {
        Some(gpu) => {
            let content = card(painter, [x, top, card_width, card_height], "GPU", &format!("{:.0}%{}", gpu.utilization * 100.0, watts(Some(gpu.watts))));
            let next = meter(painter, content[0], content[1], content[2], "Utilization", &format!("{:.0}%", gpu.utilization * 100.0), gpu.utilization, ACCENT);
            meter(painter, content[0], next, content[2], "Memory", &gibibytes(gpu.memory_used, gpu.memory_total), fraction(gpu.memory_used, gpu.memory_total), ACCENT);
        }
        None => {
            card(painter, [x, top, card_width, card_height], "GPU", "–");
        }
    }
}

/// Returns the bottom edge of the row.
fn draw_workspaces(painter: &mut Painter, app: &App, view: &OutputView, workspaces: &[&crate::sway::Workspace], top: f32, wanted_height: f32) -> f32 {
    if workspaces.is_empty() {
        return top;
    }
    let aspect = workspaces[0].rect.width.max(1.0) / workspaces[0].rect.height.max(1.0);
    let gap = painter.s(20.0);
    let count = workspaces.len() as f32;
    let available = painter.width * 0.92;
    let thumbnail_height = wanted_height.min((available - (count - 1.0) * gap) / count / aspect);
    let thumbnail_width = thumbnail_height * aspect;
    let total = count * thumbnail_width + (count - 1.0) * gap;
    let mut x = (painter.width - total) / 2.0;
    let radius = painter.s(10.0);
    let clip_all = [0.0, 0.0, painter.width, painter.height];
    for workspace in workspaces {
        let rect = [x, top, thumbnail_width, thumbnail_height];
        painter.list.rect(rect[0], rect[1], rect[2], rect[3], rgba(PANEL, 0.85), radius);
        match app.thumbnails.get(&(view.name.clone(), workspace.name.clone())) {
            Some((image, _)) => painter.list.image(image, rect[0], rect[1], rect[2], rect[3], [0.0, 0.0, 1.0, 1.0], [1.0; 4], radius),
            None => {
                // Wireframe from the sway tree.
                let factor = thumbnail_width / workspace.rect.width.max(1.0);
                for window in &workspace.windows {
                    let window_x = rect[0] + (window.rect.x - workspace.rect.x) * factor;
                    let window_y = rect[1] + (window.rect.y - workspace.rect.y) * factor;
                    let window_width = window.rect.width * factor;
                    let window_height = window.rect.height * factor;
                    let inset = painter.s(2.0);
                    painter.list.rect(window_x + inset, window_y + inset, window_width - 2.0 * inset, window_height - 2.0 * inset, rgba(SURFACE, 0.9), painter.s(4.0));
                    let color = if window.focused { ACCENT } else { SUBTLE };
                    painter.list.outline(window_x + inset, window_y + inset, window_width - 2.0 * inset, window_height - 2.0 * inset, rgba(color, 0.9), painter.s(4.0), painter.s(1.5));
                    let clip = [window_x + inset, window_y + inset, window_width - 2.0 * inset, window_height - 2.0 * inset];
                    painter.label(&window.app_id, window_x + painter.s(8.0), window_y + painter.s(6.0), 12.0, Weight::NORMAL, text_color(TEXT, 0.9), clip);
                }
            }
        }
        if workspace.focused {
            painter.list.outline(rect[0] - painter.s(3.0), rect[1] - painter.s(3.0), rect[2] + painter.s(6.0), rect[3] + painter.s(6.0), rgba(ACCENT, 1.0), radius + painter.s(3.0), painter.s(3.0));
        } else if workspace.visible {
            painter.list.outline(rect[0] - painter.s(2.0), rect[1] - painter.s(2.0), rect[2] + painter.s(4.0), rect[3] + painter.s(4.0), rgba(SUBTLE, 0.8), radius + painter.s(2.0), painter.s(2.0));
        }
        if app.focus == Focus::Workspaces && workspace.name == app.workspace_selected {
            let inset = painter.s(7.0);
            painter.list.outline(rect[0] - inset, rect[1] - inset, rect[2] + 2.0 * inset, rect[3] + 2.0 * inset, rgba(YELLOW, 1.0), radius + inset, painter.s(3.0));
        }
        let color = if workspace.focused { text_color(ACCENT, 1.0) } else { text_color(TEXT, 0.9) };
        let buffer = painter.shape(&workspace.name, 16.0, None, Weight::BOLD);
        let label_x = x + (thumbnail_width - text_width(&buffer)) / 2.0;
        painter.list.text(buffer, label_x, top + thumbnail_height + painter.s(10.0), clip_all, color);
        painter.hits.push((rect, Action::Workspace(workspace.name.clone())));
        x += thumbnail_width + gap;
    }
    top + thumbnail_height + painter.s(36.0)
}

fn panel(painter: &mut Painter, rect: [f32; 4], focused: bool, focus: Focus) {
    let radius = painter.s(14.0);
    painter.list.rect(rect[0], rect[1], rect[2], rect[3], rgba(PANEL, 0.9), radius);
    if focused {
        painter.list.outline(rect[0], rect[1], rect[2], rect[3], rgba(ACCENT, 0.9), radius, painter.s(2.0));
    }
    painter.hits.push((rect, Action::Focus(focus)));
}

fn draw_launcher(painter: &mut Painter, app: &App, rect: [f32; 4]) {
    let focused = app.focus == Focus::Launcher;
    panel(painter, rect, focused, Focus::Launcher);
    let padding = painter.s(16.0);
    let clip = [rect[0] + padding, rect[1], rect[2] - 2.0 * padding, rect[3]];

    // Query field.
    let field = [rect[0] + padding, rect[1] + padding, rect[2] - 2.0 * padding, painter.s(48.0)];
    painter.list.rect(field[0], field[1], field[2], field[3], rgba(SURFACE, 1.0), painter.s(10.0));
    let text_x = field[0] + painter.s(14.0);
    let text_y = field[1] + painter.s(12.0);
    if app.query.is_empty() {
        painter.label("Search applications", text_x, text_y, 18.0, Weight::NORMAL, text_color(SUBTLE, 0.8), field);
    }
    let query_width = if app.query.is_empty() { 0.0 } else { painter.label(&app.query, text_x, text_y, 18.0, Weight::NORMAL, text_color(TEXT, 1.0), field) };
    if focused {
        painter.list.rect(text_x + query_width + painter.s(1.0), text_y + painter.s(1.0), painter.s(2.0), painter.s(22.0), rgba(ACCENT, 1.0), 0.0);
    }

    // Results.
    let row_height = painter.s(52.0);
    let list_top = field[1] + field[3] + painter.s(12.0);
    let fit = ((rect[1] + rect[3] - padding - list_top) / row_height).floor().max(0.0) as usize;
    let icon_size = painter.s(36.0);
    for (row, &index) in app.results.iter().take(fit.min(VISIBLE_RESULTS)).enumerate() {
        let entry = &app.entries[index];
        let y = list_top + row as f32 * row_height;
        let row_rect = [rect[0] + padding, y, rect[2] - 2.0 * padding, row_height - painter.s(4.0)];
        if row == app.selected {
            painter.list.rect(row_rect[0], row_rect[1], row_rect[2], row_rect[3], rgba(ACCENT, if focused { 0.28 } else { 0.14 }), painter.s(10.0));
        }
        let icon_x = row_rect[0] + painter.s(10.0);
        let icon_y = y + (row_rect[3] - icon_size) / 2.0;
        match (&app.icons, app.icon_slots.get(&entry.icon)) {
            (Some(atlas), Some(&slot)) => {
                let columns = launcher::ATLAS_COLUMNS as f32;
                let rows = (atlas.height / launcher::ICON_SIZE) as f32;
                let uv = [(slot % launcher::ATLAS_COLUMNS) as f32 / columns, (slot / launcher::ATLAS_COLUMNS) as f32 / rows, 1.0 / columns, 1.0 / rows];
                painter.list.image(atlas, icon_x, icon_y, icon_size, icon_size, uv, [1.0; 4], 0.0);
            }
            _ => {
                painter.list.rect(icon_x, icon_y, icon_size, icon_size, rgba(SURFACE, 1.0), painter.s(8.0));
                let initial: String = entry.name.chars().take(1).collect();
                let buffer = painter.shape(&initial, 18.0, None, Weight::BOLD);
                let initial_x = icon_x + (icon_size - text_width(&buffer)) / 2.0;
                painter.list.text(buffer, initial_x, icon_y + painter.s(6.0), clip, text_color(TEXT, 1.0));
            }
        }
        let name_x = icon_x + icon_size + painter.s(14.0);
        let name_width = painter.label(&entry.name, name_x, y + painter.s(13.0), 17.0, Weight::NORMAL, text_color(TEXT, 1.0), clip);
        if !entry.generic_name.is_empty() && entry.generic_name != entry.name {
            painter.label(&entry.generic_name, name_x + name_width + painter.s(12.0), y + painter.s(16.0), 13.0, Weight::NORMAL, text_color(SUBTLE, 1.0), clip);
        }
        painter.hits.push((row_rect, Action::Launch(index)));
    }
    if app.entries.is_empty() {
        painter.label("Loading applications…", rect[0] + padding, list_top + painter.s(8.0), 15.0, Weight::NORMAL, text_color(SUBTLE, 1.0), clip);
    }
}

fn draw_files(painter: &mut Painter, app: &App, rect: [f32; 4]) {
    let focused = app.focus == Focus::Files;
    panel(painter, rect, focused, Focus::Files);
    let browser = &app.files;
    let padding = painter.s(18.0);
    let inner = [rect[0] + padding, rect[1], rect[2] - 2.0 * padding, rect[3]];
    painter.label("FILES", inner[0], rect[1] + padding, 13.0, Weight::BOLD, text_color(SUBTLE, 1.0), inner);
    let hidden = if browser.show_hidden { "  ·  hidden shown" } else { "" };
    painter.label(&format!("{}{hidden}", files::display_path(&browser.directory)), inner[0], rect[1] + padding + painter.s(24.0), 14.0, Weight::NORMAL, text_color(TEXT, 0.8), inner);

    let row_height = painter.s(30.0);
    let list_top = rect[1] + padding + painter.s(56.0);
    let fit = ((rect[1] + rect[3] - padding - list_top) / row_height).floor().max(1.0) as usize;
    // Keep the selection near the middle once the list is longer than the panel.
    let first = browser.selected.saturating_sub(fit / 2).min(browser.entries.len().saturating_sub(fit));
    for (index, entry) in browser.entries.iter().enumerate().skip(first).take(fit) {
        let y = list_top + (index - first) as f32 * row_height;
        let row_rect = [inner[0] - painter.s(6.0), y, inner[2] + painter.s(12.0), row_height - painter.s(2.0)];
        if index == browser.selected {
            painter.list.rect(row_rect[0], row_rect[1], row_rect[2], row_rect[3], rgba(ACCENT, if focused { 0.28 } else { 0.14 }), painter.s(8.0));
        }
        let (name, color) = if entry.directory { (format!("{}/", entry.name), ACCENT) } else { (entry.name.clone(), TEXT) };
        painter.label(&name, inner[0] + painter.s(4.0), y + painter.s(5.0), 15.0, Weight::NORMAL, text_color(color, 1.0), inner);
        painter.hits.push((row_rect, Action::OpenFile(index)));
    }
    if browser.entries.is_empty() {
        painter.label("empty", inner[0] + painter.s(4.0), list_top + painter.s(5.0), 15.0, Weight::NORMAL, text_color(SUBTLE, 0.8), inner);
    }
}

/// Cursor (x, top, height) inside a shaped buffer.
fn cursor_position(buffer: &Buffer, text: &str, cursor: usize) -> (f32, f32, f32) {
    let line_index = text[..cursor].matches('\n').count();
    let column = cursor - notes::line_start(text, cursor);
    let mut found = (0.0, 0.0, buffer.metrics().line_height);
    for run in buffer.layout_runs() {
        if run.line_i != line_index {
            continue;
        }
        let (Some(first), Some(last)) = (run.glyphs.first(), run.glyphs.last()) else {
            return (0.0, run.line_top, run.line_height);
        };
        if column < first.start {
            continue;
        }
        let x = run.glyphs.iter().find(|glyph| glyph.start >= column).map_or(run.line_w, |glyph| glyph.x);
        found = (x, run.line_top, run.line_height);
        if column < last.end {
            break;
        }
    }
    found
}

fn draw_notes(painter: &mut Painter, app: &App, rect: [f32; 4]) {
    let focused = app.focus == Focus::Notes;
    panel(painter, rect, focused, Focus::Notes);
    let padding = painter.s(18.0);
    let clip = [0.0, 0.0, painter.width, painter.height];
    painter.label("NOTES", rect[0] + padding, rect[1] + padding, 13.0, Weight::BOLD, text_color(SUBTLE, 1.0), clip);
    let area = [rect[0] + padding, rect[1] + padding + painter.s(28.0), rect[2] - 2.0 * padding, rect[3] - 2.0 * padding - painter.s(28.0)];
    if app.notes.is_empty() && !focused {
        painter.label("Alt+l to write notes", area[0], area[1], 16.0, Weight::NORMAL, text_color(SUBTLE, 0.7), area);
        return;
    }
    let buffer = painter.shape(&app.notes, 16.0, Some(area[2]), Weight::NORMAL);
    let (cursor_x, cursor_top, cursor_height) = cursor_position(&buffer, &app.notes, app.notes_cursor);
    let scroll = (cursor_top + cursor_height - area[3]).max(0.0);
    if focused {
        painter.list.rect(area[0] + cursor_x, area[1] + cursor_top - scroll + painter.s(2.0), painter.s(2.0), cursor_height - painter.s(4.0), rgba(ACCENT, 1.0), 0.0);
    }
    painter.list.text(buffer, area[0], area[1] - scroll, area, text_color(TEXT, 1.0));
}
