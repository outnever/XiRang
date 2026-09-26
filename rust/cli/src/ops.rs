//! 共享操作层：`xr`（CLI）与 `xr-mcp`（MCP server）共用的**唯一实现**。
//!
//! 约定：
//! - 这一层**不打印**、不决定退出码：输入 = 参数 + `Policy`，输出 = 结构化结果或 `OpError`。
//! - 护栏住在这里（模板定义保护、危险操作要显式强制），所以两个入口「拦得一样」。
//! - 本机状态（catalog 目录、sidecar 索引）不进这一层，由调用方通过 `Hooks` 挂钩。
//!
//! 注释式模板模型：
//! - 模板定义 = 根上挂 `@模板`(值=空)；其普通子节点 = 结构。
//! - 实例 = 根上挂 `@实例`(空) + `@模板`(值=引用→模板根)；实例可挂在任意父节点下。
#![allow(dead_code)] // 两个 bin 各用其中一部分，允许个别未用

use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};

use serde_json::{json, Value as JsonValue};
use xirang_core::codec::{parse_value, Node, Uuid, Value as XValue};
use xirang_core::{convert, query, shard, tree, validator};

// ============================================================================
// 错误与策略
// ============================================================================

/// 错误大类。数据本身的校验错误仍带 `X` 码（见 `errors/错误列表.md`），
/// 这里只描述「参数 / 路径 / 护栏」这类入口层错误，不新造错误码。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrKind {
    InvalidArgument,
    NotFound,
    Guarded,
    PathDenied,
    Unsupported,
    Internal,
}

impl ErrKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ErrKind::InvalidArgument => "invalid_argument",
            ErrKind::NotFound => "not_found",
            ErrKind::Guarded => "guarded",
            ErrKind::PathDenied => "path_denied",
            ErrKind::Unsupported => "unsupported",
            ErrKind::Internal => "internal",
        }
    }
}

#[derive(Clone, Debug)]
pub struct OpError {
    pub kind: ErrKind,
    pub message: String,
    /// 该怎么改的提示（人和代理都读得懂，不绑定某一端的写法）。
    pub hint: Option<String>,
}

impl OpError {
    pub fn new(kind: ErrKind, message: impl Into<String>) -> Self {
        OpError { kind, message: message.into(), hint: None }
    }

    pub fn hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }

    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new(ErrKind::InvalidArgument, message)
    }
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(ErrKind::NotFound, message)
    }
    pub fn guarded(message: impl Into<String>, hint: impl Into<String>) -> Self {
        Self::new(ErrKind::Guarded, message).hint(hint)
    }
    pub fn path_denied(message: impl Into<String>) -> Self {
        Self::new(ErrKind::PathDenied, message)
    }
    pub fn unsupported(message: impl Into<String>) -> Self {
        Self::new(ErrKind::Unsupported, message)
    }
    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(ErrKind::Internal, message)
    }

    /// MCP 的结构化错误体。
    pub fn to_json(&self) -> JsonValue {
        match &self.hint {
            Some(h) => json!({"kind": self.kind.as_str(), "message": self.message, "hint": h}),
            None => json!({"kind": self.kind.as_str(), "message": self.message}),
        }
    }
}

pub type OpResult<T> = Result<T, OpError>;

/// 调用策略：由入口决定，操作层据此拦截。
#[derive(Clone, Debug, Default)]
pub struct Policy {
    /// 危险 / 受保护操作是否已获显式授权（CLI：`--yes`；MCP：`force`）。
    pub force: bool,
    /// 允许操作的目录。空 = 不限制（CLI 沿用进程工作目录）。
    pub roots: Vec<PathBuf>,
}

impl Policy {
    /// CLI：不限制路径；`--yes` → force。
    pub fn cli(force: bool) -> Self {
        Policy { force, roots: Vec::new() }
    }

    /// MCP：路径必须落在允许目录内（roots 非空）。
    pub fn mcp(force: bool, roots: Vec<PathBuf>) -> Self {
        Policy { force, roots }
    }

    /// 把一个用户给的路径字符串解析成实际路径，并做允许目录检查。
    pub fn resolve(&self, raw: &str) -> OpResult<PathBuf> {
        let p = Path::new(raw);
        let joined = match (p.is_absolute(), self.roots.first()) {
            (true, _) => p.to_path_buf(),
            (false, Some(r0)) => r0.join(p),
            (false, None) => p.to_path_buf(),
        };
        if self.roots.is_empty() {
            return Ok(joined);
        }
        let target = realish(&joined);
        for r in &self.roots {
            if target.starts_with(realish(r)) {
                return Ok(joined);
            }
        }
        Err(OpError::path_denied(format!("路径不在允许目录内：{raw}"))
            .hint("只允许操作启动时用 --root / XIRANG_MCP_ROOTS 指定的目录"))
    }
}

/// 尽量取「真实路径」：存在就 canonicalize；不存在就从最近的已存在祖先拼出来。
/// 这样 `a/b/../c`、符号链接、尚不存在的目标文件都能正确判定边界。
fn realish(p: &Path) -> PathBuf {
    if let Ok(c) = p.canonicalize() {
        return c;
    }
    let mut tail: Vec<OsString> = Vec::new();
    let mut cur = p.to_path_buf();
    loop {
        if let Some(name) = cur.file_name() {
            tail.push(name.to_os_string());
        }
        let parent = match cur.parent() {
            Some(x) if !x.as_os_str().is_empty() => x.to_path_buf(),
            _ => break,
        };
        if let Ok(c) = parent.canonicalize() {
            let mut out = c;
            for seg in tail.iter().rev() {
                if seg == ".." {
                    out.pop();
                } else if seg != "." {
                    out.push(seg);
                }
            }
            return out;
        }
        cur = parent;
    }
    normalize(p)
}

/// 词法折叠 `.` / `..`（无法 canonicalize 时的兜底）。
fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// 本机状态挂钩：CLI 用它登记本机目录 / 索引；MCP 用 [`NoHooks`]（不碰本机状态）。
pub trait Hooks {
    fn on_load(&self, _path: &Path, _store: &tree::Store) {}
    fn on_save(&self, _path: &Path, _store: &tree::Store) {}
}

pub struct NoHooks;
impl Hooks for NoHooks {}

// ============================================================================
// 读文件 / 写文件 / 护栏
// ============================================================================

fn load_error(path: &Path, raw: &str, e: String) -> OpError {
    if path.exists() {
        OpError::internal(e)
    } else {
        OpError::not_found(format!("文件不存在：{raw}"))
    }
}

fn load(pol: &Policy, hooks: &dyn Hooks, file: &str) -> OpResult<(tree::Store, PathBuf)> {
    let path = pol.resolve(file)?;
    let store = tree::Store::load_view(&path).map_err(|e| load_error(&path, file, e))?;
    hooks.on_load(&path, &store);
    Ok((store, path))
}

/// 读一个「可能是分片词库目录」的目标：目录 → 按编号定位分片并折叠；文件 → 直接读。
/// 返回（可编辑的 Store，落盘目标路径，是否为词库目录）。
fn load_target(
    pol: &Policy,
    hooks: &dyn Hooks,
    file: &str,
    node: Option<Uuid>,
) -> OpResult<(tree::Store, PathBuf, bool)> {
    let path = pol.resolve(file)?;
    if path.is_dir() {
        let id = node.ok_or_else(|| OpError::invalid("词库目录需要给出节点编号"))?;
        let (store, shard_path) = load_shard(&path, id)?;
        return Ok((store, shard_path, true));
    }
    let store = tree::Store::load_view(&path).map_err(|e| load_error(&path, file, e))?;
    hooks.on_load(&path, &store);
    Ok((store, path, false))
}

fn save(
    hooks: &dyn Hooks,
    store: &tree::Store,
    path: &Path,
    in_collection: bool,
    before: Option<&tree::Store>,
) -> OpResult<()> {
    if in_collection {
        let before = before.ok_or_else(|| OpError::internal("词库写入缺少改动前快照"))?;
        shard::append_changes(path, before, store).map_err(OpError::internal)?;
    } else {
        store.save(path).map_err(|e| OpError::internal(e.to_string()))?;
    }
    hooks.on_save(path, store);
    update_index(path);
    Ok(())
}

/// 数据落盘后的索引维护：台账模式追加日志（侧车模式由 `Store::save` 自己写侧车）。
/// 尽力而为——索引只是缓存，失败不影响数据写入，可由 `xr index rebuild` 重建。
/// 两个 bin 共用一个入口；「碰过哪个工作区」由调用方通过 [`ON_INDEX_TOUCH`] 挂钩接收。
pub static ON_INDEX_TOUCH: std::sync::OnceLock<fn(std::path::PathBuf)> = std::sync::OnceLock::new();

/// 大于这个体积的文件，索引登记放到后台进程做（登记要重扫全文，很贵）。
const BACKGROUND_INDEX_MIN_BYTES: u64 = 20 * 1024 * 1024;

pub fn update_index(path: &Path) {
    if xirang_core::index::sidecar_enabled() {
        return;
    }
    let ws_root = xirang_core::wsidx::workspace_root(path);
    // 大文件：登记 = 重扫全文（179 MB / 347 万节点约 49 秒）。这一步不该挡住用户的命令，
    // 索引只是缓存，晚几十秒跟上没有任何数据风险（指纹不符时读路径会回退并打印原因）。
    let big = std::fs::metadata(path)
        .map(|m| m.len() >= BACKGROUND_INDEX_MIN_BYTES)
        .unwrap_or(false);
    if big && spawn_background_index_update(&ws_root) {
        eprintln!("（索引登记已在后台进行，命令先返回；XIRANG_INDEX_MAINTENANCE=off 可关闭）");
        if let Some(f) = ON_INDEX_TOUCH.get() {
            f(ws_root);
        }
        return;
    }
    let _ = xirang_core::wsidx::append_file(&ws_root, path);
    if let Some(f) = ON_INDEX_TOUCH.get() {
        f(ws_root);
    }
}

/// 起一个后台的 `xr index update <工作区>`（和自动压实同一套路）。
/// 只有确实找到 `xr` 可执行文件时才做——桌面端进程里没有这个子命令，绝不能让 App 自己再起一个自己。
fn spawn_background_index_update(ws_root: &Path) -> bool {
    if std::env::var("XIRANG_INDEX_MAINTENANCE")
        .map(|v| v == "off")
        .unwrap_or(false)
    {
        return false;
    }
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    let xr = exe.with_file_name("xr");
    if !xr.is_file() {
        return false;
    }
    std::process::Command::new(xr)
        .arg("index")
        .arg("update")
        .arg(ws_root)
        .env("XIRANG_INDEX_MAINTENANCE", "off") // 防递归
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .is_ok()
}

/// 在词库目录里按 UUID 定位分片，读出折叠后的可编辑 Store + 该分片文件路径。
fn load_shard(dir: &Path, id: Uuid) -> OpResult<(tree::Store, PathBuf)> {
    let col = shard::Collection::open(dir).map_err(OpError::internal)?;
    let filename = col
        .shard_file_for(id)
        .ok_or_else(|| OpError::not_found(format!("节点不存在于词库：{id}")))?;
    let path = dir.join(filename);
    let raw = tree::Store::load(&path).map_err(OpError::internal)?;
    Ok((shard::fold(&raw), path))
}

/// 模板定义保护：节点（或在的最近标注根）属于模板定义时拒绝直接编辑。
fn guard_editable(pol: &Policy, store: &tree::Store, id: Uuid) -> OpResult<()> {
    if pol.force {
        return Ok(());
    }
    match store.get(id) {
        Some(n) if !store.is_editable(n) => Err(OpError::guarded(
            "该节点属于模板定义，不能直接编辑",
            "模板定义请用 template 操作修改；确需直接改请显式强制（CLI：--yes，MCP：force）",
        )),
        _ => Ok(()),
    }
}

fn parse_uuid(label: &str, raw: &str) -> OpResult<Uuid> {
    Uuid::parse(raw).ok_or_else(|| OpError::invalid(format!("无效{label}：{raw}")))
}

/// `nil` / `root` / 空 → 自由根；否则必须是节点编号。
fn parse_parent(raw: Option<&str>) -> OpResult<Option<Uuid>> {
    match raw {
        None | Some("") | Some("nil") | Some("root") => Ok(None),
        Some(p) => Ok(Some(parse_uuid("父节点 ID", p)?)),
    }
}

pub fn parse_value_str(raw: &str) -> XValue {
    parse_value(raw)
}

// ============================================================================
// 展示用结构
// ============================================================================

pub fn type_name(v: &XValue) -> &'static str {
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

/// 值的显示形式（引用解析成目标名字）。
pub fn display_value(store: &tree::Store, node: &Node) -> Option<String> {
    match &node.value {
        XValue::Empty => None,
        XValue::Int(n) => Some(n.to_string()),
        XValue::Float(f) => Some(f.to_string()),
        XValue::Bool(b) => Some(if *b { "true" } else { "false" }.to_string()),
        XValue::Text(s) => Some(s.clone()),
        XValue::Reference(u) => Some(format!(
            "→ {}",
            store.get(*u).map(|t| t.name.as_str()).unwrap_or(&u.to_string())
        )),
        XValue::Blob(b) => Some(format!("[blob {} 字节]", b.len())),
    }
}

/// 一行文本标签：`名字 = 值` / `名字` / `值` / `(空节点)`。
pub fn label(store: &tree::Store, node: &Node) -> String {
    match display_value(store, node) {
        None => {
            if node.name.is_empty() {
                "(空节点)".to_string()
            } else {
                node.name.clone()
            }
        }
        Some(v) => {
            if node.name.is_empty() {
                v
            } else {
                format!("{} = {}", node.name, v)
            }
        }
    }
}

/// 一棵树 / 一行的结构视图，两个入口共用：CLI 渲染文本，MCP 转 JSON。
#[derive(Clone, Debug)]
pub struct NodeView {
    pub id: Uuid,
    pub parent: Option<Uuid>,
    pub name: String,
    pub type_name: &'static str,
    pub value: Option<String>,
    pub label: String,
    pub is_aux: bool,
    pub children: Vec<NodeView>,
}

impl NodeView {
    fn new(store: &tree::Store, node: &Node) -> Self {
        NodeView {
            id: node.id,
            parent: node.parent,
            name: node.name.clone(),
            type_name: type_name(&node.value),
            value: display_value(store, node),
            label: label(store, node),
            is_aux: node.name.starts_with('@'),
            children: Vec::new(),
        }
    }

    pub fn to_json(&self) -> JsonValue {
        json!({
            "id": self.id.to_string(),
            "name": self.name,
            "type": self.type_name,
            "value": self.value,
            "isAux": self.is_aux,
            "children": self.children.iter().map(|c| c.to_json()).collect::<Vec<_>>(),
        })
    }

    /// 扁平视图的一行（不带 children，带父边）。
    pub fn to_flat_json(&self) -> JsonValue {
        json!({
            "id": self.id.to_string(),
            "parent": self.parent.map(|p| p.to_string()),
            "name": self.name,
            "type": self.type_name,
            "value": self.value,
            "isAux": self.is_aux,
        })
    }
}

// ============================================================================
// 读操作
// ============================================================================

pub struct InfoOutcome {
    pub file: String,
    pub version: u8,
    pub header_bytes: usize,
    pub header_first_line: String,
    pub nodes: usize,
    pub roots: usize,
}

impl InfoOutcome {
    pub fn to_json(&self) -> JsonValue {
        json!({
            "file": self.file,
            "version": self.version,
            "headerBytes": self.header_bytes,
            "headerFirstLine": self.header_first_line,
            "nodeCount": self.nodes,
            "rootCount": self.roots,
        })
    }
}

pub fn info(pol: &Policy, hooks: &dyn Hooks, file: &str) -> OpResult<InfoOutcome> {
    let path = pol.resolve(file)?;
    let data = std::fs::read(&path).map_err(|e| {
        if path.exists() {
            OpError::internal(e.to_string())
        } else {
            OpError::not_found(format!("文件不存在：{file}"))
        }
    })?;
    let (version, header) = tree::read_header(&data).map_err(OpError::internal)?;
    let nodes_bytes = tree::parse_file(&data).map_err(OpError::internal)?;
    let store =
        tree::Store::decode(nodes_bytes).map_err(|e| OpError::internal(tree::codec_error(e)))?;
    hooks.on_load(&path, &store);
    Ok(InfoOutcome {
        file: file.to_string(),
        version,
        header_bytes: header.len(),
        header_first_line: header.trim().lines().next().unwrap_or("(空)").to_string(),
        nodes: store.len(),
        roots: store.roots().len(),
    })
}

/// 视图布局：缩进树 / 扁平（按存放顺序，一行一个节点）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layout {
    Tree,
    Flat,
}

/// 视图选项（`limit` 即 CLI 的 `--head` / MCP 的 `limit`）。
#[derive(Clone, Copy, Debug, Default)]
pub struct ViewOpts {
    pub skip_aux: bool,
    pub include_ids: bool,
    pub max_depth: Option<usize>,
    pub limit: Option<usize>,
}

pub struct ViewOutcome {
    pub total: usize,
    pub printed: usize,
    /// Tree 布局：每个根一棵子树。
    pub roots: Vec<NodeView>,
    /// Flat 布局：按存放顺序的每一行。
    pub items: Vec<NodeView>,
    pub layout: Layout,
}

impl ViewOutcome {
    pub fn truncated(&self) -> bool {
        self.printed < self.total
    }

    pub fn to_json(&self) -> JsonValue {
        match self.layout {
            Layout::Tree => json!({
                "nodes": self.roots.iter().map(|r| r.to_json()).collect::<Vec<_>>(),
                "total": self.total,
                "printed": self.printed,
                "truncated": self.truncated(),
            }),
            Layout::Flat => json!({
                "nodes": self.items.iter().map(|i| i.to_flat_json()).collect::<Vec<_>>(),
                "total": self.total,
                "printed": self.printed,
                "truncated": self.truncated(),
            }),
        }
    }
}

/// 读树 / 读平铺视图。
///
/// `unbounded_guard`：不给 `limit` 时，节点数超过它就报 `guarded`（CLI 用它挡住「刷屏」，
/// MCP 不需要——它总带 limit）。`None` = 不设这道闸。
pub fn view(
    pol: &Policy,
    hooks: &dyn Hooks,
    file: &str,
    node: Option<&str>,
    opts: ViewOpts,
    layout: Layout,
    unbounded_guard: Option<usize>,
) -> OpResult<ViewOutcome> {
    let (store, _path) = load(pol, hooks, file)?;
    let total = store.len();
    if let Some(g) = unbounded_guard {
        if opts.limit.is_none() && total > g {
            return Err(OpError::guarded(
                format!("这个文件有 {total} 个节点，全量打印会刷屏（可能上百 MB）"),
                format!(
                    "改用 limit（CLI：xr cat {file} --head 200），或确实要全打时加 --force"
                ),
            ));
        }
    }
    let mut budget = opts.limit.unwrap_or(usize::MAX);
    let mut seen: HashSet<Uuid> = HashSet::new();

    if layout == Layout::Flat {
        let mut items = Vec::new();
        for nd in store.nodes() {
            if budget == 0 {
                break;
            }
            if opts.skip_aux && nd.name.starts_with('@') {
                continue;
            }
            items.push(NodeView::new(&store, nd));
            budget -= 1;
        }
        let printed = items.len();
        return Ok(ViewOutcome { total, printed, roots: Vec::new(), items, layout });
    }

    let mut roots = Vec::new();
    let mut printed = 0usize;
    match node {
        Some(id) => {
            let uuid = parse_uuid("节点 ID", id)?;
            let n = store
                .get(uuid)
                .ok_or_else(|| OpError::not_found(format!("节点不存在：{id}")))?;
            if budget > 0 {
                roots.push(build_view(
                    &store,
                    n,
                    opts,
                    1,
                    &mut budget,
                    &mut printed,
                    &mut seen,
                ));
            }
        }
        None => {
            for r in store.roots() {
                if budget == 0 {
                    break;
                }
                roots.push(build_view(&store, r, opts, 1, &mut budget, &mut printed, &mut seen));
            }
        }
    }
    Ok(ViewOutcome { total, printed, roots, items: Vec::new(), layout })
}

/// 递归建视图；调用前提是「本节点一定会被打印」（budget > 0）。
fn build_view(
    store: &tree::Store,
    node: &Node,
    opts: ViewOpts,
    depth: usize,
    budget: &mut usize,
    printed: &mut usize,
    seen: &mut HashSet<Uuid>,
) -> NodeView {
    *budget -= 1;
    *printed += 1;
    let mut v = NodeView::new(store, node);
    // 父边成环（E006）时兜底：同一节点只展开一次，避免无限递归。
    if !seen.insert(node.id) {
        return v;
    }
    if let Some(m) = opts.max_depth {
        if depth >= m {
            return v;
        }
    }
    for c in store.children_opt(node, opts.skip_aux) {
        if *budget == 0 {
            break;
        }
        v.children.push(build_view(store, c, opts, depth + 1, budget, printed, seen));
    }
    v
}

pub struct ValidationItem {
    pub code: &'static str,
    pub node_id: Uuid,
    pub message: String,
}

pub struct ValidationOutcome {
    pub errors: Vec<ValidationItem>,
}

impl ValidationOutcome {
    pub fn to_json(&self) -> JsonValue {
        json!({
            "count": self.errors.len(),
            "errors": self.errors.iter().map(|e| json!({
                "code": e.code,
                "node": e.node_id.to_string(),
                "message": e.message,
            })).collect::<Vec<_>>(),
        })
    }
}

pub fn validate(pol: &Policy, hooks: &dyn Hooks, file: &str) -> OpResult<ValidationOutcome> {
    // 流式校验（core 里那份）：不再整份载入。实测 179 MB / 347 万节点：
    // 41.2 s → 约 1 s。语义与整份载入版一致（含重复编号的 E002 判定与父子成环）。
    let path = pol.resolve(file)?;
    if !path.exists() {
        return Err(load_error(&path, file, "文件不存在".into()));
    }
    let _ = hooks;
    let cancel = std::sync::atomic::AtomicBool::new(false);
    let mut progress = |_: u64| {};
    let issues = xirang_core::scan::validate_stream(&path, &cancel, &mut progress)
        .map_err(OpError::internal)?;
    let errors = issues
        .into_iter()
        .map(|e| ValidationItem {
            code: e.code,
            node_id: e.node_id,
            message: e.message,
        })
        .collect();
    Ok(ValidationOutcome { errors })
}

pub struct DiffItem {
    pub id: Uuid,
    pub name: String,
}

pub struct DiffChange {
    pub id: Uuid,
    pub from_name: String,
    pub from_value: String,
    pub to_name: String,
    pub to_value: String,
}

pub struct DiffOutcome {
    pub added: Vec<DiffItem>,
    pub removed: Vec<DiffItem>,
    pub changed: Vec<DiffChange>,
}

impl DiffOutcome {
    pub fn to_json(&self) -> JsonValue {
        json!({
            "added": self.added.iter().map(|a| json!({"id": a.id.to_string(), "name": a.name})).collect::<Vec<_>>(),
            "removed": self.removed.iter().map(|r| json!({"id": r.id.to_string(), "name": r.name})).collect::<Vec<_>>(),
            "changed": self.changed.iter().map(|c| json!({
                "id": c.id.to_string(),
                "from": {"name": c.from_name, "value": c.from_value},
                "to": {"name": c.to_name, "value": c.to_value},
            })).collect::<Vec<_>>(),
        })
    }
}

pub fn diff(pol: &Policy, hooks: &dyn Hooks, a: &str, b: &str) -> OpResult<DiffOutcome> {
    let (sa, _) = load(pol, hooks, a)?;
    let (sb, _) = load(pol, hooks, b)?;
    let map_a: HashMap<Uuid, &Node> = sa.nodes().iter().map(|n| (n.id, n)).collect();
    let map_b: HashMap<Uuid, &Node> = sb.nodes().iter().map(|n| (n.id, n)).collect();

    let mut added = Vec::new();
    let mut removed = Vec::new();
    let mut changed = Vec::new();
    for (id, n) in &map_a {
        match map_b.get(id) {
            None => removed.push(DiffItem { id: *id, name: n.name.clone() }),
            Some(m) => {
                if m.name != n.name || m.value != n.value {
                    changed.push(DiffChange {
                        id: *id,
                        from_name: n.name.clone(),
                        from_value: display_value(&sa, n).unwrap_or_default(),
                        to_name: m.name.clone(),
                        to_value: display_value(&sb, m).unwrap_or_default(),
                    });
                }
            }
        }
    }
    for (id, n) in &map_b {
        if !map_a.contains_key(id) {
            added.push(DiffItem { id: *id, name: n.name.clone() });
        }
    }
    // HashMap 迭代序不稳定 → 排序，保证输出可回归对比。
    added.sort_by(|x, y| x.id.0.cmp(&y.id.0));
    removed.sort_by(|x, y| x.id.0.cmp(&y.id.0));
    changed.sort_by(|x, y| x.id.0.cmp(&y.id.0));
    Ok(DiffOutcome { added, removed, changed })
}

#[derive(Clone, Debug)]
pub struct Hit {
    pub id: Uuid,
    pub name: String,
    pub type_name: &'static str,
    pub value: Option<String>,
}

impl Hit {
    pub fn to_json(&self) -> JsonValue {
        json!({
            "id": self.id.to_string(),
            "name": self.name,
            "type": self.type_name,
            "value": self.value,
        })
    }
}

pub fn find(pol: &Policy, hooks: &dyn Hooks, file: &str, pattern: &str) -> OpResult<Vec<Hit>> {
    // 走流式扫描（core 里那份，CLI 与桌面端共用）：大文件下不再整份载入。
    // 实测 179 MB / 347 万节点：整份载入 17.1 s → 流式 0.2 s。
    let path = pol.resolve(file)?;
    if !path.exists() {
        return Err(load_error(&path, file, "文件不存在".into()));
    }
    let cancel = std::sync::atomic::AtomicBool::new(false);
    let nodes = xirang_core::scan::collect(
        &path,
        &xirang_core::scan::Query::NameOrText(pattern.to_string()),
        &cancel,
    )
    .map_err(OpError::internal)?;
    let _ = hooks;
    let hits = nodes
        .into_iter()
        .map(|n| Hit {
            id: n.id,
            name: n.name.clone(),
            type_name: type_name(&n.value),
            value: Some(display_plain(&n.value)),
        })
        .collect();
    Ok(hits)
}

/// 不带 store 的值显示（流式扫描时拿不到整库，引用只显示目标编号）。
fn display_plain(v: &XValue) -> String {
    match v {
        XValue::Empty => String::new(),
        XValue::Int(i) => i.to_string(),
        XValue::Float(f) => f.to_string(),
        XValue::Bool(b) => if *b { "true" } else { "false" }.to_string(),
        XValue::Text(s) => s.clone(),
        XValue::Reference(u) => format!("→ {u}"),
        XValue::Blob(b) => format!("[blob {} 字节]", b.len()),
    }
}

/// 匹配条件：`root` / `shape_of` / `template` 三选一 + `where` 值约束。
#[derive(Clone, Debug, Default)]
pub struct MatchQuery {
    pub root: Option<String>,
    pub shape_of: Option<String>,
    pub template: Option<String>,
    pub wheres: Vec<(String, String)>,
    /// 命中项是否连整棵树一起返回。
    pub with_tree: bool,
}

pub struct MatchItem {
    pub id: Uuid,
    pub name: String,
    pub type_name: &'static str,
    pub value: Option<String>,
    pub tree: Option<JsonValue>,
}

impl MatchItem {
    pub fn to_json(&self) -> JsonValue {
        match &self.tree {
            Some(t) => t.clone(),
            None => json!({
                "id": self.id.to_string(),
                "name": self.name,
                "type": self.type_name,
                "value": self.value,
            }),
        }
    }
}

pub struct MatchOutcome {
    pub items: Vec<MatchItem>,
}

impl MatchOutcome {
    pub fn to_json(&self) -> JsonValue {
        json!(self.items.iter().map(|i| i.to_json()).collect::<Vec<_>>())
    }
}

pub fn search(
    pol: &Policy,
    hooks: &dyn Hooks,
    file: &str,
    q: &MatchQuery,
) -> OpResult<MatchOutcome> {
    let (store, _) = load(pol, hooks, file)?;
    let sindex = query::ShapeIndex::build(&store);
    let cands: Vec<usize> = if let Some(name) = &q.template {
        let tid = resolve_template(&store, name)?;
        find_instances_of(&store, tid)
    } else if let Some(name) = &q.root {
        query::name_index(&store).get(name).cloned().unwrap_or_default()
    } else if let Some(so) = &q.shape_of {
        let sid = parse_uuid("节点 ID", so)?;
        let idx = store
            .nodes()
            .iter()
            .position(|x| x.id == sid)
            .ok_or_else(|| OpError::not_found(format!("节点不存在：{so}")))?;
        query::by_shape(&sindex, sindex.shapes[idx])
    } else {
        return Err(OpError::invalid("需要 root 或 shape_of 或 template 三者之一"));
    };
    let conds: Vec<(&str, &str)> =
        q.wheres.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let items = cands
        .into_iter()
        .filter(|&i| query::matches_where(&store, i, &conds))
        .map(|i| {
            let nd = &store.nodes()[i];
            MatchItem {
                id: nd.id,
                name: nd.name.clone(),
                type_name: type_name(&nd.value),
                value: display_value(&store, nd),
                tree: if q.with_tree {
                    let mut budget = usize::MAX;
                    let mut printed = 0usize;
                    let mut seen = HashSet::new();
                    Some(
                        build_view(
                            &store,
                            nd,
                            ViewOpts::default(),
                            1,
                            &mut budget,
                            &mut printed,
                            &mut seen,
                        )
                        .to_json(),
                    )
                } else {
                    None
                },
            }
        })
        .collect();
    Ok(MatchOutcome { items })
}

pub struct InstancesOutcome {
    pub template_id: Uuid,
    pub template_name: String,
    /// 每个实例根一棵子树（完整展开，不过滤辅助节点）。
    pub instances: Vec<NodeView>,
}

impl InstancesOutcome {
    pub fn to_json(&self) -> JsonValue {
        json!({
            "template": {"id": self.template_id.to_string(), "name": self.template_name},
            "count": self.instances.len(),
            "instances": self.instances.iter().map(|i| i.to_json()).collect::<Vec<_>>(),
        })
    }
}

pub fn instances(
    pol: &Policy,
    hooks: &dyn Hooks,
    file: &str,
    template: &str,
) -> OpResult<InstancesOutcome> {
    let (store, _) = load(pol, hooks, file)?;
    let tid = resolve_template(&store, template)?;
    let tname = store.get(tid).map(|n| n.name.clone()).unwrap_or_default();
    let mut out = Vec::new();
    for i in find_instances_of(&store, tid) {
        let mut budget = usize::MAX;
        let mut printed = 0usize;
        let mut seen = HashSet::new();
        out.push(build_view(
            &store,
            &store.nodes()[i],
            ViewOpts::default(),
            1,
            &mut budget,
            &mut printed,
            &mut seen,
        ));
    }
    Ok(InstancesOutcome { template_id: tid, template_name: tname, instances: out })
}

#[derive(Clone, Debug)]
pub struct RefTarget {
    pub id: Uuid,
    pub name: String,
    /// 目标是否真的存在（不存在 = R001 断裂）。
    pub exists: bool,
}

impl RefTarget {
    pub fn to_json(&self) -> JsonValue {
        json!({"id": self.id.to_string(), "name": self.name, "exists": self.exists})
    }
}

pub struct RefsOutcome {
    pub id: Uuid,
    pub name: String,
    /// 出边（我引用谁）；不是引用值时为 None。
    pub reference: Option<RefTarget>,
    /// 入边（谁引用我）。
    pub incoming: Vec<RefTarget>,
}

impl RefsOutcome {
    pub fn to_json(&self) -> JsonValue {
        json!({
            "node": {"id": self.id.to_string(), "name": self.name},
            "reference": self.reference.as_ref().map(|r| r.to_json()),
            "incoming": self.incoming.iter().map(|r| r.to_json()).collect::<Vec<_>>(),
        })
    }
}

pub fn refs(pol: &Policy, hooks: &dyn Hooks, file: &str, node_id: &str) -> OpResult<RefsOutcome> {
    let (store, _) = load(pol, hooks, file)?;
    let id = parse_uuid("节点 ID", node_id)?;
    let node = store
        .get(id)
        .ok_or_else(|| OpError::not_found(format!("节点不存在：{node_id}")))?;
    let reference = match &node.value {
        XValue::Reference(t) => Some(match store.get(*t) {
            Some(target) => RefTarget { id: *t, name: target.name.clone(), exists: true },
            None => RefTarget { id: *t, name: t.to_string(), exists: false },
        }),
        _ => None,
    };
    let incoming = store
        .references_to(node)
        .into_iter()
        .map(|r| RefTarget { id: r.id, name: r.name.clone(), exists: true })
        .collect();
    Ok(RefsOutcome { id: node.id, name: node.name.clone(), reference, incoming })
}

pub struct Snapshot {
    pub name: String,
    pub value: Option<String>,
    pub replaced: Option<String>,
}

pub struct HistoryOutcome {
    pub id: Uuid,
    pub name: String,
    pub has_history: bool,
    pub snapshots: Vec<Snapshot>,
}

impl HistoryOutcome {
    pub fn to_json(&self) -> JsonValue {
        json!({
            "node": {"id": self.id.to_string(), "name": self.name},
            "hasHistory": self.has_history,
            "snapshots": self.snapshots.iter().map(|s| json!({
                "name": s.name,
                "value": s.value,
                "replaced": s.replaced,
            })).collect::<Vec<_>>(),
        })
    }
}

pub fn history(
    pol: &Policy,
    hooks: &dyn Hooks,
    file: &str,
    node_id: &str,
) -> OpResult<HistoryOutcome> {
    let (store, _) = load(pol, hooks, file)?;
    let id = parse_uuid("节点 ID", node_id)?;
    let node = store
        .get(id)
        .ok_or_else(|| OpError::not_found(format!("节点不存在：{node_id}")))?;
    let mut snapshots = Vec::new();
    let mut has_history = false;
    if let Some(hist) = store.child_by_name(node, "@history") {
        has_history = true;
        for snap in store.children(hist) {
            let replaced = match store.child_by_name(snap, "@replaced") {
                Some(r) => match &r.value {
                    XValue::Text(t) => Some(t.clone()),
                    _ => None,
                },
                None => None,
            };
            snapshots.push(Snapshot {
                name: snap.name.clone(),
                value: display_value(&store, snap),
                replaced,
            });
        }
    }
    Ok(HistoryOutcome { id: node.id, name: node.name.clone(), has_history, snapshots })
}

pub struct BlobInfoOutcome {
    pub bytes: usize,
    pub format: Option<String>,
    /// 可判为文本时给前 200 字符预览。
    pub preview: Option<String>,
}

impl BlobInfoOutcome {
    pub fn to_json(&self) -> JsonValue {
        json!({"bytes": self.bytes, "format": self.format, "textPreview": self.preview})
    }
}

pub fn blob_info(
    pol: &Policy,
    hooks: &dyn Hooks,
    file: &str,
    node_id: &str,
) -> OpResult<BlobInfoOutcome> {
    let (store, _) = load(pol, hooks, file)?;
    let id = parse_uuid("节点 ID", node_id)?;
    let node = store
        .get(id)
        .ok_or_else(|| OpError::not_found(format!("节点不存在：{node_id}")))?;
    let bytes = match &node.value {
        XValue::Blob(b) => b,
        _ => return Err(OpError::invalid(format!("节点不是二进制块：{node_id}"))),
    };
    let format = store.child_by_name(node, "@format").and_then(|f| match &f.value {
        XValue::Text(t) => Some(t.clone()),
        _ => None,
    });
    let preview = match std::str::from_utf8(&bytes[..bytes.len().min(200)]) {
        Ok(s) if !s.contains('\0') => Some(s.to_string()),
        _ => None,
    };
    Ok(BlobInfoOutcome { bytes: bytes.len(), format, preview })
}

// ============================================================================
// 写操作
// ============================================================================

pub struct CreateOutcome {
    pub id: Uuid,
    pub name: String,
    /// 是否新开了一个分片（词库目录模式下）。
    pub new_shard: bool,
    pub warnings: Vec<String>,
}

pub fn create_node(
    pol: &Policy,
    hooks: &dyn Hooks,
    file: &str,
    parent: Option<&str>,
    name: &str,
    value: XValue,
    no_history: bool,
) -> OpResult<CreateOutcome> {
    let p = parse_parent(parent)?;
    let path = pol.resolve(file)?;

    // 词库目录：nil → 新分片；有父 → 改写父所在分片。
    if path.is_dir() {
        return match p {
            None => {
                shard::read_manifest(&path).map_err(OpError::internal)?;
                let mut store = tree::Store::new();
                let n = store.create(None, name, value, !no_history);
                let filename = format!("{}.xirang", n.id);
                let shard_path = path.join(&filename);
                store.save(&shard_path).map_err(|e| OpError::internal(e.to_string()))?;
                update_index(&shard_path);
                let entry = shard::ShardEntry { name: name.to_string(), filename };
                if let Err(e) = shard::add_shard_entry(&path, entry) {
                    // 清单更新失败 → 回滚刚写的分片，别留半成品
                    let _ = std::fs::remove_file(&shard_path);
                    let _ = std::fs::remove_file(xirang_core::index::sidecar_path(&shard_path));
                    return Err(OpError::internal(e));
                }
                Ok(CreateOutcome {
                    id: n.id,
                    name: name.to_string(),
                    new_shard: true,
                    warnings: Vec::new(),
                })
            }
            Some(pid) => {
                let (mut store, shard_path) = load_shard(&path, pid)?;
                let before = store.clone();
                let n = store.create(Some(pid), name, value, !no_history);
                save(hooks, &store, &shard_path, true, Some(&before))?;
                Ok(CreateOutcome {
                    id: n.id,
                    name: name.to_string(),
                    new_shard: false,
                    warnings: Vec::new(),
                })
            }
        };
    }

    let mut warnings = Vec::new();
    let mut store = if path.exists() {
        let store = tree::Store::load_view(&path).map_err(OpError::internal)?;
        hooks.on_load(&path, &store);
        store
    } else {
        tree::Store::new()
    };

    let mut parent_name = None;
    if let Some(pid) = p {
        match store.get(pid) {
            None => {
                if !pol.force {
                    return Err(OpError::guarded(
                        format!("父节点不存在：{pid}"),
                        "继续创建会留下 E011 父边断裂；确实要建请显式强制（CLI：--yes，MCP：force）",
                    ));
                }
            }
            Some(n) => parent_name = Some(n.name.clone()),
        }
    }
    if let Some(pn) = &parent_name {
        if !pn.starts_with('@') {
            warnings.push(format!(
                "（提示：在「{pn}」下新增节点会改变该子树形状码，可能影响按结构检索；--yes 跳过）"
            ));
        }
    }
    let n = store.create(p, name, value, !no_history);
    store.save(&path).map_err(|e| OpError::internal(e.to_string()))?;
    hooks.on_save(&path, &store);
    update_index(&path);
    Ok(CreateOutcome { id: n.id, name: name.to_string(), new_shard: false, warnings })
}

pub struct SetOutcome {
    pub id: Uuid,
}

pub fn set_value(
    pol: &Policy,
    hooks: &dyn Hooks,
    file: &str,
    node: &str,
    value: XValue,
    no_history: bool,
) -> OpResult<SetOutcome> {
    let id = parse_uuid("节点 ID", node)?;
    let (mut store, path, in_collection) = load_target(pol, hooks, file, Some(id))?;
    guard_editable(pol, &store, id)?;
    let before = store.clone();
    let r = if no_history { store.set_quiet(id, value) } else { store.update(id, value) };
    r.map_err(OpError::internal)?;
    save(hooks, &store, &path, in_collection, Some(&before))?;
    Ok(SetOutcome { id })
}

pub struct RenameOutcome {
    pub id: Uuid,
    pub name: String,
}

pub fn rename_node(
    pol: &Policy,
    hooks: &dyn Hooks,
    file: &str,
    node: &str,
    new_name: &str,
    no_history: bool,
) -> OpResult<RenameOutcome> {
    let id = parse_uuid("节点 ID", node)?;
    if new_name.is_empty() {
        return Err(OpError::invalid("新名字不能为空（要清空名字请用 rm）"));
    }
    if new_name.as_bytes().len() > 255 {
        return Err(OpError::invalid("名字超 255 字节"));
    }
    let (mut store, path, in_collection) = load_target(pol, hooks, file, Some(id))?;
    guard_editable(pol, &store, id)?;
    let before = store.clone();
    let r = if no_history {
        store.rename_quiet(id, new_name.to_string())
    } else {
        store.rename(id, new_name.to_string())
    };
    r.map_err(OpError::internal)?;
    save(hooks, &store, &path, in_collection, Some(&before))?;
    Ok(RenameOutcome { id, name: new_name.to_string() })
}

pub struct RemoveOutcome {
    pub id: Uuid,
}

pub fn remove_node(
    pol: &Policy,
    hooks: &dyn Hooks,
    file: &str,
    node: &str,
) -> OpResult<RemoveOutcome> {
    let id = parse_uuid("节点 ID", node)?;
    let (mut store, path, in_collection) = load_target(pol, hooks, file, Some(id))?;
    guard_editable(pol, &store, id)?;
    let before = store.clone();
    store.remove(id).map_err(OpError::internal)?;
    save(hooks, &store, &path, in_collection, Some(&before))?;
    Ok(RemoveOutcome { id })
}

pub struct LinkOutcome {
    pub from: Uuid,
    pub to: Uuid,
}

pub fn link_nodes(
    pol: &Policy,
    hooks: &dyn Hooks,
    file: &str,
    from: &str,
    to: &str,
    no_history: bool,
) -> OpResult<LinkOutcome> {
    let f = parse_uuid("节点 ID", from)?;
    let t = parse_uuid("节点 ID", to)?;
    let path = pol.resolve(file)?;

    if path.is_dir() {
        let (mut store, shard_path) = load_shard(&path, f)?;
        guard_editable(pol, &store, f)?;
        // 目标应存在于该词库；否则会留下 R001 引用断裂。
        let target_ok = shard::Collection::open(&path)
            .ok()
            .map(|c| c.find(t).is_some())
            .unwrap_or(false);
        if !target_ok && !pol.force {
            return Err(OpError::guarded(
                format!("引用目标不在该词库：{t}"),
                "继续连边会留下 R001 引用断裂；确实要连请显式强制（CLI：--yes，MCP：force）",
            ));
        }
        let before = store.clone();
        let r = if no_history {
            store.set_quiet(f, XValue::Reference(t))
        } else {
            store.update(f, XValue::Reference(t))
        };
        r.map_err(OpError::internal)?;
        save(hooks, &store, &shard_path, true, Some(&before))?;
        return Ok(LinkOutcome { from: f, to: t });
    }

    let (mut store, path) = load(pol, hooks, file)?;
    guard_editable(pol, &store, f)?;
    if (store.get(f).is_none() || store.get(t).is_none()) && !pol.force {
        return Err(OpError::guarded(
            "from 或 to 节点不存在",
            "继续连边会留下 R001 引用断裂；确实要连请显式强制（CLI：--yes，MCP：force）",
        ));
    }
    let r = if no_history {
        store.set_quiet(f, XValue::Reference(t))
    } else {
        store.update(f, XValue::Reference(t))
    };
    r.map_err(OpError::internal)?;
    save(hooks, &store, &path, false, None)?;
    Ok(LinkOutcome { from: f, to: t })
}

pub struct CopyOutcome {
    pub src_id: Uuid,
    pub src_name: String,
    pub new_id: Uuid,
    pub warnings: Vec<String>,
}

pub fn copy_node(
    pol: &Policy,
    hooks: &dyn Hooks,
    file: &str,
    node: &str,
    parent: Option<&str>,
    blank: bool,
    no_history: bool,
) -> OpResult<CopyOutcome> {
    let (mut store, path) = load(pol, hooks, file)?;
    let id = parse_uuid("节点 ID", node)?;
    let src = store
        .get(id)
        .cloned()
        .ok_or_else(|| OpError::not_found(format!("节点不存在：{node}")))?;
    let p = parse_parent(parent)?;
    let mut warnings = Vec::new();
    // 软提示：复制到非辅助父节点下会改变其子树形状码
    if !pol.force {
        if let Some(pid) = p {
            guard_editable(pol, &store, pid)?;
            if let Some(pn) = store.get(pid) {
                if !pn.name.starts_with('@') {
                    warnings.push(format!(
                        "（提示：在「{}」下复制子树会改变该子树形状码，可能影响按结构检索；--yes 跳过）",
                        pn.name
                    ));
                }
            }
        }
    }
    let opts = tree::CopyOptions { blank_values: blank, history: !no_history };
    let new_id = store.copy_subtree(id, p, &opts).map_err(OpError::internal)?;
    save(hooks, &store, &path, false, None)?;
    Ok(CopyOutcome { src_id: id, src_name: src.name, new_id, warnings })
}

pub struct FillOutcome {
    pub count: usize,
}

/// 值的显示形式（不查引用目标），用于把赋值结果回报给调用方。
pub fn display_of_value(v: &XValue) -> String {
    match v {
        XValue::Empty => String::new(),
        XValue::Int(n) => n.to_string(),
        XValue::Float(f) => f.to_string(),
        XValue::Bool(b) => if *b { "true" } else { "false" }.to_string(),
        XValue::Text(s) => s.clone(),
        XValue::Reference(u) => format!("→ {u}"),
        XValue::Blob(b) => format!("[blob {} 字节]", b.len()),
    }
}

/// 按名字 / 路径映射赋值：`路径=值`（相对 root 节点）。
/// 值由调用方解析好（CLI 走 `parse_value` 的文本推断，MCP 按 JSON 类型）。
pub fn fill_values(
    pol: &Policy,
    hooks: &dyn Hooks,
    file: &str,
    root: &str,
    assigns: &[(String, XValue)],
    no_history: bool,
) -> OpResult<FillOutcome> {
    let root_id = parse_uuid("节点 ID", root)?;
    let (mut store, path, in_collection) = load_target(pol, hooks, file, Some(root_id))?;
    if store.get(root_id).is_none() {
        return Err(OpError::not_found(format!("节点不存在：{root}")));
    }
    guard_editable(pol, &store, root_id)?;
    let before = store.clone();

    // 先解析出全部目标（任一失败则一个都不动），再统一写入。
    let mut targets: Vec<Uuid> = Vec::new();
    for (path_expr, _value) in assigns {
        let segs: Vec<&str> = path_expr.split('/').collect();
        if segs.is_empty() || segs.iter().any(|s| s.is_empty()) {
            return Err(OpError::invalid(format!("路径为空：{path_expr}")));
        }
        let mut cur_id = root_id;
        for seg in &segs[..segs.len() - 1] {
            let Some(cur) = store.get(cur_id) else {
                return Err(OpError::not_found(format!("路径不存在：{path_expr}")));
            };
            match store.child_by_name(cur, seg) {
                Some(c) => cur_id = c.id,
                None => return Err(OpError::not_found(format!("路径不存在：{path_expr}"))),
            }
        }
        let last = segs.last().unwrap();
        let target_id = store
            .get(cur_id)
            .and_then(|c| store.child_by_name(c, last))
            .map(|c| c.id)
            .ok_or_else(|| OpError::not_found(format!("路径不存在：{path_expr}")))?;
        targets.push(target_id);
    }

    for ((path_expr, value), target_id) in assigns.iter().zip(targets) {
        let r = if no_history {
            store.set_quiet(target_id, value.clone())
        } else {
            store.update(target_id, value.clone())
        };
        r.map_err(|e| OpError::internal(format!("{e}（{path_expr}）")))?;
    }
    save(hooks, &store, &path, in_collection, Some(&before))?;
    Ok(FillOutcome { count: assigns.len() })
}

pub struct PruneHistoryOutcome {
    pub removed: usize,
    pub kept: usize,
    pub nodes_before: usize,
    pub nodes_after: usize,
    pub bytes_before: u64,
    pub bytes_after: u64,
    /// 预演：只算不落盘
    pub dry_run: bool,
}

/// 裁剪某节点的 `@history` 留痕：保留最近 `keep` 条，可选只裁早于 `before` 的。
/// 破坏性（丢掉回滚能力）→ 没有 `force` 时若确实有东西可裁，返回 `guarded` 让调用方决定。
pub fn prune_history(
    pol: &Policy,
    hooks: &dyn Hooks,
    file: &str,
    node: &str,
    keep: usize,
    before: Option<&str>,
    dry_run: bool,
) -> OpResult<PruneHistoryOutcome> {
    let id = parse_uuid("节点 ID", node)?;
    let (mut store, path, in_collection) = load_target(pol, hooks, file, Some(id))?;
    if store.get(id).is_none() {
        return Err(OpError::not_found(format!("节点不存在：{node}")));
    }
    guard_editable(pol, &store, id)?;
    let bytes_before = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    let nodes_before = store.len();
    let report = store.prune_history(id, keep, before).map_err(OpError::internal)?;
    if report.removed > 0 && !pol.force {
        return Err(OpError::guarded(
            format!(
                "裁剪留痕会丢掉这 {} 条快照的回滚能力（保留 {} 条）",
                report.removed, report.kept
            ),
            "确认要裁就带 force（CLI：--yes）",
        ));
    }
    let bytes_after = if dry_run || report.removed == 0 {
        bytes_before
    } else {
        save(hooks, &store, &path, in_collection, None)?;
        std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0)
    };
    Ok(PruneHistoryOutcome {
        removed: report.removed,
        kept: report.kept,
        nodes_before,
        nodes_after: report.after_nodes,
        bytes_before,
        bytes_after,
        dry_run,
    })
}

pub struct RevertOutcome {
    pub id: Uuid,
    pub name: String,
    /// 没有可回滚的快照时带出原因（CLI 打印后仍算成功）。
    pub no_snapshot: Option<String>,
}

pub fn revert_node(
    pol: &Policy,
    hooks: &dyn Hooks,
    file: &str,
    node: &str,
) -> OpResult<RevertOutcome> {
    let (mut store, path) = load(pol, hooks, file)?;
    let id = parse_uuid("节点 ID", node)?;
    if store.get(id).is_none() {
        return Err(OpError::not_found(format!("节点不存在：{node}")));
    }
    if let Err(e) = store.revert(id) {
        let name = store.get(id).map(|n| n.name.clone()).unwrap_or_default();
        return Ok(RevertOutcome { id, name, no_snapshot: Some(e) });
    }
    save(hooks, &store, &path, false, None)?;
    let name = store.get(id).map(|n| n.name.clone()).unwrap_or_default();
    Ok(RevertOutcome { id, name, no_snapshot: None })
}

pub struct BlobImportOutcome {
    pub id: Uuid,
    pub name: String,
}

pub fn blob_import(
    pol: &Policy,
    hooks: &dyn Hooks,
    file: &str,
    parent: Option<&str>,
    source: &str,
) -> OpResult<BlobImportOutcome> {
    let (mut store, path) = load(pol, hooks, file)?;
    let src = pol.resolve(source)?;
    let bytes = std::fs::read(&src)
        .map_err(|e| OpError::invalid(format!("读不到源文件 {source}：{e}")))?;
    let p = parse_parent(parent)?;
    let name = src
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "blob".to_string());
    let n = store.create(p, &name, XValue::Blob(bytes), true);
    save(hooks, &store, &path, false, None)?;
    Ok(BlobImportOutcome { id: n.id, name })
}

pub struct BlobExportOutcome {
    pub bytes: usize,
    pub dest: String,
}

pub fn blob_export(
    pol: &Policy,
    hooks: &dyn Hooks,
    file: &str,
    node_id: &str,
    dest: &str,
) -> OpResult<BlobExportOutcome> {
    let (store, _) = load(pol, hooks, file)?;
    let id = parse_uuid("节点 ID", node_id)?;
    let node = store
        .get(id)
        .ok_or_else(|| OpError::not_found(format!("节点不存在：{node_id}")))?;
    let bytes = match &node.value {
        XValue::Blob(b) => b,
        _ => return Err(OpError::invalid(format!("节点不是二进制块：{node_id}"))),
    };
    let out = pol.resolve(dest)?;
    if out.exists() && !pol.force {
        return Err(OpError::guarded(
            format!("目标文件已存在：{dest}"),
            "导出会覆盖它；确认覆盖请显式强制（CLI：--yes，MCP：force）",
        ));
    }
    std::fs::write(&out, bytes).map_err(|e| OpError::internal(e.to_string()))?;
    Ok(BlobExportOutcome { bytes: bytes.len(), dest: dest.to_string() })
}

// ============================================================================
// 模板 / 实例
// ============================================================================

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

/// 从样例 JSON 对象构建模板结构（键→节点名，嵌套→子树）。
pub fn build_template_structure(
    store: &mut tree::Store,
    parent: Uuid,
    val: &JsonValue,
) -> Result<(), String> {
    let obj = val.as_object().ok_or("模板样例应为 JSON 对象")?;
    for (k, v) in obj {
        let node = store.create(Some(parent), k, XValue::Empty, false);
        if v.is_object() {
            build_template_structure(store, node.id, v)?;
        }
    }
    Ok(())
}

/// 建一棵模板定义：在 `parent`（None=自由根）下建一个名为 `name` 的根，
/// 挂 `@模板`(空) 标记，并从样例 JSON 建结构；返回模板根 id。
pub fn build_template(
    store: &mut tree::Store,
    parent: Option<Uuid>,
    name: &str,
    sample: &JsonValue,
) -> Result<Uuid, String> {
    let tpl = store.create(parent, name, XValue::Empty, false);
    store.create(Some(tpl.id), "@模板", XValue::Empty, false); // 标记：我是模板
    build_template_structure(store, tpl.id, sample)?;
    Ok(tpl.id)
}

/// 递归把模板结构镜像进实例（跳过 @ 辅助节点），按名字从记录取值填充。
fn fill_instance(
    store: &mut tree::Store,
    tpl_id: Uuid,
    inst_id: Uuid,
    record: &JsonValue,
) -> Result<(), String> {
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

/// 从模板实例化一棵树：在 `inst_parent` 下建实例根（挂 `@实例` + `@模板` 引用），按记录填值。
pub fn instantiate(
    store: &mut tree::Store,
    tpl_id: Uuid,
    inst_parent: Option<Uuid>,
    record: &JsonValue,
) -> Result<Uuid, String> {
    let tpl = store.get(tpl_id).cloned().ok_or("模板不存在")?;
    let inst_root = store.create(inst_parent, &tpl.name, XValue::Empty, false);
    store.create(Some(inst_root.id), "@实例", XValue::Empty, false);
    store.create(Some(inst_root.id), "@模板", XValue::Reference(tpl_id), false);
    fill_instance(store, tpl_id, inst_root.id, record)?;
    Ok(inst_root.id)
}

/// 把一个 JSON 值建为节点。含 `{"@ref": "目标"}` → 建引用边（目标不存在则建同名占位）。
fn json_value_node(
    store: &mut tree::Store,
    parent: Option<Uuid>,
    name: &str,
    v: &JsonValue,
) -> Result<Uuid, String> {
    if let Some(tgt) = v.get("@ref").and_then(|t| t.as_str()) {
        let tid = Uuid::parse(tgt)
            .or_else(|| store.nodes().iter().find(|n| n.name == tgt).map(|n| n.id))
            .unwrap_or_else(|| store.create(parent, tgt, XValue::Empty, false).id);
        return Ok(store.create(parent, name, XValue::Reference(tid), false).id);
    }
    // 标量 → 值；对象/数组 → 空容器（子节点由递归建）。前导 0 字符串用 JSON 原样文本。
    Ok(store.create(parent, name, value_from_json(Some(v)), false).id)
}

/// 把嵌套 JSON 值构建成子树（挂在 parent 下；None = 自由根）。
/// 对象 → 有名子节点；数组 → 按下标 0,1,2 子节点；标量 → 值；`{"@ref":…}` → 引用边。
pub fn build_json_tree(
    store: &mut tree::Store,
    parent: Option<Uuid>,
    val: &JsonValue,
) -> Result<(), String> {
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
pub fn find_template_roots(store: &tree::Store) -> Vec<usize> {
    store
        .nodes()
        .iter()
        .enumerate()
        .filter(|(_, n)| store.is_template_root(n))
        .map(|(i, _)| i)
        .collect()
}

/// 某模板的所有实例根（挂 `@实例` 且 `@模板` 引用指向 tpl_id）。
pub fn find_instances_of(store: &tree::Store, tpl_id: Uuid) -> Vec<usize> {
    store
        .nodes()
        .iter()
        .enumerate()
        .filter(|(_, n)| store.is_instance_root(n) && store.template_ref(n) == Some(tpl_id))
        .map(|(i, _)| i)
        .collect()
}

/// 按「名字 或 编号」唯一解析一个模板根。名字匹配到多个时报错（建议用编号）。
pub fn resolve_template(store: &tree::Store, name_or_id: &str) -> OpResult<Uuid> {
    if let Some(id) = Uuid::parse(name_or_id) {
        if let Some(n) = store.get(id) {
            if store.is_template_root(n) {
                return Ok(id);
            }
            return Err(OpError::invalid("该节点不是模板定义（根上无 @模板 空标记）"));
        }
        return Err(OpError::not_found("模板不存在"));
    }
    let hits: Vec<&Node> = store
        .nodes()
        .iter()
        .filter(|n| n.name == name_or_id && store.is_template_root(n))
        .collect();
    match hits.len() {
        0 => Err(OpError::not_found(format!("模板不存在：{name_or_id}"))),
        1 => Ok(hits[0].id),
        n => Err(OpError::invalid(format!("模板名「{name_or_id}」匹配到 {n} 个，请改用编号"))),
    }
}

pub struct TemplateItem {
    pub id: Uuid,
    pub name: String,
    pub instances: usize,
}

pub struct TemplateDefineOutcome {
    pub id: Uuid,
    pub name: String,
}

pub fn template_define(
    pol: &Policy,
    hooks: &dyn Hooks,
    file: &str,
    name: &str,
    sample: &JsonValue,
) -> OpResult<TemplateDefineOutcome> {
    let path = pol.resolve(file)?;
    // 与 `new` 一致：文件不存在就新建空库。
    let mut store = if path.exists() {
        let s = tree::Store::load_view(&path).map_err(OpError::internal)?;
        hooks.on_load(&path, &s);
        s
    } else {
        tree::Store::new()
    };
    if store.nodes().iter().any(|n| n.name == name && store.is_template_root(n)) {
        return Err(OpError::invalid(format!("模板已存在：{name}（先删再建）")));
    }
    let id = build_template(&mut store, None, name, sample).map_err(OpError::invalid)?;
    store.save(&path).map_err(|e| OpError::internal(e.to_string()))?;
    hooks.on_save(&path, &store);
    update_index(&path);
    Ok(TemplateDefineOutcome { id, name: name.to_string() })
}

pub fn template_list(pol: &Policy, hooks: &dyn Hooks, file: &str) -> OpResult<Vec<TemplateItem>> {
    let (store, _) = load(pol, hooks, file)?;
    Ok(find_template_roots(&store)
        .into_iter()
        .map(|i| {
            let t = &store.nodes()[i];
            TemplateItem {
                id: t.id,
                name: t.name.clone(),
                instances: find_instances_of(&store, t.id).len(),
            }
        })
        .collect())
}

pub struct TemplateRemoveOutcome {
    pub name: String,
    pub id: Uuid,
    pub removed_instances: usize,
}

pub fn template_remove(
    pol: &Policy,
    hooks: &dyn Hooks,
    file: &str,
    name: &str,
) -> OpResult<TemplateRemoveOutcome> {
    let (mut store, path) = load(pol, hooks, file)?;
    let tpl_id = resolve_template(&store, name)?;
    let tpl_name = store.get(tpl_id).map(|n| n.name.clone()).unwrap_or_default();
    if !pol.force {
        return Err(OpError::guarded(
            format!("删除模板「{tpl_name}」会连同其结构 + 所有实例一并移除"),
            "确认要删请显式强制（CLI：--yes，MCP：force）",
        ));
    }
    // 先收集实例根：删了模板子树后索引会变。连同实例树一起删，
    // 否则会留下指向已删模板根的悬挂 @模板 引用（R001）与找不到的孤儿实例。
    let inst_ids: Vec<Uuid> = find_instances_of(&store, tpl_id)
        .into_iter()
        .map(|i| store.nodes()[i].id)
        .collect();
    for id in &inst_ids {
        store.remove_subtree(*id).map_err(OpError::internal)?;
    }
    store.remove_subtree(tpl_id).map_err(OpError::internal)?;
    save(hooks, &store, &path, false, None)?;
    Ok(TemplateRemoveOutcome { name: tpl_name, id: tpl_id, removed_instances: inst_ids.len() })
}

pub struct ImportAppendOutcome {
    pub nodes_after: usize,
}

pub fn import_append(
    pol: &Policy,
    hooks: &dyn Hooks,
    file: &str,
    parent: Option<&str>,
    value: &JsonValue,
) -> OpResult<ImportAppendOutcome> {
    let (mut store, path) = load(pol, hooks, file)?;
    let p = parse_parent(parent)?;
    build_json_tree(&mut store, p, value).map_err(OpError::invalid)?;
    save(hooks, &store, &path, false, None)?;
    Ok(ImportAppendOutcome { nodes_after: store.len() })
}

pub struct ImportInstancesOutcome {
    pub template_id: Uuid,
    pub template_name: String,
    pub count: usize,
}

pub fn import_instances(
    pol: &Policy,
    hooks: &dyn Hooks,
    file: &str,
    template: &str,
    records: &[JsonValue],
    under: Option<&str>,
) -> OpResult<ImportInstancesOutcome> {
    let (mut store, path) = load(pol, hooks, file)?;
    let tpl_id = resolve_template(&store, template)?;
    let tpl_name = store.get(tpl_id).map(|n| n.name.clone()).unwrap_or_default();
    let inst_parent = parse_parent(under)?;
    for rec in records {
        instantiate(&mut store, tpl_id, inst_parent, rec).map_err(OpError::invalid)?;
    }
    save(hooks, &store, &path, false, None)?;
    Ok(ImportInstancesOutcome { template_id: tpl_id, template_name: tpl_name, count: records.len() })
}

// ============================================================================
// 格式转换
// ============================================================================

pub struct ExportOutcome {
    pub format: String,
    pub text: String,
}

impl ExportOutcome {
    pub fn to_json(&self) -> JsonValue {
        json!({"format": self.format, "text": self.text})
    }
}

pub fn export_data(
    pol: &Policy,
    hooks: &dyn Hooks,
    file: &str,
    format: &str,
    subtree: Option<&str>,
) -> OpResult<ExportOutcome> {
    let (store, _) = load(pol, hooks, file)?;
    let store = match subtree {
        None => store,
        Some(id_str) => {
            let id = parse_uuid("节点 ID", id_str)?;
            let root = store
                .get(id)
                .ok_or_else(|| OpError::not_found(format!("节点不存在：{id_str}")))?;
            let mut sub = store.sub_store(root);
            // 子树重新扎根：根父指针置 nil，导出的子树才能直接再导入（否则 E011）。
            let _ = sub.set_parent(id, None);
            sub
        }
    };
    let text = match format {
        "json" => convert::to_json(&store),
        "yaml" => convert::to_yaml(&store),
        "xml" => convert::to_xml(&store),
        "md" | "markdown" => convert::to_md(&store),
        _ => {
            return Err(OpError::invalid(format!(
                "未知格式：{format}（支持 json / yaml / xml / md）"
            )))
        }
    };
    Ok(ExportOutcome { format: format.to_string(), text })
}

pub struct ImportOutcome {
    pub nodes: usize,
    /// 覆盖前该文件的节点数（None = 文件原本不存在或读不出）。
    pub previous_nodes: Option<usize>,
}

pub fn import_data(
    pol: &Policy,
    hooks: &dyn Hooks,
    file: &str,
    format: &str,
    text: &str,
) -> OpResult<ImportOutcome> {
    let path = pol.resolve(file)?;
    // 导入是「整文件替换」：原文件非空就要显式强制，避免静默丢数据。
    let before = tree::Store::load_view(&path).ok().map(|s| s.len());
    if let Some(n) = before {
        if n > 0 && !pol.force {
            return Err(OpError::guarded(
                format!("{file} 里已有 {n} 个节点，import 是整文件替换"),
                "覆盖会丢掉原内容；确认覆盖请显式强制（CLI：--yes，MCP：force），要保留原内容请用 append / instantiate",
            ));
        }
    }
    let store = match format {
        "json" => convert::from_json(text),
        "yaml" => convert::from_yaml(text),
        "xml" => convert::from_xml(text),
        _ => {
            return Err(OpError::invalid(format!(
                "未知格式：{format}（支持 json / yaml / xml）"
            )))
        }
    };
    let store = store.map_err(OpError::invalid)?;
    let nodes = store.len();
    store.save(&path).map_err(|e| OpError::internal(e.to_string()))?;
    hooks.on_save(&path, &store);
    update_index(&path);
    Ok(ImportOutcome { nodes, previous_nodes: before })
}

// ============================================================================
// MCP 工具元数据（工具名 / 动作清单的唯一出处）
// ============================================================================

/// MCP 暴露的工具名（10 个，按领域分组）。
pub const MCP_TOOLS: &[&str] = &[
    "context",
    "file_info",
    "file_validate",
    "file_diff",
    "tree",
    "query",
    "node",
    "template",
    "convert",
    "blob",
];

pub const QUERY_ACTIONS: &[&str] = &["find", "match", "instances", "refs", "history"];
pub const NODE_ACTIONS: &[&str] =
    &["create", "set", "rename", "remove", "link", "copy", "fill", "revert", "prune_history"];
pub const TEMPLATE_ACTIONS: &[&str] = &["define", "list", "instantiate", "remove"];
pub const CONVERT_ACTIONS: &[&str] = &["export", "import", "append"];
pub const BLOB_ACTIONS: &[&str] = &["import", "export", "info"];
