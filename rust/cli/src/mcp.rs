//! 息壤 MCP server：把 xr 的查询 / 导入能力暴露为 MCP 工具（JSON-RPC over stdio）。
//! 供 vibecoding / AI 代理用结构化接口消费 `.xirang` 数据。

use std::io::{self, BufRead, Write};
use std::path::Path;

use serde_json::{json, Value};
use xirang_core::codec::{Node, Uuid, Value as XValue};
use xirang_core::{query, tree, validator};

mod ops;

fn type_name(v: &XValue) -> &'static str {
    match v {
        XValue::Empty => "empty",
        XValue::Int(_) => "integer",
        XValue::Float(_) => "float",
        XValue::Bool(_) => "boolean",
        XValue::Text(_) => "text",
        XValue::Reference(_) => "reference",
        XValue::Blob(_) => "blob",
    }
}

fn fmt_value(store: &tree::Store, node: &Node) -> Option<String> {
    match &node.value {
        XValue::Empty => None,
        XValue::Int(n) => Some(n.to_string()),
        XValue::Float(f) => Some(f.to_string()),
        XValue::Bool(b) => Some(if *b { "true" } else { "false" }.to_string()),
        XValue::Text(s) => Some(s.clone()),
        XValue::Reference(u) => Some(store
            .get(*u)
            .map(|t| t.name.clone())
            .unwrap_or_else(|| u.to_string())),
        XValue::Blob(b) => Some(format!("[blob {} 字节]", b.len())),
    }
}

fn node_json(store: &tree::Store, node: &Node) -> Value {
    let mut seen = std::collections::HashSet::new();
    node_json_rec(store, node, &mut seen)
}

fn node_json_rec(
    store: &tree::Store,
    node: &Node,
    seen: &mut std::collections::HashSet<Uuid>,
) -> Value {
    // 父边成环（E006）时兜底：同一节点只展开一次。
    let children: Vec<Value> = if seen.insert(node.id) {
        store
            .children(node)
            .into_iter()
            .map(|c| node_json_rec(store, c, seen))
            .collect()
    } else {
        Vec::new()
    };
    json!({
        "id": node.id.to_string(),
        "name": node.name,
        "type": type_name(&node.value),
        "value": fmt_value(store, node),
        "isAux": node.name.starts_with('@'),
        "children": children,
    })
}

fn load_store(path: &str) -> Result<tree::Store, String> {
    tree::Store::load(Path::new(path)).map_err(|e| e.to_string())
}

// ============================================================================
// 工具实现（返回 Value 结果对象；出错返回 Err(String)）
// ============================================================================

fn cmd_info(args: &Value) -> Result<Value, String> {
    let file = args["file"].as_str().ok_or("缺 file")?;
    let store = load_store(file)?;
    Ok(json!({
        "file": file,
        "nodeCount": store.len(),
        "rootCount": store.roots().len(),
    }))
}

fn cmd_tree(args: &Value) -> Result<Value, String> {
    let file = args["file"].as_str().ok_or("缺 file")?;
    let store = load_store(file)?;
    if let Some(id) = args["node"].as_str() {
        let uuid = Uuid::parse(id).ok_or("无效节点 ID")?;
        let node = store.get(uuid).ok_or("节点不存在")?;
        return Ok(node_json(&store, node));
    }
    let roots: Vec<Value> = store
        .roots()
        .into_iter()
        .map(|r| node_json(&store, r))
        .collect();
    Ok(json!(roots))
}

fn cmd_find(args: &Value) -> Result<Value, String> {
    let file = args["file"].as_str().ok_or("缺 file")?;
    let pattern = args["pattern"].as_str().ok_or("缺 pattern")?;
    let store = load_store(file)?;
    let hits: Vec<Value> = store
        .nodes()
        .iter()
        .filter(|n| {
            let text_val = match &n.value {
                XValue::Text(s) => s.clone(),
                XValue::Int(i) => i.to_string(),
                XValue::Float(f) => f.to_string(),
                XValue::Bool(b) => if *b { "true" } else { "false" }.to_string(),
                _ => String::new(),
            };
            n.name.contains(pattern) || text_val.contains(pattern)
        })
        .map(|n| json!({"id": n.id.to_string(), "name": n.name, "type": type_name(&n.value), "value": fmt_value(&store, n)}))
        .collect();
    Ok(json!(hits))
}

fn cmd_match(args: &Value) -> Result<Value, String> {
    let file = args["file"].as_str().ok_or("缺 file")?;
    let store = load_store(file)?;
    let sindex = query::ShapeIndex::build(&store);
    // where：对象 {路径: 值}
    let mut wheres: Vec<(String, String)> = Vec::new();
    if let Some(wh) = args.get("where").and_then(|v| v.as_object()) {
        for (k, v) in wh {
            wheres.push((k.clone(), v.as_str().unwrap_or("").to_string()));
        }
    }
    let conds: Vec<(&str, &str)> = wheres.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let cands: Vec<usize> = if let Some(t) = args["template"].as_str() {
        let tid = ops::resolve_template(&store, t).map_err(|e| e.to_string())?;
        ops::find_instances_of(&store, tid)
    } else if let Some(r) = args["root"].as_str() {
        query::name_index(&store).get(r).cloned().unwrap_or_default()
    } else if let Some(so) = args["shape_of"].as_str() {
        let sid = Uuid::parse(so).ok_or("无效 shape_of")?;
        let idx = store.nodes().iter().position(|x| x.id == sid).ok_or("节点不存在")?;
        query::by_shape(&sindex, sindex.shapes[idx])
    } else {
        return Err("需要 root / shape_of / template 之一".into());
    };
    let arr: Vec<Value> = cands
        .into_iter()
        .filter(|&i| query::matches_where(&store, i, &conds))
        .map(|i| {
            let nd = &store.nodes()[i];
            json!({"id": nd.id.to_string(), "name": nd.name, "type": type_name(&nd.value), "value": fmt_value(&store, nd)})
        })
        .collect();
    Ok(json!(arr))
}

fn cmd_instances(args: &Value) -> Result<Value, String> {
    let file = args["file"].as_str().ok_or("缺 file")?;
    let name = args["template"].as_str().ok_or("缺 template")?;
    let store = load_store(file)?;
    let tid = ops::resolve_template(&store, name).map_err(|e| e.to_string())?;
    let idxs = ops::find_instances_of(&store, tid);
    let arr: Vec<Value> = idxs
        .into_iter()
        .map(|i| node_json(&store, &store.nodes()[i]))
        .collect();
    Ok(json!(arr))
}

fn cmd_validate(args: &Value) -> Result<Value, String> {
    let file = args["file"].as_str().ok_or("缺 file")?;
    let store = load_store(file)?;
    let errs = validator::validate(store.nodes());
    let arr: Vec<Value> = errs
        .into_iter()
        .map(|e| json!({"code": e.code, "node": e.node_id.to_string(), "message": e.message}))
        .collect();
    Ok(json!({"count": arr.len(), "errors": arr}))
}

fn cmd_diff(args: &Value) -> Result<Value, String> {
    let a = args["a"].as_str().ok_or("缺 a")?;
    let b = args["b"].as_str().ok_or("缺 b")?;
    let sa = load_store(a)?;
    let sb = load_store(b)?;
    let map_a: std::collections::HashMap<Uuid, &Node> = sa.nodes().iter().map(|n| (n.id, n)).collect();
    let map_b: std::collections::HashMap<Uuid, &Node> = sb.nodes().iter().map(|n| (n.id, n)).collect();
    let mut added = Vec::new();
    let mut removed = Vec::new();
    let mut changed = Vec::new();
    for (id, n) in &map_a {
        match map_b.get(id) {
            None => removed.push(json!({"id": id.to_string(), "name": n.name})),
            Some(m) if m.name != n.name || m.value != n.value => {
                changed.push(json!({"id": id.to_string(), "from": { "name": n.name, "value": fmt_value(&sa, n) }, "to": { "name": m.name, "value": fmt_value(&sb, m) }}));
            }
            _ => {}
        }
    }
    for (id, n) in &map_b {
        if !map_a.contains_key(id) {
            added.push(json!({"id": id.to_string(), "name": n.name}));
        }
    }
    Ok(json!({"added": added, "removed": removed, "changed": changed}))
}

fn cmd_import_template(args: &Value) -> Result<Value, String> {
    let file = args["file"].as_str().ok_or("缺 file")?;
    let name = args["template"].as_str().ok_or("缺 template")?;
    let data = args["data"].as_array().ok_or("缺 data（记录数组）")?;
    let mut store = load_store(file)?;
    let tpl_id = ops::resolve_template(&store, name).map_err(|e| e.to_string())?;
    for rec in data {
        ops::instantiate(&mut store, tpl_id, None, rec)?;
    }
    store.save(Path::new(file)).map_err(|e| e.to_string())?;
    Ok(json!({"imported": data.len()}))
}

/// 前 N 个节点（按文件里的存放顺序，扁平列出）——大文件时用它替代整棵 `tree`。
fn cmd_head_nodes(args: &Value) -> Result<Value, String> {
    let file = args["file"].as_str().ok_or("缺 file")?;
    let n = args["n"].as_u64().unwrap_or(200) as usize;
    let store = load_store(file)?;
    let total = store.nodes().len();
    let nodes: Vec<Value> = store
        .nodes()
        .iter()
        .take(n)
        .map(|nd| {
            json!({
                "id": nd.id.to_string(),
                "parent": nd.parent.map(|p| p.to_string()),
                "name": nd.name,
                "type": type_name(&nd.value),
                "value": fmt_value(&store, nd),
            })
        })
        .collect();
    Ok(json!({"nodes": nodes, "total": total, "truncated": total > n}))
}

/// 改名（写入文件）：编号不变、引用不断，旧名字进 @history。
fn cmd_rename_node(args: &Value) -> Result<Value, String> {
    let file = args["file"].as_str().ok_or("缺 file")?;
    let node = args["node"].as_str().ok_or("缺 node")?;
    let name = args["name"].as_str().ok_or("缺 name")?;
    if name.is_empty() {
        return Err("name 不能为空（要清空名字请用 remove 语义）".into());
    }
    let id = Uuid::parse(node).ok_or("无效节点 ID")?;
    let mut store = load_store(file)?;
    store.rename(id, name.to_string())?;
    store.save(Path::new(file)).map_err(|e| e.to_string())?;
    Ok(json!({"renamed": node, "name": name}))
}

// ============================================================================
// 工具定义（JSON Schema）
// ============================================================================

fn tool_defs() -> Vec<Value> {
    vec![
        json!({"name":"info","description":"息壤文件摘要（节点数/根数）","inputSchema":{"type":"object","properties":{"file":{"type":"string","description":".xirang 路径"}},"required":["file"]}}),
        json!({"name":"tree","description":"输出一棵子树的结构（嵌套，含名字/值/引用）","inputSchema":{"type":"object","properties":{"file":{"type":"string"},"node":{"type":"string","description":"节点 ID，可省略（默认全部根）"}},"required":["file"]}}),
        json!({"name":"find","description":"按名字/文本值搜索节点","inputSchema":{"type":"object","properties":{"file":{"type":"string"},"pattern":{"type":"string"}},"required":["file","pattern"]}}),
        json!({"name":"match","description":"按结构/名字/值匹配树。root=按根名，shape_of=按示例节点形状，template=按模板实例","inputSchema":{"type":"object","properties":{"file":{"type":"string"},"root":{"type":"string"},"shape_of":{"type":"string"},"template":{"type":"string"},"where":{"type":"object","description":"值约束，如 {\"词形\":\"火\"}"}},"required":["file"]}}),
        json!({"name":"instances","description":"列出某模板的所有实例（按 @模板 引用定位，可挂任意父下）","inputSchema":{"type":"object","properties":{"file":{"type":"string"},"template":{"type":"string"}},"required":["file","template"]}}),
        json!({"name":"validate","description":"结构校验（E/R 错误）","inputSchema":{"type":"object","properties":{"file":{"type":"string"}},"required":["file"]}}),
        json!({"name":"diff","description":"对比两个文件（增/删/改，按节点编号）","inputSchema":{"type":"object","properties":{"a":{"type":"string"},"b":{"type":"string"}},"required":["a","b"]}}),
        json!({"name":"import_template","description":"按模板批量导入实例（写入文件）","inputSchema":{"type":"object","properties":{"file":{"type":"string"},"template":{"type":"string"},"data":{"type":"array","description":"记录对象数组"}},"required":["file","template","data"]}}),
        json!({"name":"rename_node","description":"给节点改名（写入文件）。编号不变、引用不断，旧名字进 @history","inputSchema":{"type":"object","properties":{"file":{"type":"string","description":".xirang 路径"},"node":{"type":"string","description":"节点 ID"},"name":{"type":"string","description":"新名字（非空，≤255 字节）"}},"required":["file","node","name"]}}),
        json!({"name":"head_nodes","description":"按存放顺序返回前 n 个节点（扁平列表）。文件很大时用它替代整棵 tree，避免一次吐出几百万个节点","inputSchema":{"type":"object","properties":{"file":{"type":"string","description":".xirang 路径"},"n":{"type":"integer","description":"要取多少个节点，默认 200"}},"required":["file"]}}),
    ]
}

// ============================================================================
// 主循环：JSON-RPC over stdio
// ============================================================================

fn main() {
    let stdin = io::stdin();
    let mut stdout = io::stdout();
    let lines = stdin.lock().lines();
    for line in lines {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        if line.trim().is_empty() {
            continue;
        }
        let msg: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let method = msg["method"].as_str().unwrap_or("");
        if method.starts_with("notifications/") {
            continue; // 通知不回
        }
        let id = msg.get("id").cloned();
        // JSON-RPC 2.0：方法级错误必须是顶层 `error` 成员（不能塞进 result）；
        // 工具内部的业务错误才走 result.isError。
        let (result, rpc_error): (Option<Value>, Option<Value>) = match method {
            "initialize" => (Some(json!({
                "protocolVersion": "2024-11-05",
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "xirang", "version": "0.1.0" },
            })), None),
            "ping" => (Some(json!({})), None),
            "tools/list" => (Some(json!({ "tools": tool_defs() })), None),
            "tools/call" => {
                let name = msg["params"]["name"].as_str().unwrap_or("");
                let args = msg["params"]["arguments"].clone();
                let res: Result<Value, String> = match name {
                    "info" => cmd_info(&args),
                    "tree" => cmd_tree(&args),
                    "find" => cmd_find(&args),
                    "match" => cmd_match(&args),
                    "instances" => cmd_instances(&args),
                    "validate" => cmd_validate(&args),
                    "diff" => cmd_diff(&args),
                    "import_template" => cmd_import_template(&args),
                    "rename_node" => cmd_rename_node(&args),
                    "head_nodes" => cmd_head_nodes(&args),
                    _ => Err(format!("未知工具：{name}")),
                };
                let v = match res {
                    Ok(content) => json!({ "content": [{ "type": "text", "text": content.to_string() }] }),
                    Err(e) => json!({ "isError": true, "content": [{ "type": "text", "text": format!("错误：{e}") }] }),
                };
                (Some(v), None)
            }
            _ => (None, Some(json!({ "code": -32601, "message": format!("未知方法：{method}") }))),
        };
        if let Some(id) = id {
            let resp = match rpc_error {
                Some(e) => json!({ "jsonrpc": "2.0", "id": id, "error": e }),
                None => json!({ "jsonrpc": "2.0", "id": id, "result": result.unwrap_or(Value::Null) }),
            };
            writeln!(stdout, "{}", resp).ok();
        }
    }
}
