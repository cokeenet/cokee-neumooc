//! 登录会话。

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Session {
    #[serde(default, rename = "userId", alias = "user_id")]
    pub user_id: String,
    #[serde(default, rename = "tenantId", alias = "tenant_id")]
    pub tenant_id: String,
    #[serde(default, rename = "accessToken", alias = "access_token")]
    pub access_token: String,
    #[serde(default, rename = "refreshToken", alias = "refresh_token")]
    pub refresh_token: String,
    #[serde(default, rename = "expiresTime", alias = "expires_time")]
    pub expires_time: i64,
}

impl Session {
    /// 本地判定是否已过期。
    ///
    /// ⚠️ **只能用来省一次必然失败的请求**，不能当作「token 有效」的依据 ——
    /// 这个平台的 token 会被服务端提前吊销（本地还剩 5 分钟时服务端已回 401）。
    /// 真正的兜底是 `Client::with_reauth`。
    pub fn is_expired(&self) -> bool {
        self.expires_time > 0 && crate::now_ms() >= self.expires_time
    }
}
