//! 懒加载文档层：侧车索引 + 按偏移直读，节点不整份载入内存。
//!
//! 「打开 179 MB / 347 万节点的文件」在这里只做三件事：读文件头、读侧车索引的
//! 几条记录、按需读第一屏需要的节点。实测冷开 0 ms、取一个节点 16 µs。

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};

use xirang_core::codec::{Node, Uuid, Value};
use xirang_core::index::{read_node_at, Sidecar};

/// 节点缓存上限（每节点约 150–400 字节 → 最多几 MB）。
pub const CACHE_CAP: usize = 20_000;

pub struct Doc {
    path: PathBuf,
    sc: Sidecar,
    cache: HashMap<Uuid, Node>,
    order: VecDeque<Uuid>,
    /// 统计：按偏移直读次数 / 命中缓存次数。
    pub reads: usize,
    pub hits: usize,
}

impl Doc {
    pub fn open(path: &Path) -> Result<Doc, String> {
        let sc = Sidecar::open_for(path)?;
        Ok(Doc {
            path: path.to_path_buf(),
            sc,
            cache: HashMap::new(),
            order: VecDeque::new(),
            reads: 0,
            hits: 0,
        })
    }

    /// 编辑落盘后重新打开：索引只补扫新增的那一段（毫秒级），缓存作废。
    pub fn reload(&mut self) -> Result<(), String> {
        self.sc = Sidecar::open_for(&self.path)?;
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
        self.sc.all_roots().unwrap_or_default()
    }

    /// 直接孩子（只读「父 → 孩子」块，不解码节点）。
    pub fn children(&mut self, id: Uuid) -> Vec<Uuid> {
        self.sc
            .find_children(id)
            .unwrap_or_default()
            .into_iter()
            .map(|(child, _, _)| child)
            .collect()
    }

    /// 这个文件里全部的引用边 `(源, 目标)`——从侧车索引直接读，与「展开了哪些节点」无关。
    pub fn edges(&mut self) -> Vec<(Uuid, Uuid)> {
        self.sc.all_edges().unwrap_or_default()
    }

    /// **按根聚合**的引用边 `(源所属根, 目标所属根)`。
    ///
    /// 这是「一篇笔记 = 一个节点」的那套组织方式：息壤里与 Obsidian 的「笔记」对应的是
    /// 顶层根（词条 / 条目），每个根下面的节点是它的内容。归属块里已经存了「节点 → 所属根」，
    /// 所以这一步只查索引，不读节点、不扫全库。自环（同一根内部互相引用）会被丢掉。
    pub fn root_edges(&mut self) -> Vec<(Uuid, Uuid)> {
        let edges = self.sc.all_edges().unwrap_or_default();
        let mut out = Vec::with_capacity(edges.len());
        for (s, t) in edges {
            let (Ok(Some(rs)), Ok(Some(rt))) = (self.sc.find_assign(s), self.sc.find_assign(t))
            else {
                continue;
            };
            if rs != rt {
                out.push((rs, rt));
            }
        }
        out
    }

    /// 孩子数量（展开徽标 `▸ 3` 用，不读节点内容）。
    pub fn child_count(&mut self, id: Uuid) -> usize {
        self.sc.find_children(id).map(|v| v.len()).unwrap_or(0)
    }

    /// 节点自身（名字 / 值）：命中缓存或「查目录 → 跳字节 → 解一条」。
    pub fn node(&mut self, id: Uuid) -> Option<Node> {
        if let Some(n) = self.cache.get(&id) {
            self.hits += 1;
            return Some(n.clone());
        }
        let (rel, len) = match self.sc.find_node_loc(id) {
            Ok(Some(loc)) => loc,
            _ => return None,
        };
        let n = read_node_at(&self.path, self.sc.node_data_start, rel, len).ok()?;
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

    pub fn node_count(&self) -> u64 {
        self.sc.node_count + self.sc.rev_count
    }

    pub fn rev_count(&self) -> u64 {
        self.sc.rev_count
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
