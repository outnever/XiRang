//! 息壤结构校验器：对一批节点做结构校验，返回错误列表（错误码 E/R，见 errors/错误列表.md）。
//!
//! Rust 类型系统已天然消除 E003（编号格式）、E008（类型标记）、E009（值内容），
//! 此处只校验类型系统保留不了的结构类错误：E001 / E002 / E005 / E006 / E011 / R001。

use std::collections::{HashMap, HashSet};

use crate::codec::{Node, Uuid, Value};
use crate::tree::{self, Store};

/// 校验错误。
#[derive(Debug, PartialEq, Clone)]
pub struct Error {
    pub code: &'static str,
    pub node_id: Uuid,
    pub message: String,
}

pub fn validate(nodes: &[Node]) -> Vec<Error> {
    let mut errors = Vec::new();
    let mut seen: HashSet<Uuid> = HashSet::new();
    let mut index: HashMap<Uuid, &Node> = HashMap::new();

    // 第一遍：单节点校验 + 建索引 + 查编号冲突
    for n in nodes {
        if n.id.is_nil() {
            errors.push(Error {
                code: "E001",
                node_id: n.id,
                message: "节点编号缺失（为 nil）".into(),
            });
        }
        if !seen.insert(n.id) {
            errors.push(Error {
                code: "E002",
                node_id: n.id,
                message: "编号冲突（与另一节点相同）".into(),
            });
        }
        if let Some(p) = n.parent {
            if p == n.id {
                errors.push(Error {
                    code: "E005",
                    node_id: n.id,
                    message: "父节点指向自己".into(),
                });
            }
        }
        index.insert(n.id, n);
    }

    // 第二遍：树级校验（父边 + 引用）
    for n in nodes {
        if let Some(p) = n.parent {
            if !index.contains_key(&p) {
                errors.push(Error {
                    code: "E011",
                    node_id: n.id,
                    message: format!("父节点断裂：{p} 不存在"),
                });
            }
        }
        if let Value::Reference(target) = &n.value {
            if !index.contains_key(target) {
                errors.push(Error {
                    code: "R001",
                    node_id: n.id,
                    message: format!("引用断裂：目标 {target} 不存在"),
                });
            }
        }
    }

    // 父节点成环（长度 ≥ 2；自指已在 E005 单独报）。
    // 同一个环只报一次（按环内最小编号的那次报），避免日志噪音随环长线性增长。
    let mut seen_cycle: HashSet<Uuid> = HashSet::new();
    for start in nodes {
        if start.parent.is_none() || start.parent == Some(start.id) {
            continue;
        }
        if seen_cycle.contains(&start.id) {
            continue; // 已属于报过的环
        }
        let mut on_path: Vec<Uuid> = vec![start.id];
        let mut on_set: HashSet<Uuid> = HashSet::new();
        on_set.insert(start.id);
        let mut cur = start.parent.and_then(|p| index.get(&p)).copied();
        let mut cycle: Option<Vec<Uuid>> = None;
        while let Some(node) = cur {
            if node.id == start.id {
                cycle = Some(on_path.clone());
                break;
            }
            if !on_set.insert(node.id) {
                break;
            }
            on_path.push(node.id);
            cur = node.parent.and_then(|p| index.get(&p)).copied();
        }
        if let Some(members) = cycle {
            for m in &members {
                seen_cycle.insert(*m);
            }
            errors.push(Error {
                code: "E006",
                node_id: start.id,
                message: "父节点成环".into(),
            });
        }
    }

    errors
}

/// 折叠视图校验（`append-v1`）：先按「同编号取最后一条」折叠，再跑结构校验。
///
/// 声明了 `@protocol = append-v1` 的根下，重复编号 = 修订，**不报 E002**；
/// 未声明的根下出现重复编号 = 真冲突，仍然报 E002。
pub fn validate_view(store: &Store) -> Vec<Error> {
    let folded = tree::fold(store);
    let mut errors = validate(&folded.nodes());

    let mut counts: HashMap<Uuid, usize> = HashMap::new();
    for n in store.nodes() {
        *counts.entry(n.id).or_insert(0) += 1;
    }
    let mut dup_ids: Vec<Uuid> = counts
        .iter()
        .filter(|(_, c)| **c > 1)
        .map(|(id, _)| *id)
        .collect();
    dup_ids.sort_by(|a, b| a.0.cmp(&b.0));
    for id in dup_ids {
        let root = store.root_of(id).unwrap_or(id);
        if store.declares_protocol(root, tree::PROTOCOL_APPEND) {
            continue;
        }
        errors.push(Error {
            code: "E002",
            node_id: id,
            message: "编号冲突（与另一节点相同）".into(),
        });
    }
    errors
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::NIL_UUID;

    fn n(id: Uuid, parent: Option<Uuid>, value: Value) -> Node {
        Node {
            id,
            parent,
            name: String::new(),
            value,
        }
    }

    fn ids() -> [Uuid; 6] {
        [
            Uuid([1u8; 16]),
            Uuid([2u8; 16]),
            Uuid([3u8; 16]),
            Uuid([4u8; 16]),
            Uuid([5u8; 16]),
            Uuid([6u8; 16]),
        ]
    }

    #[test]
    fn valid_tree() {
        let [a, b, c, ..] = ids();
        let nodes = vec![
            n(a, None, Value::Empty),
            n(b, Some(a), Value::Text("x".into())),
            n(c, Some(b), Value::Reference(a)),
        ];
        assert!(validate(&nodes).is_empty());
    }

    #[test]
    fn e001_nil_id() {
        let [_, b, ..] = ids();
        let nodes = vec![n(NIL_UUID, None, Value::Empty), n(b, None, Value::Empty)];
        let errs = validate(&nodes);
        assert!(errs.iter().any(|e| e.code == "E001"));
    }

    #[test]
    fn e002_duplicate_id() {
        let [a, ..] = ids();
        let nodes = vec![n(a, None, Value::Empty), n(a, None, Value::Empty)];
        let errs = validate(&nodes);
        assert!(errs.iter().any(|e| e.code == "E002"));
    }

    #[test]
    fn e005_parent_self() {
        let [a, ..] = ids();
        let nodes = vec![n(a, Some(a), Value::Empty)];
        let errs = validate(&nodes);
        assert!(errs.iter().any(|e| e.code == "E005"));
    }

    #[test]
    fn e006_cycle() {
        let [a, b, ..] = ids();
        let nodes = vec![
            n(a, Some(b), Value::Empty),
            n(b, Some(a), Value::Empty),
        ];
        let errs = validate(&nodes);
        assert!(errs.iter().any(|e| e.code == "E006"));
    }

    #[test]
    fn e011_parent_dangling() {
        let [a, ..] = ids();
        let ghost = Uuid([9u8; 16]);
        let nodes = vec![n(a, Some(ghost), Value::Empty)];
        let errs = validate(&nodes);
        assert!(errs.iter().any(|e| e.code == "E011"));
    }

    #[test]
    fn r001_reference_dangling() {
        let [a, ..] = ids();
        let ghost = Uuid([9u8; 16]);
        let nodes = vec![n(a, None, Value::Reference(ghost))];
        let errs = validate(&nodes);
        assert!(errs.iter().any(|e| e.code == "R001"));
    }

    fn named(id: Uuid, parent: Option<Uuid>, name: &str, value: Value) -> Node {
        Node {
            id,
            parent,
            name: name.into(),
            value,
        }
    }

    fn manifest(id: Uuid, parent: Uuid) -> Node {
        named(
            id,
            Some(parent),
            "@protocol",
            Value::Text(tree::PROTOCOL_APPEND.into()),
        )
    }

    #[test]
    fn append_view_allows_declared_revisions() {
        let [a, b, c, ..] = ids();
        let mut s = Store::new();
        s.add(named(a, None, "根", Value::Empty));
        s.add(manifest(c, a));
        s.add(named(b, Some(a), "词形", Value::Text("灯".into())));
        s.add(named(b, Some(a), "词形", Value::Text("灯（改）".into()))); // 修订

        assert!(
            validate_view(&s).is_empty(),
            "声明了修订协议的根下，重复编号是修订，不算冲突"
        );
        assert!(
            validate(s.nodes()).iter().any(|e| e.code == "E002"),
            "原始记录视角仍然能看到重复编号"
        );
    }

    #[test]
    fn undeclared_duplicates_still_conflict_in_view() {
        let [a, b, ..] = ids();
        let mut s = Store::new();
        s.add(named(a, None, "根", Value::Empty));
        s.add(named(b, Some(a), "词形", Value::Text("灯".into())));
        s.add(named(b, Some(a), "词形", Value::Text("灯（改）".into())));
        assert!(validate_view(&s).iter().any(|e| e.code == "E002"));
    }

    #[test]
    fn append_view_accepts_empty_slot_from_delete() {
        let [a, b, c, ..] = ids();
        let mut s = Store::new();
        s.add(named(a, None, "根", Value::Empty));
        s.add(manifest(c, a));
        s.add(named(b, Some(a), "词形", Value::Text("灯".into())));
        s.add(named(b, Some(a), "", Value::Empty)); // 删 = 追加一条空记录

        let folded = tree::fold(&s);
        assert_eq!(folded.get(b).unwrap().value, Value::Empty);
        assert!(folded.get(b).unwrap().name.is_empty());
        assert!(validate_view(&s).is_empty());
    }
}
