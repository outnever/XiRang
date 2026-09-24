//! 物理引擎：Obsidian `sim.js`（d3-force 移植）的逐行 Rust 版本。
//!
//! 常数全部来自参考实现（它又是从 Obsidian 1.9.x 的 `app.asar` 里读出来的）。
//! 每步固定顺序：中心 → 连线 → 排斥（Barnes–Hut）→ 碰撞 → 积分。
//! `jiggle` 走可注入的确定性 RNG，因此可以与 JS 参考逐位对拍。

use crate::obsidian::quadtree::{QNode, Quadtree};
use xirang_core::codec::Uuid;

pub const ALPHA_MIN: f64 = 0.001;
pub const THETA2: f64 = 0.81;
pub const DISTANCE_MIN2: f64 = 900.0;
pub const COLLIDE_RADIUS: f64 = 60.0;
pub const COLLIDE_STRENGTH: f64 = 0.5;
pub const VELOCITY_DECAY: f64 = 0.6;

/// `alphaDecay = 1 − 0.001^(1/300)`（照抄，不取近似）。
pub fn alpha_decay() -> f64 {
    1.0 - 0.001_f64.powf(1.0 / 300.0)
}

/// `jiggle()` 用的确定性随机源（Numerical Recipes LCG），与 JS 侧同一序列。
#[derive(Clone, Debug)]
pub struct Lcg(u32);

impl Lcg {
    pub fn new(seed: u32) -> Self {
        Lcg(seed)
    }
    pub fn next_f64(&mut self) -> f64 {
        self.0 = self.0.wrapping_mul(1664525).wrapping_add(1013904223);
        (self.0 as f64) / 4294967296.0
    }
    pub fn jiggle(&mut self) -> f64 {
        (self.next_f64() - 0.5) * 1e-6
    }
}

#[derive(Clone, Debug, Default)]
pub struct SimNode {
    pub x: f64,
    pub y: f64,
    pub vx: f64,
    pub vy: f64,
    pub fx: Option<f64>,
    pub fy: Option<f64>,
    /// 度数权重（半径 = clamp(3√(weight+1), 8, 30)）
    pub weight: usize,
    // —— 碰撞用的临时量（每帧算）——
    r: f64,
    r2: f64,
    xi: f64,
    yi: f64,
}

#[derive(Clone, Debug)]
pub struct SimLink {
    pub source: usize,
    pub target: usize,
    pub distance: f64,
    pub strength: f64,
    pub bias: f64,
}

#[derive(Clone, Debug)]
pub struct Physics {
    /// 节点编号（重建时按编号复用旧坐标 / 速度 / 钉住状态）
    pub ids: Vec<Uuid>,
    pub nodes: Vec<SimNode>,
    pub links: Vec<SimLink>,
    pub alpha: f64,
    pub alpha_target: f64,
    pub center_strength: f64,
    pub link_strength: f64,
    pub link_distance: f64,
    /// 内部存**负值**（参考里是 `repelStrength = -|v|`）
    pub repel_strength: f64,
    pub rng: Lcg,
    charge_alpha: f64,
    current: usize,
}

impl Default for Physics {
    fn default() -> Self {
        Physics {
            ids: Vec::new(),
            nodes: Vec::new(),
            links: Vec::new(),
            alpha: 1.0,
            alpha_target: 0.0,
            center_strength: 0.1,
            link_strength: 1.0,
            link_distance: 250.0,
            repel_strength: -1000.0,
            rng: Lcg::new(1),
            charge_alpha: 0.0,
            current: 0,
        }
    }
}

impl Physics {
    pub fn new() -> Self {
        Self::default()
    }

    /// 设节点：**保留已有坐标 / 速度 / 钉住状态**（等价于参考的 `setNodes` 复用旧对象），
    /// 新节点从 (0,0) 起步（参考里 `x: old ? old.x : 0`）。
    pub fn set_nodes(&mut self, ids: &[Uuid], weights: &[usize]) {
        let old: Vec<(Uuid, SimNode)> = self
            .ids
            .iter()
            .cloned()
            .zip(std::mem::take(&mut self.nodes))
            .collect();
        let mut next = Vec::with_capacity(weights.len());
        for (i, id) in ids.iter().enumerate() {
            let mut n = old
                .iter()
                .find(|(oid, _)| oid == id)
                .map(|(_, n)| n.clone())
                .unwrap_or_default();
            n.weight = weights[i];
            next.push(n);
        }
        self.ids = ids.to_vec();
        self.nodes = next;
    }

    /// 覆盖初始坐标（测试 / 重建视图时用）。
    pub fn set_positions(&mut self, positions: &[(f64, f64)]) {
        for (i, p) in positions.iter().enumerate() {
            if let Some(n) = self.nodes.get_mut(i) {
                n.x = p.0;
                n.y = p.1;
            }
        }
    }

    /// 设边并算权重：`strength = linkStrength / min(度A, 度B)`、`bias = 度A / (度A + 度B)`。
    pub fn set_links(&mut self, pairs: &[(usize, usize)]) {
        self.links = pairs
            .iter()
            .filter(|(s, t)| *s < self.nodes.len() && *t < self.nodes.len())
            .map(|(s, t)| SimLink {
                source: *s,
                target: *t,
                distance: self.link_distance,
                strength: self.link_strength,
                bias: 0.5,
            })
            .collect();
        self.recompute_link_weights();
    }

    pub fn recompute_link_weights(&mut self) {
        let n = self.nodes.len();
        let mut count = vec![0usize; n];
        for l in &self.links {
            count[l.source] += 1;
            count[l.target] += 1;
        }
        for i in 0..self.links.len() {
            let (cs, ct) = (count[self.links[i].source], count[self.links[i].target]);
            self.links[i].strength = self.link_strength / cs.min(ct).max(1) as f64;
            self.links[i].bias = cs as f64 / (cs + ct).max(1) as f64;
            self.links[i].distance = self.link_distance;
        }
    }

    pub fn set_repel(&mut self, v: f64) {
        self.repel_strength = -v.abs().max(1.0);
    }

    /// 参数变化后唤醒（`alpha` 只升不降——参考里是 `if (alpha < msg.alpha) alpha = msg.alpha`）。
    pub fn wake(&mut self, alpha: f64) {
        if self.alpha < alpha {
            self.alpha = alpha;
        }
    }

    /// 一帧：先衰减 alpha，再跑五个力（照抄 `tick`）。
    pub fn tick(&mut self) -> bool {
        if self.alpha <= ALPHA_MIN && self.alpha_target <= 0.0 {
            return false;
        }
        self.alpha += (self.alpha_target - self.alpha) * alpha_decay();
        self.step();
        true
    }

    pub fn step(&mut self) {
        let a = self.alpha;
        self.apply_center(a);
        self.apply_link(a);
        self.apply_many_body(a);
        self.apply_collide_force();
        for n in self.nodes.iter_mut() {
            match (n.fx, n.fy) {
                (Some(fx), Some(fy)) => {
                    n.x = fx;
                    n.y = fy;
                    n.vx = 0.0;
                    n.vy = 0.0;
                }
                _ => {
                    n.vx *= VELOCITY_DECAY;
                    n.vy *= VELOCITY_DECAY;
                    n.x += n.vx;
                    n.y += n.vy;
                }
            }
        }
    }

    // —— 力 1/2：中心 ——
    fn apply_center(&mut self, a: f64) {
        let s = self.center_strength;
        for n in self.nodes.iter_mut() {
            n.vx += (0.0 - n.x) * s * a;
            n.vy += (0.0 - n.y) * s * a;
        }
    }

    // —— 力 3：连线弹簧 ——
    fn apply_link(&mut self, a: f64) {
        for i in 0..self.links.len() {
            let l = self.links[i].clone();
            let mut x = self.nodes[l.target].x + self.nodes[l.target].vx
                - self.nodes[l.source].x
                - self.nodes[l.source].vx;
            let mut y = self.nodes[l.target].y + self.nodes[l.target].vy
                - self.nodes[l.source].y
                - self.nodes[l.source].vy;
            // JS 的 `|| jiggle()`：0 与 NaN 都是「假」
            if x == 0.0 || x.is_nan() {
                x = self.rng.jiggle();
            }
            if y == 0.0 || y.is_nan() {
                y = self.rng.jiggle();
            }
            let dist = (x * x + y * y).sqrt();
            let k = (dist - l.distance) / dist * a * l.strength;
            x *= k;
            y *= k;
            self.nodes[l.target].vx -= x * l.bias;
            self.nodes[l.target].vy -= y * l.bias;
            self.nodes[l.source].vx += x * (1.0 - l.bias);
            self.nodes[l.source].vy += y * (1.0 - l.bias);
        }
    }

    // —— 力 4：排斥（Barnes–Hut）——
    fn apply_many_body(&mut self, a: f64) {
        let mut tree = build_tree(&self.nodes);
        let pos: Vec<(f64, f64)> = self.nodes.iter().map(|n| (n.x, n.y)).collect();
        let repel = self.repel_strength;
        tree.visit_after(|nodes, idx| accumulate(nodes, idx, &pos, repel));

        self.charge_alpha = a;
        for i in 0..self.nodes.len() {
            self.current = i;
            let mut vx = self.nodes[i].vx;
            let mut vy = self.nodes[i].vy;
            let cur = (self.nodes[i].x, self.nodes[i].y);
            let (repel, alpha, current, rng) = (
                self.repel_strength,
                self.charge_alpha,
                self.current,
                &mut self.rng,
            );
            tree.visit_readonly(|idx, x0, y0, x1, y1| {
                apply_charge(
                    &tree.nodes, idx, x0, y0, x1, y1, cur, repel, alpha, current, &mut vx, &mut vy, rng,
                )
            });
            self.nodes[i].vx = vx;
            self.nodes[i].vy = vy;
        }
    }

    // —— 力 5：碰撞 ——
    fn apply_collide_force(&mut self) {
        let mut tree = build_tree(&self.nodes);
        tree.visit_after(|nodes, idx| {
            if nodes[idx].data.is_some() {
                nodes[idx].r = COLLIDE_RADIUS;
            } else {
                let kids = nodes[idx].children;
                let mut r = 0.0_f64;
                for c in kids.iter().flatten() {
                    if nodes[*c].r > r {
                        r = nodes[*c].r;
                    }
                }
                nodes[idx].r = r;
            }
        });

        let radius = COLLIDE_RADIUS;
        for i in 0..self.nodes.len() {
            // 当前节点：r / r2 / xi / yi（照抄 applyCollideForce 的循环体）
            self.nodes[i].r = radius;
            self.nodes[i].r2 = radius * radius;
            self.nodes[i].xi = self.nodes[i].x + self.nodes[i].vx;
            self.nodes[i].yi = self.nodes[i].y + self.nodes[i].vy;
            self.current = i;
            let cur = (self.nodes[i].xi, self.nodes[i].yi, radius, radius * radius);
            let mut vx = self.nodes[i].vx;
            let mut vy = self.nodes[i].vy;
            let mut deltas: Vec<(usize, f64, f64)> = Vec::new();
            {
                let rng = &mut self.rng;
                let nodes = &self.nodes;
                tree.visit_readonly(|idx, x0, y0, x1, y1| {
                    apply_collide(
                        &tree.nodes,
                        idx,
                        x0,
                        y0,
                        x1,
                        y1,
                        i,
                        cur,
                        nodes,
                        &mut vx,
                        &mut vy,
                        &mut deltas,
                        rng,
                    )
                });
            }
            for (j, dx, dy) in deltas {
                self.nodes[j].vx -= dx;
                self.nodes[j].vy -= dy;
            }
            self.nodes[i].vx = vx;
            self.nodes[i].vy = vy;
        }
    }
}

/// 建树：先 `cover` 出包围盒，再逐个 `add`（照抄 `buildTree`）。
pub fn build_tree(nodes: &[SimNode]) -> Quadtree {
    let mut tree = Quadtree::new();
    let (mut x0, mut y0, mut x1, mut y1) = (
        f64::INFINITY,
        f64::INFINITY,
        f64::NEG_INFINITY,
        f64::NEG_INFINITY,
    );
    for n in nodes {
        if n.x.is_nan() || n.y.is_nan() {
            continue;
        }
        x0 = x0.min(n.x);
        x1 = x1.max(n.x);
        y0 = y0.min(n.y);
        y1 = y1.max(n.y);
    }
    if x0 > x1 || y0 > y1 {
        return tree;
    }
    tree.cover(x0, y0);
    tree.cover(x1, y1);
    let pos: Vec<(f64, f64)> = nodes.iter().map(|n| (n.x, n.y)).collect();
    for (i, n) in nodes.iter().enumerate() {
        tree.add(i, n.x, n.y, &pos);
    }
    tree
}

/// 自底向上汇总（照抄 `accumulate`）：内部节点算加权质心，叶子顺着 `next` 链累加电荷。
fn accumulate(nodes: &mut [QNode], idx: usize, pos: &[(f64, f64)], repel: f64) {
    if nodes[idx].data.is_none() {
        let mut strength = 0.0;
        let mut weight = 0.0;
        let mut x = 0.0;
        let mut y = 0.0;
        let kids = nodes[idx].children;
        for c in kids.iter().flatten() {
            let value = nodes[*c].value;
            let cw = value.abs();
            if cw != 0.0 {
                strength += value;
                weight += cw;
                x += cw * nodes[*c].cx;
                y += cw * nodes[*c].cy;
            }
        }
        nodes[idx].cx = x / weight;
        nodes[idx].cy = y / weight;
        nodes[idx].value = strength;
    } else {
        let data = nodes[idx].data.unwrap();
        nodes[idx].cx = pos[data].0;
        nodes[idx].cy = pos[data].1;
        let mut strength = 0.0;
        let mut cur = Some(idx);
        while let Some(c) = cur {
            strength += repel;
            cur = nodes[c].next;
        }
        nodes[idx].value = strength;
    }
}

/// Barnes–Hut 判据 + 单点受力（照抄 `applyCharge`）；返回 `true` = 这一格到此为止。
#[allow(clippy::too_many_arguments)]
fn apply_charge(
    tree: &[QNode],
    idx: usize,
    x0: f64,
    y0: f64,
    x1: f64,
    y1: f64,
    cur: (f64, f64),
    repel: f64,
    charge_alpha: f64,
    current: usize,
    vx: &mut f64,
    vy: &mut f64,
    rng: &mut Lcg,
) -> bool {
    let q = &tree[idx];
    if q.value == 0.0 || q.value.is_nan() {
        return true;
    }
    let mut x = q.cx - cur.0;
    let mut y = q.cy - cur.1;
    let w = x1 - x0;
    let mut l = x * x + y * y;
    if w * w / THETA2 < l {
        if x == 0.0 {
            x = rng.jiggle();
            l += x * x;
        }
        if y == 0.0 {
            y = rng.jiggle();
            l += y * y;
        }
        if l < DISTANCE_MIN2 {
            l = (DISTANCE_MIN2 * l).sqrt();
        }
        *vx += x * q.value * charge_alpha / l;
        *vy += y * q.value * charge_alpha / l;
        return true;
    }
    // 不够远：若是叶子，就顺着 `next` 链逐个施力
    if q.data.is_none() {
        return false;
    }
    let is_current = q.data == Some(current);
    if !is_current || q.next.is_some() {
        if x == 0.0 {
            x = rng.jiggle();
            l += x * x;
        }
        if y == 0.0 {
            y = rng.jiggle();
            l += y * y;
        }
        if l < DISTANCE_MIN2 {
            l = (DISTANCE_MIN2 * l).sqrt();
        }
    }
    let mut node = Some(idx);
    while let Some(c) = node {
        if tree[c].data != Some(current) {
            let wgt = repel * charge_alpha / l;
            *vx += x * wgt;
            *vy += y * wgt;
        }
        node = tree[c].next;
    }
    let _ = (y0, y1);
    false
}

/// 碰撞（照抄 `applyCollide`）：返回 `true` = 剪掉这一格。
/// `deltas` 收集「对方节点」的受力，遍历结束后再统一写回（避免同时借两个节点）。
#[allow(clippy::too_many_arguments)]
fn apply_collide(
    tree: &[QNode],
    idx: usize,
    x0: f64,
    y0: f64,
    x1: f64,
    y1: f64,
    current: usize,
    cur: (f64, f64, f64, f64),
    nodes: &[SimNode],
    vx: &mut f64,
    vy: &mut f64,
    deltas: &mut Vec<(usize, f64, f64)>,
    rng: &mut Lcg,
) -> bool {
    let (cur_xi, cur_yi, cur_r, cur_r2) = cur;
    let r = cur_r + tree[idx].r;
    let Some(data) = tree[idx].data else {
        return x0 > cur_xi + r || x1 < cur_xi - r || y0 > cur_yi + r || y1 < cur_yi - r;
    };
    if data <= current {
        return false;
    }
    let mut x = cur_xi - (nodes[data].x + nodes[data].vx);
    let mut y = cur_yi - (nodes[data].y + nodes[data].vy);
    let mut l = x * x + y * y;
    if l < r * r {
        if x == 0.0 {
            x = rng.jiggle();
            l += x * x;
        }
        if y == 0.0 {
            y = rng.jiggle();
            l += y * y;
        }
        let d = l.sqrt();
        let k = (r - d) / d * COLLIDE_STRENGTH;
        x *= k;
        y *= k;
        let qr = tree[idx].r;
        let share = qr * qr / (cur_r2 + qr * qr);
        *vx += x * share;
        *vy += y * share;
        deltas.push((data, x * (1.0 - share), y * (1.0 - share)));
    }
    false
}

// ===========================================================================
// 滑杆 → 引擎值（照抄参考里的 h1 指数映射）
// ===========================================================================

/// `h1(v, t) = (t^(1−v) − t) / (1 − t)`：滑杆低区间精度更高，默认 0.5187 → 0.1。
pub fn h1(v: f64, t: f64) -> f64 {
    (t.powf(1.0 - v) - t) / (1.0 - t)
}

/// UI 参数（与参考面板一一对应）。
#[derive(Clone, Debug, PartialEq)]
pub struct Forces {
    /// 中心引力滑杆（0–1，默认 0.5187）
    pub center: f64,
    /// 排斥力滑杆（0–20，默认 10）
    pub repel: f64,
    /// 连线拉力滑杆（0–1，默认 1）
    pub link: f64,
    /// 连线长度（默认 250）
    pub dist: f64,
}

impl Default for Forces {
    fn default() -> Self {
        Forces {
            center: 0.5187,
            repel: 10.0,
            link: 1.0,
            dist: 250.0,
        }
    }
}

impl Forces {
    pub fn apply(&self, p: &mut Physics) {
        p.center_strength = h1(self.center, 0.01).max(0.01);
        p.link_strength = h1(self.link, 0.01).max(0.01);
        p.set_repel(self.repel.powi(3));
        p.link_distance = self.dist;
        p.recompute_link_weights();
    }
}
