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
use xirang_core::convert;
use xirang_core::index::compact_file;
use xirang_core::tree::Store;

use xirang_app::edit;
use xirang_app::graph::{Graph, Settings as GraphSettings};
use xirang_app::lazy::{plain_value, Doc};
use xirang_app::scan::{self, Query};
use xirang_app::state::{FileView, ViewState};
use xirang_app::view::{flatten, Layout, Row, MAX_ROWS};

const ROW_HEIGHT: f32 = 24.0;
const IDLE_TRIM_SECS: f32 = 6.0;
const IDLE_KEEP: usize = 2_000;
const AUTO_EXPAND_MAX_CHILDREN: usize = 2_000;
const ROW_BUDGET: usize = 50_000;

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
    graph: Option<Graph>,
    graph_dirty: bool,
    graph_settings: GraphSettings,
    graph_built_orphans: bool,
    show_settings: bool,
    zoom: f32,
    pan: egui::Vec2,
    dragging: Option<Uuid>,
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
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let font_note =
            install_cjk_font(&cc.egui_ctx).unwrap_or_else(|| "未找到中文字体".to_string());
        let mut app = App {
            tabs: Vec::new(),
            active: 0,
            mode: ViewMode::Tree,
            graph: None,
            graph_dirty: true,
            graph_settings: GraphSettings::default(),
            graph_built_orphans: false,
            show_settings: false,
            zoom: 1.0,
            pan: egui::Vec2::ZERO,
            dragging: None,
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
            state: ViewState::load(),
            last_interaction: Instant::now(),
            idle_trimmed: false,
            job: None,
            show_export: false,
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
                let p = PathBuf::from(a);
                if p.exists() {
                    app.open_path(&p);
                }
            }
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
        self.graph = None;
        self.graph_dirty = true;
        self.dragging = None;
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
        let stem = path
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "export".into());
        let ext = match fmt {
            "json" => "json",
            "xml" => "xml",
            "yaml" => "yaml",
            _ => "md",
        };
        let Some(target) = rfd::FileDialog::new()
            .set_file_name(format!("{stem}.{ext}"))
            .add_filter(fmt, &[ext])
            .save_file()
        else {
            return;
        };
        match Store::load_view(&path) {
            Ok(store) => {
                let text = match fmt {
                    "json" => convert::to_json(&store),
                    "xml" => convert::to_xml(&store),
                    "yaml" => convert::to_yaml(&store),
                    _ => convert::to_md(&store),
                };
                match std::fs::write(&target, text) {
                    Ok(()) => self.status = format!("已导出 {fmt} → {}", target.display()),
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

    fn graph_candidates(&mut self) -> Vec<(Uuid, String)> {
        let mut out: Vec<(Uuid, String)> = Vec::new();
        for i in 0..self.tabs.len() {
            let label = self.tabs[i].path.display().to_string();
            if i == self.active && !self.known.is_empty() {
                out.extend(self.known.iter().cloned());
                continue;
            }
            let roots = self.tabs[i].doc.roots();
            let mut ids = roots.clone();
            for r in roots {
                ids.extend(self.tabs[i].doc.children(r));
            }
            out.extend(ids.into_iter().map(|id| (id, label.clone())));
        }
        out
    }

    fn ensure_graph(&mut self) {
        if !self.graph_dirty {
            return;
        }
        let candidates = self.graph_candidates();
        let show_aux = self.show_aux;
        let mut g = Graph::new(self.graph_settings.clone());
        {
            let tabs = &mut self.tabs;
            let mut resolve = |id: Uuid| -> Option<(Node, String)> {
                for tab in tabs.iter_mut() {
                    if let Some(n) = tab.doc.node(id) {
                        return Some((n, tab.path.display().to_string()));
                    }
                }
                None
            };
            g.build(&candidates, show_aux, &mut resolve);
        }
        self.graph = Some(g);
        self.graph_dirty = false;
        self.graph_built_orphans = self.graph_settings.show_orphans;
    }

    fn graph_settings_window(&mut self, ctx: &egui::Context) {
        let mut open = self.show_settings;
        egui::Window::new("图设置")
            .open(&mut open)
            .default_width(330.0)
            .show(ctx, |ui| {
                let s = &mut self.graph_settings;
                ui.heading("力");
                ui.add(egui::Slider::new(&mut s.center_strength, 0.0..=1.0).step_by(0.01).text("图谱向心力"));
                ui.add(egui::Slider::new(&mut s.repel_strength, 1.0..=30.0).text("节点间的排斥力"));
                ui.add(egui::Slider::new(&mut s.link_strength, 0.0..=1.0).step_by(0.01).text("相连节点间的吸引力"));
                ui.add(egui::Slider::new(&mut s.link_distance, 0.0..=500.0).text("连线长度"));
                ui.separator();
                ui.heading("显示");
                ui.add(egui::Slider::new(&mut s.text_fade_multiplier, -3.0..=3.0).step_by(0.1).text("文字淡入阈值"));
                ui.add(egui::Slider::new(&mut s.node_size_multiplier, 0.5..=3.0).text("节点大小"));
                ui.add(egui::Slider::new(&mut s.line_size_multiplier, 0.5..=3.0).text("连线粗细"));
                ui.checkbox(&mut s.show_arrow, "显示箭头（放大后）");
                ui.checkbox(&mut s.animate, "生长动画");
                ui.checkbox(&mut s.show_orphans, "显示孤立节点");
                ui.separator();
                if ui.button("重置为默认").clicked() {
                    s.reset();
                    self.graph_dirty = true;
                }
                ui.weak("默认值照搬 Obsidian：向心力 0.1 · 排斥力 10 · 连线力 1 · 连线长度 250");
            });
        self.show_settings = open;
        if self.graph_settings.show_orphans != self.graph_built_orphans {
            self.graph_dirty = true;
        } else if let Some(g) = self.graph.as_mut() {
            if g.settings != self.graph_settings {
                g.settings = self.graph_settings.clone();
                g.wake();
            }
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
        "/System/Library/Fonts/PingFang.ttc",
        "/System/Library/Fonts/Hiragino Sans GB.ttc",
        "/System/Library/Fonts/STHeiti Medium.ttc",
        "/System/Library/Fonts/Supplemental/Songti.ttc",
    ];
    for path in CANDIDATES {
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
        return Some((*path).to_string());
    }
    None
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if ctx.input(|i| i.modifiers.command && i.key_pressed(egui::Key::O)) {
            self.pick_files();
        }
        self.poll_job(ctx);

        egui::TopBottomPanel::top("toolbar").show(ctx, |ui| {
            ui.horizontal_wrapped(|ui| {
                if ui.button("打开…").clicked() {
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
                    if ui.button("关闭").clicked() {
                        let a = self.active;
                        self.close_tab(a);
                    }
                    ui.separator();
                }
                if ui.selectable_label(self.mode == ViewMode::Tree, "树").clicked()
                    && self.mode != ViewMode::Tree
                {
                    self.mode = ViewMode::Tree;
                    self.release_graph();
                }
                if ui.selectable_label(self.mode == ViewMode::Graph, "引用图").clicked()
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
                        egui::Button::selectable(layout == Some(Layout::Indent), "横向缩进"),
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
                        egui::Button::selectable(layout == Some(Layout::Layered), "纵向分层"),
                    )
                    .clicked()
                {
                    if let Some(tab) = self.tabs.get_mut(self.active) {
                        tab.layout = Layout::Layered;
                    }
                    self.rows_dirty = true;
                }
                if self.mode == ViewMode::Graph && ui.button("图设置").clicked() {
                    self.show_settings = !self.show_settings;
                }
                ui.separator();
                if ui.checkbox(&mut self.show_aux, "辅助节点").changed() {
                    self.rows_dirty = true;
                    self.graph_dirty = true;
                }
                ui.checkbox(&mut self.readonly, "只读");
                ui.separator();
                let (can_undo, can_redo) = self
                    .tabs
                    .get(self.active)
                    .and_then(|t| t.editor.as_ref())
                    .map(|e| (e.can_undo(), e.can_redo()))
                    .unwrap_or((false, false));
                if ui.add_enabled(can_undo, egui::Button::new("撤销")).clicked() {
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
                if ui.add_enabled(can_redo, egui::Button::new("重做")).clicked() {
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
                        .hint_text("搜索节点")
                        .desired_width(170.0),
                );
                egui::ComboBox::from_id_salt("query_kind")
                    .selected_text(["名字", "值", "类型"][self.query_kind.min(2)])
                    .width(70.0)
                    .show_ui(ui, |ui| {
                        for (i, k) in ["名字", "值", "类型"].iter().enumerate() {
                            ui.selectable_value(&mut self.query_kind, i, *k);
                        }
                    });
                if ui.add_enabled(!busy && has_file, egui::Button::new("搜索")).clicked() {
                    self.start_search();
                }
                if ui.add_enabled(!busy && has_file, egui::Button::new("校验")).clicked() {
                    self.start_validate();
                }
                if busy && ui.button("取消").clicked() {
                    self.cancel_job();
                }
                ui.separator();
                if ui
                    .add_enabled(has_file, egui::Button::new("导出 ▾"))
                    .clicked()
                {
                    self.show_export = true;
                }
                if ui
                    .add_enabled(has_file, egui::Button::new("合并"))
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
                        egui::Button::new("释放缓存"),
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
                if let Some(g) = self.graph.as_ref() {
                    ui.separator();
                    ui.label(format!("图 {} 节点 / {} 边", g.len(), g.edges.len()));
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
                    if let Some(g) = self.graph.as_mut() {
                        g.wake();
                    }
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
            egui::Window::new("导出")
                .open(&mut open)
                .show(ctx, |ui| {
                    ui.label("导出的是「折叠视图」（同编号只留最后一条）");
                    for fmt in ["json", "xml", "yaml", "md"] {
                        if ui.button(format!("导出为 {fmt}")).clicked() {
                            self.export(fmt);
                        }
                    }
                });
            self.show_export = open;
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
                        if ui.selectable_label(selected == Some(row.id), text).clicked() {
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
        self.ensure_graph();
        let (rect, response) =
            ui.allocate_exact_size(ui.available_size(), egui::Sense::click_and_drag());
        let painter = ui.painter_at(rect);
        let bg = ui.visuals().extreme_bg_color;
        painter.rect_filled(rect, 0.0, bg);

        let Some(graph) = self.graph.as_mut() else { return };
        if graph.is_empty() {
            painter.text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                "这些文件里没有引用关系（图是空的）",
                egui::FontId::proportional(14.0),
                ui.visuals().weak_text_color(),
            );
            return;
        }
        if graph.step(2) {
            ui.ctx().request_repaint();
        }

        let mut focus_members: HashSet<Uuid> = HashSet::new();
        if let Some(focus) = self.focus {
            let members: Vec<Uuid> = graph.nodes.iter().map(|n| n.id).collect();
            let tabs = &mut self.tabs;
            let mut subtree = Vec::new();
            for tab in tabs.iter_mut() {
                if tab.doc.node(focus).is_some() {
                    subtree = Graph::subtree(&mut tab.doc, focus, &members);
                    break;
                }
            }
            for (id, _) in &subtree {
                focus_members.insert(*id);
            }
            if subtree.len() > 1 {
                let targets = graph.focus_targets(&subtree, [0.0, 0.0], 130.0, 34.0);
                if graph.move_towards(&targets, 0.25) {
                    ui.ctx().request_repaint();
                }
            }
        }

        let pointer = ui.input(|i| i.pointer.hover_pos());
        let scroll = ui.input(|i| i.raw_scroll_delta.y);
        if response.hovered() && scroll != 0.0 {
            self.zoom = (self.zoom * (1.0 + scroll * 0.0015)).clamp(0.05, 6.0);
        }
        let center = rect.center();
        let zoom = self.zoom;
        let pan = self.pan;
        let to_screen = |p: [f32; 2]| {
            egui::pos2(
                center.x + (p[0] - pan.x) * zoom,
                center.y + (p[1] - pan.y) * zoom,
            )
        };
        let from_screen = |p: egui::Pos2| {
            [
                (p.x - center.x) / zoom + pan.x,
                (p.y - center.y) / zoom + pan.y,
            ]
        };

        let hovered_idx = pointer.and_then(|p| {
            let mut best: Option<(f32, usize)> = None;
            for (i, node) in graph.nodes.iter().enumerate() {
                let sp = to_screen(node.pos);
                let d = sp.distance(p);
                let r = graph.settings.radius(node.degree) * zoom + 4.0;
                if d <= r.max(8.0) && best.map(|(bd, _)| d < bd).unwrap_or(true) {
                    best = Some((d, i));
                }
            }
            best.map(|(_, i)| i)
        });
        if response.drag_started() {
            if let Some(i) = hovered_idx {
                self.dragging = Some(graph.nodes[i].id);
                graph.set_dragging(true);
            }
        }
        if response.dragged() {
            match self.dragging.and_then(|id| graph.index_of(id)) {
                Some(i) => {
                    if let Some(p) = pointer {
                        graph.nodes[i].pos = from_screen(p);
                        graph.nodes[i].vel = [0.0, 0.0];
                    }
                }
                None => self.pan -= response.drag_delta() / zoom,
            }
            ui.ctx().request_repaint();
        }
        if response.drag_stopped() {
            if self.dragging.is_some() {
                graph.set_dragging(false);
            }
            self.dragging = None;
        }
        let mut clicked = false;
        if response.clicked() {
            if let Some(i) = hovered_idx {
                self.selected = Some(graph.nodes[i].id);
                clicked = true;
            }
        }

        let hover_neighbors: HashSet<Uuid> = match hovered_idx {
            Some(i) => {
                let mut set = HashSet::new();
                set.insert(graph.nodes[i].id);
                for &(a, b) in &graph.edges {
                    if a == i {
                        set.insert(graph.nodes[b].id);
                    } else if b == i {
                        set.insert(graph.nodes[a].id);
                    }
                }
                set
            }
            None => HashSet::new(),
        };
        let dim_of = |id: Uuid| -> f32 {
            if hovered_idx.is_some() {
                if hover_neighbors.contains(&id) {
                    1.0
                } else {
                    0.25
                }
            } else if !focus_members.is_empty() {
                if focus_members.contains(&id) {
                    1.0
                } else {
                    0.15
                }
            } else {
                1.0
            }
        };
        let line_w = graph.settings.line_size_multiplier;
        let edge_color = ui.visuals().weak_text_color();
        for &(a, b) in &graph.edges {
            let (na, nb) = (&graph.nodes[a], &graph.nodes[b]);
            let alpha = dim_of(na.id).min(dim_of(nb.id));
            painter.line_segment(
                [to_screen(na.pos), to_screen(nb.pos)],
                egui::Stroke::new(line_w * alpha.max(0.2), edge_color.gamma_multiply(alpha)),
            );
        }
        let label_alpha = graph.settings.label_alpha(zoom);
        let text_color = ui.visuals().text_color();
        let accent = ui.visuals().selection.bg_fill;
        for node in graph.nodes.iter() {
            let sp = to_screen(node.pos);
            if !rect.contains(sp) {
                continue;
            }
            let dim = dim_of(node.id);
            let r = (graph.settings.radius(node.degree) * zoom).max(1.5);
            let fill = if self.selected == Some(node.id) {
                accent
            } else if node.aux {
                egui::Color32::from_rgb(160, 110, 70)
            } else {
                ui.visuals().widgets.inactive.bg_fill
            };
            painter.circle_filled(sp, r, fill.gamma_multiply(dim));
            painter.circle_stroke(
                sp,
                r,
                egui::Stroke::new(1.0_f32, edge_color.gamma_multiply(dim)),
            );
            if label_alpha > 0.02 {
                painter.text(
                    sp + egui::vec2(r + 4.0, 0.0),
                    egui::Align2::LEFT_CENTER,
                    &node.name,
                    egui::FontId::proportional(12.0),
                    text_color.gamma_multiply(label_alpha * dim),
                );
            }
        }
        if let Some(i) = hovered_idx {
            let node = &graph.nodes[i];
            let sp = to_screen(node.pos);
            let r = graph.settings.radius(node.degree) * zoom;
            painter.text(
                sp + egui::vec2(r + 10.0, -8.0),
                egui::Align2::LEFT_BOTTOM,
                format!("{}\n{}", node.name, node.file),
                egui::FontId::proportional(12.0),
                text_color,
            );
        }
        if clicked {
            self.set_selected(self.selected);
            self.sync_inputs();
        }
    }
}
