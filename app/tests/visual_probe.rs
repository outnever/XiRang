//! 离屏渲染探针（手动跑）：把「物理 + 构图公式」画成 PNG，用来判断观感对不对。
//!
//! `cargo test -p xirang-app --test visual_probe -- --ignored --nocapture`

use xirang_app::obsidian::physics::{Forces, Physics};
use xirang_app::obsidian::view::{get_size, node_scale};
use xirang_core::codec::Uuid;

const W: usize = 1400;
const H: usize = 1100;
const SS: usize = 2; // 2× 超采样

struct Canvas {
    w: usize,
    h: usize,
    px: Vec<[f32; 4]>,
}

impl Canvas {
    fn new(w: usize, h: usize) -> Self {
        let mut bg = vec![[1.0, 1.0, 1.0, 1.0]; w * h];
        // 浅灰底（模仿参考页面的亮色主题）
        for p in bg.iter_mut() {
            *p = [0.98, 0.98, 0.99, 1.0];
        }
        Canvas { w, h, px: bg }
    }

    fn blend(&mut self, x: isize, y: isize, c: [f32; 3], a: f32) {
        if x < 0 || y < 0 || x as usize >= self.w || y as usize >= self.h {
            return;
        }
        let p = &mut self.px[y as usize * self.w + x as usize];
        for i in 0..3 {
            p[i] = p[i] * (1.0 - a) + c[i] * a;
        }
    }

    /// 抗锯齿线段（按距离场算覆盖率）
    fn line(&mut self, x0: f32, y0: f32, x1: f32, y1: f32, c: [f32; 3], a: f32, width: f32) {
        let minx = x0.min(x1).floor() as isize - 2;
        let maxx = x0.max(x1).ceil() as isize + 2;
        let miny = y0.min(y1).floor() as isize - 2;
        let maxy = y0.max(y1).ceil() as isize + 2;
        let (dx, dy) = (x1 - x0, y1 - y0);
        let len2 = (dx * dx + dy * dy).max(1e-6);
        for y in miny..=maxy {
            for x in minx..=maxx {
                let (px, py) = (x as f32 + 0.5, y as f32 + 0.5);
                let t = (((px - x0) * dx + (py - y0) * dy) / len2).clamp(0.0, 1.0);
                let (cx, cy) = (x0 + dx * t, y0 + dy * t);
                let d = ((px - cx).powi(2) + (py - cy).powi(2)).sqrt();
                let cov = (width * 0.5 + 0.5 - d).clamp(0.0, 1.0);
                if cov > 0.0 {
                    self.blend(x, y, c, a * cov);
                }
            }
        }
    }

    fn disc(&mut self, cx: f32, cy: f32, r: f32, c: [f32; 3], a: f32) {
        let minx = (cx - r - 2.0).floor() as isize;
        let maxx = (cx + r + 2.0).ceil() as isize;
        let miny = (cy - r - 2.0).floor() as isize;
        let maxy = (cy + r + 2.0).ceil() as isize;
        for y in miny..=maxy {
            for x in minx..=maxx {
                let (px, py) = (x as f32 + 0.5, y as f32 + 0.5);
                let d = ((px - cx).powi(2) + (py - cy).powi(2)).sqrt();
                let cov = (r + 0.5 - d).clamp(0.0, 1.0);
                if cov > 0.0 {
                    self.blend(x, y, c, a * cov);
                }
            }
        }
    }

    /// 盒式降采样 → PNG
    fn save(&self, path: &str) {
        let w = self.w / SS;
        let h = self.h / SS;
        let mut out = vec![0u8; w * h * 4];
        for y in 0..h {
            for x in 0..w {
                let mut acc = [0.0f32; 4];
                for dy in 0..SS {
                    for dx in 0..SS {
                        let p = self.px[(y * SS + dy) * self.w + (x * SS + dx)];
                        for i in 0..4 {
                            acc[i] += p[i];
                        }
                    }
                }
                let n = (SS * SS) as f32;
                let o = (y * w + x) * 4;
                for i in 0..4 {
                    out[o + i] = ((acc[i] / n).clamp(0.0, 1.0) * 255.0) as u8;
                }
            }
        }
        let file = std::fs::File::create(path).unwrap();
        let mut enc = png::Encoder::new(file, w as u32, h as u32);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        let mut writer = enc.write_header().unwrap();
        writer.write_image_data(&out).unwrap();
        println!("已写出 {path}（{w}×{h}）");
    }
}

#[test]
#[ignore]
fn render_our_graph_to_png() {
    // 造一张「像真实笔记库」的图：若干枢纽 + 每个枢纽挂一批叶子 + 枢纽之间互连 + 一堆孤立节点
    let mut rng = 12345u32;
    let mut next = || {
        rng = rng.wrapping_mul(1664525).wrapping_add(1013904223);
        (rng >> 8) as f64 / 16777216.0
    };
    let hubs = 260usize;   // 词条数
    let leaves = 4;        // 每个词条下挂几个节点（内容）
    let extras = 700;      // 孤立词条（对应你截图外圈那一堆小点）
    let mut ids: Vec<Uuid> = Vec::new();
    let mut weights: Vec<usize> = Vec::new();
    let mut pairs: Vec<(usize, usize)> = Vec::new();

    let make = |n: u32| {
        let mut b = [0u8; 16];
        b[..4].copy_from_slice(&n.to_be_bytes());
        Uuid(b)
    };
    let mut idx = 0usize;
    for h in 0..hubs {
        let hub = idx as usize;
        ids.push(make(h as u32 + 1));
        weights.push(0);
        idx += 1;
        for _ in 0..leaves {
            let leaf = idx as usize;
            ids.push(make(1000 + idx as u32));
            idx += 1;
            pairs.push((hub, leaf));
        }
        let _ = h;
    }
    // 枢纽之间连成核心网
    for h in 0..hubs {
        for k in 0..6 {
            let t = (h * 7 + k * 11 + 3) % hubs;
            if t != h {
                pairs.push((h, t));
            }
        }
    }
    // 孤立节点
    for _ in 0..extras {
        ids.push(make(9000 + idx as u32));
        idx += 1;
    }
    weights.resize(ids.len(), 0);

    let mut p = Physics::new();
    p.set_nodes(&ids, &weights);
    p.set_links(&pairs);
    let f = Forces {
        center: 0.5187,
        repel: 10.0,
        link: 1.0,
        dist: 250.0,
    };
    f.apply(&mut p);
    for _ in 0..600 {
        p.alpha += (0.0 - p.alpha) * xirang_app::obsidian::physics::alpha_decay();
        p.step();
    }

    // 与界面一致的取景：95% 分位装进画布
    let mut radii: Vec<f64> = p
        .nodes
        .iter()
        .map(|n| (n.x * n.x + n.y * n.y).sqrt())
        .collect();
    radii.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let r95 = radii[(radii.len() as f64 * 0.95) as usize].max(1.0);
    let scale = (0.46 * (W.min(H) / SS) as f64 / r95).clamp(1.0 / 128.0, 8.0);
    let ns = node_scale(scale);
    let cx = W as f64 / 4.0;   // 画布是 W*SS 宽，中心 = W*SS/2 → 缩到输出图就是 W/2
    let cy = H as f64 / 4.0;

    let mut canvas = Canvas::new(W * SS, H * SS);
    let line = [0.55f32, 0.57, 0.60];
    for (s, t) in &pairs {
        let (a, b) = (&p.nodes[*s], &p.nodes[*t]);
        let rs = get_size(1.0, weights[*s]) * ns;
        let rt = get_size(1.0, weights[*t]) * ns;
        let dx = b.x - a.x;
        let dy = b.y - a.y;
        let d = (dx * dx + dy * dy).sqrt().max(1e-6);
        let sx = cx + (a.x + dx * (rs / d)) * scale;
        let sy = cy + (a.y + dy * (rs / d)) * scale;
        let len = ((d - rs - rt) * scale).max(0.0);
        if len <= 0.0 {
            continue;
        }
        canvas.line(
            (sx * SS as f64) as f32,
            (sy * SS as f64) as f32,
            ((sx + dx / d * len) * SS as f64) as f32,
            ((sy + dy / d * len) * SS as f64) as f32,
            line,
            0.55,
            1.0 * SS as f32,
        );
    }
    let blue = [0.34f32, 0.51, 0.72];
    for (i, n) in p.nodes.iter().enumerate() {
        let r = (get_size(1.0, weights[i]) * ns * scale).max(0.5);
        canvas.disc(
            (cx + n.x * scale) as f32 * SS as f32,
            (cy + n.y * scale) as f32 * SS as f32,
            r as f32 * SS as f32,
            blue,
            1.0,
        );
    }
    println!(
        "节点 {} · 边 {} · 缩放 {:.3} · 半径 95% = {:.0}",
        p.nodes.len(),
        pairs.len(),
        scale,
        r95
    );
    canvas.save("/private/tmp/our_graph.png");
}
