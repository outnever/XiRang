//! 多文件工作区：合并多个 Store，按 UUID 跨文件寻址 / 解析引用。
//!
//! 节点编号是全局唯一 UUID，所以引用天然能跨文件：维护一个「UUID → (文件, 节点)」的
//! 全局索引即可。目标文件未打开时的持久化约定（@address/@source）见 spec。

use crate::codec::{Node, Uuid, Value};
use crate::tree::Store;

/// 多文件工作区。
#[derive(Default)]
pub struct Workspace {
    files: Vec<(String, Store)>,
}

impl Workspace {
    pub fn new() -> Self {
        Self::default()
    }

    /// 加入一个文件（path = 显示名 / 路径，store = 已解析的节点集）。
    pub fn add(&mut self, path: String, store: Store) {
        self.files.push((path, store));
    }

    pub fn files(&self) -> &[(String, Store)] {
        &self.files
    }

    /// 按 UUID 跨所有文件查找节点，返回 (文件路径, 节点)。
    pub fn find(&self, id: Uuid) -> Option<(&str, &Node)> {
        for (path, store) in &self.files {
            if let Some(n) = store.get(id) {
                return Some((path.as_str(), n));
            }
        }
        None
    }

    /// 解析引用（跨文件）：返回 (目标文件路径, 目标节点)。
    pub fn resolve(&self, node: &Node) -> Option<(&str, &Node)> {
        match &node.value {
            Value::Reference(id) => self.find(*id),
            _ => None,
        }
    }

    /// 谁引用了我（跨文件）：返回所有引用指向 id 的 (文件路径, 节点)。
    pub fn references_to(&self, id: Uuid) -> Vec<(&str, &Node)> {
        let mut out = Vec::new();
        for (path, store) in &self.files {
            for n in store.nodes() {
                if matches!(&n.value, Value::Reference(t) if *t == id) {
                    out.push((path.as_str(), n));
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u(n: u8) -> Uuid {
        let mut b = [0u8; 16];
        b[15] = n;
        Uuid(b)
    }

    fn node(id: Uuid, parent: Option<Uuid>, name: &str, value: Value) -> Node {
        Node { id, parent, name: name.into(), value }
    }

    #[test]
    fn cross_file_resolve() {
        // 文件 2 里有目标节点 B
        let mut s2 = Store::new();
        s2.add(node(u(2), None, "乙", Value::Text("目标".into())));
        // 文件 1 里有引用节点 A，指向 B（u(2)）
        let mut s1 = Store::new();
        s1.add(node(u(1), None, "甲", Value::Reference(u(2))));

        let mut ws = Workspace::new();
        ws.add("file1.xirang".into(), s1);
        ws.add("file2.xirang".into(), s2);

        // 跨文件解析：甲 的引用在 file2 里找到乙
        let a = ws.find(u(1)).unwrap().1;
        let (file, target) = ws.resolve(a).expect("跨文件解析成功");
        assert_eq!(file, "file2.xirang");
        assert_eq!(target.name, "乙");

        // 反向：乙 被 file1 里的甲引用
        let back = ws.references_to(u(2));
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].0, "file1.xirang");
        assert_eq!(back[0].1.name, "甲");
    }

    #[test]
    fn unresolved_reference() {
        let mut s1 = Store::new();
        s1.add(node(u(1), None, "甲", Value::Reference(u(99))));
        let ws = Workspace { files: vec![("f.xirang".into(), s1)] };
        assert!(ws.find(u(99)).is_none());
    }
}
