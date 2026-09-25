//! 息壤 CLI：`xr` 命令。
//!
//! 这一层只做三件事：**解析参数 → 调共享操作层（`ops`）→ 把结果渲染成文本**。
//! 命令实现、护栏、错误语义都在 `ops`，所以 CLI 与 `xr-mcp` 的行为天然一致。
//! 只有「本机状态」类命令（catalog / index / collection / compact / ws）留在这里——
//! 它们操作的是你这台电脑的环境，不是文件里的数据，MCP 那边不暴露。

use std::collections::HashSet;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use serde_json::json;
use xirang_core::codec::Uuid;
use xirang_core::{catalog, index, shard, tree};

mod ops;

use ops::{ErrKind, Layout, MatchQuery, OpError, Policy, ViewOpts};

/// 读/写命令是否顺带维护本机目录（默认开，可用 `--no-index` 或 `XIRANG_INDEX=off` 关闭）。
static INDEX_ENABLED: AtomicBool = AtomicBool::new(true);

/// 本次命令碰过的索引工作区根：退出前据此决定要不要在后台整理。
static INDEX_ROOT: std::sync::Mutex<Option<PathBuf>> = std::sync::Mutex::new(None);

/// 记下「这次命令用过哪个工作区的索引」（读命令与写命令都调）。
pub(crate) fn index_touch(root: PathBuf) {
    if let Ok(mut g) = INDEX_ROOT.lock() {
        *g = Some(root);
    }
}

/// 先给结果、再整理：需要时分离一个后台进程去压实，本进程立刻退出。
fn maybe_spawn_maintenance() {
    if !xirang_core::wsidx::maintenance_enabled() {
        return;
    }
    let root = match INDEX_ROOT.lock() {
        Ok(g) => match g.clone() {
            Some(r) => r,
            None => return,
        },
        Err(_) => return,
    };
    if xirang_core::wsidx::maintenance_needed(&root).is_none() {
        return;
    }
    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(_) => return,
    };
    let spawned = std::process::Command::new(exe)
        .arg("index")
        .arg("compact")
        .arg(&root)
        .env("XIRANG_INDEX_MAINTENANCE", "off") // 防递归
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    if spawned.is_ok() {
        eprintln!("（索引日志偏大，已在后台开始整理；可用 XIRANG_INDEX_MAINTENANCE=off 关闭）");
    }
}

fn index_enabled() -> bool {
    INDEX_ENABLED.load(Ordering::Relaxed)
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
#[allow(dead_code)]
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

/// 落盘 + 增量登记目录（CLI 专属写命令用）。
fn save_store(store: &tree::Store, path: &Path) -> std::io::Result<()> {
    store.save(path)?;
    index_store(&path.to_string_lossy(), store);
    Ok(())
}

/// 本机状态挂钩：CLI 每读 / 写一次就顺手登记本机目录；MCP 用 `NoHooks` 不碰本机状态。
struct CliHooks;

impl ops::Hooks for CliHooks {
    fn on_load(&self, path: &Path, store: &tree::Store) {
        index_store(&path.to_string_lossy(), store);
    }
    fn on_save(&self, path: &Path, store: &tree::Store) {
        index_store(&path.to_string_lossy(), store);
    }
}

/// 把操作层错误打印成 CLI 文本，返回退出码。
fn report(e: &OpError) -> i32 {
    match (&e.kind, &e.hint) {
        // 护栏 / 边界：沿用括号式提示，读起来像「提醒」而不是「崩了」
        (ErrKind::Guarded, Some(h)) | (ErrKind::PathDenied, Some(h)) => {
            eprintln!("（{}；{h}）", e.message)
        }
        (ErrKind::Guarded, None) | (ErrKind::PathDenied, None) => eprintln!("（{}）", e.message),
        _ => eprintln!("错误：{}", e.message),
    }
    2
}

// ============================================================================
// 输出目的地：终端直出，或者交给分页器（`$PAGER` / `less -R`）
// ============================================================================

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

/// 打印一棵子树：缩进 + 连接符（根不缩进，其孩子缩进 3 格）。
fn render_tree_node(
    out: &mut Sink,
    v: &ops::NodeView,
    prefix: &str,
    is_last: bool,
    include_ids: bool,
) {
    let connector = if prefix.is_empty() {
        ""
    } else if is_last {
        "└─ "
    } else {
        "├─ "
    };
    let id_suffix = if include_ids { format!(" <{}>", v.id) } else { String::new() };
    let _ = writeln!(out, "{}{}{}{}", prefix, connector, v.label, id_suffix);
    if v.children.is_empty() {
        return;
    }
    let child_prefix = format!("{}{}", prefix, if is_last { "   " } else { "│  " });
    let n = v.children.len();
    for (i, c) in v.children.iter().enumerate() {
        render_tree_node(out, c, &child_prefix, i == n - 1, include_ids);
    }
}

// ============================================================================
// 读命令
// ============================================================================

fn cmd_info(file: &str) -> i32 {
    match ops::info(&Policy::cli(false), &CliHooks, file) {
        Ok(i) => {
            println!("文件: {}", i.file);
            println!("格式版本: {}", i.version);
            println!("头文本: {} 字节，首行: {}", i.header_bytes, i.header_first_line);
            println!("节点数: {}", i.nodes);
            println!("根节点数: {}", i.roots);
            0
        }
        Err(e) => report(&e),
    }
}

fn cmd_tree(file: &str, node_id: Option<&str>, opts: &ViewOpts, no_pager: bool) -> i32 {
    let outcome = match ops::view(
        &Policy::cli(false),
        &CliHooks,
        file,
        node_id,
        *opts,
        Layout::Tree,
        None,
    ) {
        Ok(o) => o,
        Err(e) => return report(&e),
    };
    let mut sink = make_sink(no_pager);
    for (i, r) in outcome.roots.iter().enumerate() {
        if i > 0 {
            let _ = writeln!(sink);
        }
        render_tree_node(&mut sink, r, "", true, opts.include_ids);
    }
    sink.finish();
    // 截断是「提示」，走 stderr，保持 stdout 干净（可直接管道给别的工具）
    if opts.limit.is_some() && outcome.truncated() {
        eprintln!(
            "（只打印了前 {} 个节点，共 {} 个；去掉 --head 打印全部）",
            outcome.printed, outcome.total
        );
    }
    if let Some(d) = opts.max_depth {
        eprintln!("（只展开到第 {d} 层；去掉 --depth 显示全部）");
    }
    0
}

/// 扁平视图：按文件里的存放顺序，一行一个节点（不缩进）。像浏览文本文件一样看。
fn cmd_cat(file: &str, opts: &ViewOpts, force: bool, no_pager: bool) -> i32 {
    const GUARD: usize = 100_000;
    let pol = Policy::cli(force);
    let outcome =
        match ops::view(&pol, &CliHooks, file, None, *opts, Layout::Flat, Some(GUARD)) {
            Ok(o) => o,
            Err(e) => return report(&e),
        };
    let mut sink = make_sink(no_pager);
    for v in &outcome.items {
        let id_suffix = if opts.include_ids { format!(" <{}>", v.id) } else { String::new() };
        if writeln!(sink, "{}{}", v.label, id_suffix).is_err() {
            break;
        }
    }
    sink.finish();
    if let Some(n) = opts.limit {
        if outcome.printed >= n && outcome.total > outcome.printed {
            eprintln!(
                "（只打印了前 {} 个节点，共 {} 个；去掉 --head 打印全部）",
                outcome.printed, outcome.total
            );
        }
    }
    0
}

fn cmd_validate(file: &str) -> i32 {
    match ops::validate(&Policy::cli(false), &CliHooks, file) {
        Ok(v) => {
            if v.errors.is_empty() {
                println!("校验通过：0 错误");
                return 0;
            }
            for e in &v.errors {
                println!("{} <{}…> {}", e.code, &e.node_id.to_string()[..8], e.message);
            }
            println!("共 {} 个错误", v.errors.len());
            1
        }
        Err(e) => report(&e),
    }
}

fn cmd_find(file: &str, pattern: &str, json_out: bool) -> i32 {
    match ops::find(&Policy::cli(false), &CliHooks, file, pattern) {
        Ok(hits) => {
            if json_out {
                println!("{}", json!(hits.iter().map(|h| h.to_json()).collect::<Vec<_>>()));
            } else {
                for h in &hits {
                    let name = if h.name.is_empty() { "(空节点)" } else { h.name.as_str() };
                    println!("{name} <{}>", h.id);
                }
                println!("共 {} 个匹配", hits.len());
            }
            0
        }
        Err(e) => report(&e),
    }
}

/// 结构/名字/值匹配：`--root` 按节点名锚定、`--shape-of` 按形状码、`--template` 按模板实例；
/// 三者选一，叠加 `--where 路径=值` 值约束；`--json` 结构化、`--tree` 整树。
fn cmd_match(
    file: &str,
    root: Option<&str>,
    shape_of: Option<&str>,
    template: Option<&str>,
    wheres: &[(String, String)],
    json_out: bool,
    tree_out: bool,
) -> i32 {
    let q = MatchQuery {
        root: root.map(|s| s.to_string()),
        shape_of: shape_of.map(|s| s.to_string()),
        template: template.map(|s| s.to_string()),
        wheres: wheres.to_vec(),
        with_tree: tree_out,
    };
    match ops::search(&Policy::cli(false), &CliHooks, file, &q) {
        Ok(m) => {
            if json_out {
                println!("{}", m.to_json());
            } else {
                for it in &m.items {
                    let nm = if it.name.is_empty() { "(空节点)" } else { it.name.as_str() };
                    println!("{nm} <{}>", it.id);
                }
                println!("共 {} 个匹配", m.items.len());
            }
            0
        }
        Err(e) => report(&e),
    }
}

fn cmd_diff(a: &str, b: &str, json_out: bool) -> i32 {
    match ops::diff(&Policy::cli(false), &CliHooks, a, b) {
        Ok(d) => {
            if json_out {
                println!("{}", d.to_json());
            } else {
                println!(
                    "新增 {} · 删除 {} · 改动 {}",
                    d.added.len(),
                    d.removed.len(),
                    d.changed.len()
                );
                if !d.added.is_empty() {
                    println!("-- 新增 --");
                    for it in &d.added {
                        println!("  + {} <{}>", it.name, it.id);
                    }
                }
                if !d.removed.is_empty() {
                    println!("-- 删除 --");
                    for it in &d.removed {
                        println!("  - {} <{}>", it.name, it.id);
                    }
                }
                if !d.changed.is_empty() {
                    println!("-- 改动 --");
                    for c in &d.changed {
                        println!(
                            "  ~ {} <{}>: {} = {} → {} = {}",
                            c.from_name, c.id, c.from_name, c.from_value, c.to_name, c.to_value
                        );
                    }
                }
            }
            0
        }
        Err(e) => report(&e),
    }
}

fn cmd_instances(file: &str, name: &str, no_pager: bool) -> i32 {
    match ops::instances(&Policy::cli(false), &CliHooks, file, name) {
        Ok(o) => {
            if o.instances.is_empty() {
                println!("（模板 {name} 暂无实例）");
                return 0;
            }
            let mut sink = make_sink(no_pager);
            for inst in &o.instances {
                render_tree_node(&mut sink, inst, "", true, false);
                let _ = writeln!(sink);
            }
            sink.finish();
            0
        }
        Err(e) => report(&e),
    }
}

fn cmd_refs(file: &str, node_id: &str) -> i32 {
    match ops::refs(&Policy::cli(false), &CliHooks, file, node_id) {
        Ok(r) => {
            let name = if r.name.is_empty() { "(空节点)" } else { r.name.as_str() };
            println!("节点：{name} <{}>", r.id);
            if let Some(t) = &r.reference {
                if t.exists {
                    println!("  引用 → {} <{}>", t.name, t.id);
                } else {
                    println!("  引用 → {} <不存在>", t.name);
                }
            }
            if r.incoming.is_empty() {
                println!("  （无反向引用）");
            } else {
                for inc in &r.incoming {
                    println!("  ← {} <{}>", inc.name, inc.id);
                }
            }
            0
        }
        Err(e) => report(&e),
    }
}

fn cmd_history(file: &str, node_id: &str) -> i32 {
    match ops::history(&Policy::cli(false), &CliHooks, file, node_id) {
        Ok(h) => {
            let name = if h.name.is_empty() { "(空节点)" } else { h.name.as_str() };
            println!("节点：{name} <{}>", h.id);
            if !h.has_history {
                println!("（无 @history）");
            } else {
                if h.snapshots.is_empty() {
                    println!("（@history 为空）");
                }
                for s in &h.snapshots {
                    println!("  快照：{} = {}", s.name, s.value.clone().unwrap_or_default());
                    if let Some(r) = &s.replaced {
                        println!("    @replaced = {r}");
                    }
                }
            }
            0
        }
        Err(e) => report(&e),
    }
}

/// `xr history prune <file> <node-id> [--keep N] [--before <ISO前缀>] [--dry-run] [--yes]`
/// 裁剪留痕：只保留最近 N 条快照（可选再要求「早于某时刻」），丢掉的是回滚能力。
fn cmd_history_prune(file: &str, node_id: &str, keep: usize, before: Option<&str>, dry_run: bool, yes: bool) -> i32 {
    match ops::prune_history(
        &Policy::cli(yes),
        &CliHooks,
        file,
        node_id,
        keep,
        before,
        dry_run,
    ) {
        Ok(o) => {
            let head = if o.dry_run { "（预演）将裁剪" } else { "已裁剪" };
            println!(
                "{head} {} 条留痕 · 保留 {} 条 · 节点 {} → {} · 文件 {} → {} 字节",
                o.removed, o.kept, o.nodes_before, o.nodes_after, o.bytes_before, o.bytes_after
            );
            if o.removed == 0 {
                println!("（没有可裁剪的快照：可能已被裁过，或 --before 比所有快照都早）");
            }
            0
        }
        Err(e) => report(&e),
    }
}

fn cmd_export(file: &str, format: &str, subtree: Option<&str>) -> i32 {
    match ops::export_data(&Policy::cli(false), &CliHooks, file, format, subtree) {
        Ok(o) => {
            // to_md 自带结尾换行，这里用 print! 避免多一个空行
            if matches!(o.format.as_str(), "md" | "markdown") {
                print!("{}", o.text);
            } else {
                println!("{}", o.text);
            }
            0
        }
        Err(e) => report(&e),
    }
}

// ============================================================================
// 写命令
// ============================================================================

fn cmd_new(
    file: &str,
    parent: &str,
    name: &str,
    value: Option<&str>,
    no_history: bool,
    yes: bool,
) -> i32 {
    let v = value.map(ops::parse_value_str).unwrap_or(xirang_core::codec::Value::Empty);
    match ops::create_node(&Policy::cli(yes), &CliHooks, file, Some(parent), name, v, no_history) {
        Ok(o) => {
            for w in &o.warnings {
                eprintln!("{w}");
            }
            if o.new_shard {
                println!("已创建：{} <{}>（新分片）", o.name, o.id);
            } else {
                println!("已创建：{} <{}>", o.name, o.id);
            }
            0
        }
        Err(e) => report(&e),
    }
}

fn cmd_set(file: &str, node: &str, value: &str, no_history: bool, yes: bool) -> i32 {
    let v = ops::parse_value_str(value);
    match ops::set_value(&Policy::cli(yes), &CliHooks, file, node, v, no_history) {
        Ok(o) => {
            println!("已更新：{}", o.id);
            0
        }
        Err(e) => report(&e),
    }
}

fn cmd_rename(file: &str, node: &str, new_name: &str, no_history: bool, yes: bool) -> i32 {
    match ops::rename_node(&Policy::cli(yes), &CliHooks, file, node, new_name, no_history) {
        Ok(o) => {
            println!("已改名：{} → {}", o.id, o.name);
            0
        }
        Err(e) => report(&e),
    }
}

fn cmd_rm(file: &str, node: &str, yes: bool) -> i32 {
    match ops::remove_node(&Policy::cli(yes), &CliHooks, file, node) {
        Ok(o) => {
            println!("已删除（置空）：{}", o.id);
            0
        }
        Err(e) => report(&e),
    }
}

fn cmd_link(file: &str, from: &str, to: &str, no_history: bool, yes: bool) -> i32 {
    match ops::link_nodes(&Policy::cli(yes), &CliHooks, file, from, to, no_history) {
        Ok(o) => {
            println!("已连边：{} → {}", o.from, o.to);
            0
        }
        Err(e) => report(&e),
    }
}

fn cmd_copy(
    file: &str,
    node: &str,
    parent: &str,
    blank: bool,
    history: bool,
    yes: bool,
) -> i32 {
    match ops::copy_node(
        &Policy::cli(yes),
        &CliHooks,
        file,
        node,
        Some(parent),
        blank,
        !history,
    ) {
        Ok(o) => {
            for w in &o.warnings {
                eprintln!("{w}");
            }
            println!("已复制：{} <{}> → 新根 <{}>", o.src_name, o.src_id, o.new_id);
            0
        }
        Err(e) => report(&e),
    }
}

fn cmd_fill(file: &str, root: &str, assigns: &[String], no_history: bool, yes: bool) -> i32 {
    let mut pairs: Vec<(String, xirang_core::codec::Value)> = Vec::new();
    for a in assigns {
        match a.split_once('=') {
            Some((p, v)) => pairs.push((p.to_string(), ops::parse_value_str(v))),
            None => {
                eprintln!("错误：赋值格式应为 路径=值：{a}");
                return 2;
            }
        }
    }
    match ops::fill_values(&Policy::cli(yes), &CliHooks, file, root, &pairs, no_history) {
        Ok(_) => {
            // 回显用户原样输入的值（不是解析后的形式），与老输出一致
            for a in assigns {
                if let Some((p, v)) = a.split_once('=') {
                    println!("已赋值：{p} = {v}");
                }
            }
            0
        }
        Err(e) => report(&e),
    }
}

fn cmd_revert(file: &str, node_id: &str) -> i32 {
    match ops::revert_node(&Policy::cli(false), &CliHooks, file, node_id) {
        Ok(o) => match o.no_snapshot {
            // 老行为：没有可回滚的快照时提示一行，但退出码仍是 0
            Some(reason) => {
                println!("（{reason}）");
                0
            }
            None => {
                println!("已回滚到最近快照：{}", o.name);
                0
            }
        },
        Err(e) => report(&e),
    }
}

fn cmd_import(file: &str, format: &str, source: &str, yes: bool) -> i32 {
    let text = match std::fs::read_to_string(source) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("错误：{e}");
            return 2;
        }
    };
    match ops::import_data(&Policy::cli(yes), &CliHooks, file, format, &text) {
        Ok(o) => {
            match o.previous_nodes {
                Some(n) => println!(
                    "已替换：{source} → {file}（原 {n} 节点 → {} 节点；import 是整文件替换，要追加请用 --append / --template）",
                    o.nodes
                ),
                None => println!("已导入：{source} → {file}（{} 节点）", o.nodes),
            }
            0
        }
        Err(e) => report(&e),
    }
}

/// `xr import <file> --append <parent|nil> <source-json>`：把嵌套 JSON 作为子树追加。
fn cmd_import_append(file: &str, parent: &str, source: &str) -> i32 {
    let text = if source == "-" {
        let mut s = String::new();
        if std::io::Read::read_to_string(&mut std::io::stdin(), &mut s).is_err() {
            eprintln!("错误：读取 stdin 失败");
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
            eprintln!("错误：JSON 解析失败：{e}");
            return 2;
        }
    };
    match ops::import_append(&Policy::cli(false), &CliHooks, file, Some(parent), &val) {
        Ok(_) => {
            println!("已导入子树");
            0
        }
        Err(e) => report(&e),
    }
}

/// `xr import <file> --template <name> <data.json> [--under <parent|nil>]`。
fn cmd_import_template(file: &str, name: &str, data_path: &str, under: Option<&str>) -> i32 {
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
            eprintln!("错误：JSON 解析失败：{e}");
            return 2;
        }
    };
    let records: Vec<serde_json::Value> = match data {
        serde_json::Value::Array(a) => a,
        other => vec![other],
    };
    match ops::import_instances(&Policy::cli(false), &CliHooks, file, name, &records, under) {
        Ok(o) => {
            println!(
                "已导入 {} 棵实例（模板 {} <{}>）",
                o.count, o.template_name, o.template_id
            );
            0
        }
        Err(e) => report(&e),
    }
}

/// `xr tmpl add <file> <name> --from-json <sample>`：建模板定义。
fn cmd_tmpl_add(file: &str, name: &str, sample_path: Option<&str>) -> i32 {
    let sample_path = match sample_path {
        Some(p) => p,
        None => {
            eprintln!("错误：需要 --from-json <样例.json> 来定模板结构");
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
            eprintln!("错误：JSON 解析失败：{e}");
            return 2;
        }
    };
    match ops::template_define(&Policy::cli(false), &CliHooks, file, name, &sample) {
        Ok(o) => {
            println!("已创建模板：{} <{}>（自由根，挂 @模板 标记）", o.name, o.id);
            0
        }
        Err(e) => report(&e),
    }
}

fn cmd_tmpl_list(file: &str) -> i32 {
    match ops::template_list(&Policy::cli(false), &CliHooks, file) {
        Ok(items) => {
            if items.is_empty() {
                println!("（无模板）");
                return 0;
            }
            for t in &items {
                let nm = if t.name.is_empty() { "(空)" } else { t.name.as_str() };
                println!("{nm} <{}>（{} 实例）", t.id, t.instances);
            }
            0
        }
        Err(e) => report(&e),
    }
}

/// `xr tmpl rm <file> <name> [--yes]`：受保护删除模板定义（连同其所有实例）。
fn cmd_tmpl_rm(file: &str, name: &str, yes: bool) -> i32 {
    match ops::template_remove(&Policy::cli(yes), &CliHooks, file, name) {
        Ok(o) => {
            println!("已删除模板：{}（连同 {} 棵实例）", o.name, o.removed_instances);
            0
        }
        Err(e) => report(&e),
    }
}

fn cmd_blob_import(file: &str, parent: &str, source: &str) -> i32 {
    match ops::blob_import(&Policy::cli(false), &CliHooks, file, Some(parent), source) {
        Ok(o) => {
            println!("已导入 blob：{} <{}>", o.name, o.id);
            0
        }
        Err(e) => report(&e),
    }
}

fn cmd_blob_export(file: &str, node_id: &str, dest: &str, yes: bool) -> i32 {
    match ops::blob_export(&Policy::cli(yes), &CliHooks, file, node_id, dest) {
        Ok(o) => {
            println!("已导出 {} 字节 → {}", o.bytes, o.dest);
            0
        }
        Err(e) => report(&e),
    }
}

fn cmd_blob_info(file: &str, node_id: &str) -> i32 {
    match ops::blob_info(&Policy::cli(false), &CliHooks, file, node_id) {
        Ok(b) => {
            println!("二进制块：{} 字节", b.bytes);
            if let Some(f) = &b.format {
                println!("@format = {f}");
            }
            match &b.preview {
                Some(s) => println!("文本预览：{s}"),
                None => println!("（非文本，无法预览）"),
            }
            0
        }
        Err(e) => report(&e),
    }
}

// ============================================================================
// 本机状态命令（CLI 专属：MCP 不暴露）
// ============================================================================

fn value_brief(node: &xirang_core::codec::Node) -> Option<String> {
    use xirang_core::codec::Value;
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

fn label_brief(node: &xirang_core::codec::Node) -> String {
    match value_brief(node) {
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

/// 跨文件读一个节点：列出它在**相关文件**里的每一份，以及并集后的孩子（每条标来源）。
/// 相关文件 = 显式给的 + （未用 `--only` 且开着索引时）本机目录里含该编号的文件。
fn cmd_ws(node_id: &str, files: &[String], only: Option<&str>, json_out: bool) -> i32 {
    let id = match Uuid::parse(node_id) {
        Some(u) => u,
        None => {
            eprintln!("错误：无效节点 ID：{node_id}");
            return 2;
        }
    };

    let mut all: Vec<String> = match only {
        Some(o) => vec![o.to_string()],
        None => files.to_vec(),
    };
    // 注意：这里**不**把这批文件登记进本机目录。
    // 它们是用户明确给的，`ws` 本来就知道该看哪里；登记要把每个文件整份读一遍
    // （445 个文件≈450 ms），而目录的用途恰恰是「找到你没点名的文件」。
    // 需要登记时用 `xr catalog scan` / 让其它命令自然登记。
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
    // 又因目录里存的是规范路径、命令行可能给符号链接路径，此处按规范路径去重。
    let mut usable: Vec<String> = Vec::new();
    let mut seen: HashSet<PathBuf> = HashSet::new();
    for p in &all {
        let path = Path::new(p);
        if !path.is_file() {
            eprintln!("（跳过：{p} 已不存在）");
            continue;
        }
        // 去重必须按真实路径（macOS 上 /var 是 /private/var 的符号链接，
        // 词法归一会把同一份文件当成两份）；这里每文件一次 canonicalize 是可接受的代价。
        let key = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        if seen.insert(key) {
            usable.push(p.clone());
        }
    }
    if usable.is_empty() {
        eprintln!("错误：没有可读的文件");
        return 2;
    }
    let mut ws = match index::LazyWorkspace::from_paths(&usable) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("错误：{e}");
            return 2;
        }
    };
    // 索引不可用时说清楚为什么（绝不静默换成另一条路）
    if let Some(why) = ws.fallback_reason() {
        eprintln!("（提示：本次未走索引，改用整份载入——{why}；可跑 xr index rebuild）");
    }
    index_touch(xirang_core::wsidx::workspace_root(Path::new(&usable[0])));

    let views = ws.node_views(id);
    if views.is_empty() {
        eprintln!("错误：节点不存在于任何文件：{node_id}");
        return 2;
    }
    let kids = ws.children_union(id);
    let incoming = ws.references_to(id);

    if json_out {
        let node = &views[0].1;
        let out = json!({
            "id": id.to_string(),
            "name": node.name,
            "type": ops::type_name(&node.value),
            "value": value_brief(node),
            "sources": views.iter().map(|(f, _)| f.clone()).collect::<Vec<_>>(),
            "children": kids.iter().map(|(f, n)| json!({
                "id": n.id.to_string(),
                "name": n.name,
                "type": ops::type_name(&n.value),
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
    if let xirang_core::codec::Value::Reference(t) = &views[0].1.value {
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

/// `xr index <子命令> [路径…] [--json] [--verbose] [--stale] [--dry-run] [--yes] [--sample N] [--deep]`
/// 索引是缓存：看得清（status / files）、修得动（update / rebuild / compact）、
/// 删得掉（drop / forget / gc）、救得回（unlock / path）。数据文件永远不动。
fn cmd_index(sub: &str, paths: &[String], flags: &[String]) -> i32 {
    let has = |f: &str| flags.iter().any(|x| x == f);
    let json_out = has("--json");
    let verbose = has("--verbose");
    let dry_run = has("--dry-run");
    let yes = has("--yes");
    let stale_only = has("--stale");
    let deep = has("--deep");
    let sample = flags
        .iter()
        .position(|x| x == "--sample")
        .and_then(|i| flags.get(i + 1))
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(5);
    let real_paths: Vec<String> = paths.iter().filter(|p| !p.starts_with("--")).cloned().collect();
    let first = real_paths.first().map(|s| s.as_str()).unwrap_or(".");
    let ws_root = xirang_core::wsidx::workspace_root(Path::new(first));
    let dir = xirang_core::wsidx::index_dir(&ws_root);

    if xirang_core::index::index_mode() == xirang_core::index::IndexMode::Sidecar {
        return cmd_index_sidecar(sub, &real_paths, &ws_root, dry_run, yes, json_out);
    }

    match sub {
        "path" => {
            if json_out {
                println!("{}", json!({"indexDir": dir.display().to_string(), "workspace": ws_root.display().to_string()}));
            } else {
                println!("{}", dir.display());
            }
            0
        }
        "status" => match xirang_core::wsidx::info(&ws_root) {
            Ok(s) => {
                if json_out {
                    println!(
                        "{}",
                        json!({
                            "mode": "workspace", "protocol": format!("wsidx-v{}", s.version),
                            "indexDir": dir.display().to_string(), "workspace": ws_root.display().to_string(),
                            "generation": s.generation, "files": s.files, "uuidTotal": s.uuid_total,
                            "indexBytes": s.index_bytes, "baseBytes": s.base_bytes, "logBytes": s.log_bytes,
                            "ledgers": {
                                "loc": {"blocks": s.loc_blocks, "entries": s.loc_entries},
                                "rel": {"blocks": s.rel_blocks, "entries": s.rel_entries},
                                "rev": {"blocks": s.rev_blocks, "entries": s.rev_entries},
                            },
                            "staleFiles": s.stale_files.len(),
                            "needCompact": s.need_compact,
                        })
                    );
                    return 0;
                }
                println!("索引模式: workspace（工作区台账 wsidx-v{}）", s.version);
                println!("索引目录: {}", dir.display());
                println!(
                    "文件 {} 个 · 编号 {} 条 · 代数 {}",
                    s.files, s.uuid_total, s.generation
                );
                println!(
                    "定位本: 块 {} · 条目 {} · 关系本: 块 {} · 条目 {} · 反向本: 块 {} · 条目 {}",
                    s.loc_blocks, s.loc_entries, s.rel_blocks, s.rel_entries, s.rev_blocks, s.rev_entries
                );
                println!(
                    "主干 {:.2} MB · 日志 {:.2} MB（占主干 {:.0}%）· 索引目录共 {:.2} MB",
                    s.base_bytes as f64 / 1e6,
                    s.log_bytes as f64 / 1e6,
                    if s.base_bytes > 0 { s.log_bytes as f64 / s.base_bytes as f64 * 100.0 } else { 0.0 },
                    s.index_bytes as f64 / 1e6
                );
                if !s.stale_files.is_empty() {
                    println!("指纹不符（读时回退整份载入）{} 个，例如：", s.stale_files.len());
                    for p in s.stale_files.iter().take(3) {
                        println!("  {p}");
                    }
                    println!("  修：xr index update");
                }
                if s.need_compact {
                    println!("日志已超过主干 30%，建议：xr index compact");
                }
                if verbose {
                    match xirang_core::wsidx::files_status(&ws_root) {
                        Ok(list) => {
                            println!("块清单（每本台账前几个块）:");
                            for (name, entries) in [
                                ("定位", s.loc_entries),
                                ("关系", s.rel_entries),
                                ("反向", s.rev_entries),
                            ] {
                                println!("  {name}本 共 {entries} 条");
                            }
                            println!("文件清单（前 10 个，共 {} 个）:", list.len());
                            for f in list.iter().take(10) {
                                println!(
                                    "  {} · {} 条 · 代号 {}/{} · {}",
                                    f.path,
                                    f.entries,
                                    f.gen,
                                    f.cur_gen,
                                    if f.fresh { "已同步" } else { "指纹不符" }
                                );
                            }
                        }
                        Err(e) => eprintln!("（提示：读文件表失败：{e}）"),
                    }
                }
                0
            }
            Err(e) => {
                eprintln!("错误：{e}");
                2
            }
        },
        "files" => match xirang_core::wsidx::files_status(&ws_root) {
            Ok(list) => {
                let shown: Vec<&xirang_core::wsidx::FileStatus> = if stale_only {
                    list.iter().filter(|f| !f.fresh || !f.exists).collect()
                } else {
                    list.iter().collect()
                };
                if json_out {
                    println!(
                        "{}",
                        json!({
                            "count": list.len(), "shown": shown.len(),
                            "files": shown.iter().map(|f| json!({
                                "path": f.path, "entries": f.entries, "gen": f.gen,
                                "curGen": f.cur_gen, "exists": f.exists, "fresh": f.fresh,
                            })).collect::<Vec<_>>(),
                        })
                    );
                    return 0;
                }
                for f in &shown {
                    println!(
                        "{} · {} 条 · 代号 {}/{} · {}{}",
                        f.path,
                        f.entries,
                        f.gen,
                        f.cur_gen,
                        if f.exists { "" } else { "文件已不存在 · " },
                        if f.fresh { "已同步" } else { "指纹不符（跑 xr index update）" }
                    );
                }
                println!("共 {} 个文件，其中 {} 个需要处理", list.len(), list.iter().filter(|f| !f.fresh).count());
                0
            }
            Err(e) => {
                eprintln!("错误：{e}");
                2
            }
        },
        "update" => {
            let stale = xirang_core::wsidx::files_status(&ws_root)
                .map(|l| l.into_iter().filter(|f| f.exists && !f.fresh).count())
                .unwrap_or(0);
            if dry_run {
                if json_out {
                    println!("{}", json!({"dryRun": true, "staleFiles": stale}));
                } else {
                    println!("（预演）将重扫 {stale} 个被改动过的文件并清理已删除文件的条目");
                }
                return 0;
            }
            match xirang_core::wsidx::update(&ws_root) {
                Ok((n, entries, removed)) => {
                    if json_out {
                        println!("{}", json!({"updatedFiles": n, "entries": entries, "removedFiles": removed}));
                    } else {
                        println!("已更新 {n} 个文件（追加 {entries} 条）· 清理 {removed} 个已删除文件");
                    }
                    0
                }
                Err(e) => {
                    eprintln!("错误：{e}");
                    2
                }
            }
        }
        "rebuild" => {
            let files: Vec<PathBuf> = real_paths.iter().map(PathBuf::from).collect();
            match xirang_core::wsidx::rebuild(&ws_root, &files) {
                Ok(s) => {
                    if json_out {
                        println!(
                            "{}",
                            json!({"files": s.files, "loc": s.loc, "rel": s.rel, "rev": s.rev, "blocks": s.blocks})
                        );
                    } else {
                        println!(
                            "已重建索引：{} 个文件 · 定位 {} · 关系 {} · 反向 {} · 块 {}",
                            s.files, s.loc, s.rel, s.rev, s.blocks
                        );
                    }
                    0
                }
                Err(e) => {
                    eprintln!("错误：{e}");
                    2
                }
            }
        }
        "compact" => {
            if dry_run {
                match xirang_core::wsidx::info(&ws_root) {
                    Ok(s) => {
                        if json_out {
                            println!("{}", json!({"dryRun": true, "baseBytes": s.base_bytes, "logBytes": s.log_bytes}));
                        } else {
                            println!(
                                "（预演）将把 {:.2} MB 日志合并回 {:.2} MB 主干（重写三本台账）",
                                s.log_bytes as f64 / 1e6,
                                s.base_bytes as f64 / 1e6
                            );
                        }
                        0
                    }
                    Err(e) => {
                        eprintln!("错误：{e}");
                        2
                    }
                }
            } else {
                match xirang_core::wsidx::compact(&ws_root) {
                    Ok(s) => {
                        if json_out {
                            println!("{}", json!({"files": s.files, "loc": s.loc, "rel": s.rel, "rev": s.rev, "blocks": s.blocks}));
                        } else {
                            println!(
                                "已压实：{} 个文件 · 定位 {} · 关系 {} · 反向 {} · 块 {}",
                                s.files, s.loc, s.rel, s.rev, s.blocks
                            );
                        }
                        0
                    }
                    Err(e) => {
                        eprintln!("错误：{e}");
                        2
                    }
                }
            }
        }
        "gc" => {
            let gone = xirang_core::wsidx::files_status(&ws_root)
                .map(|l| l.into_iter().filter(|f| !f.exists).count())
                .unwrap_or(0);
            if dry_run {
                if json_out {
                    println!("{}", json!({"dryRun": true, "missingFiles": gone}));
                } else {
                    println!("（预演）将清理 {gone} 个已不存在的文件条目");
                }
                return 0;
            }
            match xirang_core::wsidx::gc(&ws_root) {
                Ok(n) => {
                    if json_out {
                        println!("{}", json!({"removedFiles": n}));
                    } else {
                        println!("已清理 {n} 个已不存在的文件条目（下次压实后生效）");
                    }
                    0
                }
                Err(e) => {
                    eprintln!("错误：{e}");
                    2
                }
            }
        }
        "check" => {
            let mut bad = 0usize;
            let st = match xirang_core::wsidx::info(&ws_root) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("错误：{e}");
                    return 2;
                }
            };
            let mut detail: Vec<String> = Vec::new();
            if !st.stale_files.is_empty() {
                detail.push(format!("指纹不符 {} 个", st.stale_files.len()));
                bad += st.stale_files.len();
            }
            let mut reader = match xirang_core::wsidx::Reader::open(&ws_root) {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("错误：{e}");
                    return 2;
                }
            };
            for n in reader.sample_nodes(sample) {
                match reader.locate(n) {
                    Ok(hits) if !hits.is_empty() => {
                        if hits.iter().any(|h| xirang_core::wsidx::read_node_at_hit(h).is_err()) {
                            detail.push(format!("抽样读取失败：{n}"));
                            bad += 1;
                        }
                    }
                    _ => {
                        detail.push(format!("抽样定位失败：{n}"));
                        bad += 1;
                    }
                }
            }
            if deep {
                match xirang_core::wsidx::files_status(&ws_root) {
                    Ok(list) => {
                        for f in list.iter().filter(|f| f.exists) {
                            match xirang_core::wsidx::scan_file(Path::new(&f.path), 0) {
                                Ok(e) if e.loc.len() as u64 == f.entries => {}
                                Ok(e) => {
                                    detail.push(format!(
                                        "{} 条目数不符：台账 {} / 实际 {}",
                                        f.path,
                                        f.entries,
                                        e.loc.len()
                                    ));
                                    bad += 1;
                                }
                                Err(err) => {
                                    detail.push(format!("{} 重扫失败：{err}", f.path));
                                    bad += 1;
                                }
                            }
                        }
                    }
                    Err(e) => {
                        eprintln!("错误：{e}");
                        return 2;
                    }
                }
            }
            if json_out {
                println!("{}", json!({"ok": bad == 0, "problems": bad, "details": detail}));
            } else if bad == 0 {
                println!(
                    "索引一致：抽样 {} 个编号都能定位并读出{}",
                    sample,
                    if deep { "；全量条目数比对通过" } else { "" }
                );
            } else {
                for d in &detail {
                    println!("  {d}");
                }
                println!("发现 {bad} 处问题；可跑 xr index update（增量）或 xr index rebuild（全量）");
            }
            if bad == 0 {
                0
            } else {
                1
            }
        }
        "drop" => {
            let bytes = xirang_core::wsidx::info(&ws_root).map(|s| s.index_bytes).unwrap_or(0);
            let files = xirang_core::wsidx::files_status(&ws_root).map(|l| l.len()).unwrap_or(0);
            if !yes {
                eprintln!(
                    "（保护：将删除整个索引目录 {}（{:.2} MB，登记 {} 个文件）；数据文件不受影响）",
                    dir.display(),
                    bytes as f64 / 1e6,
                    files
                );
                eprintln!("（确认请加 --yes；先看看会删什么可以加 --dry-run）");
                return 2;
            }
            if dry_run {
                if json_out {
                    println!("{}", json!({"dryRun": true, "indexDir": dir.display().to_string(), "bytes": bytes, "files": files}));
                } else {
                    println!("（预演）将删除 {}（{:.2} MB）", dir.display(), bytes as f64 / 1e6);
                }
                return 0;
            }
            match xirang_core::wsidx::drop_index(&ws_root) {
                Ok((b, n)) => {
                    if json_out {
                        println!("{}", json!({"removedBytes": b, "removedFiles": n}));
                    } else {
                        println!("已删除索引目录（释放 {:.2} MB，原登记 {n} 个文件）；数据文件未动", b as f64 / 1e6);
                    }
                    0
                }
                Err(e) => {
                    eprintln!("错误：{e}");
                    2
                }
            }
        }
        "forget" => {
            let targets: Vec<String> = real_paths
                .iter()
                .filter(|p| !matches!(p.as_str(), "" | "."))
                .cloned()
                .collect();
            if targets.is_empty() {
                eprintln!("错误：forget 需要给出要移除的文件（例：xr index forget 词库.xirang）");
                return 2;
            }
            if !yes {
                eprintln!("（保护：将从台账移除 {} 个文件的条目；数据文件保留）", targets.len());
                eprintln!("（确认请加 --yes；先看看会删什么可以加 --dry-run）");
                return 2;
            }
            if dry_run {
                if json_out {
                    println!("{}", json!({"dryRun": true, "forget": targets}));
                } else {
                    println!("（预演）将移除这些文件的条目：{}", targets.join("、"));
                }
                return 0;
            }
            match xirang_core::wsidx::forget_files(&ws_root, &targets) {
                Ok(n) => {
                    if json_out {
                        println!("{}", json!({"forgotten": n, "files": targets}));
                    } else if n == 0 {
                        println!("台账里没有这些文件（可能没被索引过，或用的是别的路径写法）");
                    } else {
                        println!("已从台账移除 {n} 个文件的条目（数据文件未动）");
                    }
                    0
                }
                Err(e) => {
                    eprintln!("错误：{e}");
                    2
                }
            }
        }
        "unlock" => match xirang_core::wsidx::unlock(&ws_root) {
            Ok(None) => {
                if json_out {
                    println!("{}", json!({"lock": null}));
                } else {
                    println!("没有锁文件（索引没在被写）");
                }
                0
            }
            Ok(Some((pid, alive))) => {
                if json_out {
                    println!("{}", json!({"removedPid": pid, "alive": alive}));
                } else {
                    println!(
                        "已删除锁文件（原记录 pid {pid}，该进程{}）",
                        if alive { "仍在运行——若它确实在写索引，请留意" } else { "已不存在" }
                    );
                }
                0
            }
            Err(e) => {
                eprintln!("错误：{e}");
                2
            }
        },
        other => {
            eprintln!("错误：未知子命令：{other}");
            eprintln!("可用：status / files / update / rebuild / compact / check / gc / drop / forget / unlock / path");
            2
        }
    }
}

/// 侧车模式下的 `xr index ...`：保持旧行为（每文件一份 `.idx`）。
fn cmd_index_sidecar(
    sub: &str,
    paths: &[String],
    _ws_root: &Path,
    dry_run: bool,
    yes: bool,
    json_out: bool,
) -> i32 {
    let files: Vec<String> =
        if paths.is_empty() { vec![".".to_string()] } else { paths.to_vec() };
    match sub {
        "path" => {
            println!("（侧车模式没有单一索引目录：每个 .xirang 旁边一个 <文件>.xirang.idx）");
            0
        }
        "unlock" => {
            println!("（侧车模式没有写者锁）");
            0
        }
        "compact" | "update" => {
            println!("（侧车模式无需{}：每次保存都会重写该文件的 .idx；要修复就 xr index rebuild）",
                if sub == "compact" { "压实" } else { "增量更新" });
            0
        }
        "files" => {
            let mut n = 0;
            for f in &files {
                let p = Path::new(f);
                if p.is_file() {
                    let idx = index::sidecar_path(p);
                    println!(
                        "{} · {}（{} 字节）",
                        f,
                        if idx.exists() { "有侧车" } else { "无侧车" },
                        idx.metadata().map(|m| m.len()).unwrap_or(0)
                    );
                    n += 1;
                }
            }
            println!("共 {n} 个文件（侧车模式；改用 XIRANG_INDEX_MODE=workspace 可看台账详情）");
            0
        }
        "drop" | "forget" => {
            if !yes {
                eprintln!("（保护：将删除对应文件的 .xirang.idx；数据文件不受影响）");
                return 2;
            }
            if dry_run {
                println!("（预演）将删除 {} 个文件对应的 .idx", files.len());
                return 0;
            }
            let mut n = 0;
            for f in &files {
                let idx = index::sidecar_path(Path::new(f));
                if idx.exists() && std::fs::remove_file(&idx).is_ok() {
                    n += 1;
                }
            }
            if json_out {
                println!("{}", json!({"removed": n}));
            } else {
                println!("已删除 {n} 个侧车索引（数据文件未动）");
            }
            0
        }
        "status" | "check" => {
            for f in &files {
                let p = Path::new(f);
                if p.is_dir() {
                    continue;
                }
                let idx_path = index::sidecar_path(p);
                let fresh = index::is_fresh(p);
                let sc = match index::Sidecar::open_for(p) {
                    Ok(s) => s,
                    Err(e) => {
                        eprintln!("错误：{f}：{e}");
                        return 2;
                    }
                };
                println!("{f}");
                println!("  索引: {}", idx_path.display());
                println!("  状态: {}", if fresh { "复用" } else { "新建/重建" });
                println!(
                    "  根: {} · 节点: {} · 引用边: {}",
                    sc.root_count, sc.node_count, sc.edge_count
                );
            }
            0
        }
        "rebuild" => {
            let mut n = 0;
            for f in &files {
                let p = Path::new(f);
                if p.is_file() {
                    if let Err(e) = index::rebuild(p) {
                        eprintln!("错误：{f}：{e}");
                        return 2;
                    }
                    n += 1;
                }
            }
            println!("已重建 {n} 个侧车索引");
            0
        }
        "gc" => {
            let mut n = 0;
            for f in &files {
                // 目录：删掉没有对应数据文件的孤儿 .idx
                if let Ok(rd) = std::fs::read_dir(f) {
                    for e in rd.flatten() {
                        let p = e.path();
                        let name = p.to_string_lossy().into_owned();
                        if let Some(base) = name.strip_suffix(".idx") {
                            if !Path::new(base).exists() {
                                let _ = std::fs::remove_file(&p);
                                n += 1;
                            }
                        }
                    }
                }
            }
            println!("已清理 {n} 个孤儿侧车索引");
            0
        }
        other => {
            eprintln!("错误：未知子命令：{other}（可用：status / rebuild / compact / check / gc）");
            2
        }
    }
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
        eprintln!("错误：没有可扫描的 .xirang 文件");
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
        let mismatch = views.iter().any(|(_, n)| n.name != first.name || n.value != first.value);
        if !mismatch {
            continue; // 自身一致；孩子不同属正常，不算冲突
        }
        diffs += 1;
        println!("{u}  （同编号，自身内容不一致）");
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
            eprintln!("错误：无效节点 ID：{uuid_str}");
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
            eprintln!("错误：基准文件里没有该编号：{base_abs}");
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
        eprintln!("错误：目录里没有：{abs}");
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
        eprintln!("错误：文件不存在：{path}");
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

// ============================================================================
// 参数解析与分发
// ============================================================================

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
    println!("  xr import <file> <json|yaml|xml> <source>  导入（整文件替换；原文件非空要加 --yes 确认）");
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
    println!("  xr index <子命令> [路径…] [--json] [--verbose] [--stale] [--dry-run] [--yes] [--sample N] [--deep]");
    println!("      status 看总览 · files 逐文件状态 · update 增量修复（自愈 + gc）· rebuild 全量重建");
    println!("      compact 压实日志 · check 一致性校验 · gc 清理已删文件 · drop 删整个索引目录（要 --yes）");
    println!("      forget 从台账移除指定文件（要 --yes）· unlock 清写者锁 · path 打印索引目录");
    println!("      （XIRANG_INDEX_MODE=sidecar 时退化为每文件 .idx 的维护；默认是工作区台账）");
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
    println!("  xr history prune <file> <node-id> [--keep N] [--before <ISO前缀>] [--dry-run] [--yes]");
    println!("      裁剪留痕：只保留最近 N 条（默认 20）快照，可选只裁早于某时刻的；丢掉的是回滚能力，故要 --yes");
    println!("  xr blob-import <file> <parent|nil> <src>   导入文件为二进制块");
    println!("  xr blob-export <file> <node-id> <dest>    导出二进制块为文件（目标已存在要加 --yes 覆盖）");
    println!("  xr blob-info <file> <node-id>           二进制块信息 / 预览");
    println!("  xr revert <file> <node-id>              回滚到最近 @history 快照");
    println!();
    println!("本机状态类命令（catalog / index / collection / compact / ws）只在 CLI 提供；");
    println!("MCP 侧（xr-mcp）覆盖文件里的数据，见 docs/MCP.md。");
}

fn main() {
    // 管道关闭（如 | head）时静默退出，不 panic（标准 Unix 工具行为）
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL); }
    // 写命令也会让 ops 记下工作区，退出前统一决定要不要后台压实
    ops::ON_INDEX_TOUCH.set(index_touch).ok();

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

    let yes = has_flag(&args, "--yes");
    let no_pager = has_flag(&args, "--no-pager");

    let code = match cmd {
        "info" | "open" => cmd_info(file),
        "tree" => {
            let mut node_id = None;
            let mut opts = ViewOpts::default();
            let mut it = args[3..].iter();
            while let Some(a) = it.next() {
                match a.as_str() {
                    "--node" => node_id = it.next().map(|s| s.as_str()),
                    "--skip-aux" => opts.skip_aux = true,
                    "--ids" => opts.include_ids = true,
                    "--head" => opts.limit = it.next().and_then(|v| v.parse::<usize>().ok()),
                    "--depth" => opts.max_depth = it.next().and_then(|v| v.parse::<usize>().ok()),
                    _ => {}
                }
            }
            cmd_tree(file, node_id, &opts, no_pager)
        }
        "cat" => {
            let mut opts = ViewOpts::default();
            let mut it = args[3..].iter();
            while let Some(a) = it.next() {
                match a.as_str() {
                    "--skip-aux" => opts.skip_aux = true,
                    "--ids" => opts.include_ids = true,
                    "--head" => opts.limit = it.next().and_then(|v| v.parse::<usize>().ok()),
                    _ => {}
                }
            }
            cmd_cat(file, &opts, has_flag(&args, "--force") || yes, no_pager)
        }
        "validate" => cmd_validate(file),
        "new" => {
            if args.len() < 5 {
                usage();
                2
            } else {
                // 只把「已知标志」当标志；`--待办` 这类文本值要保留。
                let value = args.get(5).map(|s| s.as_str()).filter(|v| !is_known_flag(v));
                cmd_new(file, &args[3], &args[4], value, has_flag(&args, "--no-history"), yes)
            }
        }
        "set" => {
            if args.len() < 5 {
                usage();
                2
            } else {
                cmd_set(file, &args[3], &args[4], has_flag(&args, "--no-history"), yes)
            }
        }
        "rename" => {
            if args.len() < 5 {
                usage();
                2
            } else {
                cmd_rename(file, &args[3], &args[4], has_flag(&args, "--no-history"), yes)
            }
        }
        "rm" => {
            if args.len() < 4 {
                usage();
                2
            } else {
                cmd_rm(file, &args[3], yes)
            }
        }
        "link" => {
            if args.len() < 5 {
                usage();
                2
            } else {
                cmd_link(file, &args[3], &args[4], has_flag(&args, "--no-history"), yes)
            }
        }
        "copy" => {
            if args.len() < 5 {
                usage();
                2
            } else {
                let mut blank = false;
                let mut history = true;
                let mut it = args[5..].iter();
                while let Some(a) = it.next() {
                    if a == "--blank" {
                        blank = true;
                    } else if a == "--no-history" {
                        history = false;
                    }
                }
                cmd_copy(file, &args[3], &args[4], blank, history, yes)
            }
        }
        "fill" => {
            if args.len() < 5 {
                usage();
                2
            } else {
                let assigns: Vec<String> =
                    args[4..].iter().filter(|a| !is_known_flag(a)).cloned().collect();
                cmd_fill(file, &args[3], &assigns, has_flag(&args, "--no-history"), yes)
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
                let mut json_out = false;
                let mut tree_out = false;
                let mut wheres: Vec<(String, String)> = Vec::new();
                let mut i = 3;
                while i < args.len() {
                    match args[i].as_str() {
                        "--root" => root = args.get(i + 1).map(|s| s.as_str()),
                        "--shape-of" => shape_of = args.get(i + 1).map(|s| s.as_str()),
                        "--template" => template = args.get(i + 1).map(|s| s.as_str()),
                        "--where" => {
                            if let Some(w) = args.get(i + 1) {
                                if let Some((p, v)) = w.split_once('=') {
                                    wheres.push((p.to_string(), v.to_string()));
                                }
                            }
                        }
                        "--json" => json_out = true,
                        "--tree" => tree_out = true,
                        _ => {}
                    }
                    i += 1;
                }
                cmd_match(file, root, shape_of, template, &wheres, json_out, tree_out)
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
                        cmd_tmpl_rm(&args[3], &args[4], yes)
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
                cmd_instances(file, &args[3], no_pager)
            }
        }
        "export" => {
            if args.len() < 4 {
                usage();
                2
            } else {
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
                cmd_import(file, &args[3], &args[4], yes)
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
            const SUBS: [&str; 11] = [
                "status", "files", "update", "rebuild", "compact", "check", "gc", "drop",
                "forget", "unlock", "path",
            ];
            let maybe_sub = args.get(2).map(|s| s.as_str()).unwrap_or("");
            let (sub, paths) = if SUBS.contains(&maybe_sub) {
                (maybe_sub, args[3..].to_vec())
            } else {
                // 兼容老写法 `xr index <file>`：默认看状态（路径与标志都从这里继续解析）
                ("status", args[2..].to_vec())
            };
            // 标志与路径分开：`--sample N` 的值也算标志的一部分
            let mut paths_only: Vec<String> = Vec::new();
            let mut flags: Vec<String> = Vec::new();
            let mut i = 0;
            while i < paths.len() {
                let a = &paths[i];
                if a.starts_with("--") {
                    flags.push(a.clone());
                    if a == "--sample" {
                        if let Some(v) = paths.get(i + 1) {
                            flags.push(v.clone());
                            i += 1;
                        }
                    }
                } else {
                    paths_only.push(a.clone());
                }
                i += 1;
            }
            cmd_index(sub, &paths_only, &flags)
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
                            eprintln!("错误：--sync 需要一并给出 --base <文件>");
                            2
                        }
                        (None, _) => cmd_catalog_check(),
                    }
                }
                "forget" => match args.get(3) {
                    Some(p) => cmd_catalog_forget(p),
                    None => {
                        eprintln!("错误：需要 <路径>");
                        2
                    }
                },
                "trash" => match args.get(3) {
                    Some(p) => cmd_catalog_trash(p),
                    None => {
                        eprintln!("错误：需要 <路径>");
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
            } else if args[2] == "prune" {
                // xr history prune <file> <node-id> [--keep N] [--before T] [--dry-run] [--yes]
                if args.len() < 5 {
                    eprintln!("错误：用法 xr history prune <file> <node-id> [--keep N] [--before <ISO前缀>] [--dry-run] [--yes]");
                    2
                } else {
                    let keep = args
                        .iter()
                        .position(|a| a == "--keep")
                        .and_then(|i| args.get(i + 1))
                        .and_then(|v| v.parse::<usize>().ok())
                        .unwrap_or(20);
                    let before = args
                        .iter()
                        .position(|a| a == "--before")
                        .and_then(|i| args.get(i + 1))
                        .map(|s| s.as_str());
                    cmd_history_prune(
                        // 注意：这里的 `file`（args[2]）是子命令 "prune"，
                        // 真正的文件名在 args[3]、节点在 args[4]
                        &args[3],
                        &args[4],
                        keep,
                        before,
                        has_flag(&args, "--dry-run"),
                        yes,
                    )
                }
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
                cmd_blob_export(file, &args[3], &args[4], yes)
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
            eprintln!("错误：未知命令：{cmd}");
            usage();
            2
        }
    };
    // 先给结果（此时 stdout 已经写完），再在后台把索引整理掉
    maybe_spawn_maintenance();
    std::process::exit(code);
}
