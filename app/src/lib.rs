//! 息壤桌面端的非界面部分：懒加载文档层、视图模型、追加编辑层。
//!
//! 拆成 lib 是为了能在没有窗口的情况下测试（`cargo test -p xirang-app`）。

pub mod blobimg;
pub mod edit;
pub mod export;
pub mod graph;
pub mod i18n;
pub mod lazy;
pub mod obsidian;
pub mod scan;
pub mod state;
pub mod theme;
pub mod view;
