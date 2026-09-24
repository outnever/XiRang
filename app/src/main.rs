//! 息壤桌面端（Rust + egui，依赖内核 v1.0）。
//!
//! 多文件工作区 · 懒加载树视图（两种布局）· 引用图（Obsidian 参数 + 聚焦树布局）·
//! 自由编辑（追加落盘 + 撤销重做）· 搜索 · `@history` 时间线与回滚 · 流式校验 ·
//! 导出 · 合并 · blob 预览 · 视图态持久化 · 内存及时释放。

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;
use std::time::Instant;

use eframe::egui;
use xirang_core::codec::{parse_value, Node, Uuid, Value};
use xirang_core::index::compact_file;
use xirang_core::tree::Store;

use xirang_app::edit;
use xirang_app::blobimg;
use xirang_app::export::{self, Scope};
use xirang_app::obsidian::view::{
    local_subset, Colors, GraphView, NodeKind, NodeSpec, Options,
};
use xirang_app::i18n::{self, Lang};
use xirang_app::lazy::{plain_value, Doc};
use xirang_app::scan::{self, Query};
use xirang_app::state::{FileView, ViewState};
use xirang_app::theme::{self, Palette};
use xirang_app::view::{flatten, Layout, Row, MAX_ROWS};

/// 界面文案（跟着全局语言设置走）。
fn t(zh: &'static str) -> &'static str {
    i18n::t(zh)
}

const ROW_HEIGHT: f32 = 24.0;
const IDLE_TRIM_SECS: f32 = 6.0;
const IDLE_KEEP: usize = 2_000;
const AUTO_EXPAND_MAX_CHILDREN: usize = 2_000;
const ROW_BUDGET: usize = 50_000;
/// 图里最多画多少条引用边（超过就截断并在状态栏说明）。
const MAX_GRAPH_EDGES: usize = 20_000;
/// 孤立节点最多补多少个（保持图可控）。
const ORPHAN_LIMIT: usize = 2_000;

/// 简单哈希（配色分组 / 合成标签编号用）。
fn fnv(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// 由名字合成一个稳定的编号（标签节点用；同一名字永远同一个编号）。
fn synthetic_id(name: &str) -> Uuid {
    let h = fnv(name.as_bytes());
    let mut b = [0u8; 16];
    b[..8].copy_from_slice(&h.to_be_bytes());
    b[8..].copy_from_slice(&fnv(&[name.as_bytes(), b"tag"].concat()).to_be_bytes());
    Uuid(b)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ViewMode {
    Tree,
    Graph,
}

/// 一个打开的文件。
struct Tab {
    path: PathBuf,
    doc: Doc,
    editor: Option<edit::Editor>,
    expanded: HashSet<Uuid>,
    layout: Layout,
    selected: Option<Uuid>,
}

impl Tab {
    fn label(&self) -> String {
        self.path
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| self.path.display().to_string())
    }

    fn view(&self) -> FileView {
        FileView {
            layout: match self.layout {
                Layout::Indent => "indent".into(),
                Layout::Layered => "layered".into(),
            },
            expanded: self.expanded.iter().copied().collect(),
            focus: self.selected,
        }
    }

    fn apply_view(&mut self, v: &FileView, doc_roots: &[Uuid]) {
        if v.layout == "layered" {
            self.layout = Layout::Layered;
        }
        if !v.expanded.is_empty() {
            self.expanded = v.expanded.iter().copied().collect();
        } else {
            self.expanded = doc_roots.iter().copied().collect();
        }
        if let Some(f) = v.focus {
            self.selected = Some(f);
        } else {
            self.selected = doc_roots.first().copied();
        }
    }
}

enum JobDone {
    Search(Result<Vec<scan::Hit>, String>),
    Validate(Result<Vec<scan::Issue>, String>),
}

struct Job {
    cancel: Arc<AtomicBool>,
    rx: Receiver<JobDone>,
}

struct App {
    tabs: Vec<Tab>,
    active: usize,
    mode: ViewMode,
    graph: GraphView,
    graph_dirty: bool,
    graph_truncated: bool,
    show_settings: bool,
    focus: Option<Uuid>,
    known: Vec<(Uuid, String)>,
    rows: Vec<Row>,
    rows_dirty: bool,
    show_aux: bool,
    readonly: bool,
    selected: Option<Uuid>,
    name_input: String,
    value_input: String,
    new_name: String,
    new_value: String,
    status: String,
    error: Option<String>,
    font_note: String,
    validation: Vec<String>,
    query: String,
    query_kind: usize,
    results: Vec<scan::Hit>,
    history: Vec<(Uuid, String, String)>,
    state: ViewState,
    last_interaction: Instant,
    idle_trimmed: bool,
    job: Option<Job>,
    show_export: bool,
    /// 导出范围：0 = 完整折叠视图 · 1 = 当前视图（按展开态）· 2 = 选中子树
    export_scope: usize,
    show_palette: bool,
    lang: Lang,
    palette: Palette,
    applied_palette: Option<Palette>,
    /// 当前预览的图片（换选中节点 / 关文件时丢掉，显存及时还回去）
    image: Option<(Uuid, egui::TextureHandle)>,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let font_note =
            install_cjk_font(&cc.egui_ctx).unwrap_or_else(|| "未找到中文字体".to_string());
        let state = ViewState::load();
        let state_lang = state.lang.clone();
        let state_palette = state.palette.clone();
        i18n::set(Lang::from_str(&state_lang));
        let mut app = App {
            tabs: Vec::new(),
            active: 0,
            mode: ViewMode::Tree,
            graph: GraphView::new(),
            graph_dirty: true,
            graph_truncated: false,
            show_settings: false,
            focus: None,
            known: Vec::new(),
            rows: Vec::new(),
            rows_dirty: true,
            show_aux: true,
            readonly: false,
            selected: None,
            name_input: String::new(),
            value_input: String::new(),
            new_name: String::new(),
            new_value: String::new(),
            status: "打开一个 .xirang 文件开始（⌘O）".to_string(),
            error: None,
            font_note,
            validation: Vec::new(),
            query: String::new(),
            query_kind: 0,
            results: Vec::new(),
            history: Vec::new(),
            state,
            last_interaction: Instant::now(),
            idle_trimmed: false,
            job: None,
            show_export: false,
            export_scope: 0,
            show_palette: false,
            lang: Lang::from_str(&state_lang),
            palette: state_palette,
            applied_palette: None,
            image: None,
        };
        let args: Vec<String> = std::env::args().skip(1).collect();
        if args.is_empty() {
            // 没有给路径就打开最近一个（双击 / 重开都顺手）
            if let Some(last) = app.state.recent.first().cloned() {
                let p = PathBuf::from(last);
                if p.exists() {
                    app.open_path(&p);
                }
            }
        } else {
            for a in args {
                if a == "--graph" {
                    app.mode = ViewMode::Graph;
                    continue;
                }
                let p = PathBuf::from(a);
                if p.exists() {
                    app.open_path(&p);
                }
            }
        }
        if app.mode == ViewMode::Graph {
            app.graph_dirty = true;
        }
        app.state.prune(20);
        app
    }

    // —— 文件 / 标签 ——

    fn tab(&mut self) -> Option<&mut Tab> {
        self.tabs.get_mut(self.active)
    }

    fn open_path(&mut self, path: &Path) {
        self.error = None;
        if let Some(i) = self.tabs.iter().position(|t| t.path == path) {
            self.active = i;
            self.rows_dirty = true;
            self.graph_dirty = true;
            self.sync_inputs();
            self.status = format!("已切到 {}", path.display());
            return;
        }
        match Doc::open(path) {
            Ok(mut doc) => {
                let roots = doc.roots();
                let saved = self.state.view_of(path).cloned().unwrap_or_default();
                let editor = edit::Editor::open(path).ok();
                if editor.is_none() {
                    self.error = Some("编辑层不可用（只能浏览）".into());
                }
                let mut tab = Tab {
                    path: path.to_path_buf(),
                    doc,
                    editor,
                    expanded: HashSet::new(),
                    layout: Layout::Indent,
                    selected: None,
                };
                tab.apply_view(&saved, &roots);
                if saved.expanded.is_empty() {
                    // 首次打开：自动展开根，但孩子太多的根先折叠着
                    tab.expanded = roots
                        .iter()
                        .copied()
                        .filter(|id| tab.doc.child_count(*id) <= AUTO_EXPAND_MAX_CHILDREN)
                        .collect();
                }
                self.tabs.push(tab);
                self.active = self.tabs.len() - 1;
                self.rows_dirty = true;
                self.graph_dirty = true;
                self.state.touch_recent(path);
                let _ = self.state.save();
                self.sync_inputs();
                self.status = format!("已打开 {}", path.display());
            }
            Err(e) => self.error = Some(e),
        }
    }

    fn pick_files(&mut self) {
        if let Some(paths) = rfd::FileDialog::new()
            .add_filter("息壤文件", &["xirang"])
            .pick_files()
        {
            for p in paths {
                self.open_path(&p);
            }
        }
    }

    fn close_tab(&mut self, idx: usize) {
        if idx >= self.tabs.len() {
            return;
        }
        let mut tab = self.tabs.remove(idx);
        let view = tab.view();
        let path = tab.path.clone();
        tab.doc.clear_cache();
        self.state.set_view(&path, view);
        self.state.prune(20);
        let _ = self.state.save();
        self.active = self.active.min(self.tabs.len().saturating_sub(1));
        self.rows_dirty = true;
        self.graph_dirty = true;
        self.release_graph();
        self.history.clear();
        self.validation.clear();
        self.results.clear();
        self.status = "已关闭文件（缓存已释放）".to_string();
        self.sync_inputs();
    }

    // —— 编辑 ——

    fn after_edit(&mut self, what: &str) {
        if let Some(tab) = self.tab() {
            if let Err(e) = tab.doc.reload() {
                self.error = Some(e);
                return;
            }
        }
        self.rows_dirty = true;
        self.graph_dirty = true;
        self.status = format!("{what}（已追加落盘）");
        self.sync_inputs();
    }

    fn selected_id(&self) -> Option<Uuid> {
        self.tabs
            .get(self.active)
            .and_then(|t| t.selected)
            .or(self.selected)
    }

    fn set_selected(&mut self, id: Option<Uuid>) {
        // 换节点就把图片纹理丢掉（显存及时还给系统）
        self.image = None;
        self.selected = id;
        if let Some(tab) = self.tabs.get_mut(self.active) {
            tab.selected = id;
        }
    }

    fn current_node(&mut self) -> Option<Node> {
        let id = self.selected_id()?;
        self.tab()?.doc.node(id)
    }

    fn root_of(&mut self, id: Uuid) -> Option<Uuid> {
        let tab = self.tab()?;
        let mut cur = id;
        for _ in 0..4096 {
            let n = tab.doc.node(cur)?;
            match n.parent {
                None => return Some(n.id),
                Some(p) => cur = p,
            }
        }
        Some(id)
    }

    fn sync_inputs(&mut self) {
        let Some(id) = self.selected_id() else {
            self.history.clear();
            return;
        };
        let mut name = String::new();
        let mut value = String::new();
        if let Some(tab) = self.tab() {
            if let Some(n) = tab.doc.node(id) {
                name = n.name.clone();
                value = plain_value(&n.value);
            }
        }
        self.name_input = name;
        self.value_input = value;
        self.load_history();
    }

    fn apply_edit(&mut self, e: edit::Edit) {
        let Some(tab) = self.tabs.get_mut(self.active) else {
            return;
        };
        let Some(editor) = tab.editor.as_mut() else {
            self.error = Some("编辑层不可用（只读）".into());
            return;
        };
        match editor.apply(e) {
            Ok(()) => self.after_edit("已保存"),
            Err(err) => self.error = Some(err),
        }
    }

    /// `@history` 快照（编号 + 名字 + 值），点一下可回滚。
    fn load_history(&mut self) {
        self.history.clear();
        let Some(id) = self.selected_id() else { return };
        let Some(tab) = self.tab() else { return };
        let mut out = Vec::new();
        for kid in tab.doc.children(id) {
            let Some(kn) = tab.doc.node(kid) else { continue };
            if kn.name != "@history" {
                continue;
            }
            for snap in tab.doc.children(kid) {
                if let Some(sn) = tab.doc.node(snap) {
                    out.push((snap, sn.name.clone(), plain_value(&sn.value)));
                }
            }
        }
        self.history = out;
    }

    // —— 内存释放 ——

    fn release_graph(&mut self) {
        self.graph.set_data(Vec::new(), Vec::new());
        self.graph_dirty = true;
    }

    fn release_view_state(&mut self) {
        self.rows.clear();
        self.rows.shrink_to_fit();
        self.known.clear();
        self.known.shrink_to_fit();
        if let Some(tab) = self.tab() {
            tab.doc.trim_cache(IDLE_KEEP);
        }
    }

    fn maybe_idle_trim(&mut self, ctx: &egui::Context) {
        if ctx.input(|i| i.pointer.any_down() || i.raw_scroll_delta != egui::Vec2::ZERO) {
            self.last_interaction = Instant::now();
            self.idle_trimmed = false;
            return;
        }
        if self.last_interaction.elapsed().as_secs_f32() > IDLE_TRIM_SECS && !self.idle_trimmed {
            if let Some(tab) = self.tab() {
                let before = tab.doc.cache_len();
                tab.doc.trim_cache(IDLE_KEEP);
                if before > IDLE_KEEP {
                    self.status = format!(
                        "空闲 {} 秒：节点缓存 {before} → {}（已释放）",
                        IDLE_TRIM_SECS as i64,
                        tab.doc.cache_len()
                    );
                }
            }
            self.idle_trimmed = true;
        }
    }

    // —— 后台任务 ——

    fn start_search(&mut self) {
        let Some(tab) = self.tabs.get(self.active) else { return };
        let path = tab.path.clone();
        let q = self.query.clone();
        let query = match self.query_kind {
            0 => Query::Name(q.clone()),
            1 => Query::Value(q.clone()),
            _ => Query::Kind(q.clone()),
        };
        let cancel = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel();
        let c = cancel.clone();
        std::thread::spawn(move || {
            let r = scan::search(&path, &query, &c);
            let _ = tx.send(JobDone::Search(r));
        });
        self.job = Some(Job { cancel, rx });
        self.status = format!(
            "搜索「{q}」（{}）：扫描中…",
            ["名字", "值", "类型"][self.query_kind.min(2)]
        );
    }

    fn start_validate(&mut self) {
        let Some(tab) = self.tabs.get(self.active) else { return };
        let path = tab.path.clone();
        let cancel = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel();
        let c = cancel.clone();
        std::thread::spawn(move || {
            let mut done = 0u64;
            let r = scan::validate_stream(&path, &c, &mut |n| done = n);
            let _ = tx.send(JobDone::Validate(r));
        });
        self.job = Some(Job { cancel, rx });
        self.status = "校验中（流式扫描，不整份载入）…".to_string();
    }

    fn poll_job(&mut self, ctx: &egui::Context) {
        let Some(job) = self.job.as_ref() else { return };
        match job.rx.try_recv() {
            Ok(JobDone::Search(Ok(hits))) => {
                self.status = format!("搜索完成：{} 个节点", hits.len());
                self.results = hits;
                self.job = None;
            }
            Ok(JobDone::Search(Err(e))) => {
                self.error = Some(e);
                self.job = None;
            }
            Ok(JobDone::Validate(Ok(issues))) => {
                self.status = if issues.is_empty() {
                    "校验通过：0 错误".to_string()
                } else {
                    format!("{} 个校验错误", issues.len())
                };
                self.validation = issues
                    .iter()
                    .map(|i| format!("{} <{}> {}", i.code, &i.node_id.to_string()[..8], i.message))
                    .collect();
                self.job = None;
            }
            Ok(JobDone::Validate(Err(e))) => {
                self.error = Some(e);
                self.job = None;
            }
            Err(mpsc::TryRecvError::Empty) => ctx.request_repaint(),
            Err(mpsc::TryRecvError::Disconnected) => self.job = None,
        }
    }

    fn cancel_job(&mut self) {
        if let Some(job) = self.job.as_ref() {
            job.cancel.store(true, Ordering::Relaxed);
        }
        self.job = None;
        self.status = "已取消".to_string();
    }

    // —— 导出 / 合并 / 定位 ——

    fn export(&mut self, fmt: &str) {
        let Some(tab) = self.tabs.get(self.active) else { return };
        let path = tab.path.clone();
        let expanded = tab.expanded.clone();
        let selected = tab.selected;
        let stem = path
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "export".into());
        let scope = match self.export_scope {
            1 => Scope::View,
            2 => match selected {
                Some(id) => Scope::Subtree(id),
                None => {
                    self.error = Some("先选中一个节点再导出子树".into());
                    return;
                }
            },
            _ => Scope::Full,
        };
        let suffix = match scope {
            Scope::Full => "",
            Scope::View => "-current-view",
            Scope::Subtree(_) => "-subtree",
        };
        let ext = match fmt {
            "json" => "json",
            "xml" => "xml",
            "yaml" => "yaml",
            _ => "md",
        };
        let Some(target) = rfd::FileDialog::new()
            .set_file_name(format!("{stem}{suffix}.{ext}"))
            .add_filter(fmt, &[ext])
            .save_file()
        else {
            return;
        };
        let built = match &scope {
            Scope::Full => Store::load_view(&path),
            other => {
                let Some(tab) = self.tabs.get_mut(self.active) else { return };
                export::build(&mut tab.doc, other, &expanded)
            }
        };
        match built {
            Ok(store) => {
                let n = store.len();
                let text = export::to_text(&store, fmt);
                match std::fs::write(&target, text) {
                    Ok(()) => {
                        self.status = format!("已导出 {fmt}（{n} 个节点）→ {}", target.display())
                    }
                    Err(e) => self.error = Some(e.to_string()),
                }
            }
            Err(e) => self.error = Some(e),
        }
    }

    fn compact_active(&mut self) {
        let Some(tab) = self.tabs.get(self.active) else { return };
        let path = tab.path.clone();
        match compact_file(&path) {
            Ok((raw, folded)) => {
                if let Some(tab) = self.tabs.get_mut(self.active) {
                    let _ = tab.doc.reload();
                }
                self.rows_dirty = true;
                self.graph_dirty = true;
                self.status = format!(
                    "已合并：记录 {raw} → {folded}（折叠掉 {} 条历史记录）",
                    raw.saturating_sub(folded)
                );
            }
            Err(e) => self.error = Some(e),
        }
    }

    fn reveal(&mut self, id: Uuid) {
        let mut chain = Vec::new();
        if let Some(tab) = self.tab() {
            let mut cur = id;
            for _ in 0..4096 {
                let Some(n) = tab.doc.node(cur) else { break };
                chain.push(cur);
                match n.parent {
                    Some(p) => cur = p,
                    None => break,
                }
            }
        }
        if let Some(tab) = self.tabs.get_mut(self.active) {
            for c in chain {
                tab.expanded.insert(c);
            }
        }
        self.set_selected(Some(id));
        self.rows_dirty = true;
        self.sync_inputs();
        self.status = format!("已定位到 <{}>", &id.to_string()[..8]);
    }

    // —— 图 ——


    /// 跨打开的文件解析一个编号。
    fn resolve_node(&mut self, id: Uuid) -> Option<(Node, String)> {
        for tab in self.tabs.iter_mut() {
            if let Some(n) = tab.doc.node(id) {
                return Some((n, tab.path.display().to_string()));
            }
        }
        None
    }

    /// 把「边的两端 + 语义开关」组装成图数据（照抄参考的 buildGraph 思路）。
    fn build_graph_data(&mut self) -> (Vec<NodeSpec>, Vec<(Uuid, Uuid)>) {
        let mut edges: Vec<(Uuid, Uuid)> = Vec::new();
        for tab in self.tabs.iter_mut() {
            edges.extend(tab.doc.edges());
        }
        edges.sort_by_key(|(a, b)| (a.0, b.0));
        edges.dedup();
        let truncated = edges.len() > MAX_GRAPH_EDGES;
        if truncated {
            edges.truncate(MAX_GRAPH_EDGES);
        }
        self.graph_truncated = truncated;

        let mut specs: Vec<NodeSpec> = Vec::new();
        let mut seen: std::collections::HashSet<Uuid> = std::collections::HashSet::new();
        let mut degree: std::collections::HashMap<Uuid, usize> = std::collections::HashMap::new();
        for (a, b) in &edges {
            *degree.entry(*a).or_insert(0) += 1;
            *degree.entry(*b).or_insert(0) += 1;
        }
        let groups = self.graph.opt.groups;
        let hide_unresolved = self.graph.opt.hide_unresolved;
        let show_attachments = self.graph.opt.attachments;
        let show_tags = self.graph.opt.tags;
        let show_orphans = self.graph.opt.orphans;
        for (a, b) in &edges {
            for id in [a, b] {
                if seen.contains(id) {
                    continue;
                }
                let resolved = self.resolve_node(*id);
                let unresolved = resolved.is_none();
                let attachment = matches!(
                    resolved.as_ref().map(|(n, _)| &n.value),
                    Some(Value::Blob(_))
                );
                if unresolved && hide_unresolved {
                    continue;
                }
                if attachment && !show_attachments {
                    continue;
                }
                seen.insert(*id);
                let file = resolved
                    .as_ref()
                    .map(|(_, f)| f.clone())
                    .unwrap_or_default();
                let title = match &resolved {
                    Some((n, _)) if !n.name.is_empty() => n.name.clone(),
                    Some(_) => format!("<{}>", &id.to_string()[..8]),
                    None => format!("未解析 {}", &id.to_string()[..8]),
                };
                let kind = if unresolved {
                    NodeKind::Unresolved
                } else if attachment {
                    NodeKind::Attachment
                } else {
                    NodeKind::Note
                };
                specs.push(NodeSpec {
                    id: *id,
                    title,
                    kind,
                    weight: *degree.get(id).unwrap_or(&0),
                    series: if groups {
                        Some((fnv(file.as_bytes()) % 6) as usize)
                    } else {
                        None
                    },
                    file,
                });
            }
        }
        // 丢掉「未解析 / 附件」被关掉之后悬空的边
        let keep: std::collections::HashSet<Uuid> = specs.iter().map(|s| s.id).collect();
        let mut edges: Vec<(Uuid, Uuid)> = edges
            .into_iter()
            .filter(|(a, b)| keep.contains(a) && keep.contains(b))
            .collect();

        // 标签：节点下的 `@` 辅助子节点（按名字聚合成一个标签节点）
        if show_tags {
            let ids: Vec<Uuid> = specs.iter().map(|s| s.id).collect();
            let mut tags: std::collections::HashMap<String, Uuid> =
                std::collections::HashMap::new();
            for id in ids {
                let children = match self.tabs.iter_mut().find_map(|t| {
                    t.doc.node(id).map(|_| t.doc.children(id))
                }) {
                    Some(c) => c,
                    None => continue,
                };
                for cid in children {
                    let name = match self.resolve_node(cid) {
                        Some((n, _)) if n.name.starts_with('@') => n.name,
                        _ => continue,
                    };
                    let tag_id = *tags.entry(name.clone()).or_insert_with(|| synthetic_id(&name));
                    if !seen.contains(&tag_id) {
                        seen.insert(tag_id);
                        specs.push(NodeSpec {
                            id: tag_id,
                            title: name,
                            kind: NodeKind::Tag,
                            weight: 0,
                            series: None,
                            file: String::new(),
                        });
                    }
                    edges.push((id, tag_id));
                }
            }
        }

        // 孤立节点：把「已经加载进来的、没有边的」节点补上
        if show_orphans {
            let known = self.known.clone();
            let mut added = 0usize;
            for (id, file) in known {
                if added >= ORPHAN_LIMIT || seen.contains(&id) {
                    continue;
                }
                if let Some((n, _)) = self.resolve_node(id) {
                    seen.insert(id);
                    specs.push(NodeSpec {
                        id,
                        title: n.name,
                        kind: NodeKind::Note,
                        weight: 0,
                        series: None,
                        file,
                    });
                    added += 1;
                }
            }
        }

        // 局部图谱：以焦点为中心的 1 跳
        if self.graph.opt.local {
            if let Some(f) = self.graph.opt.focused {
                if seen.contains(&f) {
                    return local_subset(&specs, &edges, f);
                }
            }
        }
        (specs, edges)
    }

    fn refresh_graph(&mut self) {
        if !self.graph_dirty {
            return;
        }
        let (specs, edges) = self.build_graph_data();
        self.graph.set_data(specs, edges);
        self.graph.push_forces();
        self.graph_dirty = false;
    }

    fn graph_settings_window(&mut self, ctx: &egui::Context) {
        let mut open = self.show_settings;
        let mut forces_changed = false;
        let mut structure_changed = false;
        let mut want_reset_view = false;
        egui::Window::new(t("图设置"))
            .open(&mut open)
            .default_width(320.0)
            .show(ctx, |ui| {
                let o = &mut self.graph.opt;
                ui.heading(t("力"));
                forces_changed |= ui
                    .add(egui::Slider::new(&mut o.center, 0.0..=1.0).step_by(0.001).text(t("图谱向心力")))
                    .changed();
                forces_changed |= ui
                    .add(egui::Slider::new(&mut o.repel, 0.0..=20.0).step_by(1.0).text(t("节点间的排斥力")))
                    .changed();
                forces_changed |= ui
                    .add(egui::Slider::new(&mut o.link, 0.0..=1.0).step_by(0.01).text(t("相连节点间的吸引力")))
                    .changed();
                forces_changed |= ui
                    .add(egui::Slider::new(&mut o.dist, 30.0..=500.0).step_by(1.0).text(t("连线长度")))
                    .changed();
                ui.separator();
                ui.heading(t("显示"));
                ui.add(egui::Slider::new(&mut o.fade, -3.0..=3.0).step_by(0.1).text(t("文字淡出")));
                ui.add(egui::Slider::new(&mut o.node_size, 0.1..=5.0).step_by(0.1).text(t("节点大小")));
                ui.add(egui::Slider::new(&mut o.line_size, 0.1..=5.0).step_by(0.1).text(t("连线粗细")));
                ui.separator();
                structure_changed |= ui.checkbox(&mut o.arrows, t("箭头")).changed();
                structure_changed |= ui.checkbox(&mut o.groups, t("颜色分组")).changed();
                structure_changed |= ui.checkbox(&mut o.tags, t("标签")).changed();
                structure_changed |= ui.checkbox(&mut o.attachments, t("附件")).changed();
                structure_changed |= ui.checkbox(&mut o.orphans, t("孤立节点")).changed();
                structure_changed |= ui.checkbox(&mut o.hide_unresolved, t("隐藏未解析")).changed();
                structure_changed |= ui.checkbox(&mut o.local, t("局部图谱")).changed();
                ui.separator();
                if ui.button(t("重置视图")).clicked() {
                    want_reset_view = true;
                }
                if ui.button(t("重置为默认")).clicked() {
                    let focused = o.focused;
                    *o = Options::default();
                    o.focused = focused;
                    forces_changed = true;
                    structure_changed = true;
                }
                if self.graph_truncated {
                    ui.colored_label(
                        egui::Color32::from_rgb(200, 140, 60),
                        t("图已按上限截断"),
                    );
                }
                ui.weak("默认值照搬 Obsidian：向心力 0.1 · 排斥力 10 · 连线力 1 · 连线长度 250");
            });
        self.show_settings = open;
        if want_reset_view {
            self.graph.reset_view();
        }
        if structure_changed {
            self.graph_dirty = true;
        }
        if forces_changed {
            self.graph.push_forces();
        }
    }

    /// 配色：暗 / 亮预设 + 自定义（背景 / 前景 / 强调色 / 辅助色 / 边框），写进用户配置。
    fn palette_window(&mut self, ctx: &egui::Context) {
        let mut open = true;
        let mut changed = false;
        egui::Window::new(t("配色"))
            .open(&mut open)
            .default_width(320.0)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    if ui.button(t("暗色")).clicked() {
                        self.palette = Palette::default();
                        changed = true;
                    }
                    if ui.button(t("亮色")).clicked() {
                        self.palette = Palette::light();
                        changed = true;
                    }
                });
                ui.separator();
                let mut bg = theme::parse_hex(&self.palette.bg).unwrap_or(egui::Color32::BLACK);
                let mut fg = theme::parse_hex(&self.palette.fg).unwrap_or(egui::Color32::WHITE);
                let mut accent =
                    theme::parse_hex(&self.palette.accent).unwrap_or(egui::Color32::LIGHT_BLUE);
                let mut aux =
                    theme::parse_hex(&self.palette.aux).unwrap_or(egui::Color32::LIGHT_RED);
                let mut border =
                    theme::parse_hex(&self.palette.border).unwrap_or(egui::Color32::GRAY);
                ui.horizontal(|ui| {
                    ui.color_edit_button_srgba(&mut bg);
                    ui.label(t("背景"));
                });
                ui.horizontal(|ui| {
                    ui.color_edit_button_srgba(&mut fg);
                    ui.label(t("前景"));
                });
                ui.horizontal(|ui| {
                    ui.color_edit_button_srgba(&mut accent);
                    ui.label(t("强调色"));
                });
                ui.horizontal(|ui| {
                    ui.color_edit_button_srgba(&mut aux);
                    ui.label(t("辅助节点色"));
                });
                ui.horizontal(|ui| {
                    ui.color_edit_button_srgba(&mut border);
                    ui.label(t("边框"));
                });
                if changed
                    || theme::to_hex(bg) != self.palette.bg
                    || theme::to_hex(fg) != self.palette.fg
                    || theme::to_hex(accent) != self.palette.accent
                    || theme::to_hex(aux) != self.palette.aux
                    || theme::to_hex(border) != self.palette.border
                {
                    self.palette.bg = theme::to_hex(bg);
                    self.palette.fg = theme::to_hex(fg);
                    self.palette.accent = theme::to_hex(accent);
                    self.palette.aux = theme::to_hex(aux);
                    self.palette.border = theme::to_hex(border);
                    changed = true;
                }
                ui.separator();
                if ui.button(t("重置为默认")).clicked() {
                    self.palette = Palette::default();
                    changed = true;
                }
                ui.weak("配色只存在用户配置里，不写进 .xirang");
            });
        self.show_palette = open;
        if changed {
            self.state.palette = self.palette.clone();
            let _ = self.state.save();
        }
    }
}

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1280.0, 800.0])
            .with_min_inner_size([760.0, 480.0])
            .with_title("息壤 XiRang"),
        ..Default::default()
    };
    eframe::run_native(
        "息壤 XiRang",
        options,
        Box::new(|cc| Ok(Box::new(App::new(cc)))),
    )
}

fn install_cjk_font(ctx: &egui::Context) -> Option<String> {
    const CANDIDATES: &[&str] = &[
        // 西文标签优先用 Inter（放进 .app 的 Resources 或 ~/Library/Fonts 即可，OFL 许可）
        "Inter-Medium.ttf",
        "Inter.ttf",
        // 其次：系统里常见的开源无衬线（Noto Sans SC 同时覆盖中英）
        "NotoSansSC-Medium.ttf",
        "NotoSansSC.ttf",
        // 兜底：macOS 自带中文字体
        "/System/Library/Fonts/PingFang.ttc",
        "/System/Library/Fonts/Hiragino Sans GB.ttc",
        "/System/Library/Fonts/STHeiti Medium.ttc",
        "/System/Library/Fonts/Supplemental/Songti.ttc",
    ];
    // 相对文件名按「可执行文件旁的 Resources / 用户字体目录 / 系统字体目录」找
    let mut resolved: Vec<std::path::PathBuf> = Vec::new();
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()));
    let home = std::env::var("HOME").unwrap_or_default();
    for c in CANDIDATES {
        if c.starts_with('/') {
            resolved.push(std::path::PathBuf::from(c));
            continue;
        }
        if let Some(d) = &exe_dir {
            resolved.push(d.join("../Resources").join(c));
            resolved.push(d.join(c));
        }
        resolved.push(std::path::PathBuf::from(format!("{home}/Library/Fonts/{c}")));
        resolved.push(std::path::PathBuf::from(format!("/Library/Fonts/{c}")));
    }
    for path in &resolved {
        let Ok(bytes) = std::fs::read(path) else { continue };
        let mut fonts = egui::FontDefinitions::default();
        fonts
            .font_data
            .insert("cjk".to_owned(), Arc::new(egui::FontData::from_owned(bytes)));
        fonts
            .families
            .entry(egui::FontFamily::Proportional)
            .or_default()
            .insert(0, "cjk".to_owned());
        fonts
            .families
            .entry(egui::FontFamily::Monospace)
            .or_default()
            .push("cjk".to_owned());
        ctx.set_fonts(fonts);
        return Some(path.display().to_string());
    }
    None
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // 配色变了就套上去（只在变化时做）
        if self.applied_palette.as_ref() != Some(&self.palette) {
            ctx.set_visuals(theme::visuals(&self.palette));
            self.applied_palette = Some(self.palette.clone());
        }
        if ctx.input(|i| i.modifiers.command && i.key_pressed(egui::Key::O)) {
            self.pick_files();
        }
        self.poll_job(ctx);

        egui::TopBottomPanel::top("toolbar").show(ctx, |ui| {
            ui.horizontal_wrapped(|ui| {
                if ui.button(t("打开…")).clicked() {
                    self.pick_files();
                }
                if !self.tabs.is_empty() {
                    let labels: Vec<String> = self
                        .tabs
                        .iter()
                        .map(|t| {
                            let revs = t.doc.rev_count();
                            if revs > 0 {
                                format!("{}（+{revs}）", t.label())
                            } else {
                                t.label()
                            }
                        })
                        .collect();
                    let mut active = self.active;
                    egui::ComboBox::from_id_salt("tabs")
                        .selected_text(labels.get(active).cloned().unwrap_or_default())
                        .show_ui(ui, |ui| {
                            for (i, l) in labels.iter().enumerate() {
                                ui.selectable_value(&mut active, i, l);
                            }
                        });
                    if active != self.active {
                        self.active = active;
                        self.rows_dirty = true;
                        self.graph_dirty = true;
                        self.results.clear();
                        self.validation.clear();
                        self.sync_inputs();
                    }
                    if ui.button(t("关闭")).clicked() {
                        let a = self.active;
                        self.close_tab(a);
                    }
                    ui.separator();
                }
                if ui
                    .selectable_label(self.mode == ViewMode::Tree, t("树"))
                    .clicked()
                    && self.mode != ViewMode::Tree
                {
                    self.mode = ViewMode::Tree;
        self.release_graph();
        self.image = None;
                }
                if ui
                    .selectable_label(self.mode == ViewMode::Graph, t("引用图"))
                    .clicked()
                    && self.mode != ViewMode::Graph
                {
                    self.mode = ViewMode::Graph;
                    self.graph_dirty = true;
                    self.release_view_state();
                }
                let layout = self.tabs.get(self.active).map(|t| t.layout);
                if ui
                    .add_enabled(
                        layout.is_some(),
                        egui::Button::selectable(layout == Some(Layout::Indent), t("横向缩进")),
                    )
                    .clicked()
                {
                    if let Some(tab) = self.tabs.get_mut(self.active) {
                        tab.layout = Layout::Indent;
                    }
                    self.rows_dirty = true;
                }
                if ui
                    .add_enabled(
                        layout.is_some(),
                        egui::Button::selectable(layout == Some(Layout::Layered), t("纵向分层")),
                    )
                    .clicked()
                {
                    if let Some(tab) = self.tabs.get_mut(self.active) {
                        tab.layout = Layout::Layered;
                    }
                    self.rows_dirty = true;
                }
                if self.mode == ViewMode::Graph && ui.button(t("图设置")).clicked() {
                    self.show_settings = !self.show_settings;
                }
                ui.separator();
                if ui.checkbox(&mut self.show_aux, t("辅助节点")).changed() {
                    self.rows_dirty = true;
                    self.graph_dirty = true;
                }
                ui.checkbox(&mut self.readonly, t("只读"));
                if ui
                    .button(t("中文 / EN"))
                    .on_hover_text("切换界面语言 / Switch UI language")
                    .clicked()
                {
                    self.lang = self.lang.toggle();
                    i18n::set(self.lang);
                    self.state.lang = self.lang.as_str().to_string();
                    let _ = self.state.save();
                }
                if ui.button(t("配色")).on_hover_text("暗 / 亮 / 自定义").clicked() {
                    self.show_palette = !self.show_palette;
                }
                ui.separator();
                let (can_undo, can_redo) = self
                    .tabs
                    .get(self.active)
                    .and_then(|t| t.editor.as_ref())
                    .map(|e| (e.can_undo(), e.can_redo()))
                    .unwrap_or((false, false));
                if ui
                    .add_enabled(can_undo, egui::Button::new(t("撤销")))
                    .clicked()
                {
                    let r = self
                        .tabs
                        .get_mut(self.active)
                        .and_then(|t| t.editor.as_mut())
                        .map(|e| e.undo());
                    match r {
                        Some(Ok(true)) => self.after_edit("已撤销"),
                        Some(Err(e)) => self.error = Some(e),
                        _ => {}
                    }
                }
                if ui
                    .add_enabled(can_redo, egui::Button::new(t("重做")))
                    .clicked()
                {
                    let r = self
                        .tabs
                        .get_mut(self.active)
                        .and_then(|t| t.editor.as_mut())
                        .map(|e| e.redo());
                    match r {
                        Some(Ok(true)) => self.after_edit("已重做"),
                        Some(Err(e)) => self.error = Some(e),
                        _ => {}
                    }
                }
            });
            ui.horizontal_wrapped(|ui| {
                let busy = self.job.is_some();
                let has_file = !self.tabs.is_empty();
                ui.add_enabled(
                    !busy && has_file,
                    egui::TextEdit::singleline(&mut self.query)
                        .hint_text(t("搜索节点"))
                        .desired_width(170.0),
                );
                egui::ComboBox::from_id_salt("query_kind")
                    .selected_text([t("名字"), t("值"), t("类型")][self.query_kind.min(2)])
                    .width(70.0)
                    .show_ui(ui, |ui| {
                        for (i, k) in ["名字", "值", "类型"].iter().enumerate() {
                            ui.selectable_value(&mut self.query_kind, i, t(k));
                        }
                    });
                if ui
                    .add_enabled(!busy && has_file, egui::Button::new(t("搜索")))
                    .clicked()
                {
                    self.start_search();
                }
                if ui
                    .add_enabled(!busy && has_file, egui::Button::new(t("校验")))
                    .clicked()
                {
                    self.start_validate();
                }
                if busy && ui.button(t("取消")).clicked() {
                    self.cancel_job();
                }
                ui.separator();
                if ui.add_enabled(has_file, egui::Button::new(t("导出 ▾"))).clicked() {
                    self.show_export = true;
                }
                if ui
                    .add_enabled(has_file, egui::Button::new(t("合并")))
                    .on_hover_text("折叠掉同一编号的历史记录（整份重写，大文件要十几秒）")
                    .clicked()
                {
                    self.compact_active();
                }
                if ui
                    .add_enabled(
                        self.tabs
                            .get(self.active)
                            .map(|t| t.doc.cache_len() > 0)
                            .unwrap_or(false),
                        egui::Button::new(t("释放缓存")),
                    )
                    .clicked()
                {
                    if let Some(tab) = self.tabs.get_mut(self.active) {
                        let n = tab.doc.cache_len();
                        tab.doc.clear_cache();
                        self.status = format!("已释放 {n} 个节点的缓存");
                    }
                }
            });
        });

        egui::TopBottomPanel::bottom("status").show(ctx, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label(&self.status);
                if let Some(tab) = self.tabs.get(self.active) {
                    ui.separator();
                    ui.label(format!(
                        "节点 {}（含修订 {}）· 缓存 {} 个 / 约 {} KB · 命中 {} / 直读 {}",
                        tab.doc.node_count(),
                        tab.doc.rev_count(),
                        tab.doc.cache_len(),
                        tab.doc.cache_bytes() / 1024,
                        tab.doc.hits,
                        tab.doc.reads
                    ));
                }
                if self.graph.node_count() > 0 {
                    ui.separator();
                    ui.label(format!(
                        "图 {} 节点 / {} 边",
                        self.graph.node_count(),
                        self.graph.link_count()
                    ));
                }
                ui.separator();
                ui.weak(self.font_note.clone());
            });
            if !self.validation.is_empty() {
                ui.horizontal_wrapped(|ui| {
                    ui.colored_label(
                        egui::Color32::from_rgb(200, 60, 60),
                        format!("⚠ {} 个校验错误：", self.validation.len()),
                    );
                    for line in self.validation.iter().take(4) {
                        ui.label(line);
                    }
                });
            }
            if !self.results.is_empty() {
                let mut goto = None;
                ui.horizontal_wrapped(|ui| {
                    ui.label(format!("命中 {} 个：", self.results.len()));
                    for hit in self.results.iter().take(10) {
                        let text = if hit.value.is_empty() {
                            hit.name.clone()
                        } else {
                            format!("{} = {}", hit.name, hit.value)
                        };
                        if ui.small_button(text).clicked() {
                            goto = Some(hit.id);
                        }
                    }
                });
                if let Some(id) = goto {
                    self.reveal(id);
                }
            }
            if !self.history.is_empty() {
                let mut revert = None;
                ui.horizontal_wrapped(|ui| {
                    ui.label(format!("@history {} 条：", self.history.len()));
                    for (id, name, value) in self.history.iter().rev().take(6) {
                        let label = if name.is_empty() {
                            format!("(空) = {value}")
                        } else {
                            format!("{name} = {value}")
                        };
                        if ui
                            .small_button(label)
                            .on_hover_text("回滚到这个快照（追加一条记录）")
                            .clicked()
                        {
                            revert = Some((*id, name.clone(), value.clone()));
                        }
                    }
                });
                if let Some((_id, name, value)) = revert {
                    if let Some(node) = self.current_node() {
                        let root = self.root_of(node.id);
                        let e = edit::Edit {
                            id: node.id,
                            parent: node.parent,
                            before_name: node.name.clone(),
                            before_value: node.value.clone(),
                            after_name: name,
                            after_value: parse_value(&value),
                            created: false,
                            root,
                        };
                        self.apply_edit(e);
                    }
                }
            }
        });

        egui::SidePanel::right("inspector")
            .default_width(340.0)
            .show(ctx, |ui| {
                ui.heading("节点");
                let Some(node) = self.current_node() else {
                    ui.label("未选中节点");
                    ui.separator();
                    ui.label("最近打开");
                    let recent = self.state.recent.clone();
                    for r in recent.iter().take(8) {
                        if ui.small_button(r).clicked() {
                            self.open_path(Path::new(r));
                        }
                    }
                    return;
                };
                ui.label(format!("编号 {}", node.id));
                ui.label(format!("类型 {}", scan::kind_name(&node.value)));
                ui.add_space(4.0);
                ui.label("名字");
                ui.text_edit_singleline(&mut self.name_input);
                ui.label("值");
                ui.text_edit_multiline(&mut self.value_input);
                let editable = !self.readonly;
                ui.horizontal(|ui| {
                    if ui.add_enabled(editable, egui::Button::new("保存改值")).clicked() {
                        let root = self.root_of(node.id);
                        let e = edit::set_value(&node, parse_value(&self.value_input), root);
                        self.apply_edit(e);
                    }
                    if ui.add_enabled(editable, egui::Button::new("保存改名")).clicked() {
                        let root = self.root_of(node.id);
                        let e = edit::rename(&node, self.name_input.trim().to_string(), root);
                        self.apply_edit(e);
                    }
                });
                ui.separator();
                ui.label("在此节点下新增");
                ui.horizontal(|ui| {
                    ui.text_edit_singleline(&mut self.new_name);
                    ui.text_edit_singleline(&mut self.new_value);
                });
                ui.horizontal(|ui| {
                    if ui.add_enabled(editable, egui::Button::new("＋ 新增子节点")).clicked() {
                        let root = self.root_of(node.id);
                        let e = edit::create(
                            Some(node.id),
                            self.new_name.trim().to_string(),
                            parse_value(&self.new_value),
                            root,
                        );
                        self.new_name.clear();
                        self.new_value.clear();
                        self.apply_edit(e);
                    }
                    if ui.add_enabled(editable, egui::Button::new("删除（置空）")).clicked() {
                        let root = self.root_of(node.id);
                        let e = edit::delete(&node, root);
                        self.apply_edit(e);
                    }
                });
                if let Value::Blob(bytes) = &node.value {
                    ui.separator();
                    ui.label(format!("二进制块 {} 字节", bytes.len()));
                    // 能认出来的图片就画出来（PNG / JPEG）；换节点 / 关文件时纹理会被丢掉
                    let already = self.image.as_ref().map(|(id, _)| *id) == Some(node.id);
                    let decoded = if already {
                        true
                    } else if let Some((w, h, rgba)) = blobimg::decode(bytes) {
                        let img = egui::ColorImage::from_rgba_unmultiplied([w, h], &rgba);
                        let tex = ui.ctx().load_texture(
                            "blob-preview",
                            img,
                            egui::TextureOptions::LINEAR,
                        );
                        self.image = Some((node.id, tex));
                        true
                    } else {
                        self.image = None;
                        false
                    };
                    if decoded {
                        if let Some((_, tex)) = self.image.as_ref() {
                            let size = tex.size_vec2();
                            let scale = (300.0 / size.x.max(1.0)).min(1.0);
                            ui.image((tex.id(), size * scale));
                        }
                    }
                    let head: Vec<String> =
                        bytes.iter().take(24).map(|b| format!("{b:02x}")).collect();
                    ui.monospace(head.join(" "));
                    if let Ok(text) = std::str::from_utf8(&bytes[..bytes.len().min(400)]) {
                        ui.label("文本预览");
                        ui.monospace(text.chars().take(160).collect::<String>());
                    }
                    if ui.button("导出为文件").clicked() {
                        if let Some(target) = rfd::FileDialog::new().save_file() {
                            match std::fs::write(&target, bytes) {
                                Ok(()) => {
                                    self.status = format!("已导出 blob → {}", target.display())
                                }
                                Err(e) => self.error = Some(e.to_string()),
                            }
                        }
                    }
                }
                ui.separator();
                let focusing = self.focus == Some(node.id);
                if ui
                    .selectable_label(focusing, if focusing { "取消聚焦" } else { "聚焦此子树" })
                    .on_hover_text("图视图里把焦点子树排成树形，其余节点淡出为背景")
                    .clicked()
                {
                    self.focus = if focusing { None } else { Some(node.id) };
                    self.graph.opt.focused = self.focus;
                    self.graph.push_forces();
                }
            });

        egui::CentralPanel::default().show(ctx, |ui| {
            if let Some(err) = self.error.clone() {
                ui.horizontal_wrapped(|ui| {
                    ui.colored_label(egui::Color32::from_rgb(200, 60, 60), err);
                    if ui.small_button("知道了").clicked() {
                        self.error = None;
                    }
                });
            }
            if self.tabs.is_empty() {
                ui.centered_and_justified(|ui| ui.label("打开一个 .xirang 文件开始（⌘O）"));
                return;
            }
            match self.mode {
                ViewMode::Tree => self.show_tree(ui),
                ViewMode::Graph => self.show_graph(ui),
            }
        });

        if self.show_settings {
            self.graph_settings_window(ctx);
        }
        if self.show_export {
            let mut open = true;
            egui::Window::new(t("导出"))
                .open(&mut open)
                .show(ctx, |ui| {
                    ui.label(t("导出的是「折叠视图」（同编号只留最后一条）"));
                    ui.separator();
                    ui.radio_value(&mut self.export_scope, 0, t("完整折叠视图"));
                    ui.radio_value(&mut self.export_scope, 1, t("当前视图"));
                    ui.radio_value(&mut self.export_scope, 2, t("选中子树"));
                    ui.separator();
                    for fmt in ["json", "xml", "yaml", "md"] {
                        if ui.button(format!("导出为 {fmt}")).clicked() {
                            self.export(fmt);
                        }
                    }
                });
            self.show_export = open;
        }
        if self.show_palette {
            self.palette_window(ctx);
        }
        self.maybe_idle_trim(ctx);
    }
}

impl App {
    fn show_tree(&mut self, ui: &mut egui::Ui) {
        if self.rows_dirty {
            let (expanded, show_aux, label) = {
                let tab = &self.tabs[self.active];
                (
                    tab.expanded.clone(),
                    self.show_aux,
                    tab.path.display().to_string(),
                )
            };
            let rows = if let Some(tab) = self.tab() {
                flatten(&mut tab.doc, &expanded, show_aux, ROW_BUDGET.min(MAX_ROWS))
            } else {
                Vec::new()
            };
            self.known = rows.iter().map(|r| (r.id, label.clone())).collect();
            self.rows = rows;
            self.rows_dirty = false;
        }
        let layout = self
            .tabs
            .get(self.active)
            .map(|t| t.layout)
            .unwrap_or(Layout::Indent);
        let selected = self.selected_id();
        let rows = self.rows.clone();
        let aux_col = theme::aux_color(&self.palette);
        let mut toggle: Option<Uuid> = None;
        let mut select: Option<Uuid> = None;
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show_rows(ui, ROW_HEIGHT, rows.len(), |ui, range| {
                for idx in range {
                    let row = &rows[idx];
                    ui.horizontal(|ui| {
                        ui.add_space(row.indent(layout));
                        if layout == Layout::Indent {
                            ui.monospace(row.guide());
                        }
                        if row.has_children() {
                            if ui.small_button(row.twisty()).clicked() {
                                toggle = Some(row.id);
                            }
                        } else {
                            ui.weak("·");
                        }
                        let name = if row.name.is_empty() {
                            "(空槽位)".to_string()
                        } else {
                            row.name.clone()
                        };
                        let text = if row.value.is_empty() {
                            name
                        } else {
                            format!("{name} = {}", row.value)
                        };
                        let label = if row.is_aux {
                            egui::RichText::new(text).color(aux_col)
                        } else {
                            egui::RichText::new(text)
                        };
                        if ui
                            .selectable_label(selected == Some(row.id), label)
                            .clicked()
                        {
                            select = Some(row.id);
                        }
                    });
                    ui.separator();
                }
            });
        if let Some(id) = toggle {
            if let Some(tab) = self.tabs.get_mut(self.active) {
                if !tab.expanded.remove(&id) {
                    tab.expanded.insert(id);
                }
            }
            self.rows_dirty = true;
        }
        if let Some(id) = select {
            self.set_selected(Some(id));
            self.sync_inputs();
        }
    }

    fn show_graph(&mut self, ui: &mut egui::Ui) {
        self.refresh_graph();
        self.graph.colors = self.graph_colors();
        self.graph.ui(ui);
        if let Some(id) = self.graph.picked() {
            self.set_selected(Some(id));
            self.sync_inputs();
        }
        if let Some(id) = self.graph.focused_request() {
            self.focus = Some(id);
            self.graph.opt.focused = Some(id);
            if self.graph.opt.local {
                self.graph_dirty = true;
            }
        }
    }

    /// 把当前配色映射到图上（muted = 前景压暗，强调色 = 高亮 / 聚焦）。
    fn graph_colors(&self) -> Colors {
        let bg = theme::parse_hex(&self.palette.bg).unwrap_or(egui::Color32::from_rgb(31, 35, 40));
        let fg = theme::parse_hex(&self.palette.fg).unwrap_or(egui::Color32::from_rgb(230, 237, 243));
        let accent = theme::parse_hex(&self.palette.accent).unwrap_or(egui::Color32::from_rgb(88, 166, 255));
        let aux = theme::aux_color(&self.palette);
        let mut c = Colors::default();
        c.bg = bg;
        c.text = fg;
        c.fill = fg.gamma_multiply(0.55);
        c.line = fg.gamma_multiply(0.55);
        c.arrow = fg;
        c.circle = accent;
        c.focused = accent;
        c.line_highlight = accent;
        c.tag = aux;
        c.unresolved = fg.gamma_multiply(0.45);
        c
    }
}
