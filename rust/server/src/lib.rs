//! neumooc-platform 服务层。
//!
//! 从 Python 版 (`server/server/`) 移植。分三层：
//!
//! * [`db`] —— SQLite 存储，schema 与线上库**逐列兼容**
//! * `store` —— 业务查询
//! * 后续：worker（每账号串行队列）、api（axum）、auth/perm

pub mod api;
pub mod auth;
pub mod crypto;
pub mod db;
pub mod endpoints;
pub mod llm;
pub mod notify;
pub mod perm;
pub mod rows;
pub mod scheduler;
pub mod sse;
pub mod store;
pub mod tasks;
pub mod worker;
