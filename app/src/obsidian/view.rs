//! 图视图：canvas 渲染器 → egui painter，加上视图状态、交互与后台物理线程。
//!
//! 与参考实现（`obsidian-graph-lab.source.html` 行 940–1400）逐条对应：
//! 缓动 `RU(x, target) = 0.9x + 0.1·target`、缩放锚点、平移惯性、`fitScale`、
//! 悬停压暗 `NU = 0.2`、连线分批（颜色 + 1/24 量化透明度）、视口裁剪、
//! 高亮环、箭头、标签（居中、挂在节点下方、8 向偏移模拟 `strokeText` 光晕）、
//! 静止 60 帧后停止重绘。

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use eframe::egui;
use egui::{Align2, Color32, FontId, Pos2, Rect, Stroke, Vec2};
use xirang_core::codec::Uuid;

use crate::obsidian::physics::{alpha_decay, Forces, Physics, ALPHA_MIN};

/// 未高亮节点的目标透明度（照抄 `NU`）
pub const NU: f64 = 0.2;
/// 缓动插值（照抄 `RU`）
pub fn ru(from: f64, to: f64) -> f64 {
    ru_k(from, to, 0.9)
}
pub fn ru_k(from: f64, to: f64, k: f64) -> f64 {
    from * k + to * (1.0 - k)
}
pub fn clamp(v: f64, lo: f64, hi: f64) -> f64 {
    v.max(lo).min(hi)
}

/// 节点半径（照抄 `getSize`）：`opt.nodeSize × clamp(3√(weight+1), 8, 30)`
pub fn get_size(node_size: f64, weight: usize) -> f64 {
    node_size * (3.0 * ((weight + 1) as f64).sqrt()).max(8.0).min(30.0)
}

/// 文字透明度（照抄 `setScale`）：`clamp(log2(scale) + 1 − fade, 0, 1)`
pub fn text_alpha(scale: f64, fade: f64) -> f64 {
    clamp(scale.log2() + 1.0 - fade, 0.0, 1.0)
}

/// 节点缩放（照抄）：`√(1/scale)`
pub fn node_scale(scale: f64) -> f64 {
    (1.0 / scale).sqrt()
}

/// 节点类型（决定颜色与是否参与「标签 / 附件 / 未解析」开关）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NodeKind {
    Note,
    Focused,
    Tag,
    Unresolved,
    Attachment,
}

/// 图的原始数据（由主程序从 `Doc` + 索引边构建）。
#[derive(Clone, Debug)]
pub struct NodeSpec {
    pub id: Uuid,
    pub title: String,
    pub kind: NodeKind,
    pub weight: usize,
    /// 颜色分组的系列下标（0–5），None = 用类型默认色
    pub series: Option<usize>,
    pub file: String,
}

#[derive(Clone, Debug)]
pub struct Options {
    // 力与显示参数（与参考面板一一对应）
    pub center: f64,
    pub repel: f64,
    pub link: f64,
    pub dist: f64,
    pub fade: f64,
    pub node_size: f64,
    pub line_size: f64,
    pub arrows: bool,
    pub groups: bool,
    /// 标签（`@` 辅助节点聚合成的标签节点）
    pub tags: bool,
    /// 附件（二进制块节点）
    pub attachments: bool,
    /// 孤立节点（默认开，与参考一致）
    pub orphans: bool,
    /// 隐藏未解析（引用断裂）
    pub hide_unresolved: bool,
    pub local: bool,
    /// **按根聚合**：每个顶层根当成一个节点（对应 Obsidian 的「一篇笔记 = 一个节点」）
    pub by_root: bool,
    pub focused: Option<Uuid>,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            center: 0.5187,
            repel: 10.0,
            link: 1.0,
            dist: 250.0,
            fade: 0.0,
            node_size: 1.0,
            line_size: 1.0,
            arrows: false,
            groups: false,
            tags: false,
            attachments: false,
            orphans: true,
            hide_unresolved: false,
            local: false,
            by_root: true,
            focused: None,
        }
    }
}

/// 视觉节点（物理坐标从后台线程拿，渲染状态在这里）。
#[derive(Clone, Debug)]
struct VNode {
    spec: NodeSpec,
    x: f64,
    y: f64,
    fade: f64,
    disp: [f64; 3],
    disp_init: bool,
    move_text: f64,
    screen: Option<(f64, f64, f64)>,
}

#[derive(Clone, Debug)]
struct VLink {
    source: usize,
    target: usize,
    alpha: f64,
    rgb: [f64; 3],
    init: bool,
}

/// 配色（由主程序从 Palette 传入）。
#[derive(Clone, Debug)]
pub struct Colors {
    pub fill: Color32,
    pub focused: Color32,
    pub tag: Color32,
    pub attachment: Color32,
    pub unresolved: Color32,
    pub line: Color32,
    pub line_highlight: Color32,
    pub arrow: Color32,
    pub circle: Color32,
    pub text: Color32,
    pub bg: Color32,
    pub series: [Color32; 6],
    /// 各颜色的额外透明度（照抄 CSS 变量里的 alpha）
    pub line_a: f64,
    pub line_highlight_a: f64,
    pub arrow_a: f64,
    pub text_a: f64,
    pub unresolved_a: f64,
}

impl Default for Colors {
    fn default() -> Self {
        Colors {
            fill: Color32::from_gray(150),
            focused: Color32::from_rgb(88, 166, 255),
            tag: Color32::from_rgb(63, 185, 80),
            attachment: Color32::from_rgb(210, 153, 34),
            unresolved: Color32::from_gray(120),
            line: Color32::from_gray(120),
            line_highlight: Color32::from_rgb(88, 166, 255),
            arrow: Color32::from_gray(200),
            circle: Color32::from_rgb(88, 166, 255),
            text: Color32::from_gray(230),
            bg: Color32::from_rgb(31, 35, 40),
            series: [
                Color32::from_rgb(88, 166, 255),
                Color32::from_rgb(63, 185, 80),
                Color32::from_rgb(210, 153, 34),
                Color32::from_rgb(188, 140, 255),
                Color32::from_rgb(240, 136, 62),
                Color32::from_rgb(86, 194, 190),
            ],
            line_a: 0.55,
            line_highlight_a: 0.75,
            arrow_a: 0.5,
            text_a: 1.0,
            unresolved_a: 0.5,
        }
    }
}

// ===========================================================================
// 后台物理线程（等价于参考的 Worker + SharedArrayBuffer + 版本号）
// ===========================================================================

enum Cmd {
    Nodes { ids: Vec<Uuid>, weights: Vec<usize>, positions: Vec<(f64, f64)> },
    Links(Vec<(usize, usize)>),
    Forces(Forces),
    Pin { index: usize, pos: Option<(f64, f64)> },
    AlphaTarget(f64),
    Wake(f64),
}

struct Shared {
    buf: Mutex<Vec<f32>>,
    version: AtomicU32,
}

struct Worker {
    tx: Sender<Cmd>,
    shared: Arc<Shared>,
    stop: Arc<AtomicBool>,
    active: Arc<AtomicBool>,
    last_version: u32,
}

impl Worker {
    fn start() -> Worker {
        let (tx, rx) = channel::<Cmd>();
        let shared = Arc::new(Shared {
            buf: Mutex::new(Vec::new()),
            version: AtomicU32::new(0),
        });
        let stop = Arc::new(AtomicBool::new(false));
        let active = Arc::new(AtomicBool::new(true));
        let (s2, a2, st2) = (shared.clone(), active.clone(), stop.clone());
        std::thread::spawn(move || {
            let mut p = Physics::new();
            let mut alpha_target = 0.0_f64;
            loop {
                if st2.load(Ordering::Relaxed) {
                    break;
                }
                let mut idle = false;
                while let Ok(cmd) = rx.try_recv() {
                    match cmd {
                        Cmd::Nodes { ids, weights, positions } => {
                            p.set_nodes(&ids, &weights);
                            p.set_positions(&positions);
                            p.alpha = 1.0;
                        }
                        Cmd::Links(pairs) => p.set_links(&pairs),
                        Cmd::Forces(f) => {
                            f.apply(&mut p);
                            if p.alpha < 0.3 {
                                p.alpha = 0.3;
                            }
                        }
                        Cmd::Pin { index, pos } => {
                            if let Some(n) = p.nodes.get_mut(index) {
                                match pos {
                                    Some((x, y)) => {
                                        n.fx = Some(x);
                                        n.fy = Some(y);
                                        n.x = x;
                                        n.y = y;
                                    }
                                    None => {
                                        n.fx = None;
                                        n.fy = None;
                                    }
                                }
                            }
                        }
                        Cmd::AlphaTarget(v) => alpha_target = v,
                        Cmd::Wake(v) => p.wake(v),
                    }
                }
                p.alpha_target = alpha_target;
                if p.alpha <= ALPHA_MIN && alpha_target <= 0.0 {
                    idle = true;
                } else {
                    p.alpha += (alpha_target - p.alpha) * alpha_decay();
                    p.step();
                    if let Ok(mut buf) = s2.buf.lock() {
                        let need = p.nodes.len() * 2;
                        if buf.len() != need {
                            buf.resize(need, 0.0);
                        }
                        for (i, n) in p.nodes.iter().enumerate() {
                            buf[2 * i] = n.x as f32;
                            buf[2 * i + 1] = n.y as f32;
                        }
                    }
                    // 版本号：先自增，读的人按「值变了」判断有新帧
                    s2.version.fetch_add(1, Ordering::Release);
                }
                a2.store(!idle, Ordering::Relaxed);
                std::thread::sleep(Duration::from_millis(if idle { 24 } else { 16 }));
            }
        });
        Worker {
            tx,
            shared,
            stop,
            active,
            last_version: 0,
        }
    }

    fn send(&self, cmd: Cmd) {
        let _ = self.tx.send(cmd);
    }

    /// 拿最新一帧坐标（版本没变就返回 false）
    fn poll(&mut self, nodes: &mut [VNode]) -> bool {
        let v = self.shared.version.load(Ordering::Acquire);
        if v == self.last_version {
            return false;
        }
        self.last_version = v;
        if let Ok(buf) = self.shared.buf.lock() {
            for (i, n) in nodes.iter_mut().enumerate() {
                if 2 * i + 1 < buf.len() {
                    n.x = buf[2 * i] as f64;
                    n.y = buf[2 * i + 1] as f64;
                }
            }
        }
        true
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

// ===========================================================================
// 图视图
// ===========================================================================

pub struct GraphView {
    pub opt: Options,
    pub colors: Colors,
    nodes: Vec<VNode>,
    links: Vec<VLink>,
    neighbors: Vec<Vec<usize>>,
    worker: Worker,
    /// 视图状态（照抄参考里的 R）
    scale: f64,
    target_scale: f64,
    text_alpha: f64,
    pan_x: f64,
    pan_y: f64,
    panv_x: f64,
    panv_y: f64,
    width: f64,
    height: f64,
    zoom_center_x: f64,
    zoom_center_y: f64,
    auto_fit: bool,
    idle_frames: u32,
    highlight: Option<usize>,
    drag: Option<usize>,
    drag_moved: bool,
    down: Option<(f64, f64)>,
    panning: Option<(f64, f64, f64, f64)>,
    last_move: f64,
    frame_ms: f64,
    /// 交互结果：这一帧被点选的节点
    picked: Option<Uuid>,
    /// 双击（等价于参考里「点一下节点 = 聚焦」）
    focused_request: Option<Uuid>,
    label_font: Option<String>,
}

impl GraphView {
    pub fn new() -> Self {
        GraphView {
            opt: Options::default(),
            colors: Colors::default(),
            nodes: Vec::new(),
            links: Vec::new(),
            neighbors: Vec::new(),
            worker: Worker::start(),
            scale: 0.5,
            target_scale: 0.5,
            text_alpha: 0.0,
            pan_x: 0.0,
            pan_y: 0.0,
            panv_x: 0.0,
            panv_y: 0.0,
            width: 0.0,
            height: 0.0,
            zoom_center_x: 0.0,
            zoom_center_y: 0.0,
            auto_fit: true,
            idle_frames: 0,
            highlight: None,
            drag: None,
            drag_moved: false,
            down: None,
            panning: None,
            last_move: 0.0,
            frame_ms: 1.0,
            picked: None,
            focused_request: None,
            label_font: None,
        }
    }

    /// 用新的数据重建图（保留已有节点的坐标——照抄 `buildGraph({keepPositions:true})`）。
    pub fn set_data(&mut self, specs: Vec<NodeSpec>, edges: Vec<(Uuid, Uuid)>) {
        let prev: std::collections::HashMap<Uuid, VNode> =
            self.nodes.iter().map(|n| (n.spec.id, n.clone())).collect();
        let index: std::collections::HashMap<Uuid, usize> =
            specs.iter().enumerate().map(|(i, s)| (s.id, i)).collect();

        let mut nodes = Vec::with_capacity(specs.len());
        for spec in specs {
            let mut v = match prev.get(&spec.id) {
                Some(old) => VNode {
                    spec,
                    x: old.x,
                    y: old.y,
                    fade: old.fade,
                    disp: old.disp,
                    disp_init: old.disp_init,
                    move_text: old.move_text,
                    screen: None,
                },
                None => VNode {
                    spec,
                    x: 0.0,
                    y: 0.0,
                    fade: 0.0,
                    disp: [0.0; 3],
                    disp_init: false,
                    move_text: 0.0,
                    screen: None,
                },
            };
            v.screen = None;
            nodes.push(v);
        }

        self.links = edges
            .iter()
            .filter_map(|(a, b)| {
                let (s, t) = (*index.get(a)?, *index.get(b)?);
                Some(VLink {
                    source: s,
                    target: t,
                    alpha: 0.0,
                    rgb: [0.0; 3],
                    init: false,
                })
            })
            .collect();

        let mut neighbors = vec![Vec::new(); nodes.len()];
        for l in &self.links {
            neighbors[l.source].push(l.target);
            neighbors[l.target].push(l.source);
        }
        // 度数 → 权重（半径用）
        for i in 0..nodes.len() {
            nodes[i].spec.weight = neighbors[i].len();
        }

        self.neighbors = neighbors;
        self.nodes = nodes;
        self.worker.send(Cmd::Nodes {
            ids: self.nodes.iter().map(|n| n.spec.id).collect(),
            weights: self.nodes.iter().map(|n| n.spec.weight).collect(),
            positions: self.nodes.iter().map(|n| (n.x, n.y)).collect(),
        });
        self.worker
            .send(Cmd::Links(self.links.iter().map(|l| (l.source, l.target)).collect()));
        self.worker.send(Cmd::Forces(Forces {
            center: self.opt.center,
            repel: self.opt.repel,
            link: self.opt.link,
            dist: self.opt.dist,
        }));
        self.auto_fit = true;
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    pub fn link_count(&self) -> usize {
        self.links.len()
    }

    pub fn picked(&mut self) -> Option<Uuid> {
        self.picked.take()
    }

    pub fn focused_request(&mut self) -> Option<Uuid> {
        self.focused_request.take()
    }

    pub fn highlighted(&self) -> Option<NodeSpec> {
        self.highlight.map(|i| self.nodes[i].spec.clone())
    }

    /// 参数变了：把力推给线程并唤醒（照抄 `applyForces` + `{forces}` 消息）。
    pub fn push_forces(&mut self) {
        self.worker.send(Cmd::Forces(Forces {
            center: self.opt.center,
            repel: self.opt.repel,
            link: self.opt.link,
            dist: self.opt.dist,
        }));
    }

    /// 重置视图（照抄 `resetView`）：回到自适配缩放、原点居中，并把 pin 清掉。
    pub fn reset_view(&mut self) {
        self.auto_fit = true;
        let s = self.fit_scale();
        self.set_scale(s);
        self.target_scale = s;
        self.pan_x = self.width / 2.0;
        self.pan_y = self.height / 2.0;
        self.panv_x = 0.0;
        self.panv_y = 0.0;
        self.worker.send(Cmd::Wake(1.0));
    }

    fn set_scale(&mut self, scale: f64) {
        self.scale = scale;
        self.text_alpha = text_alpha(scale, self.opt.fade);
    }

    /// 95% 分位半径装进画布（照抄 `fitScale`）
    fn fit_scale(&self) -> f64 {
        if self.nodes.is_empty() {
            return 1.0;
        }
        let mut radii: Vec<f64> = self
            .nodes
            .iter()
            .map(|n| (n.x * n.x + n.y * n.y).sqrt())
            .collect();
        radii.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let idx = ((radii.len().saturating_sub(1)) as f64 * 0.95).floor() as usize;
        let r95 = radii[idx.min(radii.len() - 1)].max(1.0);
        clamp(
            0.46 * self.width.min(self.height) / r95,
            1.0 / 128.0,
            8.0,
        )
    }

    /// 缩放缓动（照抄 `updateZoom`）：每帧走 15%，直到接近目标
    fn update_zoom(&mut self) {
        self.target_scale = clamp(self.target_scale, 1.0 / 128.0, 8.0);
        let cur = self.scale;
        let t = self.target_scale;
        let ratio = if cur > t { cur / t } else { t / cur };
        if ratio - 1.0 >= 0.01 {
            let (mut cx, mut cy) = (self.zoom_center_x, self.zoom_center_y);
            if cx == 0.0 && cy == 0.0 {
                cx = self.width / 2.0;
                cy = self.height / 2.0;
            }
            let wx = (cx - self.pan_x) / cur;
            let wy = (cy - self.pan_y) / cur;
            let next = ru_k(cur, t, 0.85);
            self.pan_x -= wx * next + self.pan_x - cx;
            self.pan_y -= wy * next + self.pan_y - cy;
            self.set_scale(next);
        } else {
            self.set_scale(t);
        }
    }

    fn is_neighbor(&self, node: usize, of: usize) -> bool {
        self.neighbors
            .get(node)
            .map(|ns| ns.contains(&of))
            .unwrap_or(false)
    }

    fn highlight_idx(&self) -> Option<usize> {
        self.drag.or(self.highlight)
    }

    fn color_of(&self, i: usize) -> (Color32, f64) {
        let hi = self.highlight_idx();
        let n = &self.nodes[i];
        if hi == Some(i) {
            return (self.colors.focused, 1.0);
        }
        if n.spec.kind == NodeKind::Focused {
            return (self.colors.focused, 1.0);
        }
        if self.opt.groups {
            if let Some(s) = n.spec.series {
                return (self.colors.series[s % 6], 1.0);
            }
        }
        match n.spec.kind {
            NodeKind::Tag => (self.colors.tag, 1.0),
            NodeKind::Unresolved => (self.colors.unresolved, self.colors.unresolved_a),
            NodeKind::Attachment => (self.colors.attachment, 1.0),
            _ => (self.colors.fill, 1.0),
        }
    }

    fn pick(&self, px: f64, py: f64) -> Option<usize> {
        let mut best: Option<(f64, usize)> = None;
        for (i, n) in self.nodes.iter().enumerate() {
            let Some((sx, sy, _)) = n.screen else { continue };
            let r = get_size(self.opt.node_size, n.spec.weight) * node_scale(self.scale) * self.scale
                + 2.0;
            let dx = sx - px;
            let dy = sy - py;
            let d2 = dx * dx + dy * dy;
            if d2 <= r * r && best.map(|(bd, _)| d2 < bd).unwrap_or(true) {
                best = Some((d2, i));
            }
        }
        best.map(|(_, i)| i)
    }

    /// 一帧：交互 → 物理取帧 → 缓动 → 绘制。
    pub fn ui(&mut self, ui: &mut egui::Ui) {
        let (rect, response) = ui.allocate_exact_size(ui.available_size(), egui::Sense::click_and_drag());
        let painter = ui.painter_at(rect);
        self.width = rect.width() as f64;
        self.height = rect.height() as f64;
        // 参考用设备像素；egui 的点已经按 DPI 换算，这里直接以「点」为画布单位，等价
        if self.pan_x == 0.0 && self.pan_y == 0.0 {
            self.pan_x = self.width / 2.0;
            self.pan_y = self.height / 2.0;
        }
        painter.rect_filled(rect, 0.0, self.colors.bg);
        if self.nodes.is_empty() {
            painter.text(
                rect.center(),
                Align2::CENTER_CENTER,
                "这些文件里没有引用关系（图是空的）",
                FontId::proportional(14.0),
                self.colors.text,
            );
            return;
        }
        let now = ui.input(|i| i.time);

        // —— 交互 ——
        let hover = ui.input(|i| i.pointer.hover_pos());
        let scroll = ui.input(|i| i.raw_scroll_delta.y);
        if response.hovered() && scroll != 0.0 {
            self.auto_fit = false;
            let delta = scroll as f64;
            self.target_scale *= 1.5_f64.powf(-delta / 120.0);
            if self.target_scale < self.scale {
                self.zoom_center_x = 0.0;
                self.zoom_center_y = 0.0;
            } else if let Some(p) = hover {
                self.zoom_center_x = (p.x - rect.left()) as f64;
                self.zoom_center_y = (p.y - rect.top()) as f64;
            }
        }
        let pos = |p: Pos2| ((p.x - rect.left()) as f64, (p.y - rect.top()) as f64);

        if response.drag_started() {
            if let Some(p) = response.interact_pointer_pos() {
                let (px, py) = pos(p);
                self.down = Some((px, py));
                self.drag_moved = false;
                self.last_move = now;
                match self.pick(px, py) {
                    Some(i) => {
                        let (x, y) = (self.nodes[i].x, self.nodes[i].y);
                        self.drag = Some(i);
                        self.highlight = Some(i);
                        self.worker.send(Cmd::Pin {
                            index: i,
                            pos: Some((x, y)),
                        });
                        self.worker.send(Cmd::Wake(0.3));
                        self.worker.send(Cmd::AlphaTarget(0.3));
                    }
                    None => {
                        self.panning = Some((px, py, self.pan_x, self.pan_y));
                        self.auto_fit = false;
                    }
                }
            }
        }
        if response.dragged() {
            if let Some(p) = response.interact_pointer_pos() {
                let (px, py) = pos(p);
                if let Some((dx0, dy0)) = self.down {
                    if (px - dx0).abs() + (py - dy0).abs() > 5.0 {
                        self.drag_moved = true;
                    }
                }
                if let Some(i) = self.drag {
                    let fx = (px - self.pan_x) / self.scale;
                    let fy = (py - self.pan_y) / self.scale;
                    self.nodes[i].x = fx;
                    self.nodes[i].y = fy;
                    self.worker.send(Cmd::Pin {
                        index: i,
                        pos: Some((fx, fy)),
                    });
                    self.worker.send(Cmd::Wake(0.3));
                    self.worker.send(Cmd::AlphaTarget(0.3));
                    self.idle_frames = 0;
                } else if let Some((sx, sy, px0, py0)) = self.panning {
                    let nx = px0 + (px - sx);
                    let ny = py0 + (py - sy);
                    let dt = (now - self.last_move) * 1000.0;
                    self.frame_ms = ru_k(self.frame_ms, dt.max(1.0), 0.8);
                    self.last_move = now;
                    self.panv_x = ru_k(self.panv_x, nx - self.pan_x, 0.8);
                    self.panv_y = ru_k(self.panv_y, ny - self.pan_y, 0.8);
                    self.pan_x = nx;
                    self.pan_y = ny;
                    self.zoom_center_x = 0.0;
                    self.zoom_center_y = 0.0;
                    self.idle_frames = 0;
                }
            }
        }
        if response.drag_stopped() {
            if let Some(i) = self.drag {
                // 松手不钉住：清掉 fx/fy 让它重新参与布局（照抄 endPointer）
                self.worker.send(Cmd::Pin { index: i, pos: None });
                self.worker.send(Cmd::AlphaTarget(0.0));
                if !self.drag_moved {
                    self.picked = Some(self.nodes[i].spec.id);
                    self.focused_request = Some(self.nodes[i].spec.id);
                }
                self.drag = None;
            }
            if self.panning.is_some() {
                let elapsed = (now - self.last_move) * 1000.0;
                if elapsed > 100.0 {
                    self.panv_x = 0.0;
                    self.panv_y = 0.0;
                } else {
                    self.panv_x /= self.frame_ms.max(1.0);
                    self.panv_y /= self.frame_ms.max(1.0);
                }
                self.panning = None;
            }
        }
        if let Some(p) = hover {
            if !response.dragged() {
                let (px, py) = pos(p);
                let h = self.pick(px, py);
                if h != self.highlight {
                    self.highlight = h;
                    self.idle_frames = 0;
                }
            }
        } else if self.highlight.is_some() && !response.dragged() {
            self.highlight = None;
            self.idle_frames = 0;
        }

        // —— 物理帧 ——
        if self.worker.poll(&mut self.nodes) {
            self.idle_frames = 0;
        } else if self.worker.active.load(Ordering::Relaxed) {
            self.idle_frames = 0;
        }
        if self.auto_fit {
            self.target_scale = self.fit_scale();
        }
        // 平移惯性
        if self.panning.is_none() && (self.panv_x.abs() > 1e-4 || self.panv_y.abs() > 1e-4) {
            self.pan_x += 1000.0 * self.panv_x / 60.0;
            self.pan_y += 1000.0 * self.panv_y / 60.0;
            self.panv_x = ru(self.panv_x, 0.0);
            self.panv_y = ru(self.panv_y, 0.0);
            self.idle_frames = 0;
        }
        let zoom_moving = (self.target_scale - self.scale).abs() > 1e-9;
        self.update_zoom();

        self.draw(&painter, rect, now);

        // —— 空闲 60 帧后停止重绘（照抄 idleFrames > 60）——
        let busy = self.idle_frames <= 60
            || zoom_moving
            || self.worker.active.load(Ordering::Relaxed)
            || self.panning.is_some()
            || self.drag.is_some();
        if busy {
            ui.ctx().request_repaint();
        }
        self.idle_frames += 1;
    }

    fn draw(&mut self, painter: &egui::Painter, rect: Rect, _now: f64) {
        let scale = self.scale;
        let (pan_x, pan_y) = (self.pan_x, self.pan_y);
        let ns = node_scale(scale);
        let hi = self.highlight_idx();
        let sx = |x: f64| (x * scale + pan_x) as f32;
        let sy = |y: f64| (y * scale + pan_y) as f32;
        let to_screen = |x: f64, y: f64| Pos2::new(rect.left() + sx(x), rect.top() + sy(y));

        // 1) 淡入淡出（先算目标，避免同时借 self.nodes 与 self）
        let targets: Vec<f64> = (0..self.nodes.len())
            .map(|i| match hi {
                None => 1.0,
                Some(h) if h == i || self.is_neighbor(i, h) => 1.0,
                Some(_) => NU,
            })
            .collect();
        for (i, n) in self.nodes.iter_mut().enumerate() {
            n.fade = ru(n.fade, targets[i]);
        }

        // 2) 连线（分批：颜色 + 1/24 量化透明度）
        let mut batches: std::collections::HashMap<(u8, u8, u8, u8), Vec<[Pos2; 2]>> =
            std::collections::HashMap::new();
        let line_w = self.opt.line_size as f32;
        for k in 0..self.links.len() {
            let (s, t) = (self.links[k].source, self.links[k].target);
            let lit = hi.map(|h| h == s || h == t).unwrap_or(false);
            let target = if hi.is_none() || lit { 1.0 } else { NU };
            self.links[k].alpha = ru(self.links[k].alpha, target);
            let (col, _) = if lit {
                (self.colors.line_highlight, self.colors.line_highlight_a)
            } else {
                (self.colors.line, self.colors.line_a)
            };
            let crgb = [col.r() as f64, col.g() as f64, col.b() as f64];
            let l = &mut self.links[k];
            if !l.init {
                l.rgb = crgb;
                l.init = true;
            }
            for c in 0..3 {
                l.rgb[c] = ru(l.rgb[c], crgb[c]);
            }
            let alpha = l.alpha
                * if lit {
                    self.colors.line_highlight_a
                } else {
                    self.colors.line_a
                };
            if alpha <= 0.002 {
                continue;
            }
            let (nsx, nsy) = (self.nodes[s].x, self.nodes[s].y);
            let (ntx, nty) = (self.nodes[t].x, self.nodes[t].y);
            let dx = ntx - nsx;
            let dy = nty - nsy;
            let dist = (dx * dx + dy * dy).sqrt().max(1e-6);
            let rs = get_size(self.opt.node_size, self.nodes[s].spec.weight) * ns;
            let rt = get_size(self.opt.node_size, self.nodes[t].spec.weight) * ns;
            let x0 = sx(nsx + dx * (rs / dist));
            let y0 = sy(nsy + dy * (rs / dist));
            let length = ((dist - rs - rt) * scale).max(0.0);
            if length <= 0.0 {
                continue;
            }
            let p0 = Pos2::new(rect.left() + x0, rect.top() + y0);
            let p1 = Pos2::new(
                rect.left() + (x0 as f64 + dx / dist * length) as f32,
                rect.top() + (y0 as f64 + dy / dist * length) as f32,
            );
            let key = (
                l.rgb[0] as u8,
                l.rgb[1] as u8,
                l.rgb[2] as u8,
                (alpha * 24.0).round() as u8,
            );
            batches.entry(key).or_default().push([p0, p1]);
        }
        for ((r, g, b, a24), segs) in batches {
            let color =
                Color32::from_rgba_unmultiplied(r, g, b, ((a24 as f64 / 24.0) * 255.0) as u8);
            let shapes: Vec<egui::Shape> = segs
                .into_iter()
                .map(|[p0, p1]| egui::Shape::line_segment([p0, p1], Stroke::new(line_w, color)))
                .collect();
            painter.extend(shapes);
        }

        // 3) 节点
        for i in 0..self.nodes.len() {
            let (col, col_a) = self.color_of(i);
            {
                let n = &mut self.nodes[i];
                let crgb = [col.r() as f64, col.g() as f64, col.b() as f64];
                if !n.disp_init {
                    n.disp = crgb;
                    n.disp_init = true;
                }
                for c in 0..3 {
                    n.disp[c] = ru(n.disp[c], crgb[c]);
                }
            }
            let radius = get_size(self.opt.node_size, self.nodes[i].spec.weight) * ns * scale;
            let center = to_screen(self.nodes[i].x, self.nodes[i].y);
            if center.x < rect.left() - radius as f32
                || center.x > rect.right() + radius as f32
                || center.y < rect.top() - radius as f32
                || center.y > rect.bottom() + radius as f32
            {
                self.nodes[i].screen = None;
                continue;
            }
            let alpha = self.nodes[i].fade * col_a;
            if alpha <= 0.002 {
                self.nodes[i].screen = None;
                continue;
            }
            let n = &self.nodes[i];
            let c = Color32::from_rgba_unmultiplied(
                n.disp[0] as u8,
                n.disp[1] as u8,
                n.disp[2] as u8,
                (alpha * 255.0) as u8,
            );
            painter.circle_filled(center, radius.max(0.5) as f32, c);
            self.nodes[i].screen = Some((center.x as f64, center.y as f64, radius));
        }

        // 4) 高亮环
        if let Some(h) = hi {
            let radius = get_size(self.opt.node_size, self.nodes[h].spec.weight) * ns * scale;
            let lw = (scale.sqrt()).max(1.0);
            let center = to_screen(self.nodes[h].x, self.nodes[h].y);
            painter.circle_stroke(
                center,
                (radius + lw / 2.0) as f32,
                Stroke::new(lw as f32, self.colors.circle),
            );
        }

        // 5) 箭头
        if self.opt.arrows {
            let base = clamp(2.0 * (scale - 0.3), 0.0, 1.0);
            for k in 0..self.links.len() {
                let l = self.links[k].clone();
                if l.alpha <= 0.002 {
                    continue;
                }
                let (s, t) = (l.source, l.target);
                // 双向链接只画一支（源 id 字典序更小时跳过）
                if self.neighbors[t].contains(&s) && self.nodes[s].spec.id.to_string() < self.nodes[t].spec.id.to_string() {
                    continue;
                }
                let dx = self.nodes[t].x - self.nodes[s].x;
                let dy = self.nodes[t].y - self.nodes[s].y;
                let dist = (dx * dx + dy * dy).sqrt().max(1e-6);
                let rt = get_size(self.opt.node_size, self.nodes[t].spec.weight) * ns + 1.0 / scale;
                if dist <= rt {
                    continue;
                }
                let tip_x = sx(self.nodes[t].x) - (dx / dist * (rt * scale)) as f32;
                let tip_y = sy(self.nodes[t].y) - (dy / dist * (rt * scale)) as f32;
                let a = base * l.alpha * self.colors.arrow_a;
                if a <= 0.002 {
                    continue;
                }
                let size = 2.0 * self.opt.line_size.sqrt();
                let ang = dy.atan2(dx);
                let (ca, sa) = (ang.cos(), ang.sin());
                let pts = [(0.0, 0.0), (-4.0, -2.0), (-3.0, 0.0), (-4.0, 2.0)]
                    .iter()
                    .map(|(x, y)| {
                        let (x, y) = (x * size, y * size);
                        Pos2::new(
                            rect.left() + tip_x + (x * ca - y * sa) as f32,
                            rect.top() + tip_y + (x * sa + y * ca) as f32,
                        )
                    })
                    .collect::<Vec<_>>();
                let col = Color32::from_rgba_unmultiplied(
                    self.colors.arrow.r(),
                    self.colors.arrow.g(),
                    self.colors.arrow.b(),
                    (a * 255.0) as u8,
                );
                painter.add(egui::Shape::convex_polygon(
                    pts,
                    col,
                    Stroke::NONE,
                ));
            }
        }

        // 6) 标签
        if self.text_alpha > 0.001 {
            let halo = self.colors.bg;
            for i in 0..self.nodes.len() {
                let is_hi = hi == Some(i);
                let mut alpha = self.text_alpha * self.nodes[i].fade;
                if is_hi {
                    alpha = 1.0;
                }
                alpha *= self.colors.text_a;
                if alpha <= 0.001 {
                    continue;
                }
                let target_move = if is_hi { 15.0 } else { 0.0 };
                self.nodes[i].move_text = ru(self.nodes[i].move_text, target_move);
                let radius = get_size(self.opt.node_size, self.nodes[i].spec.weight) * ns;
                let px = sx(self.nodes[i].x);
                let py = sy(self.nodes[i].y);
                if !is_hi
                    && (px < -300.0
                        || px > rect.width() + 300.0
                        || py < -200.0
                        || py > rect.height() + 200.0)
                {
                    continue;
                }
                let mut font_scale = ns * scale;
                if is_hi && scale < 1.0 {
                    font_scale = 1.0;
                }
                let font_px = (9.0_f64).max((14.0 + get_size(self.opt.node_size, self.nodes[i].spec.weight) / 4.0) * font_scale) as f32;
                let text_y = py + ((radius + 5.0) * scale + self.nodes[i].move_text) as f32;
                let pos = Pos2::new(rect.left() + px, rect.top() + text_y);
                let font = match &self.label_font {
                    Some(name) => FontId::new(font_px, egui::FontFamily::Name(name.clone().into())),
                    None => FontId::proportional(font_px),
                };
                // 光晕：8 向偏移模拟 canvas 的 strokeText（线宽 3 ≈ 半径 1.5）
                let halo_col = Color32::from_rgba_unmultiplied(
                    halo.r(),
                    halo.g(),
                    halo.b(),
                    (0.75 * alpha * 255.0) as u8,
                );
                for (ox, oy) in [
                    (-1.5, 0.0),
                    (1.5, 0.0),
                    (0.0, -1.5),
                    (0.0, 1.5),
                    (-1.1, -1.1),
                    (1.1, -1.1),
                    (-1.1, 1.1),
                    (1.1, 1.1),
                ] {
                    painter.text(
                        pos + Vec2::new(ox, oy),
                        Align2::CENTER_TOP,
                        &self.nodes[i].spec.title,
                        font.clone(),
                        halo_col,
                    );
                }
                let text_col = Color32::from_rgba_unmultiplied(
                    self.colors.text.r(),
                    self.colors.text.g(),
                    self.colors.text.b(),
                    (alpha * 255.0) as u8,
                );
                painter.text(
                    pos,
                    Align2::CENTER_TOP,
                    &self.nodes[i].spec.title,
                    font,
                    text_col,
                );
            }
        }
    }
}

impl Default for GraphView {
    fn default() -> Self {
        Self::new()
    }
}

/// 局部图谱：以 `focus` 为中心的 1 跳（前向 + 反向），中心权重 30（照抄 Obsidian 规则）。
pub fn local_subset(
    nodes: &[NodeSpec],
    edges: &[(Uuid, Uuid)],
    focus: Uuid,
) -> (Vec<NodeSpec>, Vec<(Uuid, Uuid)>) {
    let mut visible: Vec<Uuid> = vec![focus];
    for (a, b) in edges {
        if *a == focus {
            visible.push(*b);
        }
        if *b == focus {
            visible.push(*a);
        }
    }
    visible.sort_by(|a, b| a.to_string().cmp(&b.to_string()));
    visible.dedup();
    let subset: Vec<NodeSpec> = nodes
        .iter()
        .filter(|n| visible.contains(&n.id))
        .map(|n| {
            let mut s = n.clone();
            if s.id == focus {
                s.weight = 30;
                s.kind = NodeKind::Focused;
            } else {
                s.weight = 0;
            }
            s
        })
        .collect();
    let keep: Vec<Uuid> = subset.iter().map(|n| n.id).collect();
    let edges = edges
        .iter()
        .filter(|(a, b)| {
            (keep.contains(a) && keep.contains(b)) && (*a == focus || *b == focus)
        })
        .cloned()
        .collect();
    (subset, edges)
}
