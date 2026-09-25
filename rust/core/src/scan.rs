//! 流式扫描：按字节分块读源文件、逐条解码，**不整份载入**。
//!
//! 两个用途：
//! - 搜索：按名字 / 值 / 类型找节点（`append-v1` 文件按「后写覆盖」取最后一条记录）
//! - 校验：扫出编号集合、父边、引用与重复编号，只在内存里放一个编号表

use std::collections::{HashMap, HashSet};
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::codec::{self, Node, Uuid, Value};
use crate::tree;

/// 每次读的字节数（1 MB）。
const CHUNK: usize = 1 << 20;

/// 顺序扫一遍节点流。`visit` 返回 false 表示提前停止（用户取消）。
/// 返回值 = 扫过的记录条数。
pub fn scan_file<F>(path: &Path, cancel: &AtomicBool, mut visit: F) -> Result<u64, String>
where
    F: FnMut(Node) -> bool,
{
    let mut file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut head = [0u8; 9];
    file.read_exact(&mut head).map_err(|e| e.to_string())?;
    // 头是变长的：把「魔数 + 版本 + 头长 + 头文本」整段读出来才算得出节点数据起点
    let hlen = u32::from_be_bytes(head[5..9].try_into().unwrap()) as usize;
    let mut header = vec![0u8; 9 + hlen];
    header[..9].copy_from_slice(&head);
    file.read_exact(&mut header[9..]).map_err(|e| e.to_string())?;
    let data_start = tree::node_data_start(&header).map_err(|e| e.to_string())?;
    file.seek(SeekFrom::Start(data_start))
        .map_err(|e| e.to_string())?;

    let mut carry: Vec<u8> = Vec::new();
    let mut buf = vec![0u8; CHUNK];
    let mut count = 0u64;
    loop {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let n = file.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        let mut data = std::mem::take(&mut carry);
        data.extend_from_slice(&buf[..n]);

        let mut off = 0usize;
        while off < data.len() {
            let start = off;
            match codec::decode_node(&data, &mut off) {
                Ok(node) => {
                    count += 1;
                    if !visit(node) {
                        return Ok(count);
                    }
                }
                Err(codec::Error::Truncated) => {
                    off = start; // 半条记录：留到下一块
                    break;
                }
                Err(e) => return Err(format!("{e:?}")),
            }
        }
        carry = data[off..].to_vec();
    }
    Ok(count)
}

/// 搜索条件。
#[derive(Clone, Debug, PartialEq)]
pub enum Query {
    /// 名字包含（大小写敏感）
    Name(String),
    /// 文本值包含
    Value(String),
    /// 按 7 大类的类型名（空 / 整数 / 浮点数 / 布尔 / 文本 / 引用 / 二进制块）
    Kind(String),
    /// 名字**或**文本值包含（CLI `xr find` 的语义）
    NameOrText(String),
}

pub fn kind_name(v: &Value) -> &'static str {
    match v {
        Value::Empty => "空",
        Value::Int(_) => "整数",
        Value::Float(_) => "浮点数",
        Value::Bool(_) => "布尔",
        Value::Text(_) => "文本",
        Value::Reference(_) => "引用",
        Value::Blob(_) => "二进制块",
    }
}

fn matches(node: &Node, q: &Query) -> bool {
    match q {
        Query::Name(s) => !s.is_empty() && node.name.contains(s.as_str()),
        Query::Value(s) => match &node.value {
            Value::Text(t) => !s.is_empty() && t.contains(s.as_str()),
            _ => false,
        },
        Query::Kind(k) => kind_name(&node.value) == k,
        Query::NameOrText(s) => {
            !s.is_empty()
                && (node.name.contains(s.as_str())
                    || matches!(&node.value, Value::Text(t) if t.contains(s.as_str())))
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Hit {
    pub id: Uuid,
    pub name: String,
    pub value: String,
}

/// 搜索：整份扫一遍，`append-v1` 的重复编号按「后写覆盖」（最后一条说了算）。
pub fn search(path: &Path, query: &Query, cancel: &AtomicBool) -> Result<Vec<Hit>, String> {
    let mut hits: HashMap<Uuid, Hit> = HashMap::new();
    scan_file(path, cancel, |node| {
        if matches(&node, query) {
            hits.insert(
                node.id,
                Hit {
                    id: node.id,
                    name: node.name.clone(),
                    value: value_brief(&node.value),
                },
            );
        } else {
            hits.remove(&node.id); // 后写覆盖：最新记录不匹配就撤掉旧的命中
        }
        true
    })?;
    let mut out: Vec<Hit> = hits.into_values().collect();
    out.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.id.0.cmp(&b.id.0)));
    Ok(out)
}

/// 收集匹配的**原始节点**（同编号取最后一条记录）——CLI 的 `find` 用它。
pub fn collect(path: &Path, query: &Query, cancel: &AtomicBool) -> Result<Vec<Node>, String> {
    let mut hits: HashMap<Uuid, Node> = HashMap::new();
    scan_file(path, cancel, |node| {
        if matches(&node, query) {
            hits.insert(node.id, node);
        } else {
            hits.remove(&node.id);
        }
        true
    })?;
    let mut out: Vec<Node> = hits.into_values().collect();
    out.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.id.0.cmp(&b.id.0)));
    Ok(out)
}

fn value_brief(v: &Value) -> String {
    match v {
        Value::Empty => String::new(),
        Value::Int(n) => n.to_string(),
        Value::Float(f) => f.to_string(),
        Value::Bool(b) => (if *b { "true" } else { "false" }).to_string(),
        Value::Text(t) => {
            let mut s: String = t.chars().take(60).collect();
            if t.chars().count() > 60 {
                s.push('…');
            }
            s
        }
        Value::Reference(t) => format!("→ {t}"),
        Value::Blob(b) => format!("[blob {} 字节]", b.len()),
    }
}

/// 校验结果（与 `validator::Error` 同形，便于界面统一展示）。
#[derive(Clone, Debug, PartialEq)]
pub struct Issue {
    pub code: &'static str,
    pub node_id: Uuid,
    pub message: String,
}

/// 流式校验：只用「编号 → 父节点」一张表 + 声明了 `append-v1` 的根集合。
///
/// 与 `validator::validate_view` 语义一致：声明了修订协议的根下重复编号是修订，不报 E002。
pub fn validate_stream(
    path: &Path,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(u64),
) -> Result<Vec<Issue>, String> {
    // 第一遍：编号表 + 重复编号 + 声明了 append-v1 的根 + E001/E005
    let mut parents: HashMap<Uuid, Option<Uuid>> = HashMap::new();
    let mut counts: HashMap<Uuid, u32> = HashMap::new();
    let mut declared: HashSet<Uuid> = HashSet::new();
    let mut issues = Vec::new();
    scan_file(path, cancel, |node| {
        if node.id.is_nil() {
            issues.push(Issue {
                code: "E001",
                node_id: node.id,
                message: "节点编号缺失（为 nil）".into(),
            });
        }
        if node.parent == Some(node.id) {
            issues.push(Issue {
                code: "E005",
                node_id: node.id,
                message: "父节点指向自己".into(),
            });
        }
        counts.entry(node.id).and_modify(|c| *c += 1).or_insert(1);
        if node.name == "@protocol" && node.value == Value::Text(tree::PROTOCOL_APPEND.into()) {
            if let Some(root) = node.parent {
                declared.insert(root);
            }
        }
        parents.insert(node.id, node.parent);
        progress(parents.len() as u64);
        true
    })?;
    if cancel.load(Ordering::Relaxed) {
        return Ok(Vec::new());
    }

    // 父节点成环（E006）：沿父链走，走过的点做标记，避免重复报
    {
        let mut state: HashMap<Uuid, u8> = HashMap::new(); // 1 = 在路径上, 2 = 已确认不在环里
        for start in parents.keys().copied().collect::<Vec<_>>() {
            if state.get(&start).copied().unwrap_or(0) == 2 {
                continue;
            }
            let mut path: Vec<Uuid> = Vec::new();
            let mut cur = Some(start);
            while let Some(id) = cur {
                match state.get(&id).copied().unwrap_or(0) {
                    1 => {
                        issues.push(Issue {
                            code: "E006",
                            node_id: id,
                            message: "父节点成环".into(),
                        });
                        break;
                    }
                    2 => break,
                    _ => {}
                }
                state.insert(id, 1);
                path.push(id);
                cur = parents.get(&id).copied().flatten();
            }
            for id in path {
                state.insert(id, 2);
            }
        }
    }

    // 重复编号：按「所属根是否声明了修订协议」判定
    for (id, c) in counts.iter() {
        if *c > 1 && !declared.contains(&root_of(&parents, *id)) {
            issues.push(Issue {
                code: "E002",
                node_id: *id,
                message: "编号冲突（与另一节点相同）".into(),
            });
        }
    }

    // 第二遍：父边 / 引用是否断裂
    scan_file(path, cancel, |node| {
        if let Some(Some(p)) = parents.get(&node.id) {
            if !parents.contains_key(p) {
                issues.push(Issue {
                    code: "E011",
                    node_id: node.id,
                    message: format!("父节点断裂：{p} 不存在"),
                });
            }
        }
        if let Value::Reference(t) = &node.value {
            if !parents.contains_key(t) {
                issues.push(Issue {
                    code: "R001",
                    node_id: node.id,
                    message: format!("引用断裂：目标 {t} 不存在"),
                });
            }
        }
        true
    })?;
    issues.sort_by(|a, b| a.code.cmp(b.code).then_with(|| a.node_id.0.cmp(&b.node_id.0)));
    Ok(issues)
}

fn root_of(parents: &HashMap<Uuid, Option<Uuid>>, id: Uuid) -> Uuid {
    let mut cur = id;
    for _ in 0..4096 {
        match parents.get(&cur) {
            Some(Some(p)) if parents.contains_key(p) => cur = *p,
            _ => return cur,
        }
    }
    cur
}
