//! 视图态持久化：最近文件、每个文件的布局与展开态。
//!
//! 存用户级配置目录（macOS：`~/Library/Application Support/XiRang/views.json`），
//! **不写进 `.xirang`**——视图 ≠ 数据。

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use xirang_core::codec::Uuid;

#[derive(Clone, Debug, Default)]
pub struct FileView {
    /// "indent" / "layered"
    pub layout: String,
    /// 展开的节点
    pub expanded: Vec<Uuid>,
    /// 聚焦的子树根
    pub focus: Option<Uuid>,
}

#[derive(Clone, Debug, Default)]
pub struct ViewState {
    pub recent: Vec<String>,
    pub files: HashMap<String, FileView>,
}

pub fn state_path() -> PathBuf {
    if let Ok(p) = std::env::var("XIRANG_VIEW_STATE") {
        return PathBuf::from(p);
    }
    let base = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(base)
        .join("Library")
        .join("Application Support")
        .join("XiRang")
        .join("views.json")
}

impl ViewState {
    /// 读取（文件不存在 / 坏掉都当空状态，不报错——它只是便利设施）。
    pub fn load() -> ViewState {
        let Ok(text) = std::fs::read_to_string(state_path()) else {
            return ViewState::default();
        };
        parse(&text)
    }

    pub fn save(&self) -> std::io::Result<()> {
        let path = state_path();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, self.to_text())
    }

    pub fn touch_recent(&mut self, path: &Path) {
        let s = path.display().to_string();
        self.recent.retain(|p| p != &s);
        self.recent.insert(0, s);
        self.recent.truncate(12);
    }

    pub fn forget_recent(&mut self, path: &Path) {
        let s = path.display().to_string();
        self.recent.retain(|p| p != &s);
    }

    pub fn view_of(&self, path: &Path) -> Option<&FileView> {
        self.files.get(&path.display().to_string())
    }

    pub fn set_view(&mut self, path: &Path, view: FileView) {
        self.files.insert(path.display().to_string(), view);
    }

    /// 只保留最近 N 个文件的视图态，避免无限增长。
    pub fn prune(&mut self, keep: usize) {
        if self.files.len() <= keep {
            return;
        }
        let recent = self.recent.clone();
        self.files.retain(|k, _| recent.iter().any(|r| r == k));
    }

    // —— 极简 JSON（只读写本文件需要的形状，避免引入额外依赖）——

    pub fn to_text(&self) -> String {
        let mut out = String::from("{\"recent\":[");
        for (i, r) in self.recent.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push_str(&json_string(r));
        }
        out.push_str("],\"files\":{");
        let mut keys: Vec<&String> = self.files.keys().collect();
        keys.sort();
        for (i, k) in keys.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            let v = &self.files[*k];
            out.push_str(&json_string(k));
            out.push_str(":{\"layout\":");
            out.push_str(&json_string(&v.layout));
            out.push_str(",\"focus\":");
            match v.focus {
                Some(id) => out.push_str(&json_string(&id.to_string())),
                None => out.push_str("null"),
            }
            out.push_str(",\"expanded\":[");
            for (j, id) in v.expanded.iter().enumerate() {
                if j > 0 {
                    out.push(',');
                }
                out.push_str(&json_string(&id.to_string()));
            }
            out.push_str("]}");
        }
        out.push_str("}}");
        out
    }

    pub fn from_text(text: &str) -> ViewState {
        parse(text)
    }
}

fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// 极简解析：只认自己写出来的形状（容错优先，认不出来就当空）。
fn parse(text: &str) -> ViewState {
    let mut state = ViewState::default();
    let Some(recent_start) = text.find("\"recent\":[") else {
        return state;
    };
    let recent_end = text[recent_start..]
        .find(']')
        .map(|i| recent_start + i)
        .unwrap_or(text.len());
    state.recent = parse_string_array(&text[recent_start + 10..recent_end]);

    let Some(files_start) = text.find("\"files\":{") else {
        return state;
    };
    let mut i = files_start + 9;
    let bytes = text.as_bytes();
    while i < bytes.len() {
        // 找下一个 key
        while i < bytes.len() && bytes[i] != b'"' {
            if bytes[i] == b'}' {
                return state;
            }
            i += 1;
        }
        let Some((key, next)) = parse_string(text, i) else {
            return state;
        };
        i = next;
        let Some(colon) = text[i..].find(':').map(|k| i + k) else {
            return state;
        };
        i = colon + 1;
        let Some(obj_end) = text[i..].find("]}").map(|k| i + k + 1) else {
            return state;
        };
        let obj = &text[i..=obj_end];
        let mut view = FileView::default();
        view.layout = find_string_field(obj, "layout").unwrap_or_else(|| "indent".into());
        view.focus = find_string_field(obj, "focus").and_then(|s| Uuid::parse(&s));
        if let Some(es) = obj.find("\"expanded\":[") {
            let e = obj[es..].find(']').map(|k| es + k).unwrap_or(obj.len());
            view.expanded = parse_string_array(&obj[es + 12..e])
                .iter()
                .filter_map(|s| Uuid::parse(s))
                .collect();
        }
        state.files.insert(key, view);
        i = obj_end + 1;
    }
    state
}

fn find_string_field(obj: &str, name: &str) -> Option<String> {
    let key = format!("\"{name}\":");
    let start = obj.find(&key)? + key.len();
    let rest = &obj[start..];
    if rest.starts_with("null") {
        return None;
    }
    parse_string(rest, rest.find('"')?).map(|(s, _)| s)
}

fn parse_string_array(body: &str) -> Vec<String> {
    let mut out = Vec::new();
    let bytes = body.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'"' {
            match parse_string(body, i) {
                Some((s, next)) => {
                    out.push(s);
                    i = next;
                }
                None => break,
            }
        } else {
            i += 1;
        }
    }
    out
}

fn parse_string(text: &str, start: usize) -> Option<(String, usize)> {
    let bytes = text.as_bytes();
    if bytes.get(start) != Some(&b'"') {
        return None;
    }
    let mut out = String::new();
    let mut i = start + 1;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => return Some((out, i + 1)),
            b'\\' => {
                i += 1;
                match bytes.get(i) {
                    Some(b'n') => out.push('\n'),
                    Some(b'r') => out.push('\r'),
                    Some(b't') => out.push('\t'),
                    Some(b'u') => {
                        let hex = text.get(i + 1..i + 5)?;
                        let code = u32::from_str_radix(hex, 16).ok()?;
                        out.push(char::from_u32(code)?);
                        i += 4;
                    }
                    Some(c) => out.push(*c as char),
                    None => return None,
                }
                i += 1;
            }
            _ => {
                // 按 UTF-8 边界推进
                let rest = &text[i..];
                let c = rest.chars().next()?;
                out.push(c);
                i += c.len_utf8();
            }
        }
    }
    None
}
