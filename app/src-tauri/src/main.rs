//! 息壤桌面版（Tauri，P0）：打开单个 .xirang，树视图。依赖内核 v1.0。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::path::Path;

use serde_json::json;
use xirang_core::codec::{parse_value, Node, Uuid, Value};
use xirang_core::tree::Store;
use xirang_core::workspace::Workspace;

// 值解析统一走核心库（codec::parse_value），与 CLI 行为一致（P31）。

fn node_to_json(store: &Store, node: &Node) -> serde_json::Value {
    let mut children = Vec::new();
    for c in store.children(node) {
        children.push(node_to_json(store, c));
    }
    let (type_name, value_display) = match &node.value {
        Value::Empty => ("empty", None),
        Value::Int(n) => ("integer", Some(n.to_string())),
        Value::Float(f) => ("float", Some(f.to_string())),
        Value::Bool(b) => ("boolean", Some(if *b { "true" } else { "false" }.to_string())),
        Value::Text(s) => ("text", Some(s.clone())),
        Value::Reference(u) => ("reference", Some(format!("→ {}", store.get(*u).map(|t| t.name.as_str()).unwrap_or(&u.to_string())))),
        Value::Blob(b) => ("blob", Some(format!("[blob {} 字节]", b.len()))),
    };
    json!({
        "id": node.id.to_string(),
        "name": node.name,
        "type": type_name,
        "value": value_display,
        "isAux": node.name.starts_with('@'),
        "children": children,
    })
}

#[tauri::command]
fn load_tree(paths: Vec<String>) -> Result<serde_json::Value, String> {
    let mut files = Vec::new();
    let mut total = 0;
    for p in &paths {
        let store = Store::load(Path::new(p))?;
        total += store.len();
        let roots: Vec<serde_json::Value> = store.roots().iter().map(|r| node_to_json(&store, r)).collect();
        files.push(json!({"path": p, "roots": roots, "nodeCount": store.len()}));
    }
    Ok(json!({"total": total, "files": files}))
}

#[tauri::command]
fn load_graph(paths: Vec<String>) -> Result<serde_json::Value, String> {
    let mut ws = Workspace::new();
    for p in &paths {
        ws.add(p.clone(), Store::load(Path::new(p))?);
    }
    let mut nodes = Vec::new();
    let mut edges = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for (path, store) in ws.files() {
        for n in store.nodes() {
            if let Value::Reference(t) = &n.value {
                if seen.insert(n.id) {
                    nodes.push(json!({"id": n.id.to_string(), "name": n.name, "isAux": n.name.starts_with('@'), "file": path}));
                }
                if let Some((tfile, target)) = ws.find(*t) {
                    if seen.insert(target.id) {
                        nodes.push(json!({"id": target.id.to_string(), "name": target.name, "isAux": target.name.starts_with('@'), "file": tfile}));
                    }
                    edges.push(json!({"from": n.id.to_string(), "to": target.id.to_string()}));
                }
            }
        }
    }
    Ok(json!({"nodes": nodes, "edges": edges}))
}

#[tauri::command]
fn create_node(path: String, parent: String, name: String, value: String) -> Result<String, String> {
    let mut store = Store::load(Path::new(&path))?;
    let p = if parent == "nil" || parent.is_empty() {
        None
    } else {
        Some(Uuid::parse(&parent).ok_or("无效父节点 ID")?)
    };
    let n = store.create(p, &name, parse_value(&value), true);
    store.save(Path::new(&path)).map_err(|e| e.to_string())?;
    Ok(n.id.to_string())
}

#[tauri::command]
fn rename_node(path: String, node_id: String, name: String) -> Result<(), String> {
    let mut store = Store::load(Path::new(&path))?;
    let id = Uuid::parse(&node_id).ok_or("无效节点 ID")?;
    store.rename(id, name)?;
    store.save(Path::new(&path)).map_err(|e| e.to_string())
}

#[tauri::command]
fn update_node(path: String, node_id: String, value: String) -> Result<(), String> {
    let mut store = Store::load(Path::new(&path))?;
    let id = Uuid::parse(&node_id).ok_or("无效节点 ID")?;
    store.update(id, parse_value(&value))?;
    store.save(Path::new(&path)).map_err(|e| e.to_string())
}

#[tauri::command]
fn remove_node(path: String, node_id: String) -> Result<(), String> {
    let mut store = Store::load(Path::new(&path))?;
    let id = Uuid::parse(&node_id).ok_or("无效节点 ID")?;
    store.remove(id)?;
    store.save(Path::new(&path)).map_err(|e| e.to_string())
}

#[tauri::command]
fn revert_node(path: String, node_id: String) -> Result<(), String> {
    let mut store = Store::load(Path::new(&path))?;
    let id = Uuid::parse(&node_id).ok_or("无效节点 ID")?;
    store.revert(id)?;
    store.save(Path::new(&path)).map_err(|e| e.to_string())
}

#[tauri::command]
fn validate(path: String) -> Result<Vec<String>, String> {
    let store = Store::load(Path::new(&path))?;
    let errs = xirang_core::validator::validate(store.nodes());
    Ok(errs
        .iter()
        .map(|e| format!("{} <{}> {}", e.code, &e.node_id.to_string()[..8], e.message))
        .collect())
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![load_tree, load_graph, create_node, update_node, rename_node, remove_node, revert_node, validate])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
