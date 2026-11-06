//! 平台自身的鉴权：用户密码（Argon2）+ 会话令牌（SHA-256 哈希入库）。
//!
//! ## 两条与「东软账号凭据」的区别，别混
//!
//! | | 存什么 | 用什么 |
//! | --- | --- | --- |
//! | **平台用户密码**（本模块） | `users.password_hash` | **Argon2**（慢哈希，抗爆破） |
//! | **东软账号密码**（[`crate::crypto`]） | `accounts.password_enc` | **Fernet 可逆加密**（要用它去登录） |
//!
//! 前者只用来核对，所以单向；后者要拿去登录学校系统，所以必须可逆。
//!
//! ## 会话令牌
//!
//! 令牌**明文只发给浏览器**，库里存的是它的 SHA-256 —— 库泄露也没法直接冒用。
//! 令牌用密码学随机数生成（不是 uuid4：uuid4 只有 122 位且格式可预测）。

use argon2::password_hash::{rand_core::OsRng, PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use rand::RngCore;
use sha2::{Digest, Sha256};

use neumooc_core::error::{Error, Result};

/// 会话 Cookie 名。与 Python 版一致，换掉会让所有已登录用户被登出。
pub const COOKIE: &str = "neumooc_session";

/// 会话有效期（秒）。30 天。
pub const SESSION_TTL_SECS: i64 = 30 * 24 * 3600;

/// 用 Argon2 哈希用户密码。
pub fn hash_password(plain: &str) -> Result<String> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(plain.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| Error::Invalid(format!("哈希失败：{e}")))
}

/// 校验用户密码。
///
/// 返回 `bool` 而不是 `Result` —— **「密码不对」不是错误**，
/// 是正常的业务结果。把它当 Err 会让调用方到处写 `is_err()` 判断。
pub fn verify_password(plain: &str, hash: &str) -> bool {
    match PasswordHash::new(hash) {
        Ok(parsed) => Argon2::default()
            .verify_password(plain.as_bytes(), &parsed)
            .is_ok(),
        Err(_) => false,
    }
}

/// 生成会话令牌（明文，发给浏览器）。
pub fn new_token() -> String {
    let mut buf = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut buf);
    // URL-safe，避免 cookie 里出现需要转义的字符
    base64::Engine::encode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, buf)
}

/// 令牌 → 入库用的哈希。
pub fn token_hash(token: &str) -> String {
    let mut h = Sha256::new();
    h.update(token.as_bytes());
    format!("{:x}", h.finalize())
}

/// 从 `Cookie:` 头里取会话令牌。
pub fn token_from_cookie(header: &str) -> Option<String> {
    header.split(';').find_map(|part| {
        let (k, v) = part.split_once('=')?;
        (k.trim() == COOKIE).then(|| v.trim().to_string())
    })
}

/// 该请求是不是走的 HTTPS —— 决定 Cookie 带不带 `Secure`。
///
/// ## 为什么不能硬编码
///
/// 后端只监听 `127.0.0.1:8788`，前面是 nginx（HTTPS 终止在那一层）。
/// 从后端视角看连接永远是明文 HTTP —— **按连接判断会把所有请求都算成 HTTP**。
///
/// 实测过：`secure_cookie` 硬编码 `false`，于是 `Set-Cookie` 里没有 `Secure`。
/// 站点对外是 HTTPS，但**同一个 cookie 明文 HTTP 也会发** ——
/// 校园网这种共享链路里可以被嗅探后劫持会话。
///
/// ## 判断依据
///
/// nginx 配了 `proxy_set_header X-Forwarded-Proto $neu_real_proto`，
/// 后端只监听回环、外部无法直连，所以这个头可信。
///
/// `NEUMOOC_SECURE_COOKIE` 可强制指定（反向代理没传 proto 头时用得上）。
pub fn want_secure_cookie(headers: &axum::http::HeaderMap) -> bool {
    if let Ok(v) = std::env::var("NEUMOOC_SECURE_COOKIE") {
        match v.trim().to_ascii_lowercase().as_str() {
            "1" | "on" | "true" | "yes" => return true,
            "0" | "off" | "false" | "no" => return false,
            _ => {}
        }
    }
    headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.trim().eq_ignore_ascii_case("https"))
        .unwrap_or(false)
}

/// 构造 `Set-Cookie`。
///
/// `HttpOnly` 挡 XSS 读取；`SameSite=Lax` 挡跨站写请求（GET 导航仍带得上）；
/// `Secure` 只在 HTTPS 下加 —— 本地 http 调试加了会导致 cookie 完全不被保存。
pub fn set_cookie(token: &str, max_age: i64, secure: bool) -> String {
    let mut s = format!(
        "{COOKIE}={token}; Path=/; HttpOnly; SameSite=Lax; Max-Age={max_age}"
    );
    if secure {
        s.push_str("; Secure");
    }
    s
}

/// 登出用的过期 Cookie。
pub fn clear_cookie(secure: bool) -> String {
    set_cookie("", 0, secure)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_round_trip() {
        let h = hash_password("test-password").unwrap();
        assert!(verify_password("test-password", &h));
        assert!(!verify_password("wrong", &h));
    }

    #[test]
    fn same_password_gets_different_hash() {
        // 每次都要新盐 —— 相同密码哈希相同的话，一眼就能看出谁和谁同密码
        let a = hash_password("same").unwrap();
        let b = hash_password("same").unwrap();
        assert_ne!(a, b, "盐必须随机");
        assert!(verify_password("same", &a) && verify_password("same", &b));
    }

    #[test]
    fn bad_hash_is_false_not_panic() {
        // 库里存了坏数据不能让服务崩
        assert!(!verify_password("x", "not-a-hash"));
        assert!(!verify_password("x", ""));
    }

    #[test]
    fn token_is_random_and_url_safe() {
        let a = new_token();
        let b = new_token();
        assert_ne!(a, b);
        assert!(!a.contains('+') && !a.contains('/') && !a.contains('='));
        assert!(a.len() >= 40, "32 字节的 base64url 无填充应约 43 字符");
    }

    #[test]
    fn token_hash_is_stable_and_not_reversible() {
        let t = new_token();
        assert_eq!(token_hash(&t), token_hash(&t));
        assert_ne!(token_hash(&t), t, "库里不该存明文");
        assert_eq!(token_hash(&t).len(), 64, "SHA-256 十六进制是 64 字符");
    }

    #[test]
    fn cookie_parsing() {
        assert_eq!(token_from_cookie("a=1; neumooc_session=XYZ; b=2").as_deref(), Some("XYZ"));
        assert_eq!(token_from_cookie("neumooc_session=ABC").as_deref(), Some("ABC"));
        assert!(token_from_cookie("other=1").is_none());
        assert!(token_from_cookie("").is_none());
    }

    #[test]
    fn cookie_flags() {
        let c = set_cookie("tok", 100, false);
        assert!(c.contains("HttpOnly"), "挡 XSS 读取");
        assert!(c.contains("SameSite=Lax"), "挡跨站写请求");
        assert!(!c.contains("Secure"), "本地 http 加 Secure 会导致 cookie 存不下");
        assert!(set_cookie("tok", 100, true).contains("Secure"));
        assert!(clear_cookie(false).contains("Max-Age=0"));
    }
}
