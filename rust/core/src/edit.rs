//! 单节点编辑：**不整份载入文件**的写路径（CLI 与桌面端共用同一份实现）。
//!
//! 怎么做到毫秒级：用工作区台账按编号直接跳到那一条记录 → 读出它 →
//! 只往文件末尾追加「改动后的记录」+（可选）留痕 + 幂等补 `@protocol = append-v1`
//! → 再让台账只登记新增的那一段（`wsidx::append_tail`）。
//!
//! 对照：347 万节点的文件整份载入要读 187 MB、解码 347 万个节点（约 3 秒），
//! 这条路径只读几十到几百字节。
//!
//! **绝不猜**：任何一步对不上（没台账 / 台账过期 / 编号不在这个文件里 /
//! 父链断了 / 记录读不出来）都返回 `Err`，由调用方退回「整份载入 → 整份重写」。

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use chrono::Utc;

use crate::codec::{self, Node, Uuid, Value};
use crate::tree;
use crate::wsidx;

/// 一次单节点编辑的结果。
pub struct EditOutcome {
    /// 改动前的记录（原样）。
    pub before: Node,
    /// 追加到文件末尾的那条记录（改动后的样子）。
    pub after: Node,
    /// 这次往文件里追加了多少条记录（含留痕与协议声明）。
    pub appended: usize,
}

fn canon(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

fn same_file(hit: &wsidx::Hit, want: &Path) -> bool {
    Path::new(&hit.file) == want
}

/// 按编号读到那一条记录（走台账；台账不可用就 `Err`，交给调用方整份载入）。
pub fn read_node(path: &Path, id: Uuid) -> Result<Option<Node>, String> {
    let ws_root = wsidx::workspace_root(path);
    let mut r = wsidx::Reader::open(&ws_root)?;
    let want = canon(path);
    match r.locate(id)?.into_iter().find(|h| same_file(h, &want)) {
        Some(h) => Ok(Some(wsidx::read_node_at_hit(&h)?)),
        None => Ok(None),
    }
}

/// 某个编号在这个文件里的所有孩子（读台账的关系本）。
fn children_in_file(
    r: &mut wsidx::Reader,
    path: &Path,
    id: Uuid,
) -> Result<Vec<Node>, String> {
    let want = canon(path);
    let mut out = Vec::new();
    for h in r.children_of(id)? {
        if same_file(&h, &want) {
            out.push(wsidx::read_node_at_hit(&h)?);
        }
    }
    Ok(out)
}

/// 沿父链走到树根（父链必须都在这个文件里，否则 `Err`）。
fn root_of(r: &mut wsidx::Reader, path: &Path, id: Uuid) -> Result<Uuid, String> {
    let want = canon(path);
    let mut cur = id;
    for _ in 0..4_096 {
        let h = r
            .locate(cur)?
            .into_iter()
            .find(|h| same_file(h, &want))
            .ok_or_else(|| format!("编号 {cur} 不在这个文件里（台账没登记）"))?;
        let n = wsidx::read_node_at_hit(&h)?;
        match n.parent {
            None => return Ok(n.id),
            Some(p) => cur = p,
        }
    }
    Err("父链太长（可能成环）：需要整份载入处理".into())
}

/// 该树根下是否已经声明了某个协议（幂等判断，读的是文件里的真实内容）。
pub fn declares_protocol(path: &Path, root: Uuid, protocol: &str) -> Result<bool, String> {
    let ws_root = wsidx::workspace_root(path);
    let mut r = wsidx::Reader::open(&ws_root)?;
    for c in children_in_file(&mut r, path, root)? {
        if c.name == "@protocol" && c.value == Value::Text(protocol.to_string()) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// 把一批记录只追加到文件末尾（一个缓冲区、一次 write）。返回追加的条数。
pub fn append_records(path: &Path, records: &[Node]) -> Result<usize, String> {
    if records.is_empty() {
        return Ok(0);
    }
    let mut buf = Vec::new();
    for n in records {
        buf.extend(codec::encode_node(n).map_err(|e| format!("节点编码失败（{e:?}）"))?);
    }
    let mut f = fs::OpenOptions::new()
        .append(true)
        .open(path)
        .map_err(|e| e.to_string())?;
    f.write_all(&buf).map_err(|e| e.to_string())?;
    Ok(records.len())
}

/// 写之前先收拾干净：让台账跟上文件，并把尾部残片（F015，上次写崩留下的半条记录）
/// 截到最后一条完整记录——否则新的追加会落在残片后面，把残片夹在中间。
///
/// 台账不可用时返回 `Err`（调用方可以忽略：那就下次再说）。
pub fn prepare(path: &Path) -> Result<(), String> {
    let ws_root = wsidx::workspace_root(path);
    let t = wsidx::append_tail(&ws_root, path)?;
    if t.truncated_bytes > 0 {
        let f = fs::OpenOptions::new()
            .write(true)
            .open(path)
            .map_err(|e| e.to_string())?;
        f.set_len(t.registered_upto).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// 单节点编辑：改名字 / 改值（至少给一个），**只追加**，不整份载入。
///
/// `history = true` 按 CLI 的语义留痕：先确保有 `@history` 子节点，
/// 追加一条「旧名字 + 旧值」的快照，再在快照下挂 `@replaced = 现在`；
/// 与 `Store::update` 写出来的形状一致（桌面端用自己的撤销栈，传 `false`）。
///
/// 值没变、名字也没变 → 一条记录都不写（`appended = 0`）。
pub fn edit_node(
    path: &Path,
    id: Uuid,
    new_name: Option<String>,
    new_value: Option<Value>,
    history: bool,
) -> Result<EditOutcome, String> {
    let ws_root = wsidx::workspace_root(path);
    // 台账先跟文件对齐：追加写之后台账可能还差一段，这里顺手补上；
    // 对不上（没登记过 / 文件变小 / 前缀被改写）就 Err → 调用方退回整份载入。
    wsidx::append_tail(&ws_root, path)?;
    let mut r = wsidx::Reader::open(&ws_root)?;
    let want = canon(path);
    let hit = r
        .locate(id)?
        .into_iter()
        .find(|h| same_file(h, &want))
        .ok_or_else(|| format!("编号 {id} 不在这个文件里（台账没登记）"))?;
    let before = wsidx::read_node_at_hit(&hit)?;
    let after = Node {
        id,
        parent: before.parent,
        name: new_name.unwrap_or_else(|| before.name.clone()),
        value: new_value.unwrap_or_else(|| before.value.clone()),
    };
    if after.name == before.name && after.value == before.value {
        return Ok(EditOutcome { before, after, appended: 0 });
    }

    let mut recs: Vec<Node> = Vec::new();
    if history {
        let hist_id = match children_in_file(&mut r, path, id)?
            .into_iter()
            .find(|c| c.name == "@history")
        {
            Some(h) => h.id,
            None => {
                let h = Node {
                    id: Uuid::random_v4(),
                    parent: Some(id),
                    name: "@history".into(),
                    value: Value::Empty,
                };
                recs.push(h.clone());
                h.id
            }
        };
        let snap = Node {
            id: Uuid::random_v4(),
            parent: Some(hist_id),
            name: before.name.clone(),
            value: before.value.clone(),
        };
        recs.push(snap.clone());
        recs.push(Node {
            id: Uuid::random_v4(),
            parent: Some(snap.id),
            name: "@replaced".into(),
            value: Value::Text(Utc::now().to_rfc3339()),
        });
    }
    recs.push(after.clone());

    // 幂等补 `@protocol = append-v1`：追加写之后同一个编号会有多份记录，
    // 声明了协议，`xr validate` 才不会把这种正常的重复编号报成 E002。
    let root = root_of(&mut r, path, id)?;
    if !declares_protocol(path, root, tree::PROTOCOL_APPEND)? {
        recs.push(Node {
            id: Uuid::random_v4(),
            parent: Some(root),
            name: "@protocol".into(),
            value: Value::Text(tree::PROTOCOL_APPEND.into()),
        });
    }

    let appended = append_records(path, &recs)?;
    // 只登记新增的那一段（毫秒级）
    wsidx::append_tail(&ws_root, path)?;
    Ok(EditOutcome { before, after, appended })
}

/// 单节点编辑的「护栏」：这个节点是不是**模板定义**（模板定义受保护）。
///
/// 与 `Store::is_editable` 同一套判断：沿父链往上，先碰到挂 `@模板`（值 = 空）
/// 的节点 → 是模板定义（受保护）；先碰到挂 `@实例` 的节点 → 是可编辑的实例。
/// 只读台账命中的那几条记录，不整份载入。
pub fn under_template(path: &Path, id: Uuid) -> Result<bool, String> {
    let ws_root = wsidx::workspace_root(path);
    wsidx::append_tail(&ws_root, path)?;
    let mut r = wsidx::Reader::open(&ws_root)?;
    let want = canon(path);
    let mut cur = Some(id);
    for _ in 0..4_096 {
        let Some(c) = cur else { return Ok(false) };
        let hit = r
            .locate(c)?
            .into_iter()
            .find(|h| same_file(h, &want))
            .ok_or_else(|| format!("编号 {c} 不在这个文件里（台账没登记）"))?;
        let kids = children_in_file(&mut r, path, c)?;
        if kids.iter().any(|k| k.name == "@模板" && k.value == Value::Empty) {
            return Ok(true); // 模板定义 → 受保护
        }
        if kids.iter().any(|k| k.name == "@实例") {
            return Ok(false); // 实例 → 可编辑
        }
        let n = wsidx::read_node_at_hit(&hit)?;
        cur = n.parent;
    }
    Err("父链太长（可能成环）：需要整份载入处理".into())
}
