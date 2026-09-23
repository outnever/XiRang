//! CLI 与 MCP 共用的操作层（注释式模板模型）：模板创建、实例化、按标注定位。
//! 放这里避免 xr / xr-mcp 两个 bin 重复实现。
//!
//! 注释式模型：
//! - 模板定义 = 根上挂 `@模板`(值=空)；其普通子节点 = 结构。
//! - 实例 = 根上挂 `@实例`(空) + `@模板`(值=引用→模板根)；实例可挂在任意父节点下。
#![allow(dead_code)] // 两个 bin 各用其中一部分，允许个别未用

use xirang_core::codec::{Node, Uuid, Value as XValue};
use xirang_core::tree::Store;
use serde_json::Value as JsonValue;

/// JSON 值 → 息壤值（对象/数组 → 空容器，子节点另建；缺失/空 → 空）。
pub fn value_from_json(v: Option<&JsonValue>) -> XValue {
    match v {
        None => XValue::Empty,
        Some(JsonValue::Null) => XValue::Empty,
        Some(JsonValue::String(s)) => XValue::Text(s.clone()),
        Some(JsonValue::Number(n)) => {
            if let Some(i) = n.as_i64() {
                XValue::Int(i)
            } else if let Some(f) = n.as_f64() {
                XValue::Float(f)
            } else {
                XValue::Empty
            }
        }
        Some(JsonValue::Bool(b)) => XValue::Bool(*b),
        Some(_) => XValue::Empty,
    }
}

/// 从样例 JSON 对象构建模板结构（键→节点名，嵌套→子树，字段值留空作结构骨架）。
/// 在 `parent` 下建结构子节点（不含 @ 标记；调用方另建 @模板）。
pub fn build_template_structure(store: &mut Store, parent: Uuid, val: &JsonValue) -> Result<(), String> {
    let obj = val.as_object().ok_or("模板样例应为 JSON 对象")?;
    for (k, v) in obj {
        let node = store.create(Some(parent), k, XValue::Empty, false);
        if v.is_object() {
            build_template_structure(store, node.id, v)?;
        }
    }
    Ok(())
}

/// 建一棵模板定义：在 `parent`（nil=自由根）下建一个名为 `name` 的根，
/// 挂 `@模板`(空) 标记，并从样例 JSON 建结构；返回模板根 id。
pub fn build_template(store: &mut Store, parent: Option<Uuid>, name: &str, sample: &JsonValue) -> Result<Uuid, String> {
    let tpl = store.create(parent, name, XValue::Empty, false);
    store.create(Some(tpl.id), "@模板", XValue::Empty, false); // 标记：我是模板
    build_template_structure(store, tpl.id, sample)?;
    Ok(tpl.id)
}

/// 递归把模板结构镜像进实例（跳过 @ 辅助节点），按名字从记录取值填充。
pub fn fill_instance(store: &mut Store, tpl_id: Uuid, inst_id: Uuid, record: &JsonValue) -> Result<(), String> {
    let tpl = store.get(tpl_id).cloned().ok_or("模板不存在")?;
    let children: Vec<Node> = store.children_opt(&tpl, true).into_iter().cloned().collect();
    for child in children {
        let rec_val = record.get(&child.name);
        let node = store.create(Some(inst_id), &child.name, value_from_json(rec_val), false);
        if !store.children_opt(&child, true).is_empty() {
            fill_instance(store, child.id, node.id, rec_val.unwrap_or(&JsonValue::Null))?;
        }
    }
    Ok(())
}

/// 从模板实例化一棵树：在 `inst_parent`（nil=自由根，可挂任意父下）下建实例根，
/// 挂 `@实例`(空) + `@模板`(引用→模板根)，并按记录填值；返回实例根 id。
pub fn instantiate(store: &mut Store, tpl_id: Uuid, inst_parent: Option<Uuid>, record: &JsonValue) -> Result<Uuid, String> {
    let tpl = store.get(tpl_id).cloned().ok_or("模板不存在")?;
    let inst_root = store.create(inst_parent, &tpl.name, XValue::Empty, false);
    store.create(Some(inst_root.id), "@实例", XValue::Empty, false);
    store.create(Some(inst_root.id), "@模板", XValue::Reference(tpl_id), false);
    fill_instance(store, tpl_id, inst_root.id, record)?;
    Ok(inst_root.id)
}

/// 把一个 JSON 值的某键建为节点。含 `{"@ref": "目标"}` → 建引用边（目标不存在则建同名占位）。
fn json_value_node(store: &mut Store, parent: Option<Uuid>, name: &str, v: &JsonValue) -> Result<Uuid, String> {
    if let Some(tgt) = v.get("@ref").and_then(|t| t.as_str()) {
        let tid = Uuid::parse(tgt)
            .or_else(|| store.nodes().iter().find(|n| n.name == tgt).map(|n| n.id))
            .unwrap_or_else(|| store.create(parent, tgt, XValue::Empty, false).id);
        return Ok(store.create(parent, name, XValue::Reference(tid), false).id);
    }
    // 标量 → 值；对象/数组 → 空容器（子节点由递归建）。前导 0 字符串用 JSON 原样文本。
    Ok(store.create(parent, name, value_from_json(Some(v)), false).id)
}

/// 把嵌套 JSON 值构建成子树（挂在 parent 下；parent=None=自由根）。
/// 对象 → 有名子节点；数组 → 按下标 0,1,2 子节点；标量 → 值；`{"@ref":…}` → 引用边。
pub fn build_json_tree(store: &mut Store, parent: Option<Uuid>, val: &JsonValue) -> Result<(), String> {
    match val {
        JsonValue::Object(m) => {
            for (k, v) in m {
                let node = json_value_node(store, parent, k, v)?;
                if v.get("@ref").is_none() && (v.is_object() || v.is_array()) {
                    build_json_tree(store, Some(node), v)?;
                }
            }
        }
        JsonValue::Array(a) => {
            for (i, v) in a.iter().enumerate() {
                let node = json_value_node(store, parent, &i.to_string(), v)?;
                if v.get("@ref").is_none() && (v.is_object() || v.is_array()) {
                    build_json_tree(store, Some(node), v)?;
                }
            }
        }
        _ => {}
    }
    Ok(())
}

/// 所有模板定义根（挂 `@模板`(空) 的节点）。
pub fn find_template_roots(store: &Store) -> Vec<usize> {
    store
        .nodes()
        .iter()
        .enumerate()
        .filter(|(_, n)| store.is_template_root(n))
        .map(|(i, _)| i)
        .collect()
}

/// 某模板的所有实例根（挂 `@实例` 且 `@模板` 引用指向 tpl_id）。
pub fn find_instances_of(store: &Store, tpl_id: Uuid) -> Vec<usize> {
    store
        .nodes()
        .iter()
        .enumerate()
        .filter(|(_, n)| store.is_instance_root(n) && store.template_ref(n) == Some(tpl_id))
        .map(|(i, _)| i)
        .collect()
}

/// 按「名字 或 编号」唯一解析一个模板根。名字匹配到多个时报错（建议用编号）。
pub fn resolve_template(store: &Store, name_or_id: &str) -> Result<Uuid, String> {
    // 若是 UUID，直接取（须是模板根）
    if let Some(id) = Uuid::parse(name_or_id) {
        if let Some(n) = store.get(id) {
            if store.is_template_root(n) {
                return Ok(id);
            }
            return Err("该节点不是模板定义（根上无 @模板 空标记）".into());
        }
        return Err("模板不存在".into());
    }
    // 否则按名字
    let mut hits: Vec<Uuid> = store
        .nodes()
        .iter()
        .filter(|n| n.name == name_or_id && store.is_template_root(n))
        .map(|n| n.id)
        .collect();
    match hits.len() {
        0 => Err(format!("模板不存在：{name_or_id}")),
        1 => Ok(hits.remove(0)),
        _ => Err(format!("有 {} 个模板同名「{name_or_id}」，请用编号指定", hits.len())),
    }
}
