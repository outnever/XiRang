//! 懒加载文档层：走 **CLI 的统一索引机制**（`xirang_core::index`），节点不整份载入内存。
//!
//! 索引模式由 `XIRANG_INDEX_MODE` 决定（默认「工作区总账」，`sidecar` 则走侧车），
//! 桌面端不再自己挑后端——这样它和 CLI 读写的就是同一份索引文件。
//! 查询一律「按点 / 按邻域」，带预算；不做无界的全量枚举（屏幕装不下）。

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};

use xirang_core::codec::{Node, Uuid, Value};
use xirang_core::index::{Budget, LazyWorkspace};

/// 节点缓存上限（每节点约 150–400 字节 → 最多几 MB）。
pub const CACHE_CAP: usize = 20_000;
/// 图视图的默认预算：节点 300 / 边 800（超出截断并提示）。
pub const GRAPH_BUDGET: Budget = Budget {
    max_nodes: 300,
    max_edges: 800,
};

pub struct Doc {
    path: PathBuf,
    /// 传给索引机制的路径（与台账里记录的一致）
    key: String,
    ws: LazyWorkspace,
    cache: HashMap<Uuid, Node>,
    order: VecDeque<Uuid>,
    /// 统计：按偏移直读次数 / 命中缓存次数。
    pub reads: usize,
    pub hits: usize,
    /// 最近一次查询是否因预算被截断（界面据此提示）
    pub truncated: bool,
}

impl Doc {
    pub fn open(path: &Path) -> Result<Doc, String> {
        let key = path.display().to_string();
        let ws = LazyWorkspace::from_paths(std::slice::from_ref(&key))?;
        Ok(Doc {
            path: path.to_path_buf(),
            key,
            ws,
            cache: HashMap::new(),
            order: VecDeque::new(),
            reads: 0,
            hits: 0,
            truncated: false,
        })
    }

    /// 当前用的是哪套索引（workspace / sidecar / memory）。
    pub fn mode(&self) -> &'static str {
        self.ws.backend_kind()
    }

    /// 索引没覆盖这个文件时的降级原因（正常返回 None）。
    pub fn fallback_reason(&self) -> Option<String> {
        self.ws.fallback_reason().map(|s| s.to_string())
    }

    /// 编辑落盘后重新打开：索引跟着数据走，缓存作废。
    pub fn reload(&mut self) -> Result<(), String> {
        self.ws = LazyWorkspace::from_paths(std::slice::from_ref(&self.key))?;
        self.clear_cache();
        Ok(())
    }

    /// 释放节点缓存（离开文件 / 切视图 / 空闲时调用）。
    pub fn clear_cache(&mut self) {
        self.cache.clear();
        self.cache.shrink_to_fit();
        self.order.clear();
        self.order.shrink_to_fit();
    }

    /// 只保留最近 `keep` 个节点（空闲时把缓存压小，内存及时还回去）。
    pub fn trim_cache(&mut self, keep: usize) {
        while self.order.len() > keep {
            if let Some(old) = self.order.pop_front() {
                self.cache.remove(&old);
            }
        }
        if keep == 0 {
            self.cache.shrink_to_fit();
            self.order.shrink_to_fit();
        }
    }

    /// 节点缓存的估算占用（每节点约 320 字节：UUID + 名字 + 值 + HashMap 开销）。
    pub fn cache_bytes(&self) -> usize {
        self.cache.len() * 320
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 顶层根（只读索引，不解码节点）。
    pub fn roots(&mut self) -> Vec<Uuid> {
        self.ws.roots()
    }

    /// 直接孩子（跨文件取并集——「同编号多文件」是息壤的正常语义）。
    pub fn children(&mut self, id: Uuid) -> Vec<Uuid> {
        self.ws
            .children_union(id)
            .into_iter()
            .map(|(_, n)| n.id)
            .collect()
    }

    /// 孩子数量（只数索引条目，不读节点字节）。
    pub fn child_count(&mut self, id: Uuid) -> usize {
        self.ws.child_count(id)
    }

    /// 一组节点**内部**的边（集合外的不返回）——界面画"可见的线"就用它。
    pub fn edges_among(&mut self, ids: &[Uuid], budget: Budget) -> Vec<(Uuid, Uuid)> {
        let (edges, truncated) = self.ws.edges_among(ids, budget);
        self.truncated |= truncated;
        edges
    }

    /// 以某个节点为中心的邻域（带预算）。
    pub fn neighbors(&mut self, id: Uuid, depth: usize, budget: Budget) -> Vec<(Uuid, Uuid)> {
        let n = self.ws.neighbors(id, depth, budget);
        self.truncated |= n.truncated;
        n.edges
    }

    /// 预览用：这个文件里全部的引用边（**有界**：只在顶层根 + 它们的孩子之间找）。
    ///
    /// 注意这不再是无界枚举：屏幕装不下，索引层也不提供无界版本。
    pub fn edges(&mut self) -> Vec<(Uuid, Uuid)> {
        let edges = self.ws.all_edges(GRAPH_BUDGET.max_edges);
        self.truncated |= edges.len() >= GRAPH_BUDGET.max_edges;
        edges
    }

    /// **按根聚合**的引用边（根 → 根）：一篇笔记 = 一个顶层根。
    ///
    /// 靠索引里的「节点 → 所属根」把每条引用边折算到根上；枚举**有界**
    /// （默认 800 条，超出截断并置 `truncated`）。
    pub fn root_edges(&mut self) -> Vec<(Uuid, Uuid)> {
        let roots = self.roots();
        let (edges, truncated) = self.ws.root_edges(&roots, GRAPH_BUDGET.max_edges);
        self.truncated |= truncated;
        edges
    }

    /// 图视图的候选集合：`by_root` = 只用顶层根；否则再加上它们的直接孩子。
    /// 两者都有界（受 `GRAPH_BUDGET` 约束），不再"整库枚举"。
    pub fn graph_sets(&mut self, by_root: bool) -> (Vec<Uuid>, bool) {
        let mut ids = self.roots();
        let mut truncated = ids.len() > GRAPH_BUDGET.max_nodes;
        ids.truncate(GRAPH_BUDGET.max_nodes);
        if !by_root {
            let roots = ids.clone();
            for r in roots {
                for c in self.children(r) {
                    if ids.len() >= GRAPH_BUDGET.max_nodes {
                        truncated = true;
                        break;
                    }
                    ids.push(c);
                }
            }
        }
        self.truncated |= truncated;
        (ids, truncated)
    }

    /// 节点自身（名字 / 值）：命中缓存或「查目录 → 跳字节 → 解一条」。
    pub fn node(&mut self, id: Uuid) -> Option<Node> {
        if let Some(n) = self.cache.get(&id) {
            self.hits += 1;
            return Some(n.clone());
        }
        let (_, n) = self.ws.find(id)?;
        self.reads += 1;
        self.insert(n.clone());
        Some(n)
    }

    fn insert(&mut self, n: Node) {
        if self.cache.insert(n.id, n.clone()).is_none() {
            self.order.push_back(n.id);
        }
        while self.order.len() > CACHE_CAP {
            if let Some(old) = self.order.pop_front() {
                self.cache.remove(&old);
            }
        }
    }

    /// 引用的目标名（找不到就退回短编号）。
    pub fn reference_label(&mut self, target: Uuid) -> String {
        match self.node(target) {
            Some(n) if !n.name.is_empty() => format!("→ {}", n.name),
            _ => format!("→ {}", short(target)),
        }
    }

    /// 节点总数：侧车模式能直接给出；工作区总账不存总数 → None（界面显示"—"）。
    pub fn node_count(&self) -> Option<u64> {
        None
    }

    /// 本文件的修订条数：只有侧车后端知道；总账模式下为 None。
    pub fn rev_count(&self) -> Option<u64> {
        None
    }

    pub fn cache_len(&self) -> usize {
        self.cache.len()
    }
}

/// 编号前 8 位（状态栏 / 兜底显示用）。
pub fn short(id: Uuid) -> String {
    id.to_string().chars().take(8).collect()
}

/// 7 大类值的显示文本（与 CLI 一致）。
pub fn value_text(doc: &mut Doc, node: &Node) -> String {
    match &node.value {
        Value::Empty => String::new(),
        Value::Int(n) => n.to_string(),
        Value::Float(f) => f.to_string(),
        Value::Bool(b) => (if *b { "true" } else { "false" }).to_string(),
        Value::Text(s) => s.clone(),
        Value::Reference(t) => doc.reference_label(*t),
        Value::Blob(b) => format!("[blob {} 字节]", b.len()),
    }
}

/// 可编辑文本：引用显示目标编号（而不是名字），blob 显示字节数。
pub fn plain_value(v: &Value) -> String {
    match v {
        Value::Empty => String::new(),
        Value::Int(n) => n.to_string(),
        Value::Float(f) => f.to_string(),
        Value::Bool(b) => (if *b { "true" } else { "false" }).to_string(),
        Value::Text(s) => s.clone(),
        Value::Reference(t) => t.to_string(),
        Value::Blob(b) => format!("[blob {} 字节]", b.len()),
    }
}
