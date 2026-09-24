//! d3-quadtree 的移植（`cover` / `add` / `visit` / `visitAfter`）。
//!
//! 与参考实现（`obsidian-graph-lab.source.html` 行 124–203）逐行对应：
//! 节点的四个槽位、重合点串成 `next` 链表、`visit` 的自定义栈顺序（3,2,1,0 入栈）、
//! `visitAfter` 的两段式（先父后子入队、再逆序回调）都保持一致——**顺序不能改**，
//! 否则力的累加顺序变了，浮点结果就不再逐位相同。

/// 四叉树节点。
#[derive(Clone, Debug, Default)]
pub struct QNode {
    /// 内部节点的四个孩子（0: 左上 · 1: 右上 · 2: 左下 · 3: 右下）
    pub children: [Option<usize>; 4],
    /// 叶子：数据下标
    pub data: Option<usize>,
    /// 重合点链表
    pub next: Option<usize>,
    /// 引力累加用的缓存：质心与「电荷」
    pub cx: f64,
    pub cy: f64,
    pub value: f64,
    /// 碰撞用的子树最大半径
    pub r: f64,
}

impl QNode {
    pub fn is_inner(&self) -> bool {
        self.data.is_none()
    }
}

#[derive(Clone, Debug)]
pub struct Quadtree {
    pub x0: f64,
    pub y0: f64,
    pub x1: f64,
    pub y1: f64,
    pub nodes: Vec<QNode>,
    pub root: Option<usize>,
}

impl Default for Quadtree {
    fn default() -> Self {
        Quadtree {
            x0: f64::NAN,
            y0: f64::NAN,
            x1: f64::NAN,
            y1: f64::NAN,
            nodes: Vec::new(),
            root: None,
        }
    }
}

impl Quadtree {
    pub fn new() -> Self {
        Self::default()
    }

    fn push(&mut self, node: QNode) -> usize {
        self.nodes.push(node);
        self.nodes.len() - 1
    }

    /// 把范围扩到能装下 `(x, y)`。与 d3 的 `cover` 一致：必要时在根上再包一层。
    pub fn cover(&mut self, x: f64, y: f64) {
        if x.is_nan() || y.is_nan() {
            return;
        }
        let (mut x0, mut y0, mut x1, mut y1) = (self.x0, self.y0, self.x1, self.y1);
        if x0.is_nan() {
            x0 = x.floor();
            y0 = y.floor();
            x1 = x0 + 1.0;
            y1 = y0 + 1.0;
        } else {
            let mut z = if x1 - x0 == 0.0 { 1.0 } else { x1 - x0 };
            let mut node = self.root;
            while x0 > x || x >= x1 || y0 > y || y >= y1 {
                let i = ((y < y0) as usize) << 1 | ((x < x0) as usize);
                let mut parent = QNode::default();
                parent.children[i] = node;
                let new_root = self.push(parent);
                node = Some(new_root);
                z *= 2.0;
                match i {
                    0 => {
                        x1 = x0 + z;
                        y1 = y0 + z;
                    }
                    1 => {
                        x0 = x1 - z;
                        y1 = y0 + z;
                    }
                    2 => {
                        x1 = x0 + z;
                        y0 = y1 - z;
                    }
                    _ => {
                        x0 = x1 - z;
                        y0 = y1 - z;
                    }
                }
            }
            if self.root.is_some() && self.nodes[self.root.unwrap()].is_inner() {
                self.root = node;
            }
        }
        self.x0 = x0;
        self.y0 = y0;
        self.x1 = x1;
        self.y1 = y1;
    }

    /// 插入一个点。`pos` = 全部物理节点的坐标（用于取「已有叶子」的坐标，等价于 `node.data.x/y`）。
    pub fn add(&mut self, index: usize, x: f64, y: f64, pos: &[(f64, f64)]) {
        if x.is_nan() || y.is_nan() {
            return;
        }
        self.cover(x, y);
        let leaf = self.push(QNode {
            data: Some(index),
            ..Default::default()
        });

        let mut parent: Option<usize> = None;
        let mut slot = 0usize;
        let (mut x0, mut y0, mut x1, mut y1) = (self.x0, self.y0, self.x1, self.y1);
        let Some(root) = self.root else {
            self.root = Some(leaf);
            return;
        };
        let mut node = root;

        // 顺着已有节点走到叶子（内部节点才有 children）
        while self.nodes[node].is_inner() {
            let xm = (x0 + x1) / 2.0;
            let right = x >= xm;
            if right {
                x0 = xm;
            } else {
                x1 = xm;
            }
            let ym = (y0 + y1) / 2.0;
            let bottom = y >= ym;
            if bottom {
                y0 = ym;
            } else {
                y1 = ym;
            }
            parent = Some(node);
            slot = ((bottom as usize) << 1) | (right as usize);
            match self.nodes[node].children[slot] {
                Some(child) => node = child,
                None => {
                    self.nodes[parent.unwrap()].children[slot] = Some(leaf);
                    return;
                }
            }
        }

        let old = self.nodes[node].data.unwrap();
        let (xp, yp) = pos[old];

        // 完全重合的点串成链表
        if x == xp && y == yp {
            self.nodes[leaf].next = Some(node);
            match parent {
                Some(p) => self.nodes[p].children[slot] = Some(leaf),
                None => self.root = Some(leaf),
            }
            return;
        }

        // 一直分裂到两个点分开为止（d3 的 do-while）
        loop {
            let inner = self.push(QNode::default());
            match parent {
                Some(p) => self.nodes[p].children[slot] = Some(inner),
                None => self.root = Some(inner),
            }
            parent = Some(inner);
            let xm = (x0 + x1) / 2.0;
            let right = x >= xm;
            if right {
                x0 = xm;
            } else {
                x1 = xm;
            }
            let ym = (y0 + y1) / 2.0;
            let bottom = y >= ym;
            if bottom {
                y0 = ym;
            } else {
                y1 = ym;
            }
            slot = ((bottom as usize) << 1) | (right as usize);
            let j = (((yp >= ym) as usize) << 1) | ((xp >= xm) as usize);
            if slot != j {
                self.nodes[inner].children[j] = Some(node);
                self.nodes[inner].children[slot] = Some(leaf);
                return;
            }
        }
    }

    /// 前序访问（只读树，回调可写外部状态）：顺序与 d3 的 `visit` 一致——
    /// 孩子按 3,2,1,0 入栈，于是出栈顺序是 0,1,2,3。
    ///
    /// 回调返回 `true` 表示「这一格到此为止」，不再往下走。
    pub fn visit_readonly<F>(&self, mut cb: F)
    where
        F: FnMut(usize, f64, f64, f64, f64) -> bool,
    {
        let mut stack: Vec<(usize, f64, f64, f64, f64)> = Vec::new();
        if let Some(root) = self.root {
            stack.push((root, self.x0, self.y0, self.x1, self.y1));
        }
        while let Some((idx, x0, y0, x1, y1)) = stack.pop() {
            let stop = cb(idx, x0, y0, x1, y1);
            if !stop && self.nodes[idx].is_inner() {
                let xm = (x0 + x1) / 2.0;
                let ym = (y0 + y1) / 2.0;
                let kids = self.nodes[idx].children;
                if let Some(c) = kids[3] {
                    stack.push((c, xm, ym, x1, y1));
                }
                if let Some(c) = kids[2] {
                    stack.push((c, x0, ym, xm, y1));
                }
                if let Some(c) = kids[1] {
                    stack.push((c, xm, y0, x1, ym));
                }
                if let Some(c) = kids[0] {
                    stack.push((c, x0, y0, xm, ym));
                }
            }
        }
    }

    /// 后序访问（先子后父）：自底向上汇总质心 / 电荷 / 子树最大半径。
    ///
    /// 回调拿到整张节点数组与当前下标，方便读孩子、写自己。
    pub fn visit_after<F>(&mut self, mut cb: F)
    where
        F: FnMut(&mut [QNode], usize),
    {
        let mut quads: Vec<(usize, f64, f64, f64, f64)> = Vec::new();
        let mut next: Vec<usize> = Vec::new();
        if let Some(root) = self.root {
            quads.push((root, self.x0, self.y0, self.x1, self.y1));
        }
        while let Some((idx, x0, y0, x1, y1)) = quads.pop() {
            let xm = (x0 + x1) / 2.0;
            let ym = (y0 + y1) / 2.0;
            let kids = self.nodes[idx].children;
            if let Some(c) = kids[0] {
                quads.push((c, x0, y0, xm, ym));
            }
            if let Some(c) = kids[1] {
                quads.push((c, xm, y0, x1, ym));
            }
            if let Some(c) = kids[2] {
                quads.push((c, x0, ym, xm, y1));
            }
            if let Some(c) = kids[3] {
                quads.push((c, xm, ym, x1, y1));
            }
            next.push(idx);
        }
        while let Some(idx) = next.pop() {
            cb(&mut self.nodes, idx);
        }
    }
}
