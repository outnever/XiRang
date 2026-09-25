//! 流式扫描现在住在 `xirang_core::scan`（CLI 与桌面端共用同一份实现），
//! 这里只做转发，避免调用点与测试大改。

pub use xirang_core::scan::*;
