//! 息壤桌面端的非界面部分：懒加载文档层、视图模型、追加编辑层。
//!
//! 拆成 lib 是为了能在没有窗口的情况下测试（`cargo test -p xirang-app`）。

pub mod edit;
pub mod lazy;
pub mod view;
