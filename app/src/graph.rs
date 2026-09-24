//! 引用图：力导向布局（参数默认值照搬 Obsidian 关系图谱）+ 聚焦态树布局。
//!
//! 这里不依赖 egui：建图、力模拟、标签淡入、聚焦目标位置都能在没有界面的情况下测试。

use std::collections::HashMap;

use xirang_core::codec::{Node, Uuid};

use crate::lazy::Doc;

/// 图设置。默认值取自 Obsidian 关系图谱（从它的安装包里实测出来的）。
#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    /// 图谱向心力（Obsidian 默认 0.1）
    pub center_strength: f32,
    /// 节点间的排斥力（Obsidian 默认 10；内部按立方响应）
    pub repel_strength: f32,
    /// 相连节点间的吸引力（默认 1）
    pub link_strength: f32,
    /// 连线长度（默认 250）
    pub link_distance: f32,
    /// 文字淡入阈值（默认 0，范围 −3…3：负值更早显示名字）
    pub text_fade_multiplier: f32,
    /// 节点大小（默认 1，按度数放大）
    pub node_size_multiplier: f32,
    /// 连线粗细（默认 1）
    pub line_size_multiplier: f32,
    /// 放大后显示箭头（默认关）
    pub show_arrow: bool,
    /// 播放生长动画（默认关）
    pub animate: bool,
    /// 显示孤立节点（默认关：只画有连接关系的节点）
    pub show_orphans: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            center_strength: 0.1,
            repel_strength: 10.0,
            link_strength: 1.0,
            link_distance: 250.0,
            text_fade_multiplier: 0.0,
            node_size_multiplier: 1.0,
            line_size_multiplier: 1.0,
            show_arrow: false,
            animate: false,
            show_orphans: false,
        }
    }
}

impl Settings {
    pub fn reset(&mut self) {
        *self = Settings::default();
    }

    /// 节点半径：基础 3 px + 按度数的平方根增长（与 Obsidian 的「节点大小」一致）。
    pub fn radius(&self, degree: usize) -> f32 {
        (3.0 + (degree as f32).sqrt() * 1.8) * self.node_size_multiplier
    }

    /// 标签开始出现的缩放阈值：阈值越大越晚出现。
    /// 照 Obsidian 的语义：负值 = 更早显示名字（阈值更小），正值 = 要放更大才显示。
    pub fn label_threshold(&self) -> f32 {
        (2f32.powf(self.text_fade_multiplier)).clamp(0.05, 20.0)
    }

    /// 某个缩放下标签的透明度（0 = 不显示名字，只在悬停时显示）。
    pub fn label_alpha(&self, scale: f32) -> f32 {
        let t = scale / self.label_threshold();
        ((t - 0.6) / 0.4).clamp(0.0, 1.0)
    }
}

/// 图上的一个节点（位置与速度是模拟状态）。
#[derive(Clone, Debug)]
pub struct GraphNode {
    pub id: Uuid,
    pub name: String,
    pub file: String,
    pub degree: usize,
    /// 辅助节点（`@` 开头）——画成另一种颜色。
    pub aux: bool,
    pub pos: [f32; 2],
    pub vel: [f32; 2],
    pub fixed: bool,
}

pub struct Graph {
    pub nodes: Vec<GraphNode>,
    pub edges: Vec<(usize, usize)>,
    index: HashMap<Uuid, usize>,
    /// d3 式的「活跃度」：0 = 静止，拖动时抬高。
    pub alpha: f32,
    pub alpha_target: f32,
    pub settings: Settings,
}

/// 每个节点最多和多少个「抽样邻居」算排斥 / 碰撞（大图下行避免 O(N²)）。
const SAMPLE_LIMIT: usize = 64;
/// 超过这个规模就切抽样模式。
const FULL_PAIR_LIMIT: usize = 600;

impl Graph {
    pub fn new(settings: Settings) -> Self {
        Graph {
            nodes: Vec::new(),
            edges: Vec::new(),
            index: HashMap::new(),
            alpha: 1.0,
            alpha_target: 0.0,
            settings,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn wake(&mut self) {
        self.alpha = self.alpha.max(0.8);
    }

    /// 开始拖动 / 松手（对应 Obsidian 拖拽时 `alphaTarget 0.3`）。
    pub fn set_dragging(&mut self, dragging: bool) {
        self.alpha_target = if dragging { 0.3 } else { 0.0 };
        if dragging {
            self.alpha = self.alpha.max(0.5);
        }
    }

    pub fn index_of(&self, id: Uuid) -> Option<usize> {
        self.index.get(&id).copied()
    }

    fn push(&mut self, id: Uuid, name: String, file: String, aux: bool) -> usize {
        if let Some(i) = self.index.get(&id) {
            return *i;
        }
        let i = self.nodes.len();
        // 初始位置：按编号散在一个圆上（确定性，重启后布局稳定）
        let h = fnv(id);
        let angle = (h % 3600) as f32 / 3600.0 * std::f32::consts::TAU;
        let radius = 120.0 + (h % 600) as f32;
        self.nodes.push(GraphNode {
            id,
            name,
            file,
            degree: 0,
            aux,
            pos: [angle.cos() * radius, angle.sin() * radius],
            vel: [0.0, 0.0],
            fixed: false,
        });
        self.index.insert(id, i);
        i
    }

    /// 建图：直接摆引用边（源 → 目标），两端都要能解析出名字。
    ///
    /// `edges` 应当来自实现层的索引（`Sidecar::all_edges`），而不是"当前展开了哪些节点"——
    /// 深层的引用才不会漏。`resolve` 可以跨多个已打开的文件取节点，所以跨文件引用也连得上。
    pub fn build(
        &mut self,
        edges: &[(Uuid, Uuid)],
        show_aux: bool,
        resolve: &mut dyn FnMut(Uuid) -> Option<(Node, String)>,
    ) {
        self.nodes.clear();
        self.edges.clear();
        self.index.clear();

        let mut local: Vec<(usize, usize)> = Vec::new();
        for (from, to) in edges {
            let (Some((a, af)), Some((b, bf))) = (resolve(*from), resolve(*to)) else {
                continue;
            };
            if !show_aux && (a.name.starts_with('@') || b.name.starts_with('@')) {
                continue;
            }
            let ia = self.push(a.id, a.name.clone(), af, a.name.starts_with('@'));
            let ib = self.push(b.id, b.name.clone(), bf, b.name.starts_with('@'));
            local.push(if ia <= ib { (ia, ib) } else { (ib, ia) });
            self.nodes[ia].degree += 1;
            self.nodes[ib].degree += 1;
        }
        local.sort_unstable();
        local.dedup();
        self.edges = local;
        self.alpha = if self.settings.animate { 0.0 } else { 1.0 };
    }

    /// 孤立节点：把「没有任何引用边」的候选节点也画上（默认关）。
    pub fn add_orphans(
        &mut self,
        candidates: &[(Uuid, String)],
        show_aux: bool,
        resolve: &mut dyn FnMut(Uuid) -> Option<(Node, String)>,
    ) {
        for (id, file) in candidates {
            if self.index.contains_key(id) {
                continue;
            }
            if let Some((n, nfile)) = resolve(*id) {
                if !show_aux && n.name.starts_with('@') {
                    continue;
                }
                self.push(
                    n.id,
                    n.name.clone(),
                    if nfile.is_empty() { file.clone() } else { nfile },
                    n.name.starts_with('@'),
                );
            }
        }
    }

    /// 一帧的力模拟：`iterations` 次迭代。返回是否还在动（静止后可停算省电 / 省内存）。
    pub fn step(&mut self, iterations: usize) -> bool {
        let n = self.nodes.len();
        if n == 0 {
            return false;
        }
        if self.alpha < 0.005 && self.alpha_target == 0.0 {
            self.alpha = 0.0;
            return false;
        }
        for _ in 0..iterations {
            self.iter_once();
            // d3 式衰减：alpha 逐步逼近 alpha_target
            self.alpha += (self.alpha_target - self.alpha) * 0.028;
            if self.alpha < 0.005 && self.alpha_target == 0.0 {
                self.alpha = 0.0;
                break;
            }
        }
        true
    }

    fn iter_once(&mut self) {
        let n = self.nodes.len();
        let a = self.alpha;
        let s = self.settings.clone();

        // 1) 相连节点间的吸引力（弹簧到 link_distance）
        for &(i, j) in &self.edges {
            let dx = self.nodes[j].pos[0] - self.nodes[i].pos[0];
            let dy = self.nodes[j].pos[1] - self.nodes[i].pos[1];
            let dist = (dx * dx + dy * dy).sqrt().max(0.01);
            let diff = (dist - s.link_distance) / dist;
            let f = diff * s.link_strength * a * 0.02;
            let (fx, fy) = (dx * f, dy * f);
            self.nodes[i].vel[0] += fx;
            self.nodes[i].vel[1] += fy;
            self.nodes[j].vel[0] -= fx;
            self.nodes[j].vel[1] -= fy;
        }

        // 2) 节点间的排斥力：滑块按立方响应（照搬 Obsidian），负值 = 排斥
        let charge = -s.repel_strength.max(1.0).powi(3) * 0.0009;
        if n <= FULL_PAIR_LIMIT {
            for i in 0..n {
                for j in (i + 1)..n {
                    self.repel_pair(i, j, charge, a);
                }
            }
        } else {
            // 大图：每个节点只和固定的一组「抽样邻居」算，视觉上够用、开销线性
            for i in 0..n {
                for k in 1..=SAMPLE_LIMIT {
                    let j = (i + k * 7) % n;
                    if i != j {
                        self.repel_pair(i, j, charge, a);
                    }
                }
            }
        }

        // 3) 向心力：把整张图拉向原点
        let cf = s.center_strength * a * 0.02;
        for node in self.nodes.iter_mut() {
            node.vel[0] -= node.pos[0] * cf;
            node.vel[1] -= node.pos[1] * cf;
        }

        // 4) 防重叠（碰撞）：只在靠得太近时推开
        if n <= FULL_PAIR_LIMIT * 2 {
            for i in 0..n {
                for j in (i + 1)..n {
                    self.collide_pair(i, j);
                }
            }
        }

        // 5) 积分 + 阻尼
        for node in self.nodes.iter_mut() {
            if node.fixed {
                node.vel = [0.0, 0.0];
                continue;
            }
            node.vel[0] *= 0.85;
            node.vel[1] *= 0.85;
            node.pos[0] += node.vel[0];
            node.pos[1] += node.vel[1];
        }
    }

    fn repel_pair(&mut self, i: usize, j: usize, charge: f32, alpha: f32) {
        let dx = self.nodes[j].pos[0] - self.nodes[i].pos[0];
        let dy = self.nodes[j].pos[1] - self.nodes[i].pos[1];
        let d2 = (dx * dx + dy * dy).max(4.0);
        let dist = d2.sqrt();
        let f = charge / d2 * alpha;
        let (fx, fy) = (dx / dist * f, dy / dist * f);
        self.nodes[i].vel[0] += fx;
        self.nodes[i].vel[1] += fy;
        self.nodes[j].vel[0] -= fx;
        self.nodes[j].vel[1] -= fy;
    }

    fn collide_pair(&mut self, i: usize, j: usize) {
        let ri = self.settings.radius(self.nodes[i].degree);
        let rj = self.settings.radius(self.nodes[j].degree);
        let min = ri + rj + 2.0;
        let dx = self.nodes[j].pos[0] - self.nodes[i].pos[0];
        let dy = self.nodes[j].pos[1] - self.nodes[i].pos[1];
        let dist = (dx * dx + dy * dy).sqrt().max(0.01);
        if dist >= min {
            return;
        }
        let push = (min - dist) * 0.5;
        let (ux, uy) = (dx / dist, dy / dist);
        if !self.nodes[i].fixed {
            self.nodes[i].pos[0] -= ux * push;
            self.nodes[i].pos[1] -= uy * push;
        }
        if !self.nodes[j].fixed {
            self.nodes[j].pos[0] += ux * push;
            self.nodes[j].pos[1] += uy * push;
        }
    }

    /// 聚焦：把焦点子树按树布局排开（横向缩进），其余节点留在力布局里当背景。
    ///
    /// `subtree` = (节点, 层号)，按 DFS 顺序；返回「目标位置」，调用方逐帧插值过去。
    pub fn focus_targets(
        &self,
        subtree: &[(Uuid, usize)],
        origin: [f32; 2],
        gap_x: f32,
        gap_y: f32,
    ) -> HashMap<Uuid, [f32; 2]> {
        let mut out = HashMap::new();
        let mid = subtree.len() as f32 / 2.0;
        for (row, (id, depth)) in subtree.iter().enumerate() {
            out.insert(
                *id,
                [origin[0] + *depth as f32 * gap_x, origin[1] + (row as f32 - mid) * gap_y],
            );
        }
        out
    }

    /// 把节点朝目标位置推进（过渡动画用），返回是否还有节点在移动。
    pub fn move_towards(&mut self, targets: &HashMap<Uuid, [f32; 2]>, t: f32) -> bool {
        let mut moving = false;
        for node in self.nodes.iter_mut() {
            if let Some(target) = targets.get(&node.id) {
                let dx = target[0] - node.pos[0];
                let dy = target[1] - node.pos[1];
                if dx.abs() > 0.5 || dy.abs() > 0.5 {
                    moving = true;
                }
                node.pos[0] += dx * t;
                node.pos[1] += dy * t;
                node.vel = [0.0, 0.0];
            }
        }
        moving
    }

    /// 焦点子树成员：沿父链上溯，命中焦点即算在内（返回 (节点, 相对层号)，DFS 顺序）。
    pub fn subtree(doc: &mut Doc, focus: Uuid, members: &[Uuid]) -> Vec<(Uuid, usize)> {
        let mut out = Vec::new();
        for id in members {
            if *id == focus {
                out.push((*id, 0));
                continue;
            }
            let mut cur = *id;
            let mut depth = 0usize;
            let mut guard = 0;
            while guard < 512 {
                guard += 1;
                match doc.node(cur).and_then(|n| n.parent) {
                    Some(p) => {
                        depth += 1;
                        if p == focus {
                            out.push((*id, depth));
                            break;
                        }
                        cur = p;
                    }
                    None => break,
                }
            }
        }
        out.sort_by_key(|(_, depth)| *depth);
        out
    }
}

fn fnv(id: Uuid) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in id.0 {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}
