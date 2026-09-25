//! 息壤（XiRang）核心库（Rust 交付实现，依赖内核 v1.0）。
//!
//! 与 Python `tools/` 语义对齐：
//! - `codec`：节点 ↔ 字节（内核）
//! - `validator`：结构校验（错误码 E/R）
//! - `tree`：树处理 + 文件头读写
//! - `convert`：格式转换（后续）

pub mod codec;
pub mod catalog;
pub mod convert;
pub mod index;
pub mod query;
pub mod scan;
pub mod shard;
pub mod validator;
pub mod tree;
pub mod workspace;
pub mod wsidx;
