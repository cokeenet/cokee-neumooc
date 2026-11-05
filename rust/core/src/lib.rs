//! 东软智慧教育协议层。
//!
//! 从 Python 版 (`server/neumooc/`) 一比一移植。这一层**不依赖 web、不依赖数据库**，
//! 只讲东软接口本身 —— 业务编排留在上层。
//!
//! 协议细节见 `docs/ATTENDANCE-PROTOCOL.md` 与 `docs/HOMEWORK-PROTOCOL.md`。

pub mod attendance;
pub mod client;
pub mod de;
pub mod file;
pub mod homework;
pub mod resources;
pub mod study;
pub mod error;
pub mod exam;
pub mod session;
pub mod signing;

pub use client::Client;
pub use error::{Error, Result};
pub use session::Session;

/// 当前毫秒时间戳。
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// 响应外壳。服务端统一返回 `{code, msg, data}`。
pub const WEB_API: &str = "/web-api";
pub const OK_CODE: i64 = 0;
