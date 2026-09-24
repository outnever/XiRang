//! 界面文案的中英切换。
//!
//! 用法：`i18n::set(Lang::En)` 之后，所有 `t("打开…")` 都会给出英文。
//! 表里没有的串原样返回中文（漏翻不会变空字符串）。

use std::sync::atomic::{AtomicU8, Ordering};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Lang {
    Zh,
    En,
}

impl Lang {
    pub fn as_str(&self) -> &'static str {
        match self {
            Lang::Zh => "zh",
            Lang::En => "en",
        }
    }

    pub fn from_str(s: &str) -> Lang {
        if s.eq_ignore_ascii_case("en") {
            Lang::En
        } else {
            Lang::Zh
        }
    }

    pub fn toggle(&self) -> Lang {
        match self {
            Lang::Zh => Lang::En,
            Lang::En => Lang::Zh,
        }
    }
}

static LANG: AtomicU8 = AtomicU8::new(0);

pub fn set(lang: Lang) {
    LANG.store(
        match lang {
            Lang::Zh => 0,
            Lang::En => 1,
        },
        Ordering::Relaxed,
    );
}

pub fn lang() -> Lang {
    if LANG.load(Ordering::Relaxed) == 1 {
        Lang::En
    } else {
        Lang::Zh
    }
}

/// 中文原文 → 英文。
pub const TABLE: &[(&str, &str)] = &[
    ("打开…", "Open…"),
    ("关闭", "Close"),
    ("树", "Tree"),
    ("引用图", "Graph"),
    ("横向缩进", "Indent"),
    ("纵向分层", "Layered"),
    ("图设置", "Graph settings"),
    ("辅助节点", "Auxiliary nodes"),
    ("只读", "Read-only"),
    ("撤销", "Undo"),
    ("重做", "Redo"),
    ("搜索节点", "Search nodes"),
    ("名字", "Name"),
    ("值", "Value"),
    ("类型", "Kind"),
    ("搜索", "Search"),
    ("校验", "Validate"),
    ("取消", "Cancel"),
    ("导出 ▾", "Export ▾"),
    ("合并", "Compact"),
    ("释放缓存", "Free cache"),
    ("节点", "Node"),
    ("未选中节点", "No node selected"),
    ("最近打开", "Recent"),
    ("编号", "ID"),
    ("在此节点下新增", "Add under this node"),
    ("＋ 新增子节点", "＋ Add child"),
    ("删除（置空）", "Delete (blank)"),
    ("保存改值", "Save value"),
    ("保存改名", "Save name"),
    ("聚焦此子树", "Focus subtree"),
    ("取消聚焦", "Unfocus"),
    ("导出", "Export"),
    ("导出为文件", "Export to file"),
    ("文本预览", "Text preview"),
    ("知道了", "OK"),
    ("配色", "Colors"),
    ("暗色", "Dark"),
    ("亮色", "Light"),
    ("重置为默认", "Reset to defaults"),
    ("中文 / EN", "中文 / EN"),
    (
        "打开一个 .xirang 文件开始（⌘O）",
        "Open a .xirang file to start (⌘O)",
    ),
    ("这些文件里没有引用关系（图是空的）", "No references among these files"),
    ("导出的是「折叠视图」（同编号只留最后一条）", "Exports the folded view (last record per ID)"),
    ("工作区", "Workspace"),
    ("当前视图", "Current view"),
    ("选中子树", "Selected subtree"),
    ("完整折叠视图", "Full folded view"),
    ("外观", "Appearance"),
    ("背景", "Background"),
    ("前景", "Foreground"),
    ("强调色", "Accent"),
    ("辅助节点色", "Auxiliary color"),
    ("边框", "Border"),
    ("视图", "View"),
    ("编辑", "Edit"),
    ("文件", "File"),
    ("力", "Forces"),
    ("显示", "Display"),
    ("图谱向心力", "Center force"),
    ("节点间的排斥力", "Repel force"),
    ("相连节点间的吸引力", "Link force"),
    ("连线长度", "Link distance"),
    ("文字淡入阈值", "Text fade threshold"),
    ("节点大小", "Node size"),
    ("连线粗细", "Line width"),
    ("显示箭头（放大后）", "Arrows (when zoomed in)"),
    ("生长动画", "Animate growth"),
    ("显示孤立节点", "Show orphans"),
    ("文字淡出", "Text fade"),
    ("箭头", "Arrows"),
    ("颜色分组", "Color groups"),
    ("标签", "Tags"),
    ("附件", "Attachments"),
    ("孤立节点", "Orphans"),
    ("隐藏未解析", "Hide unresolved"),
    ("局部图谱", "Local graph"),
    ("重置视图", "Reset view"),
    ("图已按上限截断", "Graph truncated at the limit"),
];

/// 取文案：表里有就用英文，没有就原样返回中文。
pub fn t(zh: &'static str) -> &'static str {
    if lang() == Lang::Zh {
        return zh;
    }
    for (k, v) in TABLE {
        if *k == zh {
            return v;
        }
    }
    zh
}

/// 表里全部的键（供测试对拍"代码里用到的串都翻了"）。
pub fn keys() -> Vec<&'static str> {
    TABLE.iter().map(|(k, _)| *k).collect()
}
