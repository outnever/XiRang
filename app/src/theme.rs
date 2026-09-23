//! 配色：暗 / 亮 / 自定义。用户级持久化（和视图态放一起），不进数据文件。

use eframe::egui;
use egui::Color32;

#[derive(Clone, Debug, PartialEq)]
pub struct Palette {
    /// 是否按"暗色"基准（影响未覆盖的其它 egui 颜色）
    pub dark: bool,
    pub bg: String,
    pub fg: String,
    pub accent: String,
    pub aux: String,
    pub border: String,
}

impl Default for Palette {
    fn default() -> Self {
        Palette {
            dark: true,
            bg: "#1f2328".into(),
            fg: "#e6edf3".into(),
            accent: "#58a6ff".into(),
            aux: "#c98a5e".into(),
            border: "#30363d".into(),
        }
    }
}

impl Palette {
    pub fn light() -> Self {
        Palette {
            dark: false,
            bg: "#ffffff".into(),
            fg: "#1f2328".into(),
            accent: "#0969da".into(),
            aux: "#8b5e3c".into(),
            border: "#d0d7de".into(),
        }
    }
}

pub fn parse_hex(s: &str) -> Option<Color32> {
    let h = s.trim().trim_start_matches('#');
    let v = |i: usize| u8::from_str_radix(h.get(i..i + 2)?, 16).ok();
    match h.len() {
        6 => Some(Color32::from_rgb(v(0)?, v(2)?, v(4)?)),
        8 => Some(Color32::from_rgba_unmultiplied(v(0)?, v(2)?, v(4)?, v(6)?)),
        _ => None,
    }
}

pub fn to_hex(c: Color32) -> String {
    format!("#{:02x}{:02x}{:02x}", c.r(), c.g(), c.b())
}

/// 把配色套到 egui 的默认主题上（只覆盖我们关心的那几项）。
pub fn visuals(p: &Palette) -> egui::Visuals {
    let mut v = if p.dark {
        egui::Visuals::dark()
    } else {
        egui::Visuals::light()
    };
    let bg = parse_hex(&p.bg).unwrap_or(if p.dark { Color32::from_rgb(31, 35, 40) } else { Color32::WHITE });
    let fg = parse_hex(&p.fg).unwrap_or(if p.dark { Color32::from_rgb(230, 237, 243) } else { Color32::BLACK });
    let accent = parse_hex(&p.accent).unwrap_or(Color32::from_rgb(88, 166, 255));
    let border = parse_hex(&p.border).unwrap_or(Color32::from_rgb(48, 54, 61));

    v.panel_fill = bg;
    v.window_fill = bg;
    v.extreme_bg_color = if p.dark {
        bg.linear_multiply(0.8)
    } else {
        bg.linear_multiply(0.98)
    };
    v.override_text_color = Some(fg);
    v.selection.bg_fill = accent;
    v.selection.stroke = egui::Stroke::new(1.0_f32, fg);
    v.widgets.noninteractive.bg_stroke = egui::Stroke::new(1.0_f32, border);
    v.widgets.inactive.bg_fill = if p.dark {
        border.linear_multiply(1.2)
    } else {
        bg.linear_multiply(0.94)
    };
    v.widgets.inactive.fg_stroke = egui::Stroke::new(1.0_f32, fg);
    v.widgets.hovered.bg_fill = accent.linear_multiply(0.5);
    v
}

/// 辅助节点（`@` 开头）的颜色。
pub fn aux_color(p: &Palette) -> Color32 {
    parse_hex(&p.aux).unwrap_or(Color32::from_rgb(201, 138, 94))
}
