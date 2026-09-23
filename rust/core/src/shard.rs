//! 分片词库协议（shard v1.0）：把一个大 `.xirang` 拆成「目录 + 若干可独立读的分片
//! `.xirang` + 一份 `manifest.xirang`」。写操作只追加到目标分片（append-only），
//! 读按「同 UUID 后写覆盖」折叠（last-write-wins）。纯协议层，不改内核。

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::codec::{self, Node, Uuid, Value};
use crate::tree::Store;

pub const PROTOCOL: &str = "shard-v1";
pub const MANIFEST_NAME: &str = "manifest.xirang";
/// 清单根节点名（普通名字，避免被当作辅助节点隐藏）。
pub const ROOT_NAME: &str = "分片清单";
/// 分片条目容器节点名。
pub const SHARDS_NODE: &str = "分片";

/// 分片根判定器：返回「分片根集合」，每个根连同其全部后代归入同一分片。
#[derive(Clone, Debug, PartialEq)]
pub enum ShardRule {
    /// 顶层根（parent=nil）。
    Root,
    /// 名字等于指定值。
    Name(String),
    /// 从根往下第 k 层（根 = 0）。
    Depth(usize),
    /// 挂了指定辅助标记子节点（如 `@分片`）。
    Marker(String),
}

impl ShardRule {
    pub fn parse(s: &str) -> Result<ShardRule, String> {
        if s == "root" {
            return Ok(ShardRule::Root);
        }
        if let Some(v) = s.strip_prefix("name:") {
            return Ok(ShardRule::Name(v.to_string()));
        }
        if let Some(v) = s.strip_prefix("depth:") {
            return v
                .parse::<usize>()
                .map(ShardRule::Depth)
                .map_err(|_| format!("W005：分片根判定器非法（{s}）"));
        }
        if let Some(v) = s.strip_prefix("marker:") {
            return Ok(ShardRule::Marker(v.to_string()));
        }
        Err(format!("W005：分片根判定器非法（{s}）"))
    }

    pub fn as_str(&self) -> String {
        match self {
            ShardRule::Root => "root".to_string(),
            ShardRule::Name(n) => format!("name:{n}"),
            ShardRule::Depth(d) => format!("depth:{d}"),
            ShardRule::Marker(m) => format!("marker:{m}"),
        }
    }

    fn matches(&self, store: &Store, node: &Node) -> bool {
        match self {
            ShardRule::Root => node.parent.is_none(),
            ShardRule::Name(n) => node.name == *n,
            ShardRule::Depth(k) => depth(store, node) == *k,
            ShardRule::Marker(m) => store.child_by_name(node, m).is_some(),
        }
    }
}

fn depth(store: &Store, node: &Node) -> usize {
    let mut d = 0;
    let mut cur = node.parent;
    let cap = store.len() + 1;
    while let Some(p) = cur {
        if d > cap {
            break; // 父边成环（E006）兜底
        }
        match store.get(p) {
            Some(parent) => {
                d += 1;
                cur = parent.parent;
            }
            None => break, // 父边缺失，停止向上
        }
    }
    d
}

/// 选出「顶层」的匹配节点（某节点满足判定器、且其祖先都不满足），保证分片不相交。
fn select_set(store: &Store, rule: &ShardRule) -> HashSet<Uuid> {
    let matching: HashSet<Uuid> = store
        .nodes()
        .iter()
        .filter(|n| rule.matches(store, n))
        .map(|n| n.id)
        .collect();
    store
        .nodes()
        .iter()
        .filter(|n| matching.contains(&n.id))
        .filter(|n| !has_matching_ancestor(store, n, &matching))
        .map(|n| n.id)
        .collect()
}

fn has_matching_ancestor(store: &Store, node: &Node, matching: &HashSet<Uuid>) -> bool {
    let mut cur = node.parent;
    let mut steps = 0usize;
    let cap = store.len() + 1;
    while let Some(p) = cur {
        if steps > cap {
            return false; // 父边成环兜底
        }
        steps += 1;
        if matching.contains(&p) {
            return true;
        }
        match store.get(p) {
            Some(parent) => cur = parent.parent,
            None => break,
        }
    }
    false
}

/// 求某节点所属分片根：向上遇到的第一个「匹配根」或「顶层根（parent=None）」或「父边缺失节点」。
fn shard_root(store: &Store, node: &Node, selected: &HashSet<Uuid>) -> Uuid {
    let mut cur = node;
    let mut steps = 0usize;
    let cap = store.len() + 1;
    loop {
        if selected.contains(&cur.id) {
            return cur.id;
        }
        if steps > cap {
            return cur.id; // 父边成环兜底
        }
        steps += 1;
        match cur.parent {
            None => return cur.id,
            Some(p) => match store.get(p) {
                Some(parent) => cur = parent,
                None => return cur.id, // 父边缺失 → 自己成根
            },
        }
    }
}

/// 无损拆分：把全部节点按「所属分片根」划分成互不相交的分片（并集 = 全集）。
/// 每个分片 = 一个分片根 + 其全部后代（父指针原样保留；分片根的父可能跨分片，由集合索引解析）。
pub fn split(store: &Store, rule: &ShardRule) -> Result<Vec<(Uuid, Store)>, String> {
    let selected = select_set(store, rule);
    let mut root_of: HashMap<Uuid, Uuid> = HashMap::new();
    for n in store.nodes() {
        root_of.insert(n.id, shard_root(store, n, &selected));
    }
    let mut order: Vec<Uuid> = Vec::new();
    let mut groups: HashMap<Uuid, Vec<Node>> = HashMap::new();
    for n in store.nodes() {
        let r = root_of[&n.id];
        if !groups.contains_key(&r) {
            order.push(r);
            groups.insert(r, Vec::new());
        }
        groups.get_mut(&r).unwrap().push(n.clone());
    }
    let mut shards = Vec::new();
    for r in order {
        let mut s = Store::new();
        for n in &groups[&r] {
            s.add(n.clone());
        }
        shards.push((r, s));
    }
    Ok(shards)
}

/// 折叠：同一 UUID 后写覆盖（last-write-wins），空名空值 = 空槽位（保留）。
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

/// 追加一条修订记录到分片文件末尾（同 UUID 后写覆盖；空名空值 = 置空）。
pub fn append_revision(path: &Path, node: &Node) -> std::io::Result<()> {
    let bytes = codec::encode_node(node).map_err(|e| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, format!("{e:?}"))
    })?;
    let mut f = fs::OpenOptions::new().append(true).open(path)?;
    f.write_all(&bytes)
}

/// 合并单个分片：fold 后重写该分片。
pub fn compact(path: &Path) -> Result<Store, String> {
    let raw = Store::load(path)?;
    let folded = fold(&raw);
    folded.save(path).map_err(|e| e.to_string())?;
    Ok(folded)
}

/// 把 `before → after` 的差异以「同 UUID 后写覆盖」追加到分片文件末尾（真正的 append-only）。
/// 未变的节点不动，只追加「新增 / 改动」的节点；被硬删的节点追加一条同 UUID 空节点（置空）。
pub fn append_changes(path: &Path, before: &Store, after: &Store) -> Result<(), String> {
    let before_map: HashMap<Uuid, &Node> = before.nodes().iter().map(|n| (n.id, n)).collect();
    let after_ids: HashSet<Uuid> = after.nodes().iter().map(|n| n.id).collect();
    let mut f = fs::OpenOptions::new()
        .append(true)
        .open(path)
        .map_err(|e| e.to_string())?;
    for n in after.nodes() {
        let changed = match before_map.get(&n.id) {
            Some(b) => **b != *n,
            None => true,
        };
        if changed {
            let bytes = codec::encode_node(n).map_err(|e| format!("{e:?}"))?;
            f.write_all(&bytes).map_err(|e| e.to_string())?;
        }
    }
    for n in before.nodes() {
        if !after_ids.contains(&n.id) {
            let empty = Node {
                id: n.id,
                parent: None,
                name: String::new(),
                value: Value::Empty,
            };
            let bytes = codec::encode_node(&empty).map_err(|e| format!("{e:?}"))?;
            f.write_all(&bytes).map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

#[derive(Clone, Debug)]
pub struct ShardEntry {
    /// 分片根节点名（便于人读，非唯一）。
    pub name: String,
    /// 相对分片文件名。
    pub filename: String,
}

#[derive(Clone, Debug)]
pub struct Manifest {
    pub rule: ShardRule,
    pub shards: Vec<ShardEntry>,
}

impl Manifest {
    pub fn to_store(&self) -> Store {
        let mut s = Store::new();
        // 清单根用普通名字（不能用 `@` 开头，否则会被当作辅助节点、被 --skip-aux 整棵隐藏）。
        let root = s.create(None, ROOT_NAME, Value::Empty, false);
        s.create(Some(root.id), "@protocol", Value::Text(PROTOCOL.to_string()), false);
        s.create(Some(root.id), "@rule", Value::Text(self.rule.as_str()), false);
        // 分片条目统一挂在一个普通容器「分片」下：这样即使分片根名以 `@` 开头，也不会被漏掉。
        let shards = s.create(Some(root.id), SHARDS_NODE, Value::Empty, false);
        for e in &self.shards {
            s.create(Some(shards.id), &e.name, Value::Text(e.filename.clone()), false);
        }
        s
    }

    pub fn from_store(store: &Store) -> Result<Manifest, String> {
        // 找清单根：挂了 `@protocol = shard-v1` 的节点（兼容旧的 `@collection` 根）。
        let root = store
            .nodes()
            .iter()
            .find(|n| {
                store
                    .child_by_name(n, "@protocol")
                    .map(|c| c.value == Value::Text(PROTOCOL.to_string()))
                    .unwrap_or(false)
            })
            .ok_or("W001：分片清单缺失")?;
        let rule_str = store
            .child_by_name(root, "@rule")
            .and_then(|c| match &c.value {
                Value::Text(t) => Some(t.clone()),
                _ => None,
            })
            .unwrap_or_else(|| "root".to_string());
        let rule = ShardRule::parse(&rule_str)?;
        // 新格式：条目挂在「分片」容器下（不按名字过滤，`@` 开头的分片根也能读到）；
        // 旧格式：条目直接挂根下，跳过 `@protocol`/`@rule`。
        let container = store.child_by_name(root, SHARDS_NODE);
        let children = match container {
            Some(c) => store.children(c),
            None => store.children(root),
        };
        let mut shards = Vec::new();
        for c in children {
            if container.is_none() && c.name.starts_with('@') {
                continue;
            }
            if let Value::Text(f) = &c.value {
                shards.push(ShardEntry { name: c.name.clone(), filename: f.clone() });
            }
        }
        Ok(Manifest { rule, shards })
    }
}

/// 词库集合：目录 + 清单 + 各分片（已折叠）+ UUID→分片 索引。
pub struct Collection {
    pub dir: PathBuf,
    pub manifest: Manifest,
    shards: Vec<(String, Store)>,
    index: HashMap<Uuid, usize>,
}

impl Collection {
    pub fn open(dir: &Path) -> Result<Collection, String> {
        let mstore = Store::load(&dir.join(MANIFEST_NAME))
            .map_err(|e| format!("W001：分片清单缺失（{e}）"))?;
        let manifest = Manifest::from_store(&mstore)?;
        let mut shards = Vec::new();
        let mut index = HashMap::new();
        for e in &manifest.shards {
            let path = dir.join(&e.filename);
            if !path.exists() {
                return Err(format!("W002：分片文件缺失（{}）", e.filename));
            }
            let raw = Store::load(&path)?;
            let folded = fold(&raw);
            let slot = shards.len();
            for n in folded.nodes() {
                index.insert(n.id, slot);
            }
            shards.push((e.filename.clone(), folded));
        }
        Ok(Collection { dir: dir.to_path_buf(), manifest, shards, index })
    }

    pub fn find(&self, id: Uuid) -> Option<&Node> {
        let slot = *self.index.get(&id)?;
        self.shards[slot].1.get(id)
    }

    pub fn resolve(&self, node: &Node) -> Option<&Node> {
        match &node.value {
            Value::Reference(t) => self.find(*t),
            _ => None,
        }
    }

    /// 某 UUID 所在的分片文件名（用于写入时定位目标分片）。
    pub fn shard_file_for(&self, id: Uuid) -> Option<&str> {
        let slot = *self.index.get(&id)?;
        Some(&self.shards[slot].0)
    }

    pub fn shards(&self) -> &[(String, Store)] {
        &self.shards
    }
}

/// 把整库按判定器无损拆成目录里的多个分片 + 清单，返回分片数。
pub fn split_to_dir(store: &Store, rule: &ShardRule, dir: &Path) -> Result<usize, String> {
    fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let shards = split(store, rule)?;
    let mut entries = Vec::new();
    for (root_id, sub) in &shards {
        let filename = format!("{root_id}.xirang");
        sub.save(&dir.join(&filename)).map_err(|e| e.to_string())?;
        let name = store.get(*root_id).map(|n| n.name.clone()).unwrap_or_default();
        entries.push(ShardEntry { name, filename });
    }
    let manifest = Manifest { rule: rule.clone(), shards: entries };
    manifest.to_store().save(&dir.join(MANIFEST_NAME)).map_err(|e| e.to_string())?;
    Ok(shards.len())
}

/// 给词库集合追加一个分片条目（新建分片根后调用），并落盘清单。
/// 读—改—写原清单：保住清单根 UUID 与其它手写内容，不再整份重建。
pub fn add_shard_entry(dir: &Path, entry: ShardEntry) -> Result<(), String> {
    let mpath = dir.join(MANIFEST_NAME);
    let mut mstore = Store::load(&mpath).map_err(|e| format!("W001：分片清单缺失（{e}）"))?;
    let root_id = {
        mstore
            .nodes()
            .iter()
            .find(|n| {
                mstore
                    .child_by_name(n, "@protocol")
                    .map(|c| c.value == Value::Text(PROTOCOL.to_string()))
                    .unwrap_or(false)
            })
            .map(|n| n.id)
    }
    .ok_or("W001：分片清单缺失")?;
    let existing = {
        let r = mstore.get(root_id).ok_or("W001：分片清单缺失")?;
        mstore
            .children(r)
            .into_iter()
            .find(|c| c.name == SHARDS_NODE)
            .map(|c| c.id)
    };
    let shards_id = match existing {
        Some(id) => id,
        None => mstore.create(Some(root_id), SHARDS_NODE, Value::Empty, false).id,
    };
    mstore.create(Some(shards_id), &entry.name, Value::Text(entry.filename), false);
    mstore.save(&mpath).map_err(|e| e.to_string())
}

/// 只读清单（不改任何文件）。用于写入前确认「这确实是分片词库」。
pub fn read_manifest(dir: &Path) -> Result<Manifest, String> {
    let mstore =
        Store::load(&dir.join(MANIFEST_NAME)).map_err(|e| format!("W001：分片清单缺失（{e}）"))?;
    Manifest::from_store(&mstore)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: Uuid, parent: Option<Uuid>, name: &str, value: Value) -> Node {
        Node { id, parent, name: name.to_string(), value }
    }

    fn tmpdir(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("xirang_shard_{tag}_{}", Uuid::random_v4()))
    }

    #[test]
    fn fold_last_write_wins() {
        let a = Uuid([1u8; 16]);
        let b = Uuid([2u8; 16]);
        let mut s = Store::new();
        s.add(node(a, None, "甲", Value::Text("旧".into())));
        s.add(node(a, None, "甲", Value::Text("新".into()))); // 同 UUID 后写覆盖
        s.add(node(b, None, "乙", Value::Empty));
        let f = fold(&s);
        assert_eq!(f.len(), 2);
        assert_eq!(f.get(a).unwrap().value, Value::Text("新".into()));
        assert_eq!(f.get(b).unwrap().name, "乙");
    }

    #[test]
    fn split_root_rule_and_orphan() {
        let a = Uuid([1u8; 16]);
        let b = Uuid([2u8; 16]);
        let xing = Uuid([11u8; 16]);
        let orphan = Uuid([3u8; 16]);
        let mut s = Store::new();
        s.add(node(a, None, "甲", Value::Empty));
        s.add(node(xing, Some(a), "词形", Value::Text("灯".into())));
        s.add(node(b, None, "乙", Value::Empty));
        s.add(node(orphan, Some(Uuid([99u8; 16])), "孤儿", Value::Empty));

        let shards = split(&s, &ShardRule::Root).unwrap();
        assert_eq!(shards.len(), 3);
        let a_shard = shards.iter().find(|(id, _)| *id == a).unwrap().1.clone();
        assert!(a_shard.get(xing).is_some(), "甲 的子节点应归入甲分片");
        assert!(shards.iter().any(|(id, _)| *id == orphan));
    }

    #[test]
    fn split_name_rule_keeps_extra_nodes() {
        let root = Uuid([1u8; 16]);
        let e1 = Uuid([2u8; 16]);
        let e2 = Uuid([3u8; 16]);
        let shiyi = Uuid([21u8; 16]);
        let extra = Uuid([22u8; 16]);
        let mut s = Store::new();
        s.add(node(root, None, "词库", Value::Empty));
        s.add(node(e1, Some(root), "词条", Value::Empty));
        s.add(node(shiyi, Some(e1), "释义", Value::Text("A".into())));
        s.add(node(extra, Some(e1), "额外", Value::Text("X".into())));
        s.add(node(e2, Some(root), "词条", Value::Empty));

        let shards = split(&s, &ShardRule::Name("词条".to_string())).unwrap();
        assert_eq!(shards.len(), 3); // 词库容器 + 2 个词条
        let e1_shard = shards.iter().find(|(id, _)| *id == e1).unwrap().1.clone();
        assert!(e1_shard.get(shiyi).is_some());
        assert!(e1_shard.get(extra).is_some(), "用户额外节点也应归入同一分片");
    }

    #[test]
    fn split_marker_rule() {
        let root = Uuid([1u8; 16]);
        let a = Uuid([2u8; 16]);
        let b = Uuid([3u8; 16]);
        let mut s = Store::new();
        s.add(node(root, None, "词库", Value::Empty));
        s.add(node(a, Some(root), "甲", Value::Empty));
        s.add(node(Uuid([21u8; 16]), Some(a), "@分片", Value::Empty));
        s.add(node(b, Some(root), "乙", Value::Empty));
        s.add(node(Uuid([31u8; 16]), Some(b), "@分片", Value::Empty));

        let shards = split(&s, &ShardRule::Marker("@分片".to_string())).unwrap();
        assert_eq!(shards.len(), 3); // 词库容器 + 2 个标记根
    }

    #[test]
    fn split_depth_rule() {
        let root = Uuid([1u8; 16]);
        let a = Uuid([2u8; 16]);
        let b = Uuid([3u8; 16]);
        let child = Uuid([21u8; 16]);
        let mut s = Store::new();
        s.add(node(root, None, "词库", Value::Empty));
        s.add(node(a, Some(root), "甲", Value::Empty));
        s.add(node(child, Some(a), "子", Value::Empty));
        s.add(node(b, Some(root), "乙", Value::Empty));

        let shards = split(&s, &ShardRule::Depth(1)).unwrap();
        assert_eq!(shards.len(), 3); // 词库容器 + 甲 + 乙
        let a_shard = shards.iter().find(|(id, _)| *id == a).unwrap().1.clone();
        assert!(a_shard.get(child).is_some());
    }

    #[test]
    fn append_changes_is_append_only() {
        use std::fs;
        let dir = tmpdir("appchg");
        fs::create_dir_all(&dir).unwrap();
        let a = Uuid([9u8; 16]);
        let mut s = Store::new();
        s.add(node(a, None, "词条", Value::Empty));
        let p = dir.join("x.xirang");
        s.save(&p).unwrap();
        let before = Store::load(&p).unwrap();
        let size0 = fs::metadata(&p).unwrap().len();
        let mut after = before.clone();
        after.set_quiet(a, Value::Text("新".into())).unwrap();
        append_changes(&p, &before, &after).unwrap();
        assert!(fs::metadata(&p).unwrap().len() > size0, "应为追加");
        let raw = Store::load(&p).unwrap();
        assert_eq!(raw.nodes().iter().filter(|n| n.id == a).count(), 2);
        assert_eq!(fold(&raw).get(a).unwrap().value, Value::Text("新".into()));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn split_to_dir_append_compact_roundtrip() {
        let root = Uuid([1u8; 16]);
        let e1 = Uuid([2u8; 16]);
        let e2 = Uuid([3u8; 16]);
        let mut s = Store::new();
        s.add(node(root, None, "词库", Value::Empty));
        s.add(node(e1, Some(root), "词条", Value::Empty));
        s.add(node(Uuid([21u8; 16]), Some(e1), "词形", Value::Text("灯".into())));
        s.add(node(e2, Some(root), "词条", Value::Empty));
        s.add(node(Uuid([31u8; 16]), Some(e2), "词形", Value::Text("火".into())));

        let dir = tmpdir("roundtrip");
        let n = split_to_dir(&s, &ShardRule::Name("词条".to_string()), &dir).unwrap();
        assert_eq!(n, 3); // 词库容器 + 2 个词条

        let col = Collection::open(&dir).unwrap();
        assert_eq!(col.find(e1).unwrap().name, "词条");

        // 追加一条修订（同 UUID 改值）到 e1 所在分片
        let e1_file = dir.join(format!("{e1}.xirang"));
        append_revision(&e1_file, &node(e1, Some(root), "词条", Value::Text("灯·新".into()))).unwrap();
        let col2 = Collection::open(&dir).unwrap();
        assert_eq!(col2.find(e1).unwrap().value, Value::Text("灯·新".into()));

        // 合并后逻辑视图不变
        compact(&e1_file).unwrap();
        let col3 = Collection::open(&dir).unwrap();
        assert_eq!(col3.find(e1).unwrap().value, Value::Text("灯·新".into()));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn manifest_roundtrip() {
        let m = Manifest {
            rule: ShardRule::Name("词条".to_string()),
            shards: vec![ShardEntry { name: "灯".to_string(), filename: "a.xirang".to_string() }],
        };
        let s = m.to_store();
        let back = Manifest::from_store(&s).unwrap();
        assert_eq!(back.rule, m.rule);
        assert_eq!(back.shards.len(), 1);
        assert_eq!(back.shards[0].filename, "a.xirang");
    }
}
