//! 树的处理层：把「一批节点」当树 / 图来读写、遍历、寻址。
//!
//! 与 Python `tools/tree.py` 语义对齐：Store（一批节点 + 索引）、
//! children/roots/walk（遍历）、resolve（引用 → 目标）、encode/decode（无头）、
//! save/load（带魔数 + 版本 + 头）。

use std::collections::{HashMap, HashSet};
use std::path::Path;

use chrono::Utc;

use crate::codec::{self, Node, Uuid, Value};

pub const MAGIC: &[u8; 4] = b"XRNG";
pub const FORMAT_VERSION: u8 = 1;
/// 协议标记（辅助节点 `@protocol` 的值）：同一文件里「同编号多记录 = 修订，后写覆盖」。
/// 见 `spec/协议.md` 的 `append-v1`。
pub const PROTOCOL_APPEND: &str = "append-v1";
/// 文件头：纯英文自描述文本，与 README 第 4 节、tools/tree.py 完全一致。
pub const HEADER: &str = include_str!("header.txt");

/// 一批节点 + 索引。树 / 图都只是 Store 里的一堆节点。
#[derive(Default, Clone, Debug)]
pub struct Store {
    nodes: Vec<Node>,           // 追加序（数组 / 顺序靠它）
    index: HashMap<Uuid, usize>, // UUID -> nodes 下标
    /// 父 UUID -> 子节点下标（按追加序）。避免每次 children 都扫全表（P34：O(N²)）。
    children_of: HashMap<Uuid, Vec<usize>>,
}

impl Store {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add(&mut self, node: Node) {
        let i = self.nodes.len();
        if let Some(p) = node.parent {
            self.children_of.entry(p).or_default().push(i);
        }
        self.index.insert(node.id, i);
        self.nodes.push(node);
    }

    /// 重建 UUID 索引与父子索引（删除节点后调用）。
    fn rebuild_index(&mut self) {
        self.index.clear();
        self.children_of.clear();
        for (i, n) in self.nodes.iter().enumerate() {
            self.index.insert(n.id, i);
            if let Some(p) = n.parent {
                self.children_of.entry(p).or_default().push(i);
            }
        }
    }

    pub fn get(&self, id: Uuid) -> Option<&Node> {
        self.index.get(&id).map(|&i| &self.nodes[i])
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// 所有节点，按追加序。
    pub fn nodes(&self) -> &[Node] {
        &self.nodes
    }

    /// 所有根节点（父为 None）。
    pub fn roots(&self) -> Vec<&Node> {
        self.nodes.iter().filter(|n| n.parent.is_none()).collect()
    }

    /// 子节点，按追加序；skip_aux=true 时跳过 `@` 辅助节点。
    pub fn children(&self, node: &Node) -> Vec<&Node> {
        self.children_opt(node, false)
    }

    pub fn children_opt(&self, node: &Node, skip_aux: bool) -> Vec<&Node> {
        match self.children_of.get(&node.id) {
            Some(idx) => idx
                .iter()
                .map(|&i| &self.nodes[i])
                .filter(|n| !skip_aux || !n.name.starts_with('@'))
                .collect(),
            None => Vec::new(),
        }
    }

    /// 按节点名取子节点；找不到返回 None。
    pub fn child_by_name(&self, node: &Node, name: &str) -> Option<&Node> {
        self.children(node).into_iter().find(|c| c.name == name)
    }

    /// 解析引用：值若是引用，返回目标节点。
    pub fn resolve(&self, node: &Node) -> Option<&Node> {
        match &node.value {
            Value::Reference(target) => self.get(*target),
            _ => None,
        }
    }

    /// 父节点；根返回 None。
    pub fn parent(&self, node: &Node) -> Option<&Node> {
        node.parent.and_then(|p| self.get(p))
    }

    /// 改某节点的父（用于子树导出重新扎根）。改完重建父子索引。
    pub fn set_parent(&mut self, id: Uuid, parent: Option<Uuid>) -> Result<(), String> {
        let idx = *self.index.get(&id).ok_or("节点不存在")?;
        self.nodes[idx].parent = parent;
        self.rebuild_index();
        Ok(())
    }

    /// 引用指向该节点的所有节点（谁引用了我）。
    pub fn references_to(&self, node: &Node) -> Vec<&Node> {
        self.nodes
            .iter()
            .filter(|n| matches!(&n.value, Value::Reference(t) if *t == node.id))
            .collect()
    }

    /// 以 node 为根的子树节点（含 node，先根序）。
    pub fn subtree<'a>(&'a self, node: &'a Node) -> Vec<&'a Node> {
        let mut out = Vec::new();
        let mut stack = vec![node];
        let mut seen: HashSet<Uuid> = HashSet::new();
        while let Some(n) = stack.pop() {
            // 父边成环（E006）时兜底：访问过的节点不再展开，避免死循环。
            if !seen.insert(n.id) {
                continue;
            }
            out.push(n);
            let children = self.children(n);
            for c in children.iter().rev() {
                stack.push(c);
            }
        }
        out
    }

    /// 按节点名查找。
    pub fn find_by_name(&self, name: &str) -> Vec<&Node> {
        self.nodes.iter().filter(|n| n.name == name).collect()
    }

    /// 以 node 为根，构建一个只含该子树的 Store（导出子树用）。
    pub fn sub_store<'a>(&'a self, root: &'a Node) -> Store {
        let mut s = Store::new();
        for n in self.subtree(root) {
            s.add(n.clone());
        }
        s
    }

    // —— 写操作（值可原位改 + 留痕）——

    /// 新增节点（自动生成编号）；record_created=true 时挂 @created 时间。
    pub fn create(&mut self, parent: Option<Uuid>, name: &str, value: Value, record_created: bool) -> Node {
        let n = Node {
            id: Uuid::random_v4(),
            parent,
            name: name.into(),
            value,
        };
        self.add(n.clone());
        if record_created {
            self.add_time(&n.id, "@created");
        }
        n
    }

    /// 改：旧值复制进本节点的 @history（快照 + @replaced），原位改值。
    /// 值未变则不动（不写 @history），避免「没改却留痕」。
    pub fn update(&mut self, node_id: Uuid, new_value: Value) -> Result<(), String> {
        let idx = *self.index.get(&node_id).ok_or("节点不存在")?;
        if self.nodes[idx].value == new_value {
            return Ok(()); // 值相同 = 无变化，不记录
        }
        let history = self.ensure_history(node_id);
        let snap = Node {
            id: Uuid::random_v4(),
            parent: Some(history),
            name: self.nodes[idx].name.clone(),
            value: self.nodes[idx].value.clone(),
        };
        self.add(snap.clone());
        self.add_time(&snap.id, "@replaced");
        self.nodes[idx].value = new_value;
        Ok(())
    }

    /// 安静赋值：直接改值，不写 @history（用于批量创建 / 初始数据 / 模板实例化）。
    /// 值相同则不动（无变化）。
    pub fn set_quiet(&mut self, node_id: Uuid, new_value: Value) -> Result<(), String> {
        let idx = *self.index.get(&node_id).ok_or("节点不存在")?;
        if self.nodes[idx].value == new_value {
            return Ok(());
        }
        self.nodes[idx].value = new_value;
        Ok(())
    }

    /// 安静改名：直接改名字，不写 @history（用于批量创建 / 初始数据）。
    /// 名字相同则不动（无变化）；超过 255 字节报错。
    pub fn rename_quiet(&mut self, node_id: Uuid, new_name: String) -> Result<(), String> {
        if new_name.as_bytes().len() > 255 {
            return Err("名字超 255 字节".into());
        }
        let idx = *self.index.get(&node_id).ok_or("节点不存在")?;
        if self.nodes[idx].name == new_name {
            return Ok(());
        }
        self.nodes[idx].name = new_name;
        Ok(())
    }

    /// 回滚：直接把节点的名字 + 值恢复为指定内容（不回写 @history）。
    pub fn restore(&mut self, node_id: Uuid, name: String, value: Value) -> Result<(), String> {
        let idx = *self.index.get(&node_id).ok_or("节点不存在")?;
        self.nodes[idx].name = name;
        self.nodes[idx].value = value;
        Ok(())
    }

    /// 撤销：把节点恢复为 @history 里最近一次快照的名字 + 值。
    pub fn revert(&mut self, node_id: Uuid) -> Result<(), String> {
        let idx = *self.index.get(&node_id).ok_or("节点不存在")?;
        let hist_id = match self
            .nodes
            .iter()
            .find(|n| n.parent == Some(node_id) && n.name == "@history")
            .map(|n| n.id)
        {
            Some(h) => h,
            None => return Err("无 @history，无法撤销".into()),
        };
        let (name, value) = {
            let last = self
                .nodes
                .iter()
                .filter(|n| n.parent == Some(hist_id))
                .last()
                .ok_or("@history 为空")?;
            (last.name.clone(), last.value.clone())
        };
        // 回滚本身也留痕：把当前状态快照进 @history（+@replaced），这样「每次修改都留痕」对回滚也成立。
        let cur_name = self.nodes[idx].name.clone();
        let cur_value = self.nodes[idx].value.clone();
        let snap = Node {
            id: Uuid::random_v4(),
            parent: Some(hist_id),
            name: cur_name,
            value: cur_value,
        };
        self.add(snap.clone());
        self.add_time(&snap.id, "@replaced");
        self.nodes[idx].name = name;
        self.nodes[idx].value = value;
        Ok(())
    }

    /// 改名：旧名字 + 值进 @history（快照 + @replaced），原位改名字。
    pub fn rename(&mut self, node_id: Uuid, new_name: String) -> Result<(), String> {
        if new_name.as_bytes().len() > 255 {
            return Err("名字超 255 字节".into());
        }
        let idx = *self.index.get(&node_id).ok_or("节点不存在")?;
        if self.nodes[idx].name == new_name {
            return Ok(()); // 同名 = 无变化，不记录
        }
        let history = self.ensure_history(node_id);
        let snap = Node {
            id: Uuid::random_v4(),
            parent: Some(history),
            name: self.nodes[idx].name.clone(),
            value: self.nodes[idx].value.clone(),
        };
        self.add(snap.clone());
        self.add_time(&snap.id, "@replaced");
        self.nodes[idx].name = new_name;
        Ok(())
    }

    /// 删：除「编号」外的字段（名字 + 值）置空，旧值进 @history（快照 + @replaced）。
    /// 节点本就是空（无名字 + 空值）则不动，避免「删了个空节点却留痕」。
    pub fn remove(&mut self, node_id: Uuid) -> Result<(), String> {
        let idx = *self.index.get(&node_id).ok_or("节点不存在")?;
        if self.nodes[idx].name.is_empty() && self.nodes[idx].value == Value::Empty {
            return Ok(()); // 已是空节点，没得删，不记录
        }
        let history = self.ensure_history(node_id);
        let snap = Node {
            id: Uuid::random_v4(),
            parent: Some(history),
            name: self.nodes[idx].name.clone(),
            value: self.nodes[idx].value.clone(),
        };
        self.add(snap.clone());
        self.add_time(&snap.id, "@replaced");
        self.nodes[idx].name = String::new();
        self.nodes[idx].value = Value::Empty;
        Ok(())
    }

    /// 该节点是否为「模板定义根」：挂了 `@模板`（值 = 空）辅助节点。
    pub fn is_template_root(&self, node: &Node) -> bool {
        self.child_by_name(node, "@模板")
            .map(|c| c.value == Value::Empty)
            .unwrap_or(false)
    }

    /// 该节点是否为「实例根」：挂了 `@实例` 辅助节点。
    pub fn is_instance_root(&self, node: &Node) -> bool {
        self.child_by_name(node, "@实例").is_some()
    }

    /// 实例根指向的模板根：`@模板` 值 = 引用；非引用值（如空 = 模板标记）视为「是模板」而非来源。
    pub fn template_ref(&self, node: &Node) -> Option<Uuid> {
        match self.child_by_name(node, "@模板") {
            Some(c) => match c.value {
                Value::Reference(u) => Some(u),
                _ => None,
            },
            None => None,
        }
    }

    /// 编辑规则（注释式，无容器例外）：向上找「最近的一个有标注的根」——
    /// 是模板定义根（`@模板` 空）→ 受保护；是实例根（`@实例`）或其它 → 可编辑。
    pub fn is_editable(&self, node: &Node) -> bool {
        let mut cur = Some(node);
        while let Some(n) = cur {
            if self.is_template_root(n) {
                return false; // 模板定义 → 受保护
            }
            if self.is_instance_root(n) {
                return true; // 实例 → 可编辑
            }
            cur = n.parent.and_then(|p| self.get(p));
        }
        true
    }

    /// 复制：把以 root_id 为根的子树复制为 new_parent 下的新节点树，返回新根编号。
    /// 每个节点都换新 UUID（身份全新）；子树内的引用重指到副本，子树外的引用保持指向原目标；
    /// 是否清空标量值、是否保留 @history 由 opts 决定。
    pub fn copy_subtree(
        &mut self,
        root_id: Uuid,
        new_parent: Option<Uuid>,
        opts: &CopyOptions,
    ) -> Result<Uuid, String> {
        // 取根 + 子树为自有数据，避免与 &mut self 的借用冲突。
        let root = self
            .get(root_id)
            .cloned()
            .ok_or("根节点不存在".to_string())?;
        let subtree: Vec<Node> = self.subtree(&root).into_iter().cloned().collect();
        // history=false 时，跳过所有 @history 节点及其后代（骨架不带编辑日志）。
        let mut excluded: HashSet<Uuid> = HashSet::new();
        if !opts.history {
            let hist_roots: Vec<Uuid> = subtree
                .iter()
                .filter(|n| n.name == "@history")
                .map(|n| n.id)
                .collect();
            for hid in hist_roots {
                excluded.insert(hid);
                let mut stack = vec![hid];
                while let Some(p) = stack.pop() {
                    for n in &subtree {
                        if n.parent == Some(p) && !excluded.contains(&n.id) {
                            excluded.insert(n.id);
                            stack.push(n.id);
                        }
                    }
                }
            }
        }
        let mut id_map: HashMap<Uuid, Uuid> = HashMap::new();
        // pass1：建节点（subtree 为先根序，父先于子，父的映射已就绪）。
        for n in &subtree {
            if excluded.contains(&n.id) {
                continue;
            }
            let new_id = Uuid::random_v4();
            let new_value = match &n.value {
                Value::Reference(_) => n.value.clone(), // 引用保留，pass2 统一重指
                _ if opts.blank_values => Value::Empty,
                _ => n.value.clone(),
            };
            let parent = if n.id == root_id {
                new_parent
            } else {
                n.parent.and_then(|p| id_map.get(&p).copied())
            };
            self.add(Node {
                id: new_id,
                parent,
                name: n.name.clone(),
                value: new_value,
            });
            id_map.insert(n.id, new_id);
        }
        // pass2：引用重指（内部 → 副本；外部 → 保持原目标）。
        for n in &subtree {
            if excluded.contains(&n.id) {
                continue;
            }
            if let Value::Reference(t) = &n.value {
                let new_id = id_map[&n.id];
                let new_target = id_map.get(t).copied().unwrap_or(*t);
                let idx = *self.index.get(&new_id).ok_or("复制内部错误")?;
                self.nodes[idx].value = Value::Reference(new_target);
            }
        }
        id_map.get(&root_id).copied().ok_or("复制失败：根节点未复制".into())
    }

    /// 整棵子树从库中移除（含根，真正的结构删除）。用于管理层（如删除模板定义）。
    /// 不同于 `remove`（只清空名字/值、留原位）。移除后重建索引。
    pub fn remove_subtree(&mut self, root_id: Uuid) -> Result<(), String> {
        let root = self.get(root_id).cloned().ok_or("根节点不存在")?;
        let to_remove: HashSet<Uuid> = self.subtree(&root).into_iter().map(|n| n.id).collect();
        self.nodes.retain(|n| !to_remove.contains(&n.id));
        self.rebuild_index();
        Ok(())
    }

    /// 裁剪某个节点的 `@history` 留痕：保留最近的 `keep` 条，并（可选）只裁掉早于 `before` 的。
    ///
    /// `before` 是 ISO 时间前缀（如 `2026-01-01` 或 `2026-09-23T22:00`），与快照上的
    /// `@replaced` 做字符串比较——同一时间格式下，字符串序就是时间序。
    /// 被裁掉的快照是**真正的结构删除**（它们只用于回滚）；`@history` 节点本身保留。
    pub fn prune_history(
        &mut self,
        node_id: Uuid,
        keep: usize,
        before: Option<&str>,
    ) -> Result<PruneReport, String> {
        let node = self.get(node_id).cloned().ok_or("节点不存在")?;
        let hist = match self.child_by_name(&node, "@history") {
            Some(h) => h.clone(),
            None => {
                return Ok(PruneReport {
                    removed: 0,
                    kept: 0,
                    before_nodes: self.nodes.len(),
                    after_nodes: self.nodes.len(),
                })
            }
        };
        let snaps: Vec<Node> = self.children(&hist).into_iter().cloned().collect();
        let total = snaps.len();
        let keep_from = total.saturating_sub(keep);
        let mut doomed: Vec<Uuid> = Vec::new();
        for (i, s) in snaps.iter().enumerate() {
            let old_enough = match before {
                None => true,
                Some(cut) => match self.child_by_name(s, "@replaced") {
                    Some(r) => match &r.value {
                        Value::Text(t) => t.as_str() < cut,
                        _ => false,
                    },
                    None => false, // 没有 @replaced 的快照保守留下
                },
            };
            if i < keep_from && old_enough {
                doomed.push(s.id);
            }
        }
        let before_nodes = self.nodes.len();
        for id in &doomed {
            self.remove_subtree(*id)?;
        }
        Ok(PruneReport {
            removed: doomed.len(),
            kept: total - doomed.len(),
            before_nodes,
            after_nodes: self.nodes.len(),
        })
    }

    /// 确保 node_id 下有 @history 辅助节点，返回其编号；没有则建。
    fn ensure_history(&mut self, node_id: Uuid) -> Uuid {
        if let Some(h) = self
            .nodes
            .iter()
            .find(|n| n.parent == Some(node_id) && n.name == "@history")
        {
            return h.id;
        }
        self.create(Some(node_id), "@history", Value::Empty, false).id
    }

    /// 给 node_id 挂一个时间辅助节点（@created / @replaced）。
    fn add_time(&mut self, node_id: &Uuid, name: &str) {
        let t = Node {
            id: Uuid::random_v4(),
            parent: Some(*node_id),
            name: name.into(),
            value: Value::Text(Utc::now().to_rfc3339()),
        };
        self.add(t);
    }

    // —— 落盘 ——

    /// 整库编码成一串字节（节点逐个接起来，无头）。
    /// 整库编码。节点名超过 255 字节（F009）等编码错误会返回 Err，不再 panic。
    pub fn encode(&self) -> Result<Vec<u8>, String> {
        let mut out = Vec::new();
        for n in &self.nodes {
            match codec::encode_node(n) {
                Ok(b) => out.extend_from_slice(&b),
                Err(codec::Error::NameTooLong(len)) => {
                    return Err(format!("F009：节点名超长（{len} 字节，上限 255）"));
                }
                Err(e) => return Err(format!("节点编码失败（{e:?}）")),
            }
        }
        Ok(out)
    }

    /// 从字节重建 Store（无头）。
    pub fn decode(data: &[u8]) -> Result<Store, codec::Error> {
        let mut store = Store::new();
        let mut off = 0;
        while off < data.len() {
            let node = codec::decode_node(data, &mut off)?;
            store.add(node);
        }
        Ok(store)
    }

    /// 落盘：写「文件 = 魔数 + 版本 + 头 + 节点字节」。
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let nodes_bytes = self
            .encode()
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        let bytes = make_file(&nodes_bytes);
        // 原子写：先在同目录写临时文件，再改名覆盖。
        // 「整份重写」（保存 / 压实）写到一半崩掉时，原文件不会被写坏。
        let mut tmp = path.to_path_buf();
        let name = format!(
            "{}.tmp{}",
            path.file_name().and_then(|n| n.to_str()).unwrap_or("xirang"),
            std::process::id()
        );
        tmp.set_file_name(name);
        std::fs::write(&tmp, &bytes)?;
        std::fs::rename(&tmp, path)?;
        // 写穿 sidecar 索引（实现层缓存，失败静默，可重建）。
        crate::index::write_for(path, &bytes);
        Ok(())
    }

    /// 读回：剥掉魔数 + 版本 + 头，再解析节点。
    pub fn load(path: &Path) -> Result<Store, String> {
        let data = std::fs::read(path).map_err(|e| e.to_string())?;
        let nodes = parse_file(&data)?;
        Store::decode(nodes).map_err(codec_error)
    }

    /// 读回「折叠视图」：同编号只留最后一条记录（后写覆盖），供 `append-v1` 文件使用。
    /// 没有重复编号的文件与 `load` 结果完全一致。
    pub fn load_view(path: &Path) -> Result<Store, String> {
        Ok(fold(&Store::load(path)?))
    }

    /// 该节点沿父边向上找到的根（原始记录视角；父链断裂或成环时回到自身）。
    pub fn root_of(&self, id: Uuid) -> Option<Uuid> {
        let mut cur = id;
        let mut guard = 0usize;
        loop {
            let n = self.get(cur)?;
            match n.parent {
                None => return Some(n.id),
                Some(p) => {
                    if p == n.id || !self.index.contains_key(&p) {
                        return Some(n.id);
                    }
                    guard += 1;
                    if guard > 1_000_000 {
                        return Some(n.id);
                    }
                    cur = p;
                }
            }
        }
    }

    /// 根下是否挂了 `@protocol = <name>`（协议声明靠内容探测，不依赖侧车索引）。
    pub fn declares_protocol(&self, root: Uuid, name: &str) -> bool {
        match self.get(root) {
            Some(r) => self
                .children(r)
                .into_iter()
                .any(|c| c.name == "@protocol" && c.value == Value::Text(name.to_string())),
            None => false,
        }
    }

    /// 全部声明了 `@protocol = <name>` 的根。
    pub fn protocol_roots(&self, name: &str) -> Vec<Uuid> {
        self.roots()
            .into_iter()
            .filter(|r| self.declares_protocol(r.id, name))
            .map(|r| r.id)
            .collect()
    }
}

/// 折叠：同一编号只留最后一条记录（后写覆盖），空名空值 = 空槽位（保留）。
/// 输出按「首次出现位置」排序，保证子节点顺序稳定。
pub fn fold(store: &Store) -> Store {
    let mut first: HashMap<Uuid, usize> = HashMap::new();
    let mut current: HashMap<Uuid, Node> = HashMap::new();
    for (i, n) in store.nodes().iter().enumerate() {
        first.entry(n.id).or_insert(i);
        current.insert(n.id, n.clone());
    }
    let mut items: Vec<(usize, Node)> =
        current.into_iter().map(|(id, n)| (first[&id], n)).collect();
    items.sort_by_key(|(p, _)| *p);
    let mut out = Store::new();
    for (_, n) in items {
        out.add(n);
    }
    out
}

/// 节点数据段在文件里的绝对起始偏移（魔数 4 + 版本 1 + 头长 4 + 头文本）。
pub fn node_data_start(data: &[u8]) -> Result<u64, String> {
    if data.len() < 9 {
        return Err("F004：文件截断（头部不完整）".into());
    }
    if &data[..4] != MAGIC {
        return Err("F001：魔数非法（不是 XRNG 息壤文件）".into());
    }
    let n = u32::from_be_bytes(data[5..9].try_into().unwrap()) as u64;
    if 9 + n > data.len() as u64 {
        return Err("F003：头长非法".into());
    }
    Ok(9 + n)
}

/// 裁剪留痕的结果。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PruneReport {
    pub removed: usize,
    pub kept: usize,
    pub before_nodes: usize,
    pub after_nodes: usize,
}

/// 复制子树的选项。
#[derive(Default, Clone, Debug)]
pub struct CopyOptions {
    /// true = 清空标量值（整数/浮点/布尔/文本/二进制块 → 空）做骨架；
    /// 引用边保留（内部重指到副本、外部保持指向原目标）。
    pub blank_values: bool,
    /// true = 连同 @history 留痕子树一起复制（克隆实例）；false = 丢弃（骨架/实例化）。
    pub history: bool,
}

/// 把编解码错误映射成带错误码的信息（不再把所有错误都说成 F004）。
pub fn codec_error(e: codec::Error) -> String {
    match e {
        codec::Error::Truncated => "F004：文件截断（节点流在节点中间结束）".to_string(),
        codec::Error::InvalidTag(t) => format!("F010：类型标记非法（{t}）"),
        codec::Error::InvalidUtf8 => "F011：文本非法 UTF-8".to_string(),
        codec::Error::NameTooLong(n) => format!("F009：节点名超长（{n} 字节，上限 255）"),
    }
}

/// 把「纯节点字节」包成「文件字节」：[魔数][版本][头长][头文本][节点字节]。
pub fn make_file(nodes_bytes: &[u8]) -> Vec<u8> {
    let header = HEADER.as_bytes();
    let mut out = Vec::with_capacity(9 + header.len() + nodes_bytes.len());
    out.extend_from_slice(MAGIC);
    out.push(FORMAT_VERSION);
    out.extend_from_slice(&(header.len() as u32).to_be_bytes());
    out.extend_from_slice(header);
    out.extend_from_slice(nodes_bytes);
    out
}

/// 读文件头，返回 (format_version, header_text)。非法则报 F 码。
pub fn read_header(data: &[u8]) -> Result<(u8, String), String> {
    // 先判长度再判魔数：4 字节的 "XRNG" 魔数其实合法，只是文件太短。
    if data.len() < 4 {
        return Err("F004：文件截断（不足 4 字节）".into());
    }
    if &data[..4] != MAGIC {
        return Err("F001：魔数非法（不是 XRNG 息壤文件）".into());
    }
    if data.len() < 9 {
        return Err("F003：文件头不完整（不足 9 字节）".into());
    }
    let version = data[4];
    if version != FORMAT_VERSION {
        return Err(format!("F002：格式版本不支持（{version}）"));
    }
    let n = u32::from_be_bytes(data[5..9].try_into().unwrap()) as usize;
    if 9 + n > data.len() {
        return Err("F003：头长非法".into());
    }
    let header =
        std::str::from_utf8(&data[9..9 + n]).map_err(|_| "头文本非法 UTF-8".to_string())?;
    Ok((version, header.to_string()))
}

/// 从「文件字节」里剥掉魔数 + 版本 + 头，返回「纯节点字节」；非法则报 F 码。
pub fn parse_file(data: &[u8]) -> Result<&[u8], String> {
    if data.len() < 4 {
        return Err("F004：文件截断（不足 4 字节）".into());
    }
    if &data[..4] != MAGIC {
        return Err("F001：魔数非法（不是 XRNG 息壤文件）".into());
    }
    if data.len() < 9 {
        return Err("F003：文件头不完整（不足 9 字节）".into());
    }
    let version = data[4];
    if version != FORMAT_VERSION {
        return Err(format!("F002：格式版本不支持（{version}）"));
    }
    let n = u32::from_be_bytes(data[5..9].try_into().unwrap()) as usize;
    if 9 + n > data.len() {
        return Err("F003：头长非法".into());
    }
    Ok(&data[9 + n..])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n(id: Uuid, parent: Option<Uuid>, name: &str, value: Value) -> Node {
        Node {
            id,
            parent,
            name: name.into(),
            value,
        }
    }

    fn build() -> (Store, [Uuid; 4]) {
        let ids = [Uuid([1u8; 16]), Uuid([2u8; 16]), Uuid([3u8; 16]), Uuid([4u8; 16])];
        let mut s = Store::new();
        s.add(n(ids[0], None, "根", Value::Empty));
        s.add(n(ids[1], Some(ids[0]), "甲", Value::Text("hello".into())));
        s.add(n(ids[2], Some(ids[0]), "乙", Value::Int(42)));
        s.add(n(ids[3], Some(ids[0]), "指向甲", Value::Reference(ids[1])));
        (s, ids)
    }

    #[test]
    fn save_load_roundtrip() {
        let (s, _) = build();
        let dir = std::env::temp_dir().join(format!("xr-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.xirang");
        s.save(&path).unwrap();
        let loaded = Store::load(&path).unwrap();
        assert_eq!(loaded.len(), 4);
        assert_eq!(loaded.nodes()[1].name, "甲");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn roots_and_children() {
        let (s, ids) = build();
        assert_eq!(s.roots().len(), 1);
        let root = s.roots()[0];
        assert_eq!(s.children(root).len(), 3);
        assert_eq!(s.child_by_name(root, "甲").unwrap().id, ids[1]);
    }

    #[test]
    fn subtree_preorder() {
        let (s, ids) = build();
        let root = s.roots()[0];
        let sub = s.subtree(root);
        assert_eq!(sub.len(), 4);
        assert_eq!(sub[0].id, ids[0]);
        assert_eq!(sub[1].id, ids[1]); // 追加序
    }

    #[test]
    fn subtree_stops_on_parent_cycle() {
        // 父边成环（E006）时 subtree 不能死循环
        let a = Uuid([1u8; 16]);
        let b = Uuid([2u8; 16]);
        let mut s = Store::new();
        s.add(n(a, Some(b), "A", Value::Empty));
        s.add(n(b, Some(a), "B", Value::Empty));
        assert_eq!(s.subtree(s.get(a).unwrap()).len(), 2);
    }

    #[test]
    fn resolve_and_references_to() {
        let (s, ids) = build();
        let root = s.roots()[0];
        let referrer = s.child_by_name(root, "指向甲").unwrap();
        assert_eq!(s.resolve(referrer).unwrap().id, ids[1]);
        let target = s.child_by_name(root, "甲").unwrap();
        assert_eq!(s.references_to(target).len(), 1);
    }

    #[test]
    fn parse_file_bad_magic() {
        assert!(parse_file(b"XXXX").unwrap_err().contains("F001"));
    }

    #[test]
    fn parse_file_bad_version() {
        let f = make_file(&[]);
        let mut bad = f.clone();
        bad[4] = 99;
        assert!(parse_file(&bad).unwrap_err().contains("F002"));
    }

    #[test]
    fn encode_decode_roundtrip() {
        let (s, _) = build();
        let bytes = s.encode().unwrap();
        let d = Store::decode(&bytes).unwrap();
        assert_eq!(d.len(), 4);
        assert_eq!(d.nodes()[0].name, "根");
    }

    /// 与 Python tools/ 对拍：同一组固定 UUID 的节点，编码字节必须逐字节一致。
    #[test]
    fn parity_with_python() {
        fn u(n: u8) -> Uuid {
            let mut b = [0u8; 16];
            b[15] = n;
            Uuid(b)
        }
        let mut s = Store::new();
        s.add(n(u(1), None, "root", Value::Empty));
        s.add(n(u(2), Some(u(1)), "text", Value::Text("灯".into())));
        s.add(n(u(3), Some(u(1)), "int", Value::Int(2046)));
        s.add(n(u(4), Some(u(1)), "float", Value::Float(1.5)));
        s.add(n(u(5), Some(u(1)), "bool", Value::Bool(true)));
        s.add(n(u(6), Some(u(2)), "ref", Value::Reference(u(3))));
        s.add(n(u(7), Some(u(2)), "blob", Value::Blob(vec![0x00, 0xff, 0x12])));

        let expect = "000000000000000000000000000000010000000000000000000000000000000004726f6f7400000000000000000000000000000000020000000000000000000000000000000104746578740400000003e781af000000000000000000000000000000030000000000000000000000000000000103696e740100000000000007fe000000000000000000000000000000040000000000000000000000000000000105666c6f6174023ff8000000000000000000000000000000000000000000050000000000000000000000000000000104626f6f6c03010000000000000000000000000000000600000000000000000000000000000002037265660500000000000000000000000000000003000000000000000000000000000000070000000000000000000000000000000204626c6f6206000000000000000300ff12";

        let got: String = s.encode().unwrap().iter().map(|b| format!("{:02x}", b)).collect();
        assert_eq!(got, expect);
    }

    #[test]
    fn write_new_update_remove() {
        let mut s = Store::new();
        let root = s.create(None, "根", Value::Empty, true);
        assert!(s.child_by_name(&root, "@created").is_some());

        let child = s.create(Some(root.id), "光源", Value::Text("能发光的物体".into()), true);

        // 改：旧值进 @history，值更新
        s.update(child.id, Value::Text("新的定义".into())).unwrap();
        assert_eq!(s.get(child.id).unwrap().value, Value::Text("新的定义".into()));
        let hist = s.child_by_name(&child, "@history").unwrap();
        let snap = s.child_by_name(hist, "光源").unwrap();
        assert_eq!(snap.value, Value::Text("能发光的物体".into()));
        assert!(s.child_by_name(snap, "@replaced").is_some());

        // 删：名字/值置空，旧值再进 @history（累计 2 条快照）
        s.remove(child.id).unwrap();
        let c = s.get(child.id).unwrap();
        assert_eq!(c.name, "");
        assert_eq!(c.value, Value::Empty);
        let hist2 = s.child_by_name(c, "@history").unwrap();
        assert_eq!(s.children(hist2).len(), 2);
    }

    #[test]
    fn rename_keeps_id_and_records_history() {
        let mut s = Store::new();
        let root = s.create(None, "根", Value::Empty, false);
        let child = s.create(Some(root.id), "旧名", Value::Text("值".into()), false);
        let grand = s.create(Some(child.id), "子", Value::Empty, false);

        // 改名：编号不变、值不变、子节点还在、旧名字进 @history
        s.rename(child.id, "新名".into()).unwrap();
        let c = s.get(child.id).unwrap();
        assert_eq!(c.name, "新名");
        assert_eq!(c.value, Value::Text("值".into()));
        assert_eq!(s.get(grand.id).unwrap().parent, Some(child.id));
        let hist = s.child_by_name(c, "@history").unwrap();
        assert_eq!(s.child_by_name(hist, "旧名").unwrap().name, "旧名");

        // 同名 = 无变化，不再记一条
        let n = s.children(hist).len();
        s.rename(child.id, "新名".into()).unwrap();
        let hist = s.child_by_name(s.get(child.id).unwrap(), "@history").unwrap();
        assert_eq!(s.children(hist).len(), n);

        // 安静改名：不写 @history
        s.rename_quiet(child.id, "安静名".into()).unwrap();
        let c = s.get(child.id).unwrap();
        assert_eq!(c.name, "安静名");
        let hist = s.child_by_name(c, "@history").unwrap();
        assert_eq!(s.children(hist).len(), n);

        // 名字超 255 字节：拒绝（100 个中文 = 300 字节）
        assert!(s.rename(child.id, "字".repeat(100)).is_err());
        assert!(s.rename_quiet(child.id, "字".repeat(100)).is_err());
        assert_eq!(s.get(child.id).unwrap().name, "安静名");
    }

    #[test]
    fn restore_and_sub_store() {
        let mut s = Store::new();
        let root = s.create(None, "根", Value::Empty, false);
        let child = s.create(Some(root.id), "甲", Value::Text("旧".into()), false);
        s.update(child.id, Value::Text("新".into())).unwrap();

        // 回滚到最近快照（"甲" = "旧"）
        let hist = s.child_by_name(&child, "@history").unwrap();
        let snap = s.children(hist).last().copied().unwrap();
        s.restore(child.id, snap.name.clone(), snap.value.clone()).unwrap();
        assert_eq!(s.get(child.id).unwrap().value, Value::Text("旧".into()));

        // sub_store 只含该子树（root + child + @history + 快照 + @replaced = 5）
        let sub = s.sub_store(&root);
        assert_eq!(sub.len(), 5);
        assert!(sub.get(child.id).is_some());
    }

    #[test]
    fn revert_restores_snapshot() {
        let mut s = Store::new();
        let root = s.create(None, "根", Value::Empty, false);
        let child = s.create(Some(root.id), "甲", Value::Text("旧".into()), false);
        s.update(child.id, Value::Text("新".into())).unwrap();
        assert_eq!(s.get(child.id).unwrap().value, Value::Text("新".into()));
        s.revert(child.id).unwrap();
        assert_eq!(s.get(child.id).unwrap().value, Value::Text("旧".into()));
    }

    #[test]
    fn set_quiet_changes_value_without_history() {
        let mut s = Store::new();
        let root = s.create(None, "根", Value::Empty, false);
        let child = s.create(Some(root.id), "甲", Value::Text("旧".into()), false);
        // 安静赋值：改值但不写 @history
        s.set_quiet(child.id, Value::Text("新".into())).unwrap();
        assert_eq!(s.get(child.id).unwrap().value, Value::Text("新".into()));
        assert!(s.child_by_name(&child, "@history").is_none());
        // 同值 = 无变化，仍不记录
        s.set_quiet(child.id, Value::Text("新".into())).unwrap();
        assert!(s.child_by_name(&child, "@history").is_none());
    }

    #[test]
    fn remove_subtree_removes_entire_tree() {
        let mut s = Store::new();
        let root = s.create(None, "根", Value::Empty, false);
        let a = s.create(Some(root.id), "甲", Value::Text("x".into()), false);
        let b = s.create(Some(a.id), "乙", Value::Empty, false);
        let total = s.len();
        assert_eq!(total, 3);
        // 删「根」子树 → 3 节点全移除
        s.remove_subtree(root.id).unwrap();
        assert_eq!(s.len(), 0);
        assert!(s.get(root.id).is_none());
        assert!(s.get(a.id).is_none());
        assert!(s.get(b.id).is_none());
    }

    #[test]
    fn is_editable_annotation_rule() {
        let mut s = Store::new();
        // 自由根：无标注 → 可编辑
        let root = s.create(None, "数据", Value::Empty, false);
        assert!(s.is_editable(s.get(root.id).unwrap()));

        // 模板定义根：挂 @模板(空) → 受保护；其子树结构受保护
        let tpl = s.create(None, "词条", Value::Empty, false);
        s.create(Some(tpl.id), "@模板", Value::Empty, false); // 标记：我是模板
        s.create(Some(tpl.id), "词形", Value::Empty, false);
        assert!(!s.is_editable(s.get(tpl.id).unwrap()));
        assert!(!s.is_editable(s.child_by_name(s.get(tpl.id).unwrap(), "词形").unwrap()));

        // 实例根：挂 @实例 + @模板(引用→模板) → 可编辑；其子树可编辑
        let inst = s.create(None, "词条", Value::Empty, false);
        s.create(Some(inst.id), "@实例", Value::Empty, false);
        s.create(Some(inst.id), "@模板", Value::Reference(tpl.id), false);
        s.create(Some(inst.id), "词形", Value::Text("火".into()), false);
        assert!(s.is_editable(s.get(inst.id).unwrap()));
        assert!(s.is_editable(s.child_by_name(s.get(inst.id).unwrap(), "词形").unwrap()));
        assert_eq!(s.template_ref(s.get(inst.id).unwrap()), Some(tpl.id));

        // 模板检测
        assert!(s.is_template_root(s.get(tpl.id).unwrap()));
        assert!(s.is_instance_root(s.get(inst.id).unwrap()));
        assert!(!s.is_instance_root(s.get(tpl.id).unwrap()));
    }

    #[test]
    fn copy_subtree_repoints_internal_and_keeps_external() {
        let mut s = Store::new();
        let root = s.create(None, "世界", Value::Empty, false);
        // root 下：灯（含 词形 文本）、光源（子树外）、父类→光源（外引用）、指向灯→自己（内引用）
        let lamp = s.create(Some(root.id), "灯", Value::Empty, false);
        let _word = s.create(Some(lamp.id), "词形", Value::Text("灯".into()), false);
        let light = s.create(Some(root.id), "光源", Value::Empty, false);
        let _parent = s.create(Some(lamp.id), "父类", Value::Reference(light.id), false);
        let _self_ref = s.create(Some(lamp.id), "指向灯", Value::Reference(lamp.id), false);
        // 改一次值 → 给 灯 造出 @history
        s.update(lamp.id, Value::Text("新".into())).unwrap();

        // 完整克隆（值 + @history）
        let opts = CopyOptions { blank_values: false, history: true };
        let new_id = s.copy_subtree(lamp.id, Some(root.id), &opts).unwrap();
        assert_eq!(s.get(new_id).unwrap().name, "灯");
        assert_eq!(s.get(new_id).unwrap().value, Value::Text("新".into()));
        // 外部引用（父类→光源）：光源在子树外，保持指向原目标
        let new_parent = s.child_by_name(s.get(new_id).unwrap(), "父类").unwrap();
        let target = match new_parent.value { Value::Reference(t) => t, _ => panic!("应为引用") };
        assert_eq!(target, light.id);
        // 内部自指（指向灯→自己）：重指到副本
        let new_self = s.child_by_name(s.get(new_id).unwrap(), "指向灯").unwrap();
        match new_self.value {
            Value::Reference(t) => assert_eq!(t, new_id),
            _ => panic!("应为引用"),
        }
        // @history 跟随（history=true）
        assert!(s.child_by_name(s.get(new_id).unwrap(), "@history").is_some());

        // 骨架模式：清空标量、丢弃 @history
        let opts2 = CopyOptions { blank_values: true, history: false };
        let new2 = s.copy_subtree(lamp.id, Some(root.id), &opts2).unwrap();
        let n = s.get(new2).unwrap();
        assert_eq!(n.value, Value::Empty); // 标量被清空
        assert_eq!(s.child_by_name(n, "词形").unwrap().value, Value::Empty);
        // 引用边保留
        assert!(matches!(s.child_by_name(n, "父类").unwrap().value, Value::Reference(_)));
        // @history 不复制
        assert!(s.child_by_name(n, "@history").is_none());
    }

    #[test]
    fn noop_operations_record_nothing() {
        let mut s = Store::new();
        let root = s.create(None, "根", Value::Empty, false);
        let child = s.create(Some(root.id), "甲", Value::Text("旧".into()), false);

        // 改同值：不写 @history
        s.update(child.id, Value::Text("旧".into())).unwrap();
        assert!(s.child_by_name(&child, "@history").is_none());

        // 改同名：不写 @history
        s.rename(child.id, "甲".into()).unwrap();
        assert!(s.child_by_name(&child, "@history").is_none());

        // 删空节点：先真删（节点变空、留痕），再删一遍应不新增留痕
        let blank = s.create(Some(root.id), "丙", Value::Empty, false).id;
        s.remove(blank).unwrap(); // 丙 有名字 → 真删，写 @history
        let hist = s.child_by_name(s.get(blank).unwrap(), "@history").unwrap();
        let snaps_before = s.children(hist).len();
        s.remove(blank).unwrap(); // 已空 → no-op，不新增
        let hist2 = s.child_by_name(s.get(blank).unwrap(), "@history").unwrap();
        assert_eq!(s.children(hist2).len(), snaps_before);

        // 真实改动仍留痕
        s.update(child.id, Value::Text("新".into())).unwrap();
        assert!(s.child_by_name(&child, "@history").is_some());
    }

    #[test]
    fn rename_changes_name_with_history() {
        let mut s = Store::new();
        let root = s.create(None, "根", Value::Empty, false);
        let child = s.create(Some(root.id), "甲", Value::Text("值".into()), false);
        s.rename(child.id, "乙".into()).unwrap();
        assert_eq!(s.get(child.id).unwrap().name, "乙");
        assert_eq!(s.get(child.id).unwrap().value, Value::Text("值".into()));
        // 旧名字留痕
        let hist = s.child_by_name(&child, "@history").unwrap();
        let snaps = s.children(hist);
        let snap = snaps.last().unwrap();
        assert_eq!(snap.name, "甲");
    }

    #[test]
    fn fold_keeps_last_record_and_first_position() {
        let mut s = Store::new();
        let root = s.create(None, "根", Value::Empty, false).id;
        let a = s.create(Some(root), "甲", Value::Text("旧".into()), false).id;
        let b = s.create(Some(root), "乙", Value::Empty, false).id;
        // 追加一条「甲」的修订（同编号、后写覆盖）
        s.add(n(a, Some(root), "甲", Value::Text("新".into())));

        let folded = fold(&s);
        assert_eq!(folded.len(), 3, "折叠后只应有 3 个编号");
        assert_eq!(folded.get(a).unwrap().value, Value::Text("新".into()));
        // 顺序按首次出现位置：根、甲、乙（修订不改变位置）
        let ids: Vec<Uuid> = folded.nodes().iter().map(|x| x.id).collect();
        assert_eq!(ids, vec![root, a, b]);
        // 孩子不重复
        assert_eq!(folded.children(folded.get(root).unwrap()).len(), 2);
    }

    #[test]
    fn fold_handles_empty_slot_and_parent_move() {
        let mut s = Store::new();
        let root = s.create(None, "根", Value::Empty, false).id;
        let other = s.create(None, "另一个根", Value::Empty, false).id;
        let a = s.create(Some(root), "甲", Value::Text("值".into()), false).id;
        // 删（置空）+ 迁移父节点
        s.add(n(a, Some(root), "", Value::Empty));
        s.add(n(a, Some(other), "甲", Value::Text("搬家".into())));

        let folded = fold(&s);
        assert_eq!(folded.children(folded.get(root).unwrap()).len(), 0);
        assert_eq!(folded.children(folded.get(other).unwrap()).len(), 1);
        assert_eq!(folded.get(a).unwrap().value, Value::Text("搬家".into()));
    }

    #[test]
    fn protocol_detection_reads_marker_node() {
        let mut s = Store::new();
        let root = s.create(None, "根", Value::Empty, false).id;
        let plain = s.create(None, "普通根", Value::Empty, false).id;
        s.create(Some(root), "@protocol", Value::Text(PROTOCOL_APPEND.into()), false);

        assert!(s.declares_protocol(root, PROTOCOL_APPEND));
        assert!(!s.declares_protocol(plain, PROTOCOL_APPEND));
        assert_eq!(s.protocol_roots(PROTOCOL_APPEND), vec![root]);
        assert_eq!(s.root_of(root), Some(root));
    }

    #[test]
    fn node_data_start_matches_make_file() {
        let mut s = Store::new();
        s.create(None, "根", Value::Empty, false);
        let bytes = make_file(&s.encode().unwrap());
        let start = node_data_start(&bytes).unwrap() as usize;
        assert_eq!(&bytes[start..], parse_file(&bytes).unwrap());
    }
}
