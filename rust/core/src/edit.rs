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

/// 按编号在这个文件里定位一条记录；查不到时**说清楚**是哪种查不到
/// （台账里根本没这个编号 / 只在别的文件里 / 本文件条目被指纹判为过期）。
fn locate_here(r: &mut wsidx::Reader, want: &Path, id: Uuid) -> Result<wsidx::Hit, String> {
    let hits = r.locate(id)?;
    if let Some(h) = hits.iter().find(|h| same_file(h, want)) {
        return Ok(h.clone());
    }
    let where_is = if hits.is_empty() {
        "台账里没有它的位置".to_string()
    } else {
        format!(
            "台账里它只出现在：{}",
            hits.iter().map(|h| h.file.clone()).collect::<Vec<_>>().join("、")
        )
    };
    let stale = r.stale_files.iter().find(|p| canon(Path::new(p)) == want);
    let why = match stale {
        Some(p) => format!("；而且这个文件的条目被判为过期（指纹不符：{p}），跑一次 xr index update 即可"),
        None => "；先跑一次 xr index update（或 rebuild）再看".to_string(),
    };
    Err(format!(
        "编号 {id} 不在这个文件（{}）的台账里：{where_is}{why}",
        want.display()
    ))
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
        let h = locate_here(r, &want, cur)?;
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
    let hit = locate_here(&mut r, &want, id)?;
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
        let hit = locate_here(&mut r, &want, c)?;
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

// ============================================================================
// 批量提交：一批改动一次落盘
// ============================================================================

/// 批量里的一条改动（只改已有节点；新增节点用 `xr import --append`）。
#[derive(Clone, Debug)]
pub enum BatchOp {
    /// 改值。
    Set { id: Uuid, value: Value },
    /// 改引用（等价于把值设成引用）。
    Link { id: Uuid, to: Uuid },
    /// 改名（编号不变）。
    Rename { id: Uuid, name: String },
    /// 删：名字与值置空（记录还在，编号不消失）。
    Remove { id: Uuid },
}

impl BatchOp {
    pub fn id(&self) -> Uuid {
        match self {
            BatchOp::Set { id, .. }
            | BatchOp::Link { id, .. }
            | BatchOp::Rename { id, .. }
            | BatchOp::Remove { id } => *id,
        }
    }
}

/// 批量提交的结果。
#[derive(Clone, Debug, Default)]
pub struct BatchOutcome {
    /// 这次真正改到的节点数（同一编号在一批里改多次只算一个）。
    pub changed: usize,
    /// 往文件里追加的记录条数（含留痕与协议声明）。
    pub appended: usize,
    /// 一共读了多少条改动（含对同一编号的多次改动）。
    pub ops: usize,
}

/// 一条已经算好、等着落盘的改动。
struct Change {
    before: Node,
    after: Node,
    noop: bool,
    /// 已有的 `@history` 编号；`None` = 需要新建一个。
    hist_id: Option<Uuid>,
    /// 这条改动要不要顺手在这个根下补 `@protocol = append-v1` 声明。
    marker_root: Option<Uuid>,
}

/// **一批改动一次落盘**：先在内存里把整批算完、全部校验通过，再连续追加；
/// 任一条不合法（编号不在这个文件里、模板定义受保护、名字超长…）就整批不动。
///
/// 为什么要有它：批量迁移是「几十万条改动」的场景，一条一个命令的话，
/// 光是进程启动就要几十分钟；这里一个进程、一次写入、一次台账登记。
/// 同一编号在一批里改多次只写一条最终记录、只留一次痕。
///
/// `force = true` 时放行模板定义（与单条编辑的 `--yes` 同一条护栏）。
pub fn apply_batch(
    path: &Path,
    ops: &[BatchOp],
    history: bool,
    force: bool,
    dry_run: bool,
) -> Result<BatchOutcome, String> {
    let mut out = BatchOutcome { changed: 0, appended: 0, ops: ops.len() };
    if ops.is_empty() {
        return Ok(out);
    }
    prepare(path)?;
    let ws_root = wsidx::workspace_root(path);
    let want = canon(path);
    let mut r = wsidx::Reader::open(&ws_root)?;

    // —— 第一遍：全部算完 + 校验，一个字节都不写 ——
    let mut changes: Vec<Change> = Vec::new();
    let mut order: std::collections::HashMap<Uuid, usize> = std::collections::HashMap::new();
    // 祖先状态按节点缓存：四十万条改动往往共享同一批祖先，不缓存的话
    // 每条都要顺着父链把「孩子表」读一遍（宽树里一次就是上万条记录）。
    let mut status: std::collections::HashMap<Uuid, bool> = std::collections::HashMap::new();
    let mut root_cache: std::collections::HashMap<Uuid, Uuid> = std::collections::HashMap::new();
    let mut protocol_cache: std::collections::HashMap<Uuid, bool> = std::collections::HashMap::new();
    let mut roots_done: std::collections::HashSet<Uuid> = std::collections::HashSet::new();
    for (i, op) in ops.iter().enumerate() {
        let id = op.id();
        let at = format!("第 {} 条", i + 1);
        // 已经在本次改动里的：接着上一个状态算（保证「改两次」的结果是对的）
        let idx = match order.get(&id) {
            Some(k) => *k,
            None => {
                let hit = locate_here(&mut r, &want, id)
                    .map_err(|e| format!("{at}：{e}"))?;
                let before = wsidx::read_node_at_hit(&hit)?;
                // 这条记录刚读出来，直接用它判模板状态（省掉一次按编号查台账）
                let protected = protected_in(&mut r, &want, id, Some(&before), &mut status, 0)?;
                if protected && !force {
                    return Err(format!(
                        "{at}：编号 {id} 属于模板定义，不能直接编辑（确要改请用 --yes）"
                    ));
                }
                // 树根、声明要不要补、`@history` 在哪：全在第一遍查好。
                // （第一遍文件还没被追加过，台账的「指纹新鲜」判断成立；
                // 第二遍一旦开始追加，文件指纹就变了，再查表会落空。）
                let root = cached_root(&mut r, &want, id, &mut root_cache, 0)?;
                let needs_marker = if roots_done.contains(&root) {
                    false
                } else {
                    let has = match protocol_cache.get(&root) {
                        Some(v) => *v,
                        None => {
                            let v =
                                declares_protocol_in(&mut r, &want, root, tree::PROTOCOL_APPEND)?;
                            protocol_cache.insert(root, v);
                            v
                        }
                    };
                    roots_done.insert(root);
                    !has
                };
                let hist_id = if history {
                    children_in_file(&mut r, &want, id)?
                        .into_iter()
                        .find(|k| k.name == "@history")
                        .map(|k| k.id)
                } else {
                    None
                };
                changes.push(Change {
                    after: before.clone(),
                    before,
                    noop: true,
                    hist_id,
                    marker_root: if needs_marker { Some(root) } else { None },
                });
                order.insert(id, changes.len() - 1);
                changes.len() - 1
            }
        };
        let after = match op {
            BatchOp::Set { value, .. } => Node { value: value.clone(), ..changes[idx].after.clone() },
            BatchOp::Link { to, .. } => Node {
                value: Value::Reference(*to),
                ..changes[idx].after.clone()
            },
            BatchOp::Rename { name, .. } => {
                if name.is_empty() {
                    return Err(format!("{at}：新名字不能为空（要清空名字请用 rm）"));
                }
                if name.as_bytes().len() > 255 {
                    return Err(format!("{at}：名字超 255 字节"));
                }
                Node { name: name.clone(), ..changes[idx].after.clone() }
            }
            BatchOp::Remove { .. } => Node {
                name: String::new(),
                value: Value::Empty,
                ..changes[idx].after.clone()
            },
        };
        changes[idx].noop = false;
        changes[idx].after = after;
    }
    changes.retain(|c| !c.noop);
    out.changed = changes.len();
    if changes.is_empty() {
        return Ok(out);
    }
    if dry_run {
        return Ok(out);
    }

    // —— 第二遍：连续追加（攒缓冲、按块 write）——
    let original_len = fs::metadata(path).map_err(|e| e.to_string())?.len();
    let mut f = fs::OpenOptions::new()
        .append(true)
        .open(path)
        .map_err(|e| e.to_string())?;
    let mut buf: Vec<u8> = Vec::new();

    let now = Utc::now().to_rfc3339();
    let mut w = Pusher { f: &mut f, buf: &mut buf, appended: 0 };
    let mut result = Ok(());
    for c in &changes {
        // 第二遍只写：所有查表都在第一遍做完了（见上面 Change 的注释）
        result = write_change(&mut w, c, history, &now);
        if result.is_err() {
            break;
        }
    }
    if result.is_ok() {
        result = w.flush();
    }
    if let Err(e) = result {
        // 只追加过：截回原来的长度就等于整批没发生
        drop(f);
        if let Ok(f) = fs::OpenOptions::new().write(true).open(path) {
            let _ = f.set_len(original_len);
        }
        return Err(e);
    }
    out.appended = w.appended;
    // 一次登记全部新增（毫秒级；与单条编辑同一入口）
    wsidx::append_tail(&ws_root, path)?;
    Ok(out)
}

/// 攒缓冲、按块写（1 MB 一次 write）。
struct Pusher<'a> {
    f: &'a mut fs::File,
    buf: &'a mut Vec<u8>,
    appended: usize,
}

impl Pusher<'_> {
    fn push(&mut self, n: &Node) -> Result<(), String> {
        self.buf
            .extend(codec::encode_node(n).map_err(|e| format!("节点编码失败（{e:?}）"))?);
        self.appended += 1;
        if self.buf.len() >= 1 << 20 {
            self.flush()?;
        }
        Ok(())
    }
    fn flush(&mut self) -> Result<(), String> {
        if !self.buf.is_empty() {
            self.f.write_all(self.buf).map_err(|e| e.to_string())?;
            self.buf.clear();
        }
        Ok(())
    }
}

/// 把一条改动写出去（先补声明，再留痕，最后是新记录）。
///
/// 只写、不查台账（要查的东西第一遍就算好了）。
fn write_change(w: &mut Pusher, c: &Change, history: bool, now: &str) -> Result<(), String> {
    // 声明放在改动的记录前面：先声明再追加，读的人任何时候都不会看到「没声明的重复编号」
    if let Some(root) = c.marker_root {
        w.push(&Node {
            id: Uuid::random_v4(),
            parent: Some(root),
            name: "@protocol".into(),
            value: Value::Text(tree::PROTOCOL_APPEND.into()),
        })?;
    }
    if history {
        let hist_id = match c.hist_id {
            Some(h) => h,
            None => {
                let h = Node {
                    id: Uuid::random_v4(),
                    parent: Some(c.after.id),
                    name: "@history".into(),
                    value: Value::Empty,
                };
                w.push(&h)?;
                h.id
            }
        };
        let snap = Node {
            id: Uuid::random_v4(),
            parent: Some(hist_id),
            name: c.before.name.clone(),
            value: c.before.value.clone(),
        };
        w.push(&snap)?;
        w.push(&Node {
            id: Uuid::random_v4(),
            parent: Some(snap.id),
            name: "@replaced".into(),
            value: Value::Text(now.to_string()),
        })?;
    }
    w.push(&c.after)
}

/// 某个编号在这个文件里是不是模板定义（受保护）。
///
/// `cache` 把「已经算过的节点」记下来：批量里几十万条改动共享同一批祖先，
/// 不缓存的话每条都要顺着父链把孩子表读一遍。
fn protected_in(
    r: &mut wsidx::Reader,
    want: &Path,
    id: Uuid,
    known: Option<&Node>,
    cache: &mut std::collections::HashMap<Uuid, bool>,
    depth: usize,
) -> Result<bool, String> {
    if let Some(v) = cache.get(&id) {
        return Ok(*v);
    }
    if depth > 4_096 {
        return Err("父链太长（可能成环）：需要整份载入处理".into());
    }
    let kids = children_in_file(r, want, id)?;
    let v = if kids.iter().any(|k| k.name == "@模板" && k.value == Value::Empty) {
        true // 模板定义 → 受保护
    } else if kids.iter().any(|k| k.name == "@实例") {
        false // 实例 → 可编辑
    } else {
        let parent = match known {
            Some(n) => n.parent,
            None => {
                let hit = locate_here(r, want, id)?;
                wsidx::read_node_at_hit(&hit)?.parent
            }
        };
        match parent {
            Some(p) => protected_in(r, want, p, None, cache, depth + 1)?,
            None => false,
        }
    };
    cache.insert(id, v);
    Ok(v)
}

/// 所属树根（同样按节点缓存；父链只在第一次走）。
fn cached_root(
    r: &mut wsidx::Reader,
    want: &Path,
    id: Uuid,
    cache: &mut std::collections::HashMap<Uuid, Uuid>,
    depth: usize,
) -> Result<Uuid, String> {
    if let Some(v) = cache.get(&id) {
        return Ok(*v);
    }
    if depth > 4_096 {
        return Err("父链太长（可能成环）：需要整份载入处理".into());
    }
    let hit = locate_here(r, want, id)?;
    let n = wsidx::read_node_at_hit(&hit)?;
    let root = match n.parent {
        None => n.id,
        Some(p) => cached_root(r, want, p, cache, depth + 1)?,
    };
    cache.insert(id, root);
    Ok(root)
}

/// 某个根下是否已经声明了某个协议（复用同一个 Reader，批量用）。
fn declares_protocol_in(
    r: &mut wsidx::Reader,
    want: &Path,
    root: Uuid,
    protocol: &str,
) -> Result<bool, String> {
    for h in r.children_of(root)? {
        if same_file(&h, want) {
            let k = wsidx::read_node_at_hit(&h)?;
            if k.name == "@protocol" && k.value == Value::Text(protocol.to_string()) {
                return Ok(true);
            }
        }
    }
    Ok(false)
}
