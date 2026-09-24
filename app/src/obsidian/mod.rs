//! Obsidian 关系图谱的 Rust 逐行移植。
//!
//! 规格 = `obsidian-graph-lab.source.html`（Obsidian `sim.js` + 图谱渲染器的移植版）：
//! - `quadtree`：d3-quadtree 的移植（`cover` / `add` / `visit` / `visitAfter`）
//! - `physics`：五个力 + alpha 衰减 + 参数映射，常数与施加顺序照抄
//! - `render`：canvas 构图 → egui painter（连线分批 / 裁剪 / 圆 / 环 / 箭头 / 标签）
//! - `view`：视图状态（缩放、平移、缓动、悬停高亮）+ egui 控件

pub mod physics;
pub mod quadtree;
pub mod view;
