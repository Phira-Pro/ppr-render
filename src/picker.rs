//! 应用内文件浏览器。
//!
//! Android 没有桌面的系统文件对话框，且 `rfd` 不支持移动端。这里提供
//! 一个纯 macroquad 绘制的浏览器，直接使用文件系统路径，回放与谱面
//! 仍能保持“同目录自动匹配”的行为。

use macroquad::prelude::*;
use std::{
    fs,
    path::{Path, PathBuf},
};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PickerKind {
    Replay,
    ChartFolder,
    ChartZip,
}

impl PickerKind {
    fn title(self) -> &'static str {
        match self {
            Self::Replay => "选择 Phira 回放",
            Self::ChartFolder => "选择谱面文件夹",
            Self::ChartZip => "选择谱面 ZIP",
        }
    }

    fn accepts_file(self, path: &Path) -> bool {
        let extension = path
            .extension()
            .map(|it| it.to_string_lossy().to_ascii_lowercase());
        match self {
            Self::Replay => matches!(extension.as_deref(), Some("ppr" | "phirar")),
            Self::ChartZip => matches!(extension.as_deref(), Some("zip" | "phira" | "pez")),
            Self::ChartFolder => false,
        }
    }

    fn picks_folder(self) -> bool {
        matches!(self, Self::ChartFolder)
    }
}

pub enum PickerResult {
    None,
    Selected(PathBuf),
    Cancelled,
}

struct Entry {
    path: PathBuf,
    name: String,
    directory: bool,
}

pub struct Picker {
    kind: PickerKind,
    dir: PathBuf,
    entries: Vec<Entry>,
    message: Option<String>,
    scroll: f32,
    drag: Option<(f32, bool)>,
}

impl Picker {
    pub fn new(kind: PickerKind, start: Option<PathBuf>) -> Self {
        let dir = start
            .filter(|path| path.is_dir())
            .or_else(crate::default_picker_dir)
            .unwrap_or_else(|| PathBuf::from("/"));
        let mut picker = Self {
            kind,
            dir,
            entries: Vec::new(),
            message: None,
            scroll: 0.,
            drag: None,
        };
        picker.reload();
        picker
    }

    pub fn kind(&self) -> PickerKind {
        self.kind
    }

    fn reload(&mut self) {
        self.entries.clear();
        self.message = None;
        self.scroll = 0.;
        let read = match fs::read_dir(&self.dir) {
            Ok(read) => read,
            Err(err) => {
                self.message = Some(format!("无法读取目录：{err}"));
                return;
            }
        };
        for entry in read.flatten() {
            let path = entry.path();
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            let directory = file_type.is_dir();
            if !directory && !self.kind.accepts_file(&path) {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            self.entries.push(Entry {
                path,
                name,
                directory,
            });
        }
        self.entries.sort_by(|a, b| {
            b.directory
                .cmp(&a.directory)
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });
        if self.entries.is_empty() {
            self.message = Some(if self.kind.picks_folder() {
                "此目录没有子文件夹，可以直接选择当前目录".into()
            } else {
                "此目录没有匹配的文件".into()
            });
        }
    }

    fn navigate(&mut self, path: PathBuf) {
        if path.is_dir() {
            self.dir = path;
            self.reload();
        }
    }

    fn parent(&self) -> Option<PathBuf> {
        self.dir.parent().map(Path::to_path_buf)
    }

    pub fn frame(&mut self, font: &Font) -> PickerResult {
        set_default_camera();
        let (w, h) = (screen_width(), screen_height());
        let s = (w / 1280.).min(h / 720.).clamp(0.55, 2.);
        draw_rectangle(0., 0., w, h, Color::new(0.01, 0.018, 0.03, 0.96));

        let margin = 24. * s;
        let card = Rect::new(margin, margin, w - margin * 2., h - margin * 2.);
        round_rect(card, 20. * s, Color::new(0.035, 0.055, 0.085, 0.98));
        round_rect(
            Rect::new(card.x + 1., card.y + 1., card.w - 2., card.h - 2.),
            19. * s,
            Color::new(0.05, 0.075, 0.11, 0.98),
        );

        let pad = 22. * s;
        let left = card.x + pad;
        let right = card.right() - pad;
        text(font, self.kind.title(), left, card.y + 40. * s, 26. * s, WHITE);
        text(
            font,
            &fit_text(font, &self.dir.display().to_string(), 15. * s, card.w - pad * 2.),
            left,
            card.y + 66. * s,
            15. * s,
            Color::new(0.62, 0.7, 0.8, 1.),
        );

        let mut result = PickerResult::None;
        let mut shortcuts = vec![
            ("下载", PathBuf::from("/sdcard/Download")),
            ("文档", PathBuf::from("/sdcard/Documents")),
            ("主目录", PathBuf::from("/sdcard")),
        ];
        #[cfg(target_os = "android")]
        if let Some(dir) = crate::android::external_files_dir() {
            shortcuts.push(("应用文件", dir));
        }
        shortcuts.push(("当前位置", self.dir.clone()));
        let mut sx = left;
        let shortcut_y = card.y + 82. * s;
        for (label, path) in shortcuts {
            if !path.is_dir() {
                continue;
            }
            let width = (label.chars().count() as f32 * 15. + 20.) * s;
            let rect = Rect::new(sx, shortcut_y, width, 30. * s);
            if button(rect, label, font, 14. * s, false) {
                self.navigate(path);
            }
            sx = rect.right() + 8. * s;
        }
        if let Some(parent) = self.parent() {
            let width = 72. * s;
            let rect = Rect::new(right - width, shortcut_y, width, 30. * s);
            if button(rect, "上一级", font, 14. * s, false) {
                self.navigate(parent);
            }
        }

        let list = Rect::new(
            left,
            card.y + 124. * s,
            card.w - pad * 2.,
            card.h - 124. * s - 66. * s,
        );
        round_rect(list, 10. * s, Color::new(0.025, 0.04, 0.062, 1.));
        let row_h = 38. * s;
        let content_h = self.entries.len() as f32 * row_h;
        let max_scroll = (content_h - list.h).max(0.);
        self.scroll = self.scroll.clamp(0., max_scroll);

        let mouse = vec2(mouse_position().0, mouse_position().1);
        let pressed = is_mouse_button_pressed(MouseButton::Left);
        let released = is_mouse_button_released(MouseButton::Left);
        let down = is_mouse_button_down(MouseButton::Left);
        if list.contains(mouse) {
            self.scroll -= mouse_wheel().1 * 36. * s;
        }
        if pressed && list.contains(mouse) {
            self.drag = Some((mouse.y, false));
        }
        if down {
            if let Some((last, moved)) = self.drag {
                let delta = mouse.y - last;
                if moved || delta.abs() > 6. {
                    self.scroll -= delta;
                    self.drag = Some((mouse.y, true));
                }
            }
        }

        let first = (self.scroll / row_h).floor().max(0.) as usize;
        let visible = (list.h / row_h).ceil() as usize + 1;
        let mut clicked = None;
        for (offset, entry) in self.entries.iter().enumerate().skip(first).take(visible) {
            let y = list.y + offset as f32 * row_h - self.scroll;
            if y + row_h < list.y || y > list.bottom() {
                continue;
            }
            let row = Rect::new(list.x + 4. * s, y + 2. * s, list.w - 8. * s, row_h - 4. * s);
            let hover = row.contains(mouse) && !matches!(self.drag, Some((_, true)));
            if hover {
                round_rect(row, 7. * s, Color::new(0.08, 0.12, 0.17, 1.));
            }
            let icon = if entry.directory { "▸" } else { "·" };
            let color = if entry.directory {
                Color::new(0.98, 0.78, 0.42, 1.)
            } else {
                Color::new(0.82, 0.88, 0.95, 1.)
            };
            text(
                font,
                icon,
                row.x + 10. * s,
                row.y + row.h / 2. + 6. * s,
                19. * s,
                color,
            );
            let name = if entry.directory {
                format!("{}/", entry.name)
            } else {
                entry.name.clone()
            };
            text(
                font,
                &fit_text(font, &name, 17. * s, row.w - 40. * s),
                row.x + 32. * s,
                row.y + row.h / 2. + 6. * s,
                17. * s,
                color,
            );
            if released && hover {
                clicked = Some(offset);
            }
        }
        if released {
            let was_drag = matches!(self.drag, Some((_, true)));
            self.drag = None;
            if !was_drag {
                if let Some(index) = clicked {
                    let entry = &self.entries[index];
                    if entry.directory {
                        self.navigate(entry.path.clone());
                    } else {
                        result = PickerResult::Selected(entry.path.clone());
                    }
                }
            }
        }

        if let Some(message) = &self.message {
            text(
                font,
                &fit_text(font, message, 16. * s, list.w - pad),
                list.x + 16. * s,
                list.y + 34. * s,
                16. * s,
                Color::new(0.68, 0.74, 0.82, 1.),
            );
        }

        let buttons_y = card.bottom() - 50. * s;
        let cancel = Rect::new(left, buttons_y, 96. * s, 34. * s);
        if button(cancel, "取消", font, 16. * s, false) {
            result = PickerResult::Cancelled;
        }
        if self.kind.picks_folder() {
            let select = Rect::new(right - 150. * s, buttons_y, 150. * s, 34. * s);
            if button(select, "选择当前文件夹", font, 16. * s, true) {
                result = PickerResult::Selected(self.dir.clone());
            }
        }

        result
    }
}

fn text(font: &Font, value: &str, x: f32, y: f32, size: f32, color: Color) {
    draw_text_ex(
        value,
        x,
        y,
        TextParams {
            font: *font,
            font_size: size.round() as u16,
            color,
            ..Default::default()
        },
    );
}

fn fit_text(font: &Font, value: &str, size: f32, max_width: f32) -> String {
    if measure_text(value, Some(*font), size.round() as u16, 1.).width <= max_width {
        return value.to_owned();
    }
    let mut output = String::new();
    for ch in value.chars() {
        let mut next = output.clone();
        next.push(ch);
        next.push('…');
        if measure_text(&next, Some(*font), size.round() as u16, 1.).width > max_width {
            break;
        }
        output.push(ch);
    }
    output.push('…');
    output
}

fn button(rect: Rect, label: &str, font: &Font, size: f32, primary: bool) -> bool {
    let hover = rect.contains(vec2(mouse_position().0, mouse_position().1));
    let fill = if primary {
        if hover {
            Color::new(0.15, 0.24, 0.36, 1.)
        } else {
            Color::new(0.1, 0.18, 0.28, 1.)
        }
    } else if hover {
        Color::new(0.09, 0.13, 0.18, 1.)
    } else {
        Color::new(0.055, 0.085, 0.12, 1.)
    };
    round_rect(rect, 8., fill);
    let width = measure_text(label, Some(*font), size.round() as u16, 1.).width;
    text(
        font,
        label,
        rect.x + (rect.w - width) / 2.,
        rect.y + rect.h / 2. + size * 0.34,
        size,
        if primary {
            Color::new(0.95, 0.97, 1., 1.)
        } else {
            Color::new(0.78, 0.84, 0.9, 1.)
        },
    );
    hover && is_mouse_button_released(MouseButton::Left)
}

fn round_rect(rect: Rect, radius: f32, color: Color) {
    draw_rectangle(rect.x + radius, rect.y, rect.w - radius * 2., rect.h, color);
    draw_rectangle(rect.x, rect.y + radius, rect.w, rect.h - radius * 2., color);
    for (x, y) in [
        (rect.x + radius, rect.y + radius),
        (rect.right() - radius, rect.y + radius),
        (rect.x + radius, rect.bottom() - radius),
        (rect.right() - radius, rect.bottom() - radius),
    ] {
        draw_circle(x, y, radius, color);
    }
}
