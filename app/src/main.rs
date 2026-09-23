//! 息壤桌面端（Rust + egui）：单一画布，树 / 网络共存，读走懒加载，写走追加。
//!
//! 当前版本（v1 · 第一步）：树的浏览（两种布局、展开徽标、辅助节点开关）+
//! 编辑闭环（改名 / 改值 / 新增 / 删除 + 完整撤销栈，全部即时追加落盘）。
//! 力导向图与背景淡出按方案排在下一步。

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use eframe::egui;
use xirang_core::codec::{parse_value, Node, Uuid, Value};
use xirang_core::validator;

use xirang_app::edit;
use xirang_app::graph::{Graph, Settings as GraphSettings};
use xirang_app::lazy::Doc;
use xirang_app::view::{flatten, Layout, Row, MAX_ROWS};

const ROW_HEIGHT: f32 = 24.0;
/// 空闲多久之后把节点缓存压小（内存及时还回去）。
const IDLE_TRIM_SECS: f32 = 6.0;
/// 空闲后保留的节点缓存条数。
const IDLE_KEEP: usize = 2_000;
/// 自动展开根的孩子数上限（超过就折叠着，避免一次摊出几十万行）。
const AUTO_EXPAND_MAX_CHILDREN: usize = 2_000;
/// 一次摊平的行数上限（配合虚拟化列表，够铺满很多屏）。
const ROW_BUDGET: usize = 50_000;

/// 画布模式：树列表 / 引用图（同一块画布，两种布局策略）。
#[derive(Clone, Copy, PartialEq, Eq)]
enum ViewMode {
    Tree,
    Graph,
}

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1180.0, 760.0])
            .with_min_inner_size([720.0, 480.0])
            .with_title("息壤 XiRang"),
        ..Default::default()
    };
    eframe::run_native(
        "息壤 XiRang",
        options,
        Box::new(|cc| Ok(Box::new(App::new(cc)))),
    )
}

struct App {
    doc: Option<Doc>,
    editor: Option<edit::Editor>,
    mode: ViewMode,
    graph: Option<Graph>,
    graph_dirty: bool,
    graph_settings: GraphSettings,
    graph_built_orphans: bool,
    show_settings: bool,
    zoom: f32,
    pan: egui::Vec2,
    dragging: Option<Uuid>,
    hovered: Option<Uuid>,
    focus: Option<Uuid>,
    /// 最近一次摊平出来的节点（建图时的候选集合，避免全库扫描）。
    known: Vec<Uuid>,
    last_interaction: Instant,
    idle_trimmed: bool,
    expanded: HashSet<Uuid>,
    rows: Vec<Row>,
    rows_dirty: bool,
    show_aux: bool,
    readonly: bool,
    layout: Layout,
    selected: Option<Uuid>,
    path_input: String,
    name_input: String,
    value_input: String,
    new_name: String,
    new_value: String,
    status: String,
    error: Option<String>,
    font_note: String,
    validation: Vec<String>,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let font_note = install_cjk_font(&cc.egui_ctx).unwrap_or_else(|| "未找到中文字体".to_string());
        let mut app = App {
            doc: None,
            editor: None,
            mode: ViewMode::Tree,
            graph: None,
            graph_dirty: true,
            graph_settings: GraphSettings::default(),
            graph_built_orphans: false,
            show_settings: false,
            zoom: 1.0,
            pan: egui::Vec2::ZERO,
            dragging: None,
            hovered: None,
            focus: None,
            known: Vec::new(),
            last_interaction: Instant::now(),
            idle_trimmed: false,
            expanded: HashSet::new(),
            rows: Vec::new(),
            rows_dirty: true,
            show_aux: true,
            readonly: false,
            layout: Layout::default(),
            selected: None,
            path_input: String::new(),
            name_input: String::new(),
            value_input: String::new(),
            new_name: String::new(),
            new_value: String::new(),
            status: "打开一个 .xirang 文件开始（⌘O）".to_string(),
            error: None,
            font_note,
            validation: Vec::new(),
        };
        // 命令行给路径就直接打开（也方便双击文件关联后续接）
        if let Some(path) = std::env::args().nth(1) {
            let p = Path::new(&path).to_path_buf();
            if p.exists() {
                app.open_path(&p);
            }
        }
        app
    }

    fn open_path(&mut self, path: &Path) {
        self.error = None;
        match Doc::open(path) {
            Ok(mut doc) => {
                let roots = doc.roots();
                // 自动展开根，但孩子特别多的根先折叠着（大文件里一展开就是几十万行）
                self.expanded = roots
                    .iter()
                    .copied()
                    .filter(|id| doc.child_count(*id) <= AUTO_EXPAND_MAX_CHILDREN)
                    .collect();
                self.selected = roots.first().copied();
                self.doc = Some(doc);
                self.editor = match edit::Editor::open(path) {
                    Ok(e) => Some(e),
                    Err(e) => {
                        self.error = Some(format!("编辑层不可用：{e}"));
                        None
                    }
                };
                self.path_input = path.display().to_string();
                self.rows_dirty = true;
                self.status = format!("已打开 {}", path.display());
                self.sync_inputs();
                self.validation.clear();
            }
            Err(e) => self.error = Some(e),
        }
    }

    /// 折叠视图校验（E/R 错误）：先折叠再校验，`append-v1` 的修订不算冲突。
    fn validate(&mut self) {
        self.validation.clear();
        let Some(doc) = self.doc.as_ref() else { return };
        let path = doc.path().to_path_buf();
        let store = match xirang_core::tree::Store::load_view(&path) {
            Ok(s) => s,
            Err(e) => {
                self.error = Some(e);
                return;
            }
        };
        let errs = validator::validate_view(&store);
        if errs.is_empty() {
            self.status = format!("校验通过：0 错误（{} 个节点）", store.len());
        } else {
            self.validation = errs
                .iter()
                .map(|e| format!("{} <{}> {}", e.code, &e.node_id.to_string()[..8], e.message))
                .collect();
            self.status = format!("{} 个校验错误", errs.len());
        }
    }

    fn pick_file(&mut self) {
        if let Some(p) = rfd::FileDialog::new()
            .add_filter("息壤文件", &["xirang"])
            .pick_file()
        {
            self.open_path(&p);
        }
    }

    /// 编辑落盘后：索引只补扫新增的那一段，缓存作废，行重算。
    fn after_edit(&mut self, what: &str) {
        if let Some(doc) = self.doc.as_mut() {
            if let Err(e) = doc.reload() {
                self.error = Some(e);
                return;
            }
        }
        self.rows_dirty = true;
        self.graph_dirty = true;
        self.status = format!("{what}（已追加落盘）");
        self.sync_inputs();
    }

    /// 释放一切与文件相关的大块内存（关闭文件 / 切换视图时用）。
    fn release_view_state(&mut self) {
        self.rows.clear();
        self.rows.shrink_to_fit();
        self.known.clear();
        self.known.shrink_to_fit();
        if let Some(doc) = self.doc.as_mut() {
            // 树上已渲染的节点缓存不再需要 → 压到最小值
            doc.trim_cache(IDLE_KEEP);
        }
    }

    fn release_graph(&mut self) {
        self.graph = None;
        self.graph_dirty = true;
        self.dragging = None;
        self.hovered = None;
    }

    fn close_file(&mut self) {
        if let Some(doc) = self.doc.as_mut() {
            doc.clear_cache();
        }
        self.doc = None;
        self.editor = None;
        self.expanded.clear();
        self.expanded.shrink_to_fit();
        self.selected = None;
        self.validation.clear();
        self.validation.shrink_to_fit();
        self.focus = None;
        self.release_graph();
        self.release_view_state();
        self.status = "已关闭文件（缓存已释放）".to_string();
    }

    /// 空闲时把节点缓存压小：不需要内存时及时还回去。
    fn maybe_idle_trim(&mut self, ctx: &egui::Context) {
        if ctx.input(|i| i.pointer.any_down() || i.raw_scroll_delta != egui::Vec2::ZERO) {
            self.last_interaction = Instant::now();
            self.idle_trimmed = false;
            return;
        }
        let idle = self.last_interaction.elapsed().as_secs_f32();
        if idle > IDLE_TRIM_SECS && !self.idle_trimmed {
            if let Some(doc) = self.doc.as_mut() {
                let before = doc.cache_len();
                doc.trim_cache(IDLE_KEEP);
                if before > IDLE_KEEP {
                    self.status = format!(
                        "空闲 {} 秒：节点缓存 {before} → {}（已释放）",
                        IDLE_TRIM_SECS as i64,
                        doc.cache_len()
                    );
                }
            }
            self.idle_trimmed = true;
        }
    }

    fn sync_inputs(&mut self) {
        let Some(id) = self.selected else { return };
        let Some(doc) = self.doc.as_mut() else { return };
        if let Some(n) = doc.node(id) {
            self.name_input = n.name.clone();
            self.value_input = plain_value(&n.value);
        }
    }

    fn current_node(&mut self) -> Option<Node> {
        let id = self.selected?;
        self.doc.as_mut()?.node(id)
    }

    /// 选中节点所属的根（首次编辑时要在根下挂 `@protocol`）。
    fn root_of(&mut self, id: Uuid) -> Option<Uuid> {
        let doc = self.doc.as_mut()?;
        let mut cur = id;
        let mut guard = 0;
        loop {
            let n = doc.node(cur)?;
            match n.parent {
                None => return Some(n.id),
                Some(p) => {
                    guard += 1;
                    if guard > 4096 {
                        return Some(n.id);
                    }
                    cur = p;
                }
            }
        }
    }

    fn apply_edit(&mut self, e: edit::Edit) {
        let Some(editor) = self.editor.as_mut() else {
            self.error = Some("只读模式：没有编辑会话".into());
            return;
        };
        match editor.apply(e) {
            Ok(()) => self.after_edit("已保存"),
            Err(err) => self.error = Some(err),
        }
    }
}

/// 把值还原成可编辑的文本（引用显示目标编号，blob 显示字节数）。
fn plain_value(v: &Value) -> String {
    match v {
        Value::Empty => String::new(),
        Value::Int(n) => n.to_string(),
        Value::Float(f) => f.to_string(),
        Value::Bool(b) => (if *b { "true" } else { "false" }).to_string(),
        Value::Text(s) => s.clone(),
        Value::Reference(t) => t.to_string(),
        Value::Blob(b) => format!("[blob {} 字节]", b.len()),
    }
}

/// 中文字体：按候选清单探测系统字体（macOS 先行），失败则回退内置字体。
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
        // ⌘O 打开文件
        if ctx.input(|i| i.modifiers.command && i.key_pressed(egui::Key::O)) {
            self.pick_file();
        }

        egui::TopBottomPanel::top("toolbar").show(ctx, |ui| {
            ui.horizontal_wrapped(|ui| {
                if ui.button("打开…").clicked() {
                    self.pick_file();
                }
                ui.separator();
                if ui
                    .selectable_label(self.layout == Layout::Indent, "横向缩进")
                    .clicked()
                {
                    self.layout = Layout::Indent;
                    self.rows_dirty = true;
                }
                if ui
                    .selectable_label(self.layout == Layout::Layered, "纵向分层")
                    .clicked()
                {
                    self.layout = Layout::Layered;
                    self.rows_dirty = true;
                }
                ui.separator();
                if ui
                    .selectable_label(self.mode == ViewMode::Tree, "树")
                    .clicked()
                    && self.mode != ViewMode::Tree
                {
                    self.mode = ViewMode::Tree;
                    // 离开图视图：图数据与位置一次性释放
                    self.release_graph();
                }
                if ui
                    .selectable_label(self.mode == ViewMode::Graph, "引用图")
                    .clicked()
                    && self.mode != ViewMode::Graph
                {
                    self.mode = ViewMode::Graph;
                    self.graph_dirty = true;
                    // 进图视图：不再需要整棵树的骨架行，释放掉（重建图时按需取数）
                    self.release_view_state();
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
                let can_undo = self.editor.as_ref().map(|e| e.can_undo()).unwrap_or(false);
                let can_redo = self.editor.as_ref().map(|e| e.can_redo()).unwrap_or(false);
                if ui.add_enabled(can_undo, egui::Button::new("撤销")).clicked() {
                    if let Some(e) = self.editor.as_mut() {
                        if let Err(err) = e.undo() {
                            self.error = Some(err);
                        } else {
                            self.after_edit("已撤销");
                        }
                    }
                }
                if ui.add_enabled(can_redo, egui::Button::new("重做")).clicked() {
                    if let Some(e) = self.editor.as_mut() {
                        if let Err(err) = e.redo() {
                            self.error = Some(err);
                        } else {
                            self.after_edit("已重做");
                        }
                    }
                }
                ui.separator();
                if ui.button("校验").clicked() {
                    self.validate();
                }
                if ui
                    .add_enabled(
                        self.doc
                            .as_ref()
                            .map(|d| d.cache_len() > 0)
                            .unwrap_or(false),
                        egui::Button::new("释放缓存"),
                    )
                    .on_hover_text("把已读过的节点缓存还回系统（需要时会重新按索引读取）")
                    .clicked()
                {
                    if let Some(doc) = self.doc.as_mut() {
                        let n = doc.cache_len();
                        doc.clear_cache();
                        self.status = format!("已释放 {n} 个节点的缓存");
                    }
                }
                ui.separator();
                if ui
                    .add_enabled(self.doc.is_some(), egui::Button::new("关闭"))
                    .clicked()
                {
                    self.close_file();
                }
            });
        });

        egui::TopBottomPanel::bottom("status").show(ctx, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label(&self.status);
                if let Some(doc) = self.doc.as_ref() {
                    ui.separator();
                    let total = doc.node_count();
                    let revs = doc.rev_count();
                    let hits = doc.hits;
                    let reads = doc.reads;
                    let cache = doc.cache_len();
                    let bytes = doc.cache_bytes() / 1024;
                    ui.label(format!(
                        "节点 {total}（含修订 {revs}）· 缓存 {cache} 个 / 约 {bytes} KB · 命中 {hits} / 直读 {reads}"
                    ));
                }
                if let Some(g) = self.graph.as_ref() {
                    ui.separator();
                    ui.label(format!("图 {} 节点 / {} 边", g.len(), g.edges.len()));
                }
                if !self.font_note.is_empty() {
                    ui.separator();
                    ui.weak(self.font_note.clone());
                }
            });
            if !self.validation.is_empty() {
                ui.horizontal_wrapped(|ui| {
                    ui.colored_label(
                        egui::Color32::from_rgb(200, 60, 60),
                        format!("⚠ {} 个校验错误：", self.validation.len()),
                    );
                    for line in self.validation.iter().take(3) {
                        ui.label(line);
                    }
                });
            }
        });

        egui::SidePanel::right("inspector")
            .default_width(320.0)
            .show(ctx, |ui| {
                ui.heading("节点");
                let Some(node) = self.current_node() else {
                    ui.label("未选中节点");
                    return;
                };
                ui.label(format!("编号 {}", node.id));
                ui.label(format!("类型 {}", value_kind(&node.value)));
                ui.add_space(6.0);
                ui.label("名字");
                ui.text_edit_singleline(&mut self.name_input);
                ui.label("值");
                ui.text_edit_multiline(&mut self.value_input);
                ui.add_space(6.0);
                let editable = !self.readonly;
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(editable, egui::Button::new("保存改值"))
                        .clicked()
                    {
                        let root = self.root_of(node.id);
                        let e = edit::set_value(&node, parse_value(&self.value_input), root);
                        self.apply_edit(e);
                    }
                    if ui
                        .add_enabled(editable, egui::Button::new("保存改名"))
                        .clicked()
                    {
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
                    if ui
                        .add_enabled(editable, egui::Button::new("＋ 新增子节点"))
                        .clicked()
                    {
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
                    if ui
                        .add_enabled(editable, egui::Button::new("删除（置空）"))
                        .clicked()
                    {
                        let root = self.root_of(node.id);
                        let e = edit::delete(&node, root);
                        self.apply_edit(e);
                    }
                });
                ui.separator();
                let appended = self.editor.as_ref().map(|e| e.appended).unwrap_or(0);
                ui.weak(format!(
                    "本次会话追加 {appended} 条记录；合并（compact）留待下一步接入"
                ));
                ui.separator();
                let focusing = self.focus == Some(node.id);
                if ui
                    .selectable_label(focusing, if focusing { "取消聚焦" } else { "聚焦此子树" })
                    .on_hover_text("聚焦后：这棵子树按树布局排开，图里其余节点淡出为背景")
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
                ui.colored_label(egui::Color32::from_rgb(200, 60, 60), err);
            }
            if self.doc.is_none() {
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
        self.maybe_idle_trim(ctx);
    }
}

impl App {
    fn show_graph(&mut self, ui: &mut egui::Ui) {
        // 1) 需要时重建：候选集合 = 最近一次树摊平出来的节点（没有就退到根）
        if self.graph_dirty {
            let candidates = if self.known.is_empty() {
                self.doc.as_mut().map(|d| d.roots()).unwrap_or_default()
            } else {
                self.known.clone()
            };
            let label = self.path_input.clone();
            if let Some(doc) = self.doc.as_mut() {
                let mut g = Graph::new(self.graph_settings.clone());
                g.build(doc, &candidates, self.show_aux, &label);
                self.graph = Some(g);
            }
            self.graph_dirty = false;
            self.graph_built_orphans = self.graph_settings.show_orphans;
        }

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
                "这个文件里没有引用关系（图是空的）",
                egui::FontId::proportional(14.0),
                ui.visuals().weak_text_color(),
            );
            return;
        }

        // 2) 力模拟：每帧 2 次迭代，静止后自动停算（省电）
        if graph.step(2) {
            ui.ctx().request_repaint();
        }

        // 3) 聚焦：焦点子树改用树布局（横向缩进），逐帧过渡过去
        let mut focus_members: HashSet<Uuid> = HashSet::new();
        if let Some(focus) = self.focus {
            let members: Vec<Uuid> = graph.nodes.iter().map(|n| n.id).collect();
            if let Some(doc) = self.doc.as_mut() {
                let subtree = Graph::subtree(doc, focus, &members);
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
        }

        // 4) 缩放与平移
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

        // 5) 悬停命中的节点（用于高亮、拖动、悬停标签）
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
        self.hovered = hovered_idx.map(|i| graph.nodes[i].id);

        // 6) 拖动节点 / 拖空白平移
        if response.drag_started() {
            match hovered_idx {
                Some(i) => {
                    self.dragging = Some(graph.nodes[i].id);
                    graph.set_dragging(true);
                }
                None => {}
            }
        }
        if response.dragged() {
            match self.dragging.and_then(|id| graph.index_of(id)) {
                Some(i) => {
                    if let Some(p) = pointer {
                        let world = from_screen(p);
                        graph.nodes[i].pos = world;
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
                let id = graph.nodes[i].id;
                self.selected = Some(id);
                clicked = true;
            }
        }

        // 7) 画边（聚焦 / 悬停时其余淡出）
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
            let color = edge_color.gamma_multiply(alpha);
            painter.line_segment(
                [to_screen(na.pos), to_screen(nb.pos)],
                egui::Stroke::new(line_w * alpha.max(0.2), color),
            );
        }

        // 8) 画节点 + 标签（文字按缩放阈值淡入；缩小时只在悬停显示）
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
            let is_selected = self.selected == Some(node.id);
            let fill = if is_selected {
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

        // 悬停：显示「节点名 + 所在文件」（缩小时名字就靠它看）
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
        let _ = zoom;
        // 借用结束后再同步右侧面板
        if clicked {
            self.sync_inputs();
        }
    }

    fn graph_settings_window(&mut self, ctx: &egui::Context) {
        let mut open = self.show_settings;
        egui::Window::new("图设置")
            .open(&mut open)
            .default_width(320.0)
            .show(ctx, |ui| {
                let s = &mut self.graph_settings;
                ui.heading("力");
                ui.add(
                    egui::Slider::new(&mut s.center_strength, 0.0..=1.0)
                        .step_by(0.01)
                        .text("图谱向心力"),
                );
                ui.add(egui::Slider::new(&mut s.repel_strength, 1.0..=30.0).text("节点间的排斥力"));
                ui.add(
                    egui::Slider::new(&mut s.link_strength, 0.0..=1.0)
                        .step_by(0.01)
                        .text("相连节点间的吸引力"),
                );
                ui.add(egui::Slider::new(&mut s.link_distance, 0.0..=500.0).text("连线长度"));
                ui.separator();
                ui.heading("显示");
                ui.add(
                    egui::Slider::new(&mut s.text_fade_multiplier, -3.0..=3.0)
                        .step_by(0.1)
                        .text("文字淡入阈值"),
                );
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
                ui.weak("默认值照搬 Obsidian 关系图谱：向心力 0.1 · 排斥力 10 · 连线力 1 · 连线长度 250");
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

    fn show_tree(&mut self, ui: &mut egui::Ui) {
            if self.rows_dirty {
                let (expanded, show_aux) = (self.expanded.clone(), self.show_aux);
                if let Some(doc) = self.doc.as_mut() {
                    self.rows = flatten(doc, &expanded, show_aux, ROW_BUDGET.min(MAX_ROWS));
                    self.known = self.rows.iter().map(|r| r.id).collect();
                }
                self.rows_dirty = false;
            }
            let rows = self.rows.clone();
            let mut toggle: Option<Uuid> = None;
            let mut select: Option<Uuid> = None;
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show_rows(ui, ROW_HEIGHT, rows.len(), |ui, range| {
                    for idx in range {
                        let row = &rows[idx];
                        ui.horizontal(|ui| {
                            ui.add_space(row.indent(self.layout));
                            if self.layout == Layout::Indent {
                                ui.monospace(row.guide());
                            }
                            let twisty = row.twisty();
                            if row.has_children() {
                                if ui.small_button(twisty).clicked() {
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
                            let color = if row.is_aux {
                                egui::Color32::from_rgb(160, 110, 70)
                            } else if row.name.is_empty() {
                                egui::Color32::GRAY
                            } else {
                                ui.visuals().text_color()
                            };
                            if ui
                                .selectable_label(self.selected == Some(row.id), text)
                                .clicked()
                            {
                                select = Some(row.id);
                            }
                            let _ = color;
                        });
                        ui.separator();
                    }
                });
            if let Some(id) = toggle {
                if !self.expanded.remove(&id) {
                    self.expanded.insert(id);
                }
                self.rows_dirty = true;
            }
            if let Some(id) = select {
                self.selected = Some(id);
                self.sync_inputs();
            }
    }
}

fn value_kind(v: &Value) -> &'static str {
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
