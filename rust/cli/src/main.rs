//! 息壤 CLI：xr 命令（info / tree / validate），复用 xirang-core。

use std::collections::{HashMap, HashSet};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use serde_json::json;
use xirang_core::codec::{parse_value, Node, Uuid, Value};
use xirang_core::{catalog, convert, index, query, shard, tree, validator};

mod ops;

/// 读/写命令是否顺带维护本机目录（默认开，可用 `--no-index` 或 `XIRANG_INDEX=off` 关闭）。
static INDEX_ENABLED: AtomicBool = AtomicBool::new(true);

fn index_enabled() -> bool {
    INDEX_ENABLED.load(Ordering::Relaxed)
}

fn fmt_value(store: &tree::Store, node: &Node) -> Option<String> {
    match &node.value {
        Value::Empty => None,
        Value::Int(n) => Some(n.to_string()),
        Value::Float(f) => Some(f.to_string()),
        Value::Bool(b) => Some(if *b { "true" } else { "false" }.to_string()),
        Value::Text(s) => Some(s.clone()),
        Value::Reference(u) => {
            let target = store.get(*u);
            Some(format!("→ {}", target.map(|t| t.name.as_str()).unwrap_or(&u.to_string())))
        }
        Value::Blob(b) => Some(format!("[blob {} 字节]", b.len())),
    }
}

fn label(store: &tree::Store, node: &Node) -> String {
    match fmt_value(store, node) {
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

/// 树视图的输出选项。
#[derive(Clone, Copy, Default)]
struct TreeOpts {
    skip_aux: bool,
    show_ids: bool,
    /// 只展开到第 N 层（根算第 1 层）；None = 不限。
    max_depth: Option<usize>,
}

/// 输出目的地：终端直出，或者交给分页器（`$PAGER` / `less -R`）。
enum Sink {
    Plain(std::io::BufWriter<std::io::Stdout>),
    Paged { child: std::process::Child, w: std::io::BufWriter<std::process::ChildStdin> },
}

impl std::io::Write for Sink {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        match self {
            Sink::Plain(w) => w.write(b),
            Sink::Paged { w, .. } => w.write(b),
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Sink::Plain(w) => w.flush(),
            Sink::Paged { w, .. } => w.flush(),
        }
    }
}

impl Sink {
    /// 收尾：直出就 flush；分页就关掉管道再等分页器退出。
    fn finish(self) {
        match self {
            Sink::Plain(mut w) => {
                let _ = w.flush();
            }
            Sink::Paged { mut child, w } => {
                drop(w);
                let _ = child.wait();
            }
        }
    }
}

/// stdout 是终端（不是管道/重定向）时才考虑分页；`--no-pager` 一律直出。
fn make_sink(no_pager: bool) -> Sink {
    use std::io::Write as _;
    use std::process::{Command, Stdio};
    let tty = unsafe { libc::isatty(1) } == 1;
    if no_pager || !tty {
        return Sink::Plain(std::io::BufWriter::new(std::io::stdout()));
    }
    let cmd = std::env::var("PAGER").unwrap_or_else(|_| "less -R".to_string());
    let spawned = Command::new("sh")
        .arg("-c")
        .arg(&cmd)
        .stdin(Stdio::piped())
        .spawn()
        .ok()
        .and_then(|mut c| c.stdin.take().map(|w| (c, w)));
    match spawned {
        Some((child, w)) => {
            let mut w = std::io::BufWriter::new(w);
            let _ = w.flush();
            Sink::Paged { child, w }
        }
        // 没装分页器：照常直出，别让命令失败
        None => Sink::Plain(std::io::BufWriter::new(std::io::stdout())),
    }
}

/// 打印一棵子树；`budget` 是「还剩几行可打」，打完即停（用于 `--head`）。
/// 返回本次实际打印的节点数。
fn print_tree(
    out: &mut Sink,
    store: &tree::Store,
    node: &Node,
    opts: &TreeOpts,
    budget: &mut usize,
) -> usize {
    let mut seen = HashSet::new();
    let before = *budget;
    let _ = print_tree_rec(out, store, node, "", true, 1, opts, &mut seen, budget);
    before - *budget
}

fn print_tree_rec(
    out: &mut Sink,
    store: &tree::Store,
    node: &Node,
    prefix: &str,
    is_last: bool,
    depth: usize,
    opts: &TreeOpts,
    seen: &mut HashSet<Uuid>,
    budget: &mut usize,
) -> std::io::Result<()> {
    if *budget == 0 {
        return Ok(());
    }
    let connector = if prefix.is_empty() {
        ""
    } else if is_last {
        "└─ "
    } else {
        "├─ "
    };
    let id_suffix = if opts.show_ids { format!(" <{}>", node.id) } else { String::new() };
    writeln!(out, "{}{}{}{}", prefix, connector, label(store, node), id_suffix)?;
    *budget -= 1;
    // 父边成环（E006）时兜底：同一节点只展开一次，避免无限递归/栈溢出。
    if !seen.insert(node.id) {
        return Ok(());
    }
    // --depth：到层数就不再往下展开
    if let Some(m) = opts.max_depth {
        if depth >= m {
            return Ok(());
        }
    }
    let child_prefix = format!("{}{}", prefix, if is_last { "   " } else { "│  " });
    let children = store.children_opt(node, opts.skip_aux);
    let n = children.len();
    for (i, c) in children.iter().enumerate() {
        if *budget == 0 {
            break;
        }
        print_tree_rec(out, store, c, &child_prefix, i == n - 1, depth + 1, opts, seen, budget)?;
    }
    Ok(())
}

fn cmd_info(file: &str) -> i32 {
    let data = match std::fs::read(file) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("错误：{e}");
            return 2;
        }
    };
    let (version, header) = match tree::read_header(&data) {
        Ok(x) => x,
        Err(e) => {
            eprintln!("错误：{e}");
            return 2;
        }
    };
    let nodes = tree::parse_file(&data).unwrap();
    let store = tree::Store::decode(nodes).map_err(tree::codec_error).unwrap_or_else(|e| {
        eprintln!("错误：{e}");
        std::process::exit(2);
    });
    index_store(file, &store);
    let first_line = header.trim().lines().next().unwrap_or("(空)");
    println!("文件: {file}");
    println!("格式版本: {version}");
    println!("头文本: {} 字节，首行: {first_line}", header.len());
    println!("节点数: {}", store.len());
    println!("根节点数: {}", store.roots().len());
    0
}

fn cmd_tree(file: &str, node_id: Option<&str>, opts: &TreeOpts, head: Option<usize>, no_pager: bool) -> i32 {
    let store = match tree::Store::load_view(Path::new(file)) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("错误：{e}");
            return 2;
        }
    };
    index_store(file, &store);
    let total = store.nodes().len();
    let mut sink = make_sink(no_pager);
    let mut budget = head.unwrap_or(usize::MAX);
    let mut printed = 0usize;
    if let Some(id) = node_id {
        let uuid = match xirang_core::codec::Uuid::parse(id) {
            Some(u) => u,
            None => {
                eprintln!("无效节点 ID：{id}");
                return 2;
            }
        };
        match store.get(uuid) {
            Some(root) => printed += print_tree(&mut sink, &store, root, opts, &mut budget),
            None => {
                eprintln!("节点不存在：{id}");
                return 2;
            }
        }
    } else {
        let roots = store.roots();
        for (i, r) in roots.iter().enumerate() {
            if budget == 0 {
                break;
            }
            if i > 0 {
                let _ = writeln!(sink);
            }
            printed += print_tree(&mut sink, &store, r, opts, &mut budget);
        }
    }
    sink.finish();
    // 截断是「提示」，走 stderr，保持 stdout 干净（可直接管道给别的工具）
    if head.is_some() && printed < total {
        eprintln!("（只打印了前 {printed} 个节点，共 {total} 个；去掉 --head 打印全部）");
    }
    if opts.max_depth.is_some() {
        eprintln!("（只展开到第 {} 层；去掉 --depth 显示全部）", opts.max_depth.unwrap_or(0));
    }
    0
}

/// 扁平视图：按文件里的存放顺序，一行一个节点（不缩进）。像浏览文本文件一样看。
fn cmd_cat(file: &str, opts: &TreeOpts, head: Option<usize>, force: bool, no_pager: bool) -> i32 {
    const GUARD: usize = 100_000;
    let store = match tree::Store::load_view(Path::new(file)) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("错误：{e}");
            return 2;
        }
    };
    index_store(file, &store);
    let nodes = store.nodes();
    let total = nodes.len();
    if head.is_none() && !force && total > GUARD {
        eprintln!("这个文件有 {total} 个节点，全量打印会刷屏（可能上百 MB）。");
        eprintln!("请改用：xr cat {file} --head 200，或确实要全打时加 --force。");
        return 2;
    }
    let mut sink = make_sink(no_pager);
    let mut printed = 0usize;
    for nd in nodes {
        if opts.skip_aux && nd.name.starts_with('@') {
            continue;
        }
        if let Some(n) = head {
            if printed >= n {
                break;
            }
        }
        let id_suffix = if opts.show_ids { format!(" <{}>", nd.id) } else { String::new() };
        if writeln!(sink, "{}{}", label(&store, nd), id_suffix).is_err() {
            break;
        }
        printed += 1;
    }
    sink.finish();
    if let Some(n) = head {
        if printed >= n && total > printed {
            eprintln!("（只打印了前 {printed} 个节点，共 {total} 个；去掉 --head 打印全部）");
        }
    }
    0
}

fn cmd_validate(file: &str) -> i32 {
    let store = match tree::Store::load_view(Path::new(file)) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("错误：{e}");
            return 2;
        }
    };
    index_store(file, &store);
    let errs = validator::validate_view(&store);
    if errs.is_empty() {
        println!("校验通过：0 错误");
        return 0;
    }
    for e in &errs {
        println!("{} <{}…> {}", e.code, &e.node_id.to_string()[..8], e.message);
    }
    println!("共 {} 个错误", errs.len());
    1
}

// 值解析统一走核心库（codec::parse_value），避免 CLI / 桌面 / MCP 三处各写一份、行为分叉。

fn load_store(file: &str) -> Result<tree::Store, i32> {
    match tree::Store::load_view(Path::new(file)) {
        Ok(store) => {
            index_store(file, &store);
            Ok(store)
        }
        Err(e) => {
            eprintln!("错误：{e}");
            Err(2)
        }
    }
}

/// 把一个已加载的 Store 增量登记进本机目录（尽力而为，失败静默）。
fn index_store(file: &str, store: &tree::Store) {
    if !index_enabled() {
        return;
    }
    let p = Path::new(file);
    if p.is_dir() {
        return;
    }
    let fp = match catalog::fingerprint(p) {
        Some(f) => f,
        None => return,
    };
    let abs = p
        .canonicalize()
        .ok()
        .and_then(|c| c.to_str().map(|s| s.to_string()))
        .unwrap_or_else(|| file.to_string());
    let cpath = catalog::default_path();
    if catalog::Catalog::is_fresh(&cpath, &abs, fp) {
        return;
    }
    let mut cat = catalog::Catalog::load(&cpath).unwrap_or_default();
    cat.upsert(&abs, fp, &catalog::Catalog::store_uuids(store));
    let _ = cat.save(&cpath);
}

/// 没有现成 Store 时，读文件后登记（用于 `ws` 等）。
fn index_file(file: &str) {
    if !index_enabled() {
        return;
    }
    let p = Path::new(file);
    if p.is_dir() {
        return;
    }
    let fp = match catalog::fingerprint(p) {
        Some(f) => f,
        None => return,
    };
    let abs = p
        .canonicalize()
        .ok()
        .and_then(|c| c.to_str().map(|s| s.to_string()))
        .unwrap_or_else(|| file.to_string());
    if catalog::Catalog::is_fresh(&catalog::default_path(), &abs, fp) {
        return;
    }
    if let Ok(store) = tree::Store::load_view(p) {
        index_store(file, &store);
    }
}

/// 落盘 + 增量登记目录（写命令用）。
fn save_store(store: &tree::Store, path: &Path) -> std::io::Result<()> {
    store.save(path)?;
    index_store(&path.to_string_lossy(), store);
    Ok(())
}

/// 在词库目录里按 UUID 定位分片，读出折叠后的可编辑 Store + 该分片文件路径。
fn load_shard(dir: &Path, id: Uuid) -> Result<(tree::Store, PathBuf), i32> {
    let col = match shard::Collection::open(dir) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("错误：{e}");
            return Err(2);
        }
    };
    let filename = match col.shard_file_for(id) {
        Some(f) => f.to_string(),
        None => {
            eprintln!("节点不存在于词库：{id}");
            return Err(2);
        }
    };
    let path = dir.join(&filename);
    let raw = match tree::Store::load_view(&path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("错误：{e}");
            return Err(2);
        }
    };
    Ok((shard::fold(&raw), path))
}

/// 模板定义保护：若节点（或其在的最近标注根）属于模板定义，则拒绝直接编辑。
/// 返回 Ok(()) = 可编辑；Err(2) = 受保护（除非 --yes）。
fn edit_guard(store: &tree::Store, id: Uuid, yes: bool) -> Result<(), i32> {
    if yes {
        return Ok(());
    }
    match store.get(id) {
        Some(n) if !store.is_editable(n) => {
            eprintln!("（受保护：该节点属于模板定义，不能直接编辑；请用 xr tmpl，或加 --yes 强制）");
            Err(2)
        }
        _ => Ok(()),
    }
}

fn cmd_new(file: &str, parent: &str, name: &str, value: Option<&str>, no_history: bool, yes: bool) -> i32 {
    let p = if parent == "nil" || parent == "root" {
        None
    } else {
        match Uuid::parse(parent) {
            Some(u) => Some(u),
            None => {
                eprintln!("无效父节点 ID：{parent}");
                return 2;
            }
        }
    };
    let v = value.map(parse_value).unwrap_or(Value::Empty);
    let path = Path::new(file);
    if path.is_dir() {
        return new_in_collection(path, p, name, v, no_history);
    }

    // 单文件模式（原有逻辑）
    let mut store = if path.exists() {
        match load_store(file) {
            Ok(s) => s,
            Err(c) => return c,
        }
    } else {
        tree::Store::new()
    };
    // 父节点必须存在，否则会写出父边断裂（E011）的坏数据；--yes 可强制。
    if let Some(pid) = p {
        if store.get(pid).is_none() {
            eprintln!("（父节点不存在：{pid}；加 --yes 仍要创建会留下 E011 父边断裂）");
            if !yes {
                return 2;
            }
        }
    }
    let parent_name = p.and_then(|pid| store.get(pid)).map(|n| n.name.clone());
    if !yes {
        if let Some(pn) = &parent_name {
            if !pn.starts_with('@') {
                eprintln!("（提示：在「{pn}」下新增节点会改变该子树形状码，可能影响按结构检索；--yes 跳过）");
            }
        }
    }
    // no_history = 不挂 @created（初始/批量数据）
    let n = store.create(p, name, v, !no_history);
    if let Err(e) = save_store(&store, path) {
        eprintln!("错误：{e}");
        return 2;
    }
    println!("已创建：{} <{}>", name, n.id);
    0
}

fn new_in_collection(dir: &Path, parent: Option<Uuid>, name: &str, value: Value, no_history: bool) -> i32 {
    match parent {
        None => {
            // 先确认这是一个分片词库：不是就什么都不写，避免留下半成品目录。
            if let Err(e) = shard::read_manifest(dir) {
                eprintln!("错误：{e}");
                return 2;
            }
            let mut store = tree::Store::new();
            let n = store.create(None, name, value, !no_history);
            let filename = format!("{}.xirang", n.id);
            let path = dir.join(&filename);
            if let Err(e) = save_store(&store, &path) {
                eprintln!("错误：{e}");
                return 2;
            }
            let entry = shard::ShardEntry { name: name.to_string(), filename };
            if let Err(e) = shard::add_shard_entry(dir, entry) {
                // 清单更新失败 → 回滚刚写的分片，别留半成品
                let _ = std::fs::remove_file(&path);
                let _ = std::fs::remove_file(index::sidecar_path(&path));
                eprintln!("错误：{e}");
                return 2;
            }
            println!("已创建：{name} <{}>（新分片）", n.id);
            0
        }
        Some(pid) => {
            let (mut store, path) = match load_shard(dir, pid) {
                Ok(x) => x,
                Err(c) => return c,
            };
            let before = store.clone();
            let n = store.create(Some(pid), name, value, !no_history);
            if let Err(e) = shard::append_changes(&path, &before, &store) {
                eprintln!("错误：{e}");
                return 2;
            }
            println!("已创建：{name} <{}>", n.id);
            0
        }
    }
}

fn cmd_set(file: &str, node: &str, value: &str, no_history: bool, yes: bool) -> i32 {
    let id = match Uuid::parse(node) {
        Some(u) => u,
        None => {
            eprintln!("无效节点 ID：{node}");
            return 2;
        }
    };
    let path = Path::new(file);
    if path.is_dir() {
        let (mut store, shard_path) = match load_shard(path, id) {
            Ok(x) => x,
            Err(c) => return c,
        };
        if let Err(c) = edit_guard(&store, id, yes) {
            return c;
        }
        let before = store.clone();
        let r = if no_history {
            store.set_quiet(id, parse_value(value))
        } else {
            store.update(id, parse_value(value))
        };
        if let Err(e) = r {
            eprintln!("错误：{e}");
            return 2;
        }
        if let Err(e) = shard::append_changes(&shard_path, &before, &store) {
            eprintln!("错误：{e}");
            return 2;
        }
        println!("已更新：{node}");
        return 0;
    }

    let mut store = match load_store(file) {
        Ok(s) => s,
        Err(c) => return c,
    };
    if let Err(c) = edit_guard(&store, id, yes) {
        return c;
    }
    let r = if no_history {
        store.set_quiet(id, parse_value(value))
    } else {
        store.update(id, parse_value(value))
    };
    if let Err(e) = r {
        eprintln!("错误：{e}");
        return 2;
    }
    if let Err(e) = save_store(&store, path) {
        eprintln!("错误：{e}");
        return 2;
    }
    println!("已更新：{node}");
    0
}

/// 改名：旧名字进 @history（`--no-history` 则不记），编号不变、引用不断。
fn cmd_rename(file: &str, node: &str, new_name: &str, no_history: bool, yes: bool) -> i32 {
    let id = match Uuid::parse(node) {
        Some(u) => u,
        None => {
            eprintln!("无效节点 ID：{node}");
            return 2;
        }
    };
    if new_name.is_empty() {
        eprintln!("新名字不能为空（要清空名字请用 xr rm）");
        return 2;
    }
    if new_name.as_bytes().len() > 255 {
        eprintln!("名字超 255 字节");
        return 2;
    }
    let path = Path::new(file);
    if path.is_dir() {
        let (mut store, shard_path) = match load_shard(path, id) {
            Ok(x) => x,
            Err(c) => return c,
        };
        if let Err(c) = edit_guard(&store, id, yes) {
            return c;
        }
        let before = store.clone();
        let r = if no_history {
            store.rename_quiet(id, new_name.to_string())
        } else {
            store.rename(id, new_name.to_string())
        };
        if let Err(e) = r {
            eprintln!("错误：{e}");
            return 2;
        }
        if let Err(e) = shard::append_changes(&shard_path, &before, &store) {
            eprintln!("错误：{e}");
            return 2;
        }
        println!("已改名：{node} → {new_name}");
        return 0;
    }

    let mut store = match load_store(file) {
        Ok(s) => s,
        Err(c) => return c,
    };
    if let Err(c) = edit_guard(&store, id, yes) {
        return c;
    }
    let r = if no_history {
        store.rename_quiet(id, new_name.to_string())
    } else {
        store.rename(id, new_name.to_string())
    };
    if let Err(e) = r {
        eprintln!("错误：{e}");
        return 2;
    }
    if let Err(e) = save_store(&store, path) {
        eprintln!("错误：{e}");
        return 2;
    }
    println!("已改名：{node} → {new_name}");
    0
}

fn cmd_rm(file: &str, node: &str, yes: bool) -> i32 {
    let id = match Uuid::parse(node) {
        Some(u) => u,
        None => {
            eprintln!("无效节点 ID：{node}");
            return 2;
        }
    };
    let path = Path::new(file);
    if path.is_dir() {
        let (mut store, shard_path) = match load_shard(path, id) {
            Ok(x) => x,
            Err(c) => return c,
        };
        if let Err(c) = edit_guard(&store, id, yes) {
            return c;
        }
        let before = store.clone();
        if let Err(e) = store.remove(id) {
            eprintln!("错误：{e}");
            return 2;
        }
        if let Err(e) = shard::append_changes(&shard_path, &before, &store) {
            eprintln!("错误：{e}");
            return 2;
        }
        println!("已删除（置空）：{node}");
        return 0;
    }

    let mut store = match load_store(file) {
        Ok(s) => s,
        Err(c) => return c,
    };
    if let Err(c) = edit_guard(&store, id, yes) {
        return c;
    }
    if let Err(e) = store.remove(id) {
        eprintln!("错误：{e}");
        return 2;
    }
    if let Err(e) = save_store(&store, path) {
        eprintln!("错误：{e}");
        return 2;
    }
    println!("已删除（置空）：{node}");
    0
}

fn cmd_link(file: &str, from: &str, to: &str, no_history: bool, yes: bool) -> i32 {
    let (f, t) = match (Uuid::parse(from), Uuid::parse(to)) {
        (Some(f), Some(t)) => (f, t),
        _ => {
            eprintln!("无效节点 ID：{from} 或 {to}");
            return 2;
        }
    };
    let path = Path::new(file);
    if path.is_dir() {
        let (mut store, shard_path) = match load_shard(path, f) {
            Ok(x) => x,
            Err(c) => return c,
        };
        if let Err(c) = edit_guard(&store, f, yes) {
            return c;
        }
        // 目标应存在于该词库；否则会留下 R001 引用断裂。
        let target_ok = shard::Collection::open(path)
            .ok()
            .map(|c| c.find(t).is_some())
            .unwrap_or(false);
        if !target_ok && !yes {
            eprintln!("（引用目标不在该词库：{t}；加 --yes 仍要连边会留下 R001 引用断裂）");
            return 2;
        }
        let before = store.clone();
        let r = if no_history {
            store.set_quiet(f, Value::Reference(t))
        } else {
            store.update(f, Value::Reference(t))
        };
        if let Err(e) = r {
            eprintln!("错误：{e}");
            return 2;
        }
        if let Err(e) = shard::append_changes(&shard_path, &before, &store) {
            eprintln!("错误：{e}");
            return 2;
        }
        println!("已连边：{from} → {to}");
        return 0;
    }

    let mut store = match load_store(file) {
        Ok(s) => s,
        Err(c) => return c,
    };
    if let Err(c) = edit_guard(&store, f, yes) {
        return c;
    }
    if (store.get(f).is_none() || store.get(t).is_none()) && !yes {
        eprintln!("（from 或 to 节点不存在；加 --yes 仍要连边会留下 R001 引用断裂）");
        return 2;
    }
    let r = if no_history {
        store.set_quiet(f, Value::Reference(t))
    } else {
        store.update(f, Value::Reference(t))
    };
    if let Err(e) = r {
        eprintln!("错误：{e}");
        return 2;
    }
    if let Err(e) = save_store(&store, path) {
        eprintln!("错误：{e}");
        return 2;
    }
    println!("已连边：{from} → {to}");
    0
}

fn cmd_copy(file: &str, node: &str, parent: &str, blank: bool, history: bool, yes: bool) -> i32 {
    let mut store = match load_store(file) {
        Ok(s) => s,
        Err(c) => return c,
    };
    let id = match Uuid::parse(node) {
        Some(u) => u,
        None => {
            eprintln!("无效节点 ID：{node}");
            return 2;
        }
    };
    if store.get(id).is_none() {
        eprintln!("节点不存在：{node}");
        return 2;
    }
    let name = store.get(id).map(|n| n.name.clone()).unwrap_or_default();
    let p = if parent == "nil" || parent == "root" {
        None
    } else {
        match Uuid::parse(parent) {
            Some(u) => Some(u),
            None => {
                eprintln!("无效父节点 ID：{parent}");
                return 2;
            }
        }
    };
    // 软提示：复制到非辅助父节点下会改变其子树形状码
    if !yes {
        if let Some(pid) = p {
            if let Err(c) = edit_guard(&store, pid, yes) {
                return c;
            }
            if let Some(pn) = store.get(pid) {
                if !pn.name.starts_with('@') {
                    eprintln!("（提示：在「{}」下复制子树会改变该子树形状码，可能影响按结构检索；--yes 跳过）", pn.name);
                }
            }
        }
    }
    let opts = tree::CopyOptions { blank_values: blank, history };
    match store.copy_subtree(id, p, &opts) {
        Ok(new_id) => {
            if let Err(e) = save_store(&store, Path::new(file)) {
                eprintln!("错误：{e}");
                return 2;
            }
            println!("已复制：{} <{}> → 新根 <{}>", name, id, new_id);
            0
        }
        Err(e) => {
            eprintln!("错误：{e}");
            2
        }
    }
}

/// 按名字 / 路径映射给节点赋值：`名=值` 或 `名字段/子名=值`。路径相对 root 节点。
fn cmd_fill(file: &str, root: &str, assigns: &[&str], no_history: bool, yes: bool) -> i32 {
    let root_id = match Uuid::parse(root) {
        Some(u) => u,
        None => {
            eprintln!("无效节点 ID：{root}");
            return 2;
        }
    };
    let path = Path::new(file);
    let is_collection = path.is_dir();
    let (mut store, save_path) = if path.is_dir() {
        match load_shard(path, root_id) {
            Ok((s, p)) => (s, p),
            Err(c) => return c,
        }
    } else {
        match load_store(file) {
            Ok(s) => (s, path.to_path_buf()),
            Err(c) => return c,
        }
    };
    if store.get(root_id).is_none() {
        eprintln!("节点不存在：{root}");
        return 2;
    }
    if let Err(c) = edit_guard(&store, root_id, yes) {
        return c;
    }
    let before = store.clone();
    for a in assigns {
        let (path, value) = match a.split_once('=') {
            Some(pp) => pp,
            None => {
                eprintln!("赋值格式应为 路径=值：{a}");
                return 2;
            }
        };
        let segs: Vec<&str> = path.split('/').collect();
        if segs.is_empty() || segs.iter().any(|s| s.is_empty()) {
            eprintln!("路径为空：{path}");
            return 2;
        }
        // 导航到目标节点的父（前 N-1 段），最后一段是目标名
        let mut cur_id = root_id;
        let mut found = true;
        for seg in &segs[..segs.len() - 1] {
            let cur = match store.get(cur_id) {
                Some(c) => c,
                None => { found = false; break; }
            };
            match store.child_by_name(cur, seg) {
                Some(c) => cur_id = c.id,
                None => { found = false; break; }
            }
        }
        if !found {
            eprintln!("路径不存在：{path}");
            return 2;
        }
        let last = segs.last().unwrap();
        let target_id = store
            .get(cur_id)
            .and_then(|c| store.child_by_name(c, last))
            .map(|c| c.id);
        let target_id = match target_id {
            Some(t) => t,
            None => {
                eprintln!("路径不存在：{path}");
                return 2;
            }
        };
        let r = if no_history {
            store.set_quiet(target_id, parse_value(value))
        } else {
            store.update(target_id, parse_value(value))
        };
        if let Err(e) = r {
            eprintln!("错误：{e}（{path}）");
            return 2;
        }
        println!("已赋值：{path} = {value}");
    }
    let saved = if is_collection {
        shard::append_changes(&save_path, &before, &store)
    } else {
        save_store(&store, &save_path).map_err(|e| e.to_string())
    };
    if let Err(e) = saved {
        eprintln!("错误：{e}");
        return 2;
    }
    0
}

fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Empty => "empty",
        Value::Int(_) => "integer",
        Value::Float(_) => "float",
        Value::Bool(_) => "boolean",
        Value::Text(_) => "text",
        Value::Reference(_) => "reference",
        Value::Blob(_) => "blob",
    }
}

/// 把一棵子树的根节点转成嵌套 JSON（供 --json --tree 整树返回用）。
fn node_json(store: &tree::Store, node: &Node) -> serde_json::Value {
    let mut seen = HashSet::new();
    node_json_rec(store, node, &mut seen)
}

fn node_json_rec(
    store: &tree::Store,
    node: &Node,
    seen: &mut HashSet<Uuid>,
) -> serde_json::Value {
    // 父边成环（E006）时兜底：同一节点只展开一次。
    let children: Vec<serde_json::Value> = if seen.insert(node.id) {
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

/// 结构/名字/值匹配：`--root <name>` 按**节点名**锚定候选子树（不要求顶层根）、
/// `--shape-of <node-id>` 按形状码、`--template <name>` 按模板实例锚定；
/// 三者选一，叠加 `--where 路径=值` 值约束；`--json` 结构化、`--tree` 整树。
fn cmd_match(file: &str, root: Option<&str>, shape_of: Option<&str>, template: Option<&str>, wheres: &[(&str, &str)], json: bool, tree: bool) -> i32 {
    let store = match load_store(file) {
        Ok(s) => s,
        Err(c) => return c,
    };
    let sindex = query::ShapeIndex::build(&store);
    let cands: Vec<usize> = if let Some(name) = template {
        // 注释式：按模板名/编号定位模板，取其所有实例根
        match ops::resolve_template(&store, name) {
            Ok(tid) => ops::find_instances_of(&store, tid),
            Err(e) => {
                eprintln!("{e}");
                return 2;
            }
        }
    } else if let Some(name) = root {
        query::name_index(&store).get(name).cloned().unwrap_or_default()
    } else if let Some(so) = shape_of {
        let sid = match Uuid::parse(so) {
            Some(u) => u,
            None => {
                eprintln!("无效节点 ID：{so}");
                return 2;
            }
        };
        let idx = match store.nodes().iter().position(|x| x.id == sid) {
            Some(i) => i,
            None => {
                eprintln!("节点不存在：{so}");
                return 2;
            }
        };
        query::by_shape(&sindex, sindex.shapes[idx])
    } else {
        eprintln!("需要 --root <name> 或 --shape-of <node-id> 或 --template <name>");
        return 2;
    };
    let cands: Vec<usize> = cands
        .into_iter()
        .filter(|&i| query::matches_where(&store, i, wheres))
        .collect();
    if json {
        let arr: Vec<serde_json::Value> = cands
            .iter()
            .map(|&i| {
                let nd = &store.nodes()[i];
                if tree {
                    node_json(&store, nd)
                } else {
                    json!({
                        "id": nd.id.to_string(),
                        "name": nd.name,
                        "type": type_name(&nd.value),
                        "value": fmt_value(&store, nd),
                    })
                }
            })
            .collect();
        println!("{}", json!(arr));
    } else {
        for &i in &cands {
            let nd = &store.nodes()[i];
            let nm = if nd.name.is_empty() { "(空节点)" } else { nd.name.as_str() };
            println!("{nm} <{}>", nd.id);
        }
        println!("共 {} 个匹配", cands.len());
    }
    0
}

/// 命令：`xr tmpl add <file> <name> --from-json <sample>` —— 建一棵模板定义（自由根，挂 @模板 空标记 + 结构）。
fn cmd_tmpl_add(file: &str, name: &str, sample_path: Option<&str>) -> i32 {
    // 与 `xr new` 一致：文件不存在就新建空库。
    let mut store = if Path::new(file).exists() {
        match load_store(file) {
            Ok(s) => s,
            Err(c) => return c,
        }
    } else {
        tree::Store::new()
    };
    // 同名模板已存在则报错
    if store
        .nodes()
        .iter()
        .any(|n| n.name == name && store.is_template_root(n))
    {
        eprintln!("模板已存在：{name}（用 xr tmpl rm 删除再建）");
        return 2;
    }
    let sample_path = match sample_path {
        Some(p) => p,
        None => {
            eprintln!("需要 --from-json <样例.json> 来定模板结构");
            return 2;
        }
    };
    let text = match std::fs::read_to_string(sample_path) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("错误：{e}");
            return 2;
        }
    };
    let sample: serde_json::Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("JSON 解析失败：{e}");
            return 2;
        }
    };
    let tpl_id = match ops::build_template(&mut store, None, name, &sample) {
        Ok(id) => id,
        Err(e) => {
            eprintln!("错误：{e}");
            return 2;
        }
    };
    if let Err(e) = save_store(&store, Path::new(file)) {
        eprintln!("错误：{e}");
        return 2;
    }
    println!("已创建模板：{name} <{tpl_id}>（自由根，挂 @模板 标记）");
    0
}

/// 命令：`xr import <file> --append <parent|nil> <source-json>` —— 把嵌套 JSON 作为子树追加。
fn cmd_import_append(file: &str, parent: &str, source: &str) -> i32 {
    let mut store = match load_store(file) {
        Ok(s) => s,
        Err(c) => return c,
    };
    let p = if parent == "nil" || parent == "root" {
        None
    } else {
        match Uuid::parse(parent) {
            Some(u) => Some(u),
            None => {
                eprintln!("无效父节点 ID：{parent}");
                return 2;
            }
        }
    };
    let text = if source == "-" {
        let mut s = String::new();
        if std::io::Read::read_to_string(&mut std::io::stdin(), &mut s).is_err() {
            eprintln!("读取 stdin 失败");
            return 2;
        }
        s
    } else {
        match std::fs::read_to_string(source) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("错误：{e}");
                return 2;
            }
        }
    };
    let val: serde_json::Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("JSON 解析失败：{e}");
            return 2;
        }
    };
    if let Err(e) = ops::build_json_tree(&mut store, p, &val) {
        eprintln!("错误：{e}");
        return 2;
    }
    if let Err(e) = save_store(&store, Path::new(file)) {
        eprintln!("错误：{e}");
        return 2;
    }
    println!("已导入子树");
    0
}

/// 命令：`xr import <file> --template <name> <data.json> [--under <parent|nil>]`。
/// 注释式：每条记录 → 一棵实例树（挂 @实例 + @模板 引用），可自由挂在任意父节点下。
fn cmd_import_template(file: &str, name: &str, data_path: &str, under: Option<&str>) -> i32 {
    let mut store = match load_store(file) {
        Ok(s) => s,
        Err(c) => return c,
    };
    let tpl_id = match ops::resolve_template(&store, name) {
        Ok(id) => id,
        Err(e) => {
            eprintln!("{e}");
            return 2;
        }
    };
    let inst_parent = match under {
        Some("nil") | Some("root") | None => None,
        Some(p) => match Uuid::parse(p) {
            Some(u) => Some(u),
            None => {
                eprintln!("无效父节点 ID：{p}");
                return 2;
            }
        },
    };
    let text = match std::fs::read_to_string(data_path) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("错误：{e}");
            return 2;
        }
    };
    let data: serde_json::Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("JSON 解析失败：{e}");
            return 2;
        }
    };
    let records: Vec<&serde_json::Value> = match &data {
        serde_json::Value::Array(a) => a.iter().collect(),
        other => std::slice::from_ref(other).iter().collect(),
    };
    for rec in &records {
        if let Err(e) = ops::instantiate(&mut store, tpl_id, inst_parent, rec) {
            eprintln!("错误：{e}");
            return 2;
        }
    }
    if let Err(e) = save_store(&store, Path::new(file)) {
        eprintln!("错误：{e}");
        return 2;
    }
    println!("已导入 {} 棵实例（模板 {name} <{tpl_id}>）", records.len());
    0
}

/// 命令：`xr tmpl list <file>` —— 列出所有模板及其实例数。
fn cmd_tmpl_list(file: &str) -> i32 {
    let store = match load_store(file) {
        Ok(s) => s,
        Err(c) => return c,
    };
    let tpls = ops::find_template_roots(&store);
    if tpls.is_empty() {
        println!("（无模板）");
        return 0;
    }
    for &i in &tpls {
        let t = &store.nodes()[i];
        let inst_count = ops::find_instances_of(&store, t.id).len();
        let nm = if t.name.is_empty() { "(空)" } else { t.name.as_str() };
        println!("{nm} <{}>（{inst_count} 实例）", t.id);
    }
    0
}

/// 命令：`xr tmpl rm <file> <name> [--yes]` —— 受保护删除模板定义（连同其所有实例）。
fn cmd_tmpl_rm(file: &str, name: &str, yes: bool) -> i32 {
    let mut store = match load_store(file) {
        Ok(s) => s,
        Err(c) => return c,
    };
    let tpl_id = match ops::resolve_template(&store, name) {
        Ok(id) => id,
        Err(e) => {
            eprintln!("{e}");
            return 2;
        }
    };
    if !yes {
        eprintln!("（保护：删除模板 {name} 会连同其结构 + 所有实例一并移除；用 --yes 确认）");
        return 2;
    }
    // 先收集实例根（删了模板子树后索引会变），连同实例树一起删除，
    // 否则会留下指向已删模板根的悬挂 @模板 引用（R001）与找不到的孤儿实例。
    let inst_ids: Vec<Uuid> = ops::find_instances_of(&store, tpl_id)
        .into_iter()
        .map(|i| store.nodes()[i].id)
        .collect();
    for id in &inst_ids {
        if let Err(e) = store.remove_subtree(*id) {
            eprintln!("错误：{e}");
            return 2;
        }
    }
    if let Err(e) = store.remove_subtree(tpl_id) {
        eprintln!("错误：{e}");
        return 2;
    }
    if let Err(e) = save_store(&store, Path::new(file)) {
        eprintln!("错误：{e}");
        return 2;
    }
    println!("已删除模板：{name}（连同 {} 棵实例）", inst_ids.len());
    0
}

/// 命令：`xr diff <a.xirang> <b.xirang> [--json]` —— 按节点编号对比两文件（增/删/改）。
fn cmd_diff(a: &str, b: &str, json: bool) -> i32 {
    let sa = match load_store(a) {
        Ok(s) => s,
        Err(c) => return c,
    };
    let sb = match load_store(b) {
        Ok(s) => s,
        Err(c) => return c,
    };
    let map_a: HashMap<Uuid, &Node> = sa.nodes().iter().map(|n| (n.id, n)).collect();
    let map_b: HashMap<Uuid, &Node> = sb.nodes().iter().map(|n| (n.id, n)).collect();

    let mut added = Vec::new();
    let mut removed = Vec::new();
    let mut changed = Vec::new();
    for (id, n) in &map_a {
        match map_b.get(id) {
            None => removed.push((n.id, n.name.clone())),
            Some(m) => {
                if m.name != n.name || m.value != n.value {
                    let va = fmt_value(&sa, n).unwrap_or_default();
                    let vb = fmt_value(&sb, m).unwrap_or_default();
                    changed.push((n.id, (n.name.clone(), va), (m.name.clone(), vb)));
                }
            }
        }
    }
    for (id, n) in &map_b {
        if !map_a.contains_key(id) {
            added.push((n.id, n.name.clone()));
        }
    }
    // HashMap 迭代序不稳定 → 输出前按 UUID 排序，保证 --json 可回归对比。
    added.sort_by(|x, y| x.0 .0.cmp(&y.0 .0));
    removed.sort_by(|x, y| x.0 .0.cmp(&y.0 .0));
    changed.sort_by(|x, y| x.0 .0.cmp(&y.0 .0));
    if json {
        let out = json!({
            "added": added.iter().map(|(i, n)| json!({"id": i.to_string(), "name": n})).collect::<Vec<_>>(),
            "removed": removed.iter().map(|(i, n)| json!({"id": i.to_string(), "name": n})).collect::<Vec<_>>(),
            "changed": changed.iter().map(|(i, (an, av), (bn, bv))| json!({"id": i.to_string(), "from": {"name": an, "value": av}, "to": {"name": bn, "value": bv}})).collect::<Vec<_>>(),
        });
        println!("{}", out);
    } else {
        println!("新增 {} · 删除 {} · 改动 {}", added.len(), removed.len(), changed.len());
        if !added.is_empty() {
            println!("-- 新增 --");
            for (i, n) in &added {
                println!("  + {} <{}>", n, i);
            }
        }
        if !removed.is_empty() {
            println!("-- 删除 --");
            for (i, n) in &removed {
                println!("  - {} <{}>", n, i);
            }
        }
        if !changed.is_empty() {
            println!("-- 改动 --");
            for (i, (an, av), (bn, bv)) in &changed {
                println!("  ~ {} <{}>: {} = {} → {} = {}", an, i, an, av, bn, bv);
            }
        }
    }
    0
}

/// 命令：`xr instances <file> <name>` —— 列出某模板的实例根（@实例 下的节点）。
fn cmd_instances(file: &str, name: &str) -> i32 {
    let store = match load_store(file) {
        Ok(s) => s,
        Err(c) => return c,
    };
    let tpl_id = match ops::resolve_template(&store, name) {
        Ok(id) => id,
        Err(e) => {
            eprintln!("{e}");
            return 2;
        }
    };
    let insts = ops::find_instances_of(&store, tpl_id);
    if insts.is_empty() {
        println!("（模板 {name} 暂无实例）");
        return 0;
    }
    let mut sink = make_sink(has_flag(&std::env::args().collect::<Vec<_>>(), "--no-pager"));
    for &i in &insts {
        let mut budget = usize::MAX;
        print_tree(&mut sink, &store, &store.nodes()[i], &TreeOpts::default(), &mut budget);
        let _ = writeln!(sink);
    }
    sink.finish();
    0
}

fn cmd_refs(file: &str, node_id: &str) -> i32 {
    let store = match load_store(file) {
        Ok(s) => s,
        Err(c) => return c,
    };
    let id = match Uuid::parse(node_id) {
        Some(u) => u,
        None => {
            eprintln!("无效节点 ID：{node_id}");
            return 2;
        }
    };
    let node = match store.get(id) {
        Some(n) => n,
        None => {
            eprintln!("节点不存在：{node_id}");
            return 2;
        }
    };
    let name = if node.name.is_empty() { "(空节点)" } else { node.name.as_str() };
    println!("节点：{name} <{}>", node.id);
    // 出边（我引用谁）
    if let Value::Reference(t) = &node.value {
        match store.get(*t) {
            Some(target) => println!("  引用 → {} <{}>", target.name, target.id),
            None => println!("  引用 → {t} <不存在>"),
        }
    }
    // 入边（谁引用我）
    let incoming = store.references_to(node);
    if incoming.is_empty() {
        println!("  （无反向引用）");
    } else {
        for r in incoming {
            println!("  ← {} <{}>", r.name, r.id);
        }
    }
    0
}

/// 只看值本身（不查引用目标），用于跨文件视图里的简述。
fn value_brief(node: &Node) -> Option<String> {
    match &node.value {
        Value::Empty => None,
        Value::Int(n) => Some(n.to_string()),
        Value::Float(f) => Some(f.to_string()),
        Value::Bool(b) => Some(if *b { "true" } else { "false" }.to_string()),
        Value::Text(s) => Some(s.clone()),
        Value::Reference(u) => Some(format!("→ {u}")),
        Value::Blob(b) => Some(format!("[blob {} 字节]", b.len())),
    }
}

fn label_brief(node: &Node) -> String {
    match value_brief(node) {
        None => if node.name.is_empty() { "(空节点)".to_string() } else { node.name.clone() },
        Some(v) => if node.name.is_empty() { v } else { format!("{} = {}", node.name, v) },
    }
}

/// 跨文件读一个节点：列出它在**相关文件**里的每一份，以及并集后的孩子（每条标来源）。
/// 相关文件 = 显式给的 + （未用 `--only` 且开着索引时）本机目录里含该编号的文件。
fn cmd_ws(node_id: &str, files: &[String], only: Option<&str>, json_out: bool) -> i32 {
    let id = match Uuid::parse(node_id) {
        Some(u) => u,
        None => {
            eprintln!("无效节点 ID：{node_id}");
            return 2;
        }
    };

    let mut all: Vec<String> = match only {
        Some(o) => vec![o.to_string()],
        None => files.to_vec(),
    };
    for f in all.clone() {
        index_file(&f);
    }
    if only.is_none() && index_enabled() {
        if let Ok(mut r) = catalog::CatalogReader::open(&catalog::default_path()) {
            if let Ok(paths) = r.lookup_all(id) {
                for p in paths {
                    if !all.contains(&p) {
                        all.push(p);
                    }
                }
            }
        }
    }
    // 目录里可能记着已被删 / 挪走的文件：跳过并提示，不因此失败。
    // 又因目录里存的是规范路径、命令行可能给符号链接路径，此处按规范路径去重，避免同一文件算两份。
    let mut usable: Vec<String> = Vec::new();
    let mut seen: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
    for p in &all {
        let path = Path::new(p);
        if !path.is_file() {
            eprintln!("（跳过：{p} 已不存在）");
            continue;
        }
        let key = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        if seen.insert(key) {
            usable.push(p.clone());
        }
    }
    if usable.is_empty() {
        eprintln!("没有可读的文件");
        return 2;
    }
    let mut ws = match index::LazyWorkspace::from_paths(&usable) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("错误：{e}");
            return 2;
        }
    };

    let views = ws.node_views(id);
    if views.is_empty() {
        eprintln!("节点不存在于任何文件：{node_id}");
        return 2;
    }
    let kids = ws.children_union(id);
    let incoming = ws.references_to(id);

    if json_out {
        let node = &views[0].1;
        let out = json!({
            "id": id.to_string(),
            "name": node.name,
            "type": type_name(&node.value),
            "value": value_brief(node),
            "sources": views.iter().map(|(f, _)| f.clone()).collect::<Vec<_>>(),
            "children": kids.iter().map(|(f, n)| json!({
                "id": n.id.to_string(),
                "name": n.name,
                "type": type_name(&n.value),
                "value": value_brief(n),
                "source": f,
            })).collect::<Vec<_>>(),
            "references_to": incoming.iter().map(|(f, n)| json!({
                "id": n.id.to_string(),
                "name": n.name,
                "source": f,
            })).collect::<Vec<_>>(),
        });
        println!("{out}");
        return 0;
    }

    println!("节点 {} <{id}>（{} 处）", label_brief(&views[0].1), views.len());
    for (f, n) in &views {
        println!("  {}  （{f}）", label_brief(n));
    }
    if !kids.is_empty() {
        println!("  孩子（并集 {} 个）：", kids.len());
        for (f, n) in &kids {
            println!("    {} <{}>  （{f}）", label_brief(n), n.id);
        }
    }
    if let Value::Reference(t) = &views[0].1.value {
        match ws.find(*t) {
            Some((tfile, target)) => {
                println!("  引用 → {} <{}>  （在 {tfile}）", label_brief(&target), target.id);
            }
            None => println!("  引用 → {t} <不存在>"),
        }
    }
    if incoming.is_empty() {
        println!("  （无反向引用）");
    } else {
        for (rfile, r) in incoming {
            println!("  ← {} <{}>  （在 {rfile}）", label_brief(&r), r.id);
        }
    }
    0
}

fn cmd_index(files: &[String]) -> i32 {
    for f in files {
        let idx_path = index::sidecar_path(Path::new(f));
        let fresh = index::is_fresh(Path::new(f));
        let sc = match index::Sidecar::open_for(Path::new(f)) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("错误：{f}：{e}");
                return 2;
            }
        };
        println!("{f}");
        println!("  索引: {}", idx_path.display());
        println!("  状态: {}", if fresh { "复用" } else { "新建/重建" });
        println!("  根: {} · 节点: {} · 引用边: {}", sc.root_count, sc.node_count, sc.edge_count);
    }
    0
}

fn cmd_collection_split(file: &str, rule: &str, out: &Path) -> i32 {
    let store = match tree::Store::load_view(Path::new(file)) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("错误：{e}");
            return 2;
        }
    };
    let rule = match shard::ShardRule::parse(rule) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("错误：{e}");
            return 2;
        }
    };
    match shard::split_to_dir(&store, &rule, out) {
        Ok(n) => {
            println!("已拆分 {n} 个分片 → {}", out.display());
            0
        }
        Err(e) => {
            eprintln!("错误：{e}");
            2
        }
    }
}

fn cmd_collection_list(dir: &str) -> i32 {
    let col = match shard::Collection::open(Path::new(dir)) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("错误：{e}");
            return 2;
        }
    };
    println!("判定器: {}", col.manifest.rule.as_str());
    println!("分片: {} 个", col.manifest.shards.len());
    for e in &col.manifest.shards {
        let cnt = col
            .shards()
            .iter()
            .find(|(f, _)| *f == e.filename)
            .map(|(_, s)| s.len())
            .unwrap_or(0);
        let nm = if e.name.is_empty() { "(空)" } else { e.name.as_str() };
        println!("  {nm}  →  {}（{cnt} 节点）", e.filename);
    }
    0
}

fn cmd_compact(dir: &str, all: bool) -> i32 {
    // 单文件：折叠掉 append-v1 累积的历史记录（整份重写 = 顺手合并）
    let plain = Path::new(dir);
    if plain.is_file() {
        return match index::compact_file(plain) {
            Ok((raw, folded)) => {
                println!("已合并：{dir}");
                println!(
                    "  记录: {raw} → {folded}（折叠掉 {} 条历史记录）",
                    raw.saturating_sub(folded)
                );
                0
            }
            Err(e) => {
                eprintln!("错误：{e}");
                2
            }
        };
    }
    let mstore = match tree::Store::load_view(&Path::new(dir).join(shard::MANIFEST_NAME)) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("错误：{e}");
            return 2;
        }
    };
    let manifest = match shard::Manifest::from_store(&mstore) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("错误：{e}");
            return 2;
        }
    };
    let mut compacted = 0;
    for e in &manifest.shards {
        let path = Path::new(dir).join(&e.filename);
        let raw = match tree::Store::load_view(&path) {
            Ok(s) => s,
            Err(err) => {
                eprintln!("错误：{err}");
                return 2;
            }
        };
        let folded = shard::fold(&raw);
        if all || folded.len() != raw.len() {
            if let Err(err) = save_store(&folded, &path) {
                eprintln!("错误：{err}");
                return 2;
            }
            compacted += 1;
        }
    }
    println!("已合并 {compacted} 个分片");
    0
}

fn collect_xirang(dir: &Path, out: &mut Vec<PathBuf>) {
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                collect_xirang(&p, out);
            } else if p.extension().and_then(|s| s.to_str()) == Some("xirang") {
                out.push(p);
            }
        }
    }
}

fn abs_str(p: &Path) -> String {
    p.canonicalize()
        .ok()
        .and_then(|c| c.to_str().map(|s| s.to_string()))
        .unwrap_or_else(|| p.to_string_lossy().into_owned())
}

fn cmd_catalog_scan(paths: &[String]) -> i32 {
    let cpath = catalog::default_path();
    let mut cat = match catalog::Catalog::load(&cpath) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("错误：{e}");
            return 2;
        }
    };
    let mut targets: Vec<PathBuf> = Vec::new();
    if paths.is_empty() {
        collect_xirang(Path::new("."), &mut targets);
    } else {
        for p in paths {
            let path = Path::new(p);
            if path.is_dir() {
                collect_xirang(path, &mut targets);
            } else {
                targets.push(path.to_path_buf());
            }
        }
    }
    if targets.is_empty() {
        eprintln!("没有可扫描的 .xirang 文件");
        return 2;
    }
    let mut indexed = 0usize;
    for t in &targets {
        let store = match tree::Store::load_view(t) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("跳过 {}：{e}", t.display());
                continue;
            }
        };
        let fp = match catalog::fingerprint(t) {
            Some(f) => f,
            None => continue,
        };
        cat.upsert(&abs_str(t), fp, &catalog::Catalog::store_uuids(&store));
        indexed += 1;
    }
    if let Err(e) = cat.save(&cpath) {
        eprintln!("错误：{e}");
        return 2;
    }
    println!("已扫描 {indexed} 个文件 → {}", cpath.display());
    0
}

fn cmd_catalog_list() -> i32 {
    let cpath = catalog::default_path();
    let cat = match catalog::Catalog::load(&cpath) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("错误：{e}");
            return 2;
        }
    };
    let total: usize = cat.files().iter().map(|f| f.uuids.len()).sum();
    println!("目录: {}", cpath.display());
    println!("文件: {} 个 · UUID: {} 条", cat.files().len(), total);
    for f in cat.files() {
        println!("  {}（{} UUID）", f.path, f.uuids.len());
    }
    0
}

/// 跨文件一致性检查：只看「同编号多文件」里**节点自身（名字 / 值）不一致**的那些编号。
/// 两个库各留各的「猫」是正常现象；只有自身内容对不上时才需要人来裁决。
/// 每条都列出两边各自的来源文件、自身内容差异，以及**两边各自的孩子列表**（判断时的上下文）。
fn cmd_catalog_check() -> i32 {
    let cat = match catalog::Catalog::load(&catalog::default_path()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("错误：{e}");
            return 2;
        }
    };
    let dup = cat.duplicated();
    if dup.is_empty() {
        println!("（没有同编号多文件的情况）");
        return 0;
    }
    // 一次性把所有相关文件开成一个懒工作区（只读节点片段，不整读）。
    let mut all_files: Vec<String> = Vec::new();
    for (_u, files) in &dup {
        for f in files {
            if !all_files.contains(f) {
                all_files.push(f.clone());
            }
        }
    }
    let ex: Vec<String> = all_files.iter().filter(|p| Path::new(p).is_file()).cloned().collect();
    let mut ws = match index::LazyWorkspace::from_paths(&ex) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("错误：{e}");
            return 2;
        }
    };

    let mut diffs = 0usize;
    for (u, _files) in &dup {
        let views = ws.node_views(*u);
        if views.len() < 2 {
            continue;
        }
        let first = &views[0].1;
        let mismatch = views
            .iter()
            .any(|(_, n)| n.name != first.name || n.value != first.value);
        if !mismatch {
            continue; // 自身一致；孩子不同属正常，不算冲突
        }
        diffs += 1;
        println!("{u}  （同编号，自身内容不一致）");
        // 按来源文件归拢这一编号在各文件里的孩子（孩子差异只作上下文，不参与判定）。
        let kids = ws.children_union(*u);
        for (path, n) in &views {
            println!("  - {path}");
            println!("      自身：{}", label_brief(n));
            let mine: Vec<String> = kids
                .iter()
                .filter(|(kf, _)| kf == path)
                .map(|(_, c)| format!("{} <{}>", label_brief(c), c.id))
                .collect();
            if mine.is_empty() {
                println!("      孩子：（无）");
            } else {
                println!("      孩子（{} 个）：{}", mine.len(), mine.join(" · "));
            }
        }
        println!("      （如需对齐：xr catalog check --sync {u} --base {}）", views[0].0);
    }
    if diffs == 0 {
        println!("（同编号多文件共 {} 个，但节点自身内容都一致，无需处理）", dup.len());
    } else {
        println!("共 {} 个编号自身内容不一致。", diffs);
    }
    0
}

/// 把基准之外那些文件里的该节点，**只把名字 / 值**改成与基准一致；**孩子一律不动**。
fn cmd_catalog_check_sync(uuid_str: &str, base: &str) -> i32 {
    let id = match Uuid::parse(uuid_str) {
        Some(u) => u,
        None => {
            eprintln!("无效节点 ID：{uuid_str}");
            return 2;
        }
    };
    let base_abs = abs_str(Path::new(base));
    let base_store = match tree::Store::load_view(Path::new(base)) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("错误：{e}");
            return 2;
        }
    };
    let base_node = match base_store.get(id) {
        Some(n) => n.clone(),
        None => {
            eprintln!("基准文件里没有该编号：{base_abs}");
            return 2;
        }
    };
    let cat = match catalog::Catalog::load(&catalog::default_path()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("错误：{e}");
            return 2;
        }
    };
    let mut targets: Vec<String> = cat.lookup(id).into_iter().map(|s| s.to_string()).collect();
    if !targets.contains(&base_abs) {
        targets.insert(0, base_abs.clone());
    }
    let mut changed = 0usize;
    for f in &targets {
        if f == &base_abs {
            continue;
        }
        let mut store = match tree::Store::load_view(Path::new(f)) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let cur = match store.get(id) {
            Some(n) => n.clone(),
            None => continue,
        };
        if cur.name == base_node.name && cur.value == base_node.value {
            continue; // 已一致
        }
        // 只改名字 / 值；孩子（父边指向该编号的节点）保持原样。
        if cur.name != base_node.name {
            if let Err(e) = store.rename(id, base_node.name.clone()) {
                eprintln!("同步失败 {f}：{e}");
                continue;
            }
        }
        if cur.value != base_node.value {
            if let Err(e) = store.update(id, base_node.value.clone()) {
                eprintln!("同步失败 {f}：{e}");
                continue;
            }
        }
        if let Err(e) = save_store(&store, Path::new(f)) {
            eprintln!("写回失败 {f}：{e}");
            return 2;
        }
        println!("已同步 {f}（自身 → {}）", label_brief(&base_node));
        changed += 1;
    }
    if changed == 0 {
        println!("无需同步：其它文件里该编号的自身内容已与基准一致（或不存在）。");
    } else {
        println!("共同步 {changed} 个文件（孩子未改动）。");
    }
    0
}

fn cmd_catalog_forget(path: &str) -> i32 {
    let cpath = catalog::default_path();
    let mut cat = match catalog::Catalog::load(&cpath) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("错误：{e}");
            return 2;
        }
    };
    let abs = abs_str(Path::new(path));
    let before = cat.files().len();
    cat.remove_file(&abs);
    if cat.files().len() == before {
        eprintln!("目录里没有：{abs}");
        return 2;
    }
    if let Err(e) = cat.save(&cpath) {
        eprintln!("错误：{e}");
        return 2;
    }
    println!("已从目录移除：{abs}");
    0
}

fn trash_dest(p: &Path) -> PathBuf {
    if let Some(home) = std::env::var_os("HOME") {
        let trash = PathBuf::from(&home).join(".Trash");
        if trash.is_dir() {
            let name = p
                .file_name()
                .map(|s| s.to_os_string())
                .unwrap_or_else(|| std::ffi::OsString::from("file"));
            let mut d = trash.join(&name);
            if d.exists() {
                d = trash.join(format!("{}-{}", std::process::id(), name.to_string_lossy()));
            }
            return d;
        }
    }
    let mut s = p.as_os_str().to_os_string();
    s.push(".trashed");
    PathBuf::from(s)
}

fn cmd_catalog_trash(path: &str) -> i32 {
    let p = Path::new(path);
    if !p.is_file() {
        eprintln!("文件不存在：{path}");
        return 2;
    }
    let abs = abs_str(p);
    let dest = trash_dest(p);
    if let Err(e) = std::fs::rename(p, &dest) {
        eprintln!("错误：{e}");
        return 2;
    }
    // 从目录移除该文件
    let cpath = catalog::default_path();
    if let Ok(mut cat) = catalog::Catalog::load(&cpath) {
        cat.remove_file(&abs);
        let _ = cat.save(&cpath);
    }
    println!("已移入回收站：{} → {}（可恢复）", abs, dest.display());
    0
}

fn cmd_blob_import(file: &str, parent: &str, source: &str) -> i32 {
    let mut store = match load_store(file) {
        Ok(s) => s,
        Err(c) => return c,
    };
    let bytes = match std::fs::read(source) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("错误：{e}");
            return 2;
        }
    };
    let p = if parent == "nil" || parent == "root" {
        None
    } else {
        match Uuid::parse(parent) {
            Some(u) => Some(u),
            None => {
                eprintln!("无效父节点 ID：{parent}");
                return 2;
            }
        }
    };
    let name = Path::new(source)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "blob".to_string());
    let n = store.create(p, &name, Value::Blob(bytes), true);
    if let Err(e) = save_store(&store, Path::new(file)) {
        eprintln!("错误：{e}");
        return 2;
    }
    println!("已导入 blob：{name} <{}>", n.id);
    0
}

fn cmd_blob_export(file: &str, node_id: &str, dest: &str) -> i32 {
    let store = match load_store(file) {
        Ok(s) => s,
        Err(c) => return c,
    };
    let id = match Uuid::parse(node_id) {
        Some(u) => u,
        None => {
            eprintln!("无效节点 ID：{node_id}");
            return 2;
        }
    };
    let node = match store.get(id) {
        Some(n) => n,
        None => {
            eprintln!("节点不存在：{node_id}");
            return 2;
        }
    };
    match &node.value {
        Value::Blob(b) => {
            if let Err(e) = std::fs::write(dest, b) {
                eprintln!("错误：{e}");
                return 2;
            }
            println!("已导出 {} 字节 → {dest}", b.len());
            0
        }
        _ => {
            eprintln!("节点不是二进制块：{node_id}");
            2
        }
    }
}

fn cmd_revert(file: &str, node_id: &str) -> i32 {
    let mut store = match load_store(file) {
        Ok(s) => s,
        Err(c) => return c,
    };
    let id = match Uuid::parse(node_id) {
        Some(u) => u,
        None => {
            eprintln!("无效节点 ID：{node_id}");
            return 2;
        }
    };
    if store.get(id).is_none() {
        eprintln!("节点不存在：{node_id}");
        return 2;
    }
    // 回滚也留痕（Store::revert 会把回滚前的状态快照进 @history）。
    if let Err(e) = store.revert(id) {
        println!("（{e}）");
        return 0;
    }
    if let Err(e) = save_store(&store, Path::new(file)) {
        eprintln!("错误：{e}");
        return 2;
    }
    let name = store.get(id).map(|n| n.name.clone()).unwrap_or_default();
    println!("已回滚到最近快照：{name}");
    0
}

fn cmd_blob_info(file: &str, node_id: &str) -> i32 {
    let store = match load_store(file) {
        Ok(s) => s,
        Err(c) => return c,
    };
    let id = match Uuid::parse(node_id) {
        Some(u) => u,
        None => {
            eprintln!("无效节点 ID：{node_id}");
            return 2;
        }
    };
    let node = match store.get(id) {
        Some(n) => n,
        None => {
            eprintln!("节点不存在：{node_id}");
            return 2;
        }
    };
    match &node.value {
        Value::Blob(b) => {
            println!("二进制块：{} 字节", b.len());
            if let Some(f) = store.child_by_name(node, "@format") {
                if let Value::Text(t) = &f.value {
                    println!("@format = {t}");
                }
            }
            // 若可判为 UTF-8 文本，预览前 200 字符
            match std::str::from_utf8(&b[..b.len().min(200)]) {
                Ok(s) if !s.contains('\0') => println!("文本预览：{s}"),
                _ => println!("（非文本，无法预览）"),
            }
        }
        _ => {
            eprintln!("节点不是二进制块：{node_id}");
            return 2;
        }
    }
    0
}

fn cmd_history(file: &str, node_id: &str) -> i32 {
    let store = match load_store(file) {
        Ok(s) => s,
        Err(c) => return c,
    };
    let id = match Uuid::parse(node_id) {
        Some(u) => u,
        None => {
            eprintln!("无效节点 ID：{node_id}");
            return 2;
        }
    };
    let node = match store.get(id) {
        Some(n) => n,
        None => {
            eprintln!("节点不存在：{node_id}");
            return 2;
        }
    };
    let name = if node.name.is_empty() { "(空节点)" } else { node.name.as_str() };
    println!("节点：{name} <{}>", node.id);
    match store.child_by_name(node, "@history") {
        None => println!("（无 @history）"),
        Some(hist) => {
            let snaps = store.children(hist);
            if snaps.is_empty() {
                println!("（@history 为空）");
            }
            for snap in snaps {
                let sv = fmt_value(&store, &snap).unwrap_or_default();
                println!("  快照：{} = {}", snap.name, sv);
                if let Some(r) = store.child_by_name(&snap, "@replaced") {
                    if let Value::Text(t) = &r.value {
                        println!("    @replaced = {t}");
                    }
                }
            }
        }
    }
    0
}

fn cmd_find(file: &str, pattern: &str, json: bool) -> i32 {
    let store = match load_store(file) {
        Ok(s) => s,
        Err(c) => return c,
    };
    let mut hits: Vec<&Node> = Vec::new();
    for n in store.nodes() {
        let text_val = match &n.value {
            Value::Text(s) => s.clone(),
            Value::Int(i) => i.to_string(),
            Value::Float(f) => f.to_string(),
            Value::Bool(b) => if *b { "true" } else { "false" }.to_string(),
            _ => String::new(),
        };
        if n.name.contains(pattern) || text_val.contains(pattern) {
            hits.push(n);
        }
    }
    if json {
        let arr: Vec<serde_json::Value> = hits
            .iter()
            .map(|n| {
                json!({
                    "id": n.id.to_string(),
                    "name": n.name,
                    "type": type_name(&n.value),
                    "value": fmt_value(&store, n),
                })
            })
            .collect();
        println!("{}", json!(arr));
    } else {
        for n in &hits {
            let name = if n.name.is_empty() { "(空节点)" } else { n.name.as_str() };
            println!("{name} <{}>", n.id);
        }
        println!("共 {} 个匹配", hits.len());
    }
    0
}

fn cmd_export(file: &str, format: &str, subtree: Option<&str>) -> i32 {
    let store = match load_store(file) {
        Ok(s) => s,
        Err(c) => return c,
    };
    // 若指定子树，只导出该子树
    let store = match subtree {
        None => store,
        Some(id_str) => {
            let id = match Uuid::parse(id_str) {
                Some(u) => u,
                None => {
                    eprintln!("无效节点 ID：{id_str}");
                    return 2;
                }
            };
            let root = match store.get(id) {
                Some(n) => n,
                None => {
                    eprintln!("节点不存在：{id_str}");
                    return 2;
                }
            };
            let mut sub = store.sub_store(root);
            // 子树重新扎根：把根父指针置 nil，导出的子树才能直接再导入（否则 E011）。
            let _ = sub.set_parent(id, None);
            sub
        }
    };
    match format {
        "json" => println!("{}", convert::to_json(&store)),
        "yaml" => println!("{}", convert::to_yaml(&store)),
        "xml" => println!("{}", convert::to_xml(&store)),
        // to_md 自带结尾换行，这里用 print! 避免多一个空行
        "md" | "markdown" => print!("{}", convert::to_md(&store)),
        _ => {
            eprintln!("未知格式：{format}（支持 json / yaml / xml / md）");
            return 2;
        }
    }
    0
}

fn cmd_import(file: &str, format: &str, source: &str) -> i32 {
    // 导入是「整文件替换」：先记下原节点数，输出里说清楚，避免静默覆盖。
    let before = tree::Store::load_view(Path::new(file)).ok().map(|s| s.len());
    let text = match std::fs::read_to_string(source) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("错误：{e}");
            return 2;
        }
    };
    let store = match format {
        "json" => convert::from_json(&text),
        "yaml" => convert::from_yaml(&text),
        "xml" => convert::from_xml(&text),
        _ => {
            eprintln!("未知格式：{format}（支持 json / yaml / xml）");
            return 2;
        }
    };
    let store = match store {
        Ok(s) => s,
        Err(e) => {
            eprintln!("错误：{e}");
            return 2;
        }
    };
    if let Err(e) = save_store(&store, Path::new(file)) {
        eprintln!("错误：{e}");
        return 2;
    }
    match before {
        Some(n) => println!(
            "已替换：{source} → {file}（原 {n} 节点 → {} 节点；import 是整文件替换，要追加请用 --append / --template）",
            store.len()
        ),
        None => println!("已导入：{source} → {file}（{} 节点）", store.len()),
    }
    0
}

/// 参数里是否出现某个标志（如 `--no-history`）。
fn has_flag(args: &[String], flag: &str) -> bool {
    args.iter().any(|a| a == flag)
}

/// CLI 认得的标志：用来把「值恰好以 -- 开头」（如文本 `--待办`）和「拼错的标志」区分开。
const KNOWN_FLAGS: &[&str] = &[
    "--help", "-h", "--version", "-V", "--json", "--tree", "--skip-aux", "--ids", "--node",
    "--no-history", "--no-index", "--yes", "--blank", "--all", "--root", "--shape-of",
    "--template", "--where", "--subtree", "--append", "--under", "--from-json", "--rule",
    "--out", "--only", "--sync", "--base", "--depth", "--no-pager", "--force",
];

fn is_known_flag(s: &str) -> bool {
    KNOWN_FLAGS.contains(&s)
}

/// 对「很像某个已知标志但写错」的参数提醒一行（如 `--no-histroy`）；
/// 只对前 5 字符撞上已知标志的才提醒，避免把以 `--` 开头的文本值误报。
fn warn_unknown_flags(args: &[String]) {
    for a in args {
        if !a.starts_with("--") || is_known_flag(a) {
            continue;
        }
        let typo = KNOWN_FLAGS
            .iter()
            .any(|k| k.len() >= 5 && a.starts_with(&k[..5]) && *k != a.as_str());
        if typo {
            eprintln!("（提示：未知标志 {a}，是不是写错了？已知标志见 xr --help）");
        }
    }
}

fn usage() {
    println!("息壤 CLI：xr 命令");
    println!();
    println!("用法：");
    println!("  xr info <file>                      文件摘要");
    println!("  xr tree <file> [--node <id>] [--skip-aux] [--ids] [--head N] [--depth N]   缩进树");
    println!("       --ids 显示编号；--head 只打前 N 个节点；--depth 只展开到第 N 层；--no-pager 关掉自动分页");
    println!("  xr cat <file> [--head N] [--ids] [--skip-aux] [--force]   扁平视图：一行一个节点，按存放顺序（像看文本）");
    println!("  xr validate <file>                  校验（E/R）");
    println!("  xr new <file> <parent|nil> <name> [value] [--no-history]   新增节点");
    println!("  xr set <file> <node-id> <value> [--no-history]   改值");
    println!("  xr rename <file> <node-id> <新名字> [--no-history]   改名（编号不变、引用不断；旧名字进 @history）");
    println!("  xr rm <file> <node-id>              删除（置空）");
    println!("  xr link <file> <from-id> <to-id> [--no-history]   建引用边");
    println!("  xr copy <file> <node-id> <parent|nil> [--blank] [--no-history]  复制子树");
    println!("  xr fill <file> <root-id> <名/路径=值>... [--no-history]  按名字/路径映射赋值");
    println!("  xr match <file> --root <名>|--shape-of <node-id>|--template <名> [--where 路径=值] [--json] [--tree]");
    println!("      结构/名字/值匹配（按根名/形状码/模板实例；--json 结构化、--tree 整树）");
    println!("  （--no-history：不写 @history / @created，适合批量创建 & 初始数据）");
    println!("  xr export <file> <json|yaml|xml|md> [--subtree <id>]  导出");
    println!("  xr import <file> <json|yaml|xml> <source>  导入");
    println!("  xr import <file> --append <parent|nil> <source-json>  把嵌套 JSON 追加为子树（数组→0,1,2；{{\"@ref\":\"名\"}}→引用边）");
    println!("  xr import <file> --template <name> <data.json>  按模板批量导入实例");
    println!("  xr tmpl add <file> <name> --from-json <sample>  创建模板（@模板/<name> + 结构 + @实例）");
    println!("  xr tmpl list <file>                  列出模板及实例数");
    println!("  xr tmpl rm <file> <name> [--yes]      受保护删除模板（连同结构+实例）");
    println!("  xr diff <a.xirang> <b.xirang> [--json]  两文件对比（增/删/改）");
    println!("  xr instances <file> <name>            列出某模板的实例（@实例 下）");
    println!("  xr find <file> <pattern> [--json]    搜索（名字 / 文本值）");
    println!("  xr refs <file> <node-id>            查看引用边（出 / 入）");
    println!("  xr ws <node-id> <file1> [file2...] [--only <file>] [--json]");
    println!("      按编号跨文件解析：默认列出该编号在各相关文件里的每一份、孩子取并集（每条标来源）；--only 只看一个文件");
    println!("  xr index <file1> [file2...]          重建 / 刷新 sidecar 索引并打印摘要");
    println!("  xr collection split <file> --rule <规则> [--out <dir>]   无损拆成 shard 词库");
    println!("  xr collection list <dir>            列出 shard 词库分片");
    println!("  xr compact <dir> [--all]            合并分片的覆盖日志");
    println!("  xr catalog scan [路径...]           扫描文件/目录进本机目录");
    println!("  xr catalog list                     列出本机目录的文件");
    println!("  xr catalog check                    列出同编号但自身名字/值不一致的编号（附两边孩子）");
    println!("  xr catalog check --sync <uuid> --base <文件>   以某文件为准，同步其它文件里该节点的名字/值（孩子不动）");
    println!("  xr catalog forget <路径>            从本机目录移除一个文件");
    println!("  xr catalog trash <路径>             移文件到回收站并从目录移除");
    println!("  （读命令默认维护本机目录；--no-index 或 XIRANG_INDEX=off 关闭）");
    println!("  xr history <file> <node-id>          查看 @history 快照");
    println!("  xr blob-import <file> <parent|nil> <src>   导入文件为二进制块");
    println!("  xr blob-export <file> <node-id> <dest>    导出二进制块为文件");
    println!("  xr blob-info <file> <node-id>           二进制块信息 / 预览");
    println!("  xr revert <file> <node-id>              回滚到最近 @history 快照");
}

fn main() {
    // 管道关闭（如 | head）时静默退出，不 panic（标准 Unix 工具行为）
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL); }

    let args: Vec<String> = std::env::args().collect();
    // --help / --version：不要求 3 个参数，退出码 0（SKILL.md 里承诺了 `xr --help`）。
    if args.len() >= 2 {
        match args[1].as_str() {
            "--help" | "-h" | "help" => {
                usage();
                std::process::exit(0);
            }
            "--version" | "-V" => {
                println!("xr {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            _ => {}
        }
    }
    if args.len() < 3 {
        usage();
        std::process::exit(2);
    }
    let cmd = args[1].as_str();
    let file = args[2].as_str();

    // 读/写命令是否顺带维护本机目录：默认开；XIRANG_INDEX=off 或 --no-index 关闭。
    let env_on = match std::env::var("XIRANG_INDEX") {
        Ok(v) => !matches!(
            v.to_ascii_lowercase().as_str(),
            "0" | "off" | "false" | "no" | "disable" | "disabled"
        ),
        Err(_) => true,
    };
    INDEX_ENABLED.store(env_on && !has_flag(&args, "--no-index"), Ordering::Relaxed);
    warn_unknown_flags(&args);

    let code = match cmd {
        "info" | "open" => cmd_info(file),
        "tree" => {
            let mut node_id = None;
            let mut opts = TreeOpts::default();
            let mut head: Option<usize> = None;
            let mut it = args[3..].iter();
            while let Some(a) = it.next() {
                match a.as_str() {
                    "--node" => node_id = it.next().map(|s| s.as_str()),
                    "--skip-aux" => opts.skip_aux = true,
                    "--ids" => opts.show_ids = true,
                    "--head" => head = it.next().and_then(|v| v.parse::<usize>().ok()),
                    "--depth" => opts.max_depth = it.next().and_then(|v| v.parse::<usize>().ok()),
                    _ => {}
                }
            }
            cmd_tree(file, node_id, &opts, head, has_flag(&args, "--no-pager"))
        }
        "cat" => {
            let mut opts = TreeOpts::default();
            let mut head: Option<usize> = None;
            let mut it = args[3..].iter();
            while let Some(a) = it.next() {
                match a.as_str() {
                    "--skip-aux" => opts.skip_aux = true,
                    "--ids" => opts.show_ids = true,
                    "--head" => head = it.next().and_then(|v| v.parse::<usize>().ok()),
                    _ => {}
                }
            }
            cmd_cat(file, &opts, head, has_flag(&args, "--force"), has_flag(&args, "--no-pager"))
        }
        "validate" => cmd_validate(file),
        "new" => {
            if args.len() < 5 {
                usage();
                2
            } else {
                let nh = has_flag(&args, "--no-history");
                let yes = has_flag(&args, "--yes");
                // 只把「已知标志」当标志；`--待办` 这类文本值要保留（P28）。
                let value = args.get(5).map(|s| s.as_str()).filter(|v| !is_known_flag(v));
                cmd_new(file, &args[3], &args[4], value, nh, yes)
            }
        }
        "set" => {
            if args.len() < 5 {
                usage();
                2
            } else {
                cmd_set(file, &args[3], &args[4], has_flag(&args, "--no-history"), has_flag(&args, "--yes"))
            }
        }
        "rename" => {
            if args.len() < 5 {
                usage();
                2
            } else {
                cmd_rename(file, &args[3], &args[4], has_flag(&args, "--no-history"), has_flag(&args, "--yes"))
            }
        }
        "rm" => {
            if args.len() < 4 {
                usage();
                2
            } else {
                cmd_rm(file, &args[3], has_flag(&args, "--yes"))
            }
        }
        "link" => {
            if args.len() < 5 {
                usage();
                2
            } else {
                cmd_link(file, &args[3], &args[4], has_flag(&args, "--no-history"), has_flag(&args, "--yes"))
            }
        }
        "copy" => {
            if args.len() < 5 {
                usage();
                2
            } else {
                let mut blank = false;
                let mut history = true;
                let yes = has_flag(&args, "--yes");
                let mut it = args[5..].iter();
                while let Some(a) = it.next() {
                    if a == "--blank" { blank = true; }
                    else if a == "--no-history" { history = false; }
                }
                cmd_copy(file, &args[3], &args[4], blank, history, yes)
            }
        }
        "fill" => {
            if args.len() < 5 {
                usage();
                2
            } else {
                let assigns: Vec<&str> = args[4..]
                    .iter()
                    .map(|s| s.as_str())
                    .filter(|a| !is_known_flag(a))
                    .collect();
                cmd_fill(file, &args[3], &assigns, has_flag(&args, "--no-history"), has_flag(&args, "--yes"))
            }
        }
        "match" => {
            if args.len() < 4 {
                usage();
                2
            } else {
                let mut root: Option<&str> = None;
                let mut shape_of: Option<&str> = None;
                let mut template: Option<&str> = None;
                let mut json = false;
                let mut tree = false;
                let mut wheres: Vec<(&str, &str)> = Vec::new();
                let mut i = 3;
                while i < args.len() {
                    match args[i].as_str() {
                        "--root" => root = args.get(i + 1).map(|s| s.as_str()),
                        "--shape-of" => shape_of = args.get(i + 1).map(|s| s.as_str()),
                        "--template" => template = args.get(i + 1).map(|s| s.as_str()),
                        "--where" => {
                            if let Some(w) = args.get(i + 1) {
                                if let Some((p, v)) = w.split_once('=') {
                                    wheres.push((p, v));
                                }
                            }
                        }
                        "--json" => json = true,
                        "--tree" => tree = true,
                        _ => {}
                    }
                    i += 1;
                }
                cmd_match(file, root, shape_of, template, &wheres, json, tree)
            }
        }
        "tmpl" => {
            let sub = args.get(2).map(|s| s.as_str()).unwrap_or("");
            match sub {
                "add" => {
                    if args.len() < 6 {
                        usage();
                        2
                    } else {
                        let mut sample: Option<&str> = None;
                        let mut i = 5;
                        while i < args.len() {
                            if args[i] == "--from-json" {
                                sample = args.get(i + 1).map(|s| s.as_str());
                            }
                            i += 1;
                        }
                        cmd_tmpl_add(&args[3], &args[4], sample)
                    }
                }
                "rm" => {
                    if args.len() < 5 {
                        usage();
                        2
                    } else {
                        cmd_tmpl_rm(&args[3], &args[4], has_flag(&args, "--yes"))
                    }
                }
                "list" => {
                    if args.len() < 4 {
                        usage();
                        2
                    } else {
                        cmd_tmpl_list(&args[3])
                    }
                }
                _ => {
                    usage();
                    2
                }
            }
        }
        "diff" => {
            if args.len() < 4 {
                usage();
                2
            } else {
                cmd_diff(&args[2], &args[3], has_flag(&args, "--json"))
            }
        }
        "instances" => {
            if args.len() < 4 {
                usage();
                2
            } else {
                cmd_instances(file, &args[3])
            }
        }
        "export" => {
            if args.len() < 4 {
                usage();
                2
            } else {
                // 支持 --subtree <id>
                let mut subtree = None;
                let mut it = args[4..].iter();
                while let Some(a) = it.next() {
                    if a == "--subtree" {
                        subtree = it.next().map(|s| s.as_str());
                    }
                }
                cmd_export(file, &args[3], subtree)
            }
        }
        "import" => {
            if args.len() < 5 {
                usage();
                2
            } else if has_flag(&args, "--append") {
                // xr import <file> --append <parent|nil> <source-json>
                let parent = args.get(4).map(|s| s.as_str()).unwrap_or("nil");
                let source = args.get(5).map(|s| s.as_str()).unwrap_or("");
                cmd_import_append(file, parent, source)
            } else if has_flag(&args, "--template") {
                // xr import <file> --template <name> <data.json> [--under <parent|nil>]
                let name = args.get(4).map(|s| s.as_str()).unwrap_or("");
                let data = args.get(5).map(|s| s.as_str()).unwrap_or("");
                let mut under: Option<&str> = None;
                let mut i = 6;
                while i < args.len() {
                    if args[i] == "--under" {
                        under = args.get(i + 1).map(|s| s.as_str());
                    }
                    i += 1;
                }
                cmd_import_template(file, name, data, under)
            } else {
                cmd_import(file, &args[3], &args[4])
            }
        }
        "find" => {
            if args.len() < 4 {
                usage();
                2
            } else {
                cmd_find(file, &args[3], has_flag(&args, "--json"))
            }
        }
        "refs" => {
            if args.len() < 4 {
                usage();
                2
            } else {
                cmd_refs(file, &args[3])
            }
        }
        "ws" => {
            if args.len() < 4 {
                usage();
                2
            } else {
                let mut only: Option<&str> = None;
                let mut files: Vec<String> = Vec::new();
                let mut i = 3;
                while i < args.len() {
                    match args[i].as_str() {
                        "--only" => {
                            only = args.get(i + 1).map(|s| s.as_str());
                            i += 2;
                            continue;
                        }
                        "--json" => {}
                        other => files.push(other.to_string()),
                    }
                    i += 1;
                }
                cmd_ws(&args[2], &files, only, has_flag(&args, "--json"))
            }
        }
        "index" => {
            if args.len() < 3 {
                usage();
                2
            } else {
                cmd_index(&args[2..])
            }
        }
        "collection" => {
            let sub = args.get(2).map(|s| s.as_str()).unwrap_or("");
            match sub {
                "split" => {
                    if args.len() < 4 {
                        usage();
                        2
                    } else {
                        let mut rule = "root".to_string();
                        let mut out: Option<PathBuf> = None;
                        let mut i = 4;
                        while i < args.len() {
                            match args[i].as_str() {
                                "--rule" => {
                                    if let Some(r) = args.get(i + 1) {
                                        rule = r.clone();
                                    }
                                }
                                "--out" => {
                                    if let Some(o) = args.get(i + 1) {
                                        out = Some(PathBuf::from(o));
                                    }
                                }
                                _ => {}
                            }
                            i += 1;
                        }
                        let out_path =
                            out.unwrap_or_else(|| PathBuf::from(format!("{}.shards", &args[3])));
                        cmd_collection_split(&args[3], &rule, &out_path)
                    }
                }
                "list" => {
                    if args.len() < 4 {
                        usage();
                        2
                    } else {
                        cmd_collection_list(&args[3])
                    }
                }
                _ => {
                    usage();
                    2
                }
            }
        }
        "compact" => {
            if args.len() < 3 {
                usage();
                2
            } else {
                cmd_compact(&args[2], has_flag(&args, "--all"))
            }
        }
        "catalog" => {
            let sub = args.get(2).map(|s| s.as_str()).unwrap_or("");
            match sub {
                "scan" => cmd_catalog_scan(&args[3..]),
                "list" => cmd_catalog_list(),
                "check" => {
                    let mut sync: Option<&str> = None;
                    let mut base: Option<&str> = None;
                    let mut i = 3;
                    while i < args.len() {
                        match args[i].as_str() {
                            "--sync" => {
                                sync = args.get(i + 1).map(|s| s.as_str());
                                i += 2;
                            }
                            "--base" => {
                                base = args.get(i + 1).map(|s| s.as_str());
                                i += 2;
                            }
                            _ => i += 1,
                        }
                    }
                    match (sync, base) {
                        (Some(u), Some(b)) => cmd_catalog_check_sync(u, b),
                        (Some(_), None) => {
                            eprintln!("--sync 需要一并给出 --base <文件>");
                            2
                        }
                        (None, _) => cmd_catalog_check(),
                    }
                }
                "forget" => match args.get(3) {
                    Some(p) => cmd_catalog_forget(p),
                    None => {
                        eprintln!("需要 <路径>");
                        2
                    }
                },
                "trash" => match args.get(3) {
                    Some(p) => cmd_catalog_trash(p),
                    None => {
                        eprintln!("需要 <路径>");
                        2
                    }
                },
                _ => {
                    usage();
                    2
                }
            }
        }
        "history" => {
            if args.len() < 4 {
                usage();
                2
            } else {
                cmd_history(file, &args[3])
            }
        }
        "blob-import" => {
            if args.len() < 5 {
                usage();
                2
            } else {
                cmd_blob_import(file, &args[3], &args[4])
            }
        }
        "blob-export" => {
            if args.len() < 5 {
                usage();
                2
            } else {
                cmd_blob_export(file, &args[3], &args[4])
            }
        }
        "blob-info" => {
            if args.len() < 4 {
                usage();
                2
            } else {
                cmd_blob_info(file, &args[3])
            }
        }
        "revert" => {
            if args.len() < 4 {
                usage();
                2
            } else {
                cmd_revert(file, &args[3])
            }
        }
        _ => {
            eprintln!("未知命令：{cmd}");
            usage();
            2
        }
    };
    std::process::exit(code);
}
