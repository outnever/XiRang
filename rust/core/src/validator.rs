//! 息壤结构校验器：对一批节点做结构校验，返回错误列表（错误码 E/R，见 errors/错误列表.md）。
//!
//! Rust 类型系统已天然消除 E003（编号格式）、E008（类型标记）、E009（值内容），
//! 此处只校验类型系统保留不了的结构类错误：E001 / E002 / E005 / E006 / E011 / R001。

use std::collections::{HashMap, HashSet};

use crate::codec::{Node, Uuid, Value};

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
}
