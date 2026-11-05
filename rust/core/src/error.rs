//! 协议层错误。
//!
//! **刻意把「业务码」与「HTTP 状态」分开**，因为在这个平台上两者会打架：
//! 服务端经常用 HTTP 200 + 业务码表达失败，而 403 又常常是参数问题而不是没权限
//! （见 docs/ATTENDANCE-PROTOCOL.md 坑 5）。

use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
    /// 网络 / 传输层失败
    #[error("网络错误：{0}")]
    Transport(#[from] reqwest::Error),

    /// 本地文件读写失败（上传下载用）
    #[error("文件错误：{0}")]
    Io(#[from] std::io::Error),

    /// 业务码非成功。`code` 是服务端返回的 `code` 字段。
    #[error("[{code}] {msg}")]
    Biz { code: i64, msg: String },

    /// JSON 结构不符预期
    #[error("返回结构异常：{0}")]
    Shape(String),

    /// 本地参数不合法（还没发请求就发现了）
    #[error("{0}")]
    Invalid(String),
}

impl Error {
    /// 是否为**登录态失效**。
    ///
    /// 判据要宽：这个平台的 token 会被服务端提前吊销，而本地 `expiresTime`
    /// 还显示有效。只要沾上 401 / 账号未登录，就该重登重试。
    pub fn is_auth_error(&self) -> bool {
        match self {
            Error::Biz { code, msg } => *code == 401 || msg.contains("账号未登录"),
            _ => false,
        }
    }

    pub fn code(&self) -> Option<i64> {
        match self {
            Error::Biz { code, .. } => Some(*code),
            _ => None,
        }
    }
}
