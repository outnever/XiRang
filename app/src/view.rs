//! 视图模型：把懒加载文档摊平成「可渲染的行」，并计算两种树布局的缩进。
//!
//! 这里刻意不依赖 egui：树 / 图的取数逻辑都能在没有界面的情况下测试。

use std::collections::HashSet;

use xirang_core::codec::Uuid;

use crate::lazy::Doc;

/// 树布局方向。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Layout {
    /// 横向缩进（CLI 风）：每层右移一个固定步长，父子间用 `├─`/`└─` 引导线。
    Indent,
    /// 纵向分层：按层给缩进 + 竖线，适合宽屏铺开看层级。
    Layered,
}

impl Default for Layout {
    fn default() -> Self {
        Layout::Indent
    }
}

/// 渲染一行所需的一切（虚拟化列表只渲染可见的几十行）。
#[derive(Clone, Debug)]
pub struct Row {
    pub id: Uuid,
    pub depth: usize,
    pub name: String,
    pub value: String,
    pub is_aux: bool,
    pub child_count: usize,
    pub expanded: bool,
    /// 同级里是不是最后一个（画 `└─` 还是 `├─`）。
    pub is_last: bool,
    /// 每一层祖先是不是「最后一个」，用来画竖向参考线。
    pub spine: Vec<bool>,
    pub is_root: bool,
}

impl Row {
    pub fn has_children(&self) -> bool {
        self.child_count > 0
    }

    /// 展开标记：`▾` 已展开 · `▸ N` 可展开（N = 孩子数，来自索引，不用先读孩子）· `·` 叶子。
    pub fn twisty(&self) -> String {
        if self.child_count == 0 {
            "·".to_string()
        } else if self.expanded {
            "▾".to_string()
        } else {
            format!("▸ {}", self.child_count)
        }
    }

    /// 两种布局的缩进（像素）。
    pub fn indent(&self, layout: Layout) -> f32 {
        match layout {
            Layout::Indent => self.depth as f32 * 18.0,
            Layout::Layered => self.depth as f32 * 28.0,
        }
    }

    /// CLI 风的引导线前缀（`│  ├─ ` / `│  └─ `）。
    pub fn guide(&self) -> String {
        if self.is_root {
            return String::new();
        }
        let mut s = String::new();
        for last in self.spine.iter().take(self.spine.len().saturating_sub(1)) {
            s.push_str(if *last { "   " } else { "│  " });
        }
        s.push_str(if self.is_last { "└─ " } else { "├─ " });
        s
    }
}

/// 一次摊平的上限（防止「整棵树都展开」把内存吃满）。
pub const MAX_ROWS: usize = 200_000;

/// 从根按「已展开集合」摊平成行列表。只解码要显示名字 / 值的节点。
pub fn flatten(
    doc: &mut Doc,
    expanded: &HashSet<Uuid>,
    show_aux: bool,
    budget: usize,
) -> Vec<Row> {
    let mut rows = Vec::new();
    let roots = doc.roots();
    let last_idx = roots.len().saturating_sub(1);
    for (i, id) in roots.iter().enumerate() {
        walk(doc, *id, 0, i == last_idx, &mut Vec::new(), expanded, show_aux, budget, &mut rows);
        if rows.len() >= budget {
            break;
        }
    }
    rows
}

#[allow(clippy::too_many_arguments)]
fn walk(
    doc: &mut Doc,
    id: Uuid,
    depth: usize,
    is_last: bool,
    spine: &mut Vec<bool>,
    expanded: &HashSet<Uuid>,
    show_aux: bool,
    budget: usize,
    rows: &mut Vec<Row>,
) {
    if rows.len() >= budget {
        return;
    }
    let Some(node) = doc.node(id) else { return };
    let is_aux = node.name.starts_with('@');
    if is_aux && !show_aux {
        return;
    }
    let child_count = doc.child_count(id);
    let open = expanded.contains(&id) && child_count > 0;
    rows.push(Row {
        id,
        depth,
        name: node.name.clone(),
        value: crate::lazy::value_text(doc, &node),
        is_aux,
        child_count,
        expanded: open,
        is_last,
        spine: spine.clone(),
        is_root: depth == 0,
    });
    if !open {
        return;
    }
    let kids = doc.children(id);
    let last = kids.len().saturating_sub(1);
    spine.push(is_last);
    for (i, kid) in kids.iter().enumerate() {
        walk(doc, *kid, depth + 1, i == last, spine, expanded, show_aux, budget, rows);
        if rows.len() >= budget {
            break;
        }
    }
    spine.pop();
}
