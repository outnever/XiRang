//! 查询层：给 Store 加「形状码」与「名字」索引，支持按 结构+名字+值 匹配。
//!
//! 背景：息壤数据是自由摆放的（无规范层），`find` 按关键字会返回大量同值节点。
//! 所以我们提供按「树形结构」+「根节点名」+「值约束」筛选的能力。
//!
//! 关键概念：
//! - **形状码（shape_id）**：一棵子树的拓扑的规范编码。不区分子节点顺序，只统计
//!   非辅助节点（跳过 @ 开头）的子节点。用 interning 保证形状相同 → 同一形状码、
//!   形状不同 → 不同形状码（无碰撞）。自底向上算，节点不删、只改名/值，所以
//!   拓扑是稳定身份。
//! - **名字索引**：节点名 → 下标列表，按根名锚定用。
//!
//! 两个索引都是**加载时物化**的（不算进盘上格式），只读用于查询。

use std::collections::HashMap;

use crate::codec::{Uuid, Value};
use crate::tree::Store;

/// 计算每个节点的形状码（返回按节点追加序下标的 shape_id）。
/// 子节点按「形状」归一化不区分顺序；只统计非辅助（非 @ 开头）子节点。
pub fn shape_ids(store: &Store) -> Vec<u64> {
    let nodes = store.nodes();
    let n = nodes.len();
    // id → 下标，用于把父 id 映射到下标
    let mut by_id: HashMap<Uuid, usize> = HashMap::with_capacity(n);
    for (i, nd) in nodes.iter().enumerate() {
        by_id.insert(nd.id, i);
    }
    // 父下标 → 子下标列表
    let mut children_of: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (i, nd) in nodes.iter().enumerate() {
        if let Some(p) = nd.parent {
            if let Some(&pi) = by_id.get(&p) {
                children_of[pi].push(i);
            }
        }
    }
    let mut shape: Vec<u64> = vec![0; n];
    let mut to_id: HashMap<Vec<u64>, u64> = HashMap::new();
    let mut next: u64 = 0;
    // 追加序 = 父先于子；倒序处理 = 子先于父（子形状先算好）
    for i in (0..n).rev() {
        let mut key: Vec<u64> = children_of[i]
            .iter()
            .filter(|&&c| !nodes[c].name.starts_with('@')) // 跳过辅助节点
            .map(|&c| shape[c])
            .collect();
        key.sort(); // 不区分顺序
        let id = *to_id.entry(key).or_insert_with(|| {
            let v = next;
            next += 1;
            v
        });
        shape[i] = id;
    }
    shape
}

/// 一次性算出形状码 + 形状码 → 节点下标索引（避免重复计算）。
pub struct ShapeIndex {
    /// 每个节点（按追加序下标）的形状码。
    pub shapes: Vec<u64>,
    /// 形状码 → 节点下标列表。
    pub by_shape: HashMap<u64, Vec<usize>>,
}

impl ShapeIndex {
    pub fn build(store: &Store) -> Self {
        let shapes = shape_ids(store);
        let mut by_shape: HashMap<u64, Vec<usize>> = HashMap::new();
        for (i, s) in shapes.iter().enumerate() {
            by_shape.entry(*s).or_default().push(i);
        }
        ShapeIndex { shapes, by_shape }
    }
}

/// 名字索引：节点名 → 节点下标列表。
pub fn name_index(store: &Store) -> HashMap<String, Vec<usize>> {
    let mut idx: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, nd) in store.nodes().iter().enumerate() {
        idx.entry(nd.name.clone()).or_default().push(i);
    }
    idx
}

/// 值约束：`path` 是相对某节点的一串子节点名（用 `/` 分隔），最后一段为要匹配的节点名；
/// 要求该节点值 == `value`。
pub fn value_at<'a>(store: &'a Store, root_id: Uuid, path: &[&str]) -> Option<&'a Value> {
    // 从 root 开始向下走 path，返回最后一个节点的值
    let mut cur_id = root_id;
    for seg in path {
        let cur = store.get(cur_id)?;
        let child = store.child_by_name(cur, seg)?;
        cur_id = child.id;
    }
    // 最后一段是目标节点本身？不——path 的最后一段也是子节点名。
    // 这里路 径 以「目标子节点」收尾，所以目标节点 = cur_id 已指向它。
    Some(&store.get(cur_id)?.value)
}

/// 判断某个节点（按追加序下标）是否满足所有 where 条件（value_at 校验，全于则 true）。
pub fn matches_where(store: &Store, idx: usize, wheres: &[(&str, &str)]) -> bool {
    let nd = &store.nodes()[idx];
    for (path, expected) in wheres {
        let segs: Vec<&str> = path.split('/').collect();
        match value_at(store, nd.id, &segs) {
            Some(v) if value_matches(v, expected) => {}
            _ => return false,
        }
    }
    true
}

/// 找子树根：根名为 `root_name`，且（可选）满足 `where_conds` 各项值约束。
/// 用名字索引锚定根，再局部校验值（几乎不遍历）。
pub fn find_roots(
    store: &Store,
    root_name: &str,
    where_conds: &[(&str, &str)],
) -> Vec<usize> {
    let names = name_index(store);
    let Some(candidates) = names.get(root_name) else {
        return Vec::new();
    };
    candidates
        .iter()
        .copied()
        .filter(|&i| matches_where(store, i, where_conds))
        .collect()
}

/// 形状码 → 匹配的节点下标：返回所有 `shape == shape_id` 的节点。
pub fn by_shape(sidx: &ShapeIndex, shape_id: u64) -> Vec<usize> {
    sidx.by_shape.get(&shape_id).cloned().unwrap_or_default()
}

/// 值匹配：把期望字符串解析成息壤值再比较（整数/浮点/布尔/文本）。
fn value_matches(actual: &Value, expected: &str) -> bool {
    if let Ok(i) = expected.parse::<i64>() {
        if let Value::Int(a) = actual {
            return *a == i;
        }
    }
    if let Ok(f) = expected.parse::<f64>() {
        if let Value::Float(a) = actual {
            return (*a - f).abs() < f64::EPSILON;
        }
    }
    match expected {
        "true" => *actual == Value::Bool(true),
        "false" => *actual == Value::Bool(false),
        _ => match actual {
            Value::Text(s) => s == expected,
            _ => false,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::Store;

    #[test]
    fn shape_ignores_order_and_aux() {
        let mut s = Store::new();
        let root = s.create(None, "词条", Value::Empty, true); // @created 挂上
        let a = s.create(Some(root.id), "词形", Value::Text("灯".into()), true);
        let b = s.create(Some(root.id), "词频", Value::Int(2046), true);
        let _c = s.create(Some(root.id), "词义", Value::Empty, true);
        // root: 非 aux 子 = 词形/词频/词义（@created 是 aux，跳过）
        let _ = a;
        let _ = b;
        let shapes = shape_ids(&s);
        // root 的 shape 与另一个「同样三个非 aux 子节点」的根应相同
        let mut s2 = Store::new();
        let r2 = s2.create(None, "词条", Value::Empty, false);
        s2.create(Some(r2.id), "词义", Value::Empty, false);
        s2.create(Some(r2.id), "词形", Value::Text("灯".into()), false);
        s2.create(Some(r2.id), "词频", Value::Int(42), false);
        let shapes2 = shape_ids(&s2);
        assert_eq!(shapes[0], shapes2[0]); // 顺序无关 + 跳过 @aux
    }

    #[test]
    fn shape_index_groups_same_structure() {
        let mut s = Store::new();
        let r1 = s.create(None, "词条", Value::Empty, false);
        s.create(Some(r1.id), "词形", Value::Text("灯".into()), false);
        s.create(Some(r1.id), "词频", Value::Int(2046), false);
        let r2 = s.create(None, "词条", Value::Empty, false);
        s.create(Some(r2.id), "词形", Value::Text("火".into()), false);
        s.create(Some(r2.id), "词频", Value::Int(7), false);
        let sidx = ShapeIndex::build(&s);
        let shape_of_r1 = sidx.shapes[0];
        let r2_idx = s.nodes().iter().position(|x| x.id == r2.id).unwrap();
        assert_eq!(sidx.shapes[r2_idx], shape_of_r1); // 结构相同 → 形状码相同
        assert_eq!(sidx.by_shape[&shape_of_r1].len(), 2); // 两个词条都命中
    }

    #[test]
    fn find_roots_by_name_and_where() {
        let mut s = Store::new();
        let r1 = s.create(None, "词条", Value::Empty, false);
        s.create(Some(r1.id), "词形", Value::Text("灯".into()), false);
        let r2 = s.create(None, "词条", Value::Empty, false);
        s.create(Some(r2.id), "词形", Value::Text("火".into()), false);
        // 找根名 词条 且 词形=灯
        let hits = find_roots(&s, "词条", &[("词形", "灯")]);
        assert_eq!(hits.len(), 1);
        assert_eq!(s.nodes()[hits[0]].id, r1.id);
    }
}
