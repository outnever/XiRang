//! 导出：把「当前视图 / 选中子树 / 完整折叠视图」变成一棵可导出的树。
//!
//! 三种范围：
//! - `Full`：整份折叠视图（同编号只留最后一条）
//! - `View`：**按当前展开态裁剪**——折叠着的节点不往下走，导出的就是你在屏幕上看到的那棵
//! - `Subtree`：只导出选中节点的子树

use std::collections::HashSet;

use xirang_core::codec::Uuid;
use xirang_core::tree::Store;

use crate::lazy::Doc;

#[derive(Clone, Debug, PartialEq)]
pub enum Scope {
    Full,
    View,
    Subtree(Uuid),
}

/// 按范围搭出一棵 Store（只包含范围内的节点，父边原样保留）。
pub fn build(doc: &mut Doc, scope: &Scope, expanded: &HashSet<Uuid>) -> Result<Store, String> {
    match scope {
        Scope::Full => Store::load_view(doc.path()),
        Scope::Subtree(id) => {
            let mut out = Store::new();
            // 导出子树时把它的根当「根」（父不在导出范围里，否则树形解读会看不到内容）
            collect(doc, *id, true, &mut out)?;
            Ok(out)
        }
        Scope::View => {
            let mut out = Store::new();
            for root in doc.roots() {
                collect_view(doc, root, expanded, &mut out)?;
            }
            Ok(out)
        }
    }
}

/// 子树：整棵收下来。
fn collect(doc: &mut Doc, id: Uuid, is_root: bool, out: &mut Store) -> Result<(), String> {
    let Some(node) = doc.node(id) else {
        return Ok(());
    };
    out.add(xirang_core::codec::Node {
        id: node.id,
        parent: if is_root { None } else { node.parent },
        name: node.name.clone(),
        value: node.value.clone(),
    });
    for kid in doc.children(id) {
        collect(doc, kid, false, out)?;
    }
    Ok(())
}

/// 当前视图：折叠着的节点不再往下走（叶子之外只在展开时收孩子）。
fn collect_view(
    doc: &mut Doc,
    id: Uuid,
    expanded: &HashSet<Uuid>,
    out: &mut Store,
) -> Result<(), String> {
    let Some(node) = doc.node(id) else {
        return Ok(());
    };
    out.add(xirang_core::codec::Node {
        id: node.id,
        parent: node.parent,
        name: node.name.clone(),
        value: node.value.clone(),
    });
    if !expanded.contains(&id) {
        return Ok(());
    }
    for kid in doc.children(id) {
        collect_view(doc, kid, expanded, out)?;
    }
    Ok(())
}

/// 导出成文本（json / xml / yaml / md）。
pub fn to_text(store: &Store, fmt: &str) -> String {
    match fmt {
        "json" => xirang_core::convert::to_json(store),
        "xml" => xirang_core::convert::to_xml(store),
        "yaml" => xirang_core::convert::to_yaml(store),
        _ => xirang_core::convert::to_md(store),
    }
}
