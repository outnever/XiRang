//! 息壤桌面端（Rust + egui）：单一画布，树 / 网络共存，读走懒加载，写走追加。
//!
//! 当前版本（v1 · 第一步）：树的浏览（两种布局、展开徽标、辅助节点开关）+
//! 编辑闭环（改名 / 改值 / 新增 / 删除 + 完整撤销栈，全部即时追加落盘）。
//! 力导向图与背景淡出按方案排在下一步。

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;

use eframe::egui;
use xirang_core::codec::{parse_value, Node, Uuid, Value};
use xirang_core::validator;

use xirang_app::edit;
use xirang_app::lazy::Doc;
use xirang_app::view::{flatten, Layout, Row, MAX_ROWS};

const ROW_HEIGHT: f32 = 24.0;

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
        App {
            doc: None,
            editor: None,
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
        }
    }

    fn open_path(&mut self, path: &Path) {
        self.error = None;
        match Doc::open(path) {
            Ok(mut doc) => {
                let roots = doc.roots();
                self.expanded = roots.iter().copied().collect();
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
        self.status = format!("{what}（已追加落盘）");
        self.sync_inputs();
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
                if ui.checkbox(&mut self.show_aux, "辅助节点").changed() {
                    self.rows_dirty = true;
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
                    ui.label(format!(
                        "节点 {total}（含修订 {revs}）· 缓存 {cache} · 命中 {hits} / 直读 {reads}"
                    ));
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
            });

        egui::CentralPanel::default().show(ctx, |ui| {
            if let Some(err) = self.error.clone() {
                ui.colored_label(egui::Color32::from_rgb(200, 60, 60), err);
            }
            if self.doc.is_none() {
                ui.centered_and_justified(|ui| ui.label("打开一个 .xirang 文件开始（⌘O）"));
                return;
            }
            if self.rows_dirty {
                let (expanded, show_aux) = (self.expanded.clone(), self.show_aux);
                if let Some(doc) = self.doc.as_mut() {
                    self.rows = flatten(doc, &expanded, show_aux, MAX_ROWS);
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
        });
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
