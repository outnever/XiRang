//! 格式转换：Store ↔ JSON / YAML（无损往返）、Markdown（有损导出）。
//! XML 后续补充。错误码 C001/C002/C003/C005 见 errors/错误列表.md。

use base64::Engine as _;
use serde_json::{json, Value as Json};

use crate::codec::{self, Node, Uuid, Value};
use crate::tree::Store;

pub const KERNEL: &str = "1.0";

const TYPE_NAMES: [(&str, u8); 7] = [
    ("empty", codec::EMPTY),
    ("integer", codec::INT),
    ("float", codec::FLOAT),
    ("boolean", codec::BOOL),
    ("text", codec::TEXT),
    ("reference", codec::REFERENCE),
    ("blob", codec::BLOB),
];

fn name_to_tag(name: &str) -> Option<u8> {
    TYPE_NAMES.iter().find(|(n, _)| *n == name).map(|(_, t)| *t)
}

fn value_to_json(v: &Value) -> Json {
    match v {
        Value::Empty => json!({"type": "empty", "value": null}),
        Value::Int(n) => json!({"type": "integer", "value": n}),
        Value::Float(f) => json!({"type": "float", "value": f}),
        Value::Bool(b) => json!({"type": "boolean", "value": b}),
        Value::Text(s) => json!({"type": "text", "value": s}),
        Value::Reference(u) => json!({"type": "reference", "value": u.to_string()}),
        Value::Blob(b) => json!({
            "type": "blob",
            "value": base64::engine::general_purpose::STANDARD.encode(b),
            "encoding": "base64",
        }),
    }
}

fn value_from_json(j: &Json) -> Result<Value, String> {
    let name = j
        .get("type")
        .and_then(|t| t.as_str())
        .ok_or("C001：类型名非法（缺 type）")?;
    let tag = name_to_tag(name).ok_or_else(|| format!("C001：类型名非法（{name}）"))?;
    match tag {
        codec::EMPTY => Ok(Value::Empty),
        codec::INT => j
            .get("value")
            .and_then(|v| v.as_i64())
            .map(Value::Int)
            .ok_or_else(|| "C001：整数类型值非法".into()),
        codec::FLOAT => j
            .get("value")
            .and_then(|v| v.as_f64())
            .map(Value::Float)
            .ok_or_else(|| "C001：浮点类型值非法".into()),
        codec::BOOL => j
            .get("value")
            .and_then(|v| v.as_bool())
            .map(Value::Bool)
            .ok_or_else(|| "C001：布尔类型值非法".into()),
        codec::TEXT => j
            .get("value")
            .and_then(|v| v.as_str())
            .map(|s| Value::Text(s.into()))
            .ok_or_else(|| "C001：文本类型值非法".into()),
        codec::REFERENCE => {
            let s = j
                .get("value")
                .and_then(|v| v.as_str())
                .ok_or("C002：UUID 非法")?;
            Uuid::parse(s)
                .map(Value::Reference)
                .ok_or_else(|| format!("C002：UUID 非法（{s}）"))
        }
        codec::BLOB => {
            let s = j
                .get("value")
                .and_then(|v| v.as_str())
                .ok_or("C003：二进制块编码非法")?;
            base64::engine::general_purpose::STANDARD
                .decode(s)
                .map(Value::Blob)
                .map_err(|_| "C003：二进制块编码非法（Base64 解码失败）".into())
        }
        _ => Err("C001：类型名非法".into()),
    }
}

/// 整库 → 扁平节点数组（含编号 / 父编号 / 值类型）。
fn flat_nodes(store: &Store) -> Vec<Json> {
    store
        .nodes()
        .iter()
        .map(|n| {
            json!({
                "id": n.id.to_string(),
                "parent": n.parent.map(|p| p.to_string()),
                "name": n.name,
                "value": value_to_json(&n.value),
            })
        })
        .collect()
}

/// 扁平节点数组 → Store。字段缺失报 C005，UUID 非法报 C002。
fn flat_to_store(nodes: &[Json]) -> Result<Store, String> {
    let mut store = Store::new();
    for jn in nodes {
        let id = Uuid::parse(
            jn.get("id")
                .and_then(|v| v.as_str())
                .ok_or("C005：节点字段缺失（id）")?,
        )
        .ok_or("C002：UUID 非法（id）")?;
        let name = jn
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or("C005：节点字段缺失（name）")?;
        let value = jn.get("value").ok_or("C005：节点字段缺失（value）")?;
        let parent = match jn.get("parent").and_then(|v| v.as_str()) {
            None => None,
            Some(s) => Some(Uuid::parse(s).ok_or_else(|| format!("C002：UUID 非法（parent={s}）"))?),
        };
        store.add(Node {
            id,
            parent,
            name: name.into(),
            value: value_from_json(value)?,
        });
    }
    Ok(store)
}

/// 无损导出：整库 → JSON 字符串。
pub fn to_json(store: &Store) -> String {
    serde_json::to_string_pretty(&json!({"format": "xirang", "kernel": KERNEL, "nodes": flat_nodes(store)})).unwrap()
}

/// 无损导入：JSON 字符串 → Store。
pub fn from_json(text: &str) -> Result<Store, String> {
    let root: Json = serde_json::from_str(text).map_err(|e| format!("JSON 解析失败：{e}"))?;
    let nodes = root.get("nodes").and_then(|n| n.as_array()).ok_or("C005：缺 nodes 数组")?;
    flat_to_store(nodes)
}

/// 无损导出：整库 → YAML 字符串。
pub fn to_yaml(store: &Store) -> String {
    serde_yaml::to_string(&json!({"format": "xirang", "kernel": KERNEL, "nodes": flat_nodes(store)})).unwrap()
}

/// 无损导入：YAML 字符串 → Store。
pub fn from_yaml(text: &str) -> Result<Store, String> {
    let root: Json = serde_yaml::from_str(text).map_err(|e| format!("YAML 解析失败：{e}"))?;
    let nodes = root.get("nodes").and_then(|n| n.as_array()).ok_or("C005：缺 nodes 数组")?;
    flat_to_store(nodes)
}

// —— Markdown（有损，只导出）——

fn md_value(store: &Store, node: &Node) -> String {
    match &node.value {
        Value::Empty => String::new(),
        Value::Int(n) => n.to_string(),
        Value::Float(f) => f.to_string(),
        Value::Bool(b) => if *b { "true" } else { "false" }.to_string(),
        Value::Text(s) => s.clone(),
        Value::Reference(u) => match store.get(*u) {
            Some(t) => format!("→ {}", t.name),
            None => format!("→ {u}"),
        },
        Value::Blob(b) => format!("[二进制块] {} 字节", b.len()),
    }
}

fn md_node(
    store: &Store,
    node: &Node,
    lines: &mut Vec<String>,
    level: usize,
    seen: &mut std::collections::HashSet<crate::codec::Uuid>,
) {
    lines.push(String::new());
    // 父边成环（E006）时兜底：同一节点只展开一次。
    if !seen.insert(node.id) {
        lines.push(format!("{} （父边成环，已跳过）", "#".repeat(level)));
        lines.push(String::new());
        return;
    }
    let name = if node.name.is_empty() { "(未命名)" } else { node.name.as_str() };
    lines.push(format!("{} {}", "#".repeat(level), name));
    let v = md_value(store, node);
    if !v.is_empty() {
        lines.push(String::new());
        lines.push(v);
    }
    let children = store.children(node);
    if !children.is_empty() {
        lines.push(String::new());
        for c in children {
            if !store.children(c).is_empty() {
                md_node(store, c, lines, level + 1, seen);
            } else {
                let cname = if c.name.is_empty() { "(未命名)" } else { c.name.as_str() };
                lines.push(format!("- **{cname}**: {}", md_value(store, c)));
            }
        }
    }
    lines.push(String::new());
}

/// 有损导出：整库 → Markdown（名字当键、引用退化成名字）。
pub fn to_md(store: &Store) -> String {
    let mut lines = vec!["# 息壤树".to_string()];
    let mut seen = std::collections::HashSet::new();
    for root in store.roots() {
        md_node(store, root, &mut lines, 2, &mut seen);
    }
    format!("{}\n", lines.join("\n").trim_end())
}

// —— XML（无损往返）——

fn tag_to_name(tag: u8) -> &'static str {
    TYPE_NAMES.iter().find(|(_, t)| *t == tag).map(|(n, _)| *n).unwrap_or("empty")
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn value_text(v: &Value) -> String {
    match v {
        Value::Empty => String::new(),
        Value::Int(n) => n.to_string(),
        Value::Float(f) => f.to_string(),
        Value::Bool(b) => if *b { "true" } else { "false" }.to_string(),
        Value::Text(s) => s.clone(),
        Value::Reference(u) => u.to_string(),
        Value::Blob(_) => String::new(),
    }
}

/// 无损导出：整库 → XML 字符串。
pub fn to_xml(store: &Store) -> String {
    let mut out = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    out.push_str(&format!("<xirang kernel=\"{}\">", KERNEL));
    for n in store.nodes() {
        out.push_str(&format!("<node id=\"{}\"", n.id));
        if let Some(p) = n.parent {
            out.push_str(&format!(" parent=\"{}\"", p));
        }
        out.push_str("><name>");
        out.push_str(&xml_escape(&n.name));
        out.push_str("</name><value type=\"");
        out.push_str(tag_to_name(n.value.tag()));
        out.push('"');
        match &n.value {
            Value::Blob(b) => {
                out.push_str(" encoding=\"base64\">");
                out.push_str(&base64::engine::general_purpose::STANDARD.encode(b));
            }
            _ => {
                out.push('>');
                out.push_str(&xml_escape(&value_text(&n.value)));
            }
        }
        out.push_str("</value></node>");
    }
    out.push_str("</xirang>");
    out
}

fn xml_unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/// 无损导入：XML 字符串 → Store。
pub fn from_xml(text: &str) -> Result<Store, String> {
    use quick_xml::events::Event;
    use quick_xml::Reader;

    let mut reader = Reader::from_str(text);
    // 不要 trim_text：元素之间的空白由 in_name/in_value 自然忽略，
    // 但名字/值内部的首尾空白必须保留（否则「XML 无损往返」对带空格的文本不成立）。

    let mut store = Store::new();
    let mut cur_id: Option<Uuid> = None;
    let mut cur_parent: Option<Uuid> = None;
    let mut in_name = false;
    let mut in_value = false;
    let mut name_buf = String::new();
    let mut value_type = String::new();
    let mut value_buf = String::new();
    let mut is_blob = false;

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => match e.name().as_ref() {
                "node" => {
                    cur_id = None;
                    cur_parent = None;
                    for a in e.attributes() {
                        let a = a.map_err(|err| format!("XML 属性错误：{err}"))?;
                        match a.key.as_ref() {
                            "id" => cur_id = Uuid::parse(a.value.as_ref()),
                            "parent" => cur_parent = Uuid::parse(a.value.as_ref()),
                            _ => {}
                        }
                    }
                }
                "name" => {
                    in_name = true;
                    name_buf.clear();
                }
                "value" => {
                    in_value = true;
                    value_type.clear();
                    value_buf.clear();
                    is_blob = false;
                    for a in e.attributes() {
                        let a = a.map_err(|err| format!("XML 属性错误：{err}"))?;
                        match a.key.as_ref() {
                            "type" => value_type = a.value.as_ref().to_string(),
                            "encoding" => is_blob = true,
                            _ => {}
                        }
                    }
                }
                _ => {}
            },
            Ok(Event::Text(t)) => {
                let txt = xml_unescape(t.as_ref());
                if in_name {
                    name_buf.push_str(&txt);
                } else if in_value {
                    value_buf.push_str(&txt);
                }
            }
            Ok(Event::End(e)) => match e.name().as_ref() {
                "name" => in_name = false,
                "value" => in_value = false,
                "node" => {
                    let id = cur_id.ok_or("C005：节点字段缺失（id）")?;
                    let name = std::mem::take(&mut name_buf);
                    let tag = name_to_tag(&value_type)
                        .ok_or_else(|| format!("C001：类型名非法（{value_type}）"))?;
                    let value = match tag {
                        codec::BLOB => {
                            if is_blob {
                                Value::Blob(
                                    base64::engine::general_purpose::STANDARD
                                        .decode(&value_buf)
                                        .map_err(|_| "C003：二进制块编码非法".to_string())?,
                                )
                            } else {
                                Value::Blob(Vec::new())
                            }
                        }
                        codec::REFERENCE => {
                            Value::Reference(Uuid::parse(&value_buf).ok_or("C002：UUID 非法")?)
                        }
                        codec::EMPTY => Value::Empty,
                        codec::INT => Value::Int(
                            value_buf.parse().map_err(|_| "C001：整数类型值非法".to_string())?,
                        ),
                        codec::FLOAT => Value::Float(
                            value_buf.parse().map_err(|_| "C001：浮点类型值非法".to_string())?,
                        ),
                        codec::BOOL => Value::Bool(value_buf == "true"),
                        _ => Value::Text(value_buf.clone()),
                    };
                    store.add(Node { id, parent: cur_parent, name, value });
                }
                _ => {}
            },
            Ok(Event::Eof) => break,
            Err(e) => return Err(format!("XML 解析失败：{e}")),
            _ => {}
        }
    }
    Ok(store)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u(n: u8) -> Uuid {
        let mut b = [0u8; 16];
        b[15] = n;
        Uuid(b)
    }

    fn build() -> Store {
        let mut s = Store::new();
        s.add(Node { id: u(1), parent: None, name: "root".into(), value: Value::Empty });
        s.add(Node { id: u(2), parent: Some(u(1)), name: "text".into(), value: Value::Text("灯".into()) });
        s.add(Node { id: u(3), parent: Some(u(1)), name: "num".into(), value: Value::Int(2046) });
        s.add(Node { id: u(4), parent: Some(u(1)), name: "ref".into(), value: Value::Reference(u(3)) });
        s.add(Node { id: u(5), parent: Some(u(1)), name: "bin".into(), value: Value::Blob(vec![0, 255, 18]) });
        s
    }

    #[test]
    fn json_roundtrip() {
        let s = build();
        let text = to_json(&s);
        let d = from_json(&text).unwrap();
        assert_eq!(d.len(), 5);
        assert_eq!(d.nodes()[1].value, Value::Text("灯".into()));
        assert_eq!(d.nodes()[3].value, Value::Reference(u(3)));
        assert_eq!(d.nodes()[4].value, Value::Blob(vec![0, 255, 18]));
    }

    #[test]
    fn json_contains_id_and_type() {
        let s = build();
        let text = to_json(&s);
        assert!(text.contains("\"kernel\": \"1.0\""));
        assert!(text.contains("\"type\": \"reference\""));
    }

    #[test]
    fn c001_bad_type() {
        let err = from_json(r#"{"nodes":[{"id":"00000000000000000000000000000001","name":"x","value":{"type":"string","value":"y"}}]}"#).unwrap_err();
        assert!(err.contains("C001"));
    }

    #[test]
    fn c002_bad_uuid() {
        let err = from_json(r#"{"nodes":[{"id":"abc","name":"x","value":{"type":"empty","value":null}}]}"#).unwrap_err();
        assert!(err.contains("C002"));
    }

    #[test]
    fn c005_missing_value() {
        let err = from_json(r#"{"nodes":[{"id":"00000000000000000000000000000001","name":"x"}]}"#).unwrap_err();
        assert!(err.contains("C005"));
    }

    #[test]
    fn yaml_roundtrip() {
        let s = build();
        let text = to_yaml(&s);
        let d = from_yaml(&text).unwrap();
        assert_eq!(d.len(), 5);
        assert_eq!(d.nodes()[1].value, Value::Text("灯".into()));
    }

    #[test]
    fn md_export_lossy() {
        let s = build();
        let md = to_md(&s);
        assert!(md.starts_with("# 息壤树"));
        assert!(md.contains("## root"));
        assert!(md.contains("**text**: 灯"));
        assert!(md.contains("→ num")); // 引用退化成目标名
    }

    #[test]
    fn xml_roundtrip() {
        let s = build();
        let xml = to_xml(&s);
        let d = from_xml(&xml).unwrap();
        assert_eq!(d.len(), 5);
        assert_eq!(d.nodes()[1].value, Value::Text("灯".into()));
        assert_eq!(d.nodes()[3].value, Value::Reference(u(3)));
        assert_eq!(d.nodes()[4].value, Value::Blob(vec![0, 255, 18]));
    }
}
