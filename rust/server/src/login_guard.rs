//! 登录爆破防护。
//!
//! ## 为什么不能只靠 Argon2
//!
//! Argon2 一次要 ~150ms，看起来"爆破很慢"。但那是**单线程串行**的假设 ——
//! 攻击者发 50 个并发，服务端照样会并行算 50 个 Argon2，实际速率
//! 取决于 CPU 核数，不是那 150ms。实测本机连打 9 次错误密码没有任何阻力。
//!
//! ## 三层防护
//!
//! 1. **按用户名**：连续失败 `USER_MAX` 次 → 锁 `USER_LOCK_SECS`
//! 2. **按 IP**：连续失败 `IP_MAX` 次 → 锁 `IP_LOCK_SECS`（防止换用户名刷）
//! 3. **成功即清零**：正常用户打错一次两次不该被罚
//!
//! ## 为什么在内存里，不入库
//!
//! * 这是**临时状态**，重启丢失可接受（重启本身就很慢，爆破窗口很短）
//! * 入库要给每次失败写一行，等于给攻击者一个写放大入口
//!
//! 内存表的代价是**多实例部署时各自为政** —— 本平台是单实例，够用。
//! 以后要多实例，得换成 Redis 之类的共享计数（这里留了注释标记）。

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// 从请求里取**真实客户端 IP**。
///
/// ## 为什么不能直接用连接地址
///
/// 后端监听 `127.0.0.1:8788`，前面是 nginx。连接地址永远是 `127.0.0.1` ——
/// 拿它做限流等于**所有人共用一个桶**：任意一个人爆破 5 次，
/// 全体用户一起被锁。
///
/// ## 为什么信任 `X-Real-IP`
///
/// nginx 配置了 `proxy_set_header X-Real-IP $remote_addr`（见站点配置）。
/// 后端**只监听回环**，外部无法直连，所以这个头只可能来自本机 nginx —— 可信。
///
/// ⚠️ 如果哪天把 8788 暴露到公网，这个头就能被伪造，
///    到时必须换成「白名单代理 IP + 校验 XFF 链路」的做法。
pub fn client_ip(headers: &axum::http::HeaderMap, fallback: &str) -> String {
    // 优先 X-Real-IP（nginx 写的，单值，最干净）
    if let Some(v) = headers.get("x-real-ip").and_then(|v| v.to_str().ok()) {
        let v = v.trim();
        if !v.is_empty() {
            return v.to_string();
        }
    }
    // 退而求其次：X-Forwarded-For 取**第一个**（最靠近客户端的那个）
    if let Some(v) = headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()) {
        if let Some(first) = v.split(',').next() {
            let first = first.trim();
            if !first.is_empty() {
                return first.to_string();
            }
        }
    }
    fallback.to_string()
}

/// 同一用户名连续失败多少次上锁。
const USER_MAX: u32 = 5;
/// 用户名锁多久。
const USER_LOCK: Duration = Duration::from_secs(15 * 60);
/// 同一 IP 连续失败多少次上锁（比用户名宽松些：可能是同一间宿舍的人）
const IP_MAX: u32 = 20;
/// IP 锁多久。
const IP_LOCK: Duration = Duration::from_secs(30 * 60);
/// 失败计数多久没新失败就清零（避免"一天错 4 次"累积成锁）
const DECAY: Duration = Duration::from_secs(60 * 60);

#[derive(Default)]
struct Entry {
    fails: u32,
    last: Option<Instant>,
    locked_until: Option<Instant>,
}

impl Entry {
    /// 顺手清理过期状态：锁到期就解开，太久没失败就清零。
    fn refresh(&mut self, now: Instant) {
        if let Some(until) = self.locked_until {
            if now >= until {
                self.locked_until = None;
                self.fails = 0;
                self.last = None;
            }
        }
        if let Some(last) = self.last {
            if now.duration_since(last) > DECAY && self.locked_until.is_none() {
                self.fails = 0;
            }
        }
    }
}

#[derive(Default)]
pub struct LoginGuard {
    by_user: Mutex<HashMap<String, Entry>>,
    by_ip: Mutex<HashMap<String, Entry>>,
}

/// 被锁了：还要等多久。
#[derive(Debug)]
pub struct Locked {
    pub secs: u64,
    /// 锁的是用户还是 IP（给日志和界面区分用）
    pub scope: &'static str,
}

impl LoginGuard {
    pub fn new() -> Self {
        Self::default()
    }

    /// 登录**前**检查。返回 `Err` 表示已锁定，直接拒绝。
    pub fn check(&self, username: &str, ip: &str) -> Result<(), Locked> {
        let now = Instant::now();
        if let Some(l) = self.peek(&self.by_user, username, now, "用户名") {
            return Err(l);
        }
        if let Some(l) = self.peek(&self.by_ip, ip, now, "IP") {
            return Err(l);
        }
        Ok(())
    }

    fn peek(
        &self,
        map: &Mutex<HashMap<String, Entry>>,
        key: &str,
        now: Instant,
        scope: &'static str,
    ) -> Option<Locked> {
        let mut m = map.lock().ok()?;
        let e = m.get_mut(key)?;
        e.refresh(now);
        let until = e.locked_until?;
        Some(Locked {
            secs: until.saturating_duration_since(now).as_secs().max(1),
            scope,
        })
    }

    /// 登录**失败**时调用，累计计数；够了就上锁。
    pub fn fail(&self, username: &str, ip: &str) {
        let now = Instant::now();
        bump(&self.by_user, username, now, USER_MAX, USER_LOCK);
        bump(&self.by_ip, ip, now, IP_MAX, IP_LOCK);
    }

    /// 登录**成功**时调用，清零 —— 正常用户不该被自己的一次手误罚。
    pub fn ok(&self, username: &str, ip: &str) {
        for map in [&self.by_user, &self.by_ip] {
            if let Ok(mut m) = map.lock() {
                m.remove(username);
                m.remove(ip);
            }
        }
    }

    /// 当前是否有人被锁（给状态接口看）。
    pub fn snapshot(&self) -> serde_json::Value {
        let now = Instant::now();
        let dump = |map: &Mutex<HashMap<String, Entry>>| {
            let Ok(mut m) = map.lock() else {
                return serde_json::json!({});
            };
            // 顺手清理过期的，别让表无限增长
            m.retain(|_, e| {
                e.refresh(now);
                e.locked_until.is_some() || e.fails > 0
            });
            let mut out = serde_json::Map::new();
            for (k, e) in m.iter() {
                if let Some(until) = e.locked_until {
                    out.insert(
                        // 只露前几位 —— 状态接口不该把完整用户名/内网 IP 打出去
                        mask(k),
                        serde_json::json!({
                            "secs_left": until.saturating_duration_since(now).as_secs(),
                            "fails": e.fails,
                        }),
                    );
                }
            }
            serde_json::Value::Object(out)
        };
        serde_json::json!({
            "levels": {
                "user": { "max": USER_MAX, "lock_secs": USER_LOCK.as_secs() },
                "ip":   { "max": IP_MAX,   "lock_secs": IP_LOCK.as_secs() },
            },
            "locked_users": dump(&self.by_user),
            "locked_ips": dump(&self.by_ip),
        })
    }
}

fn bump(
    map: &Mutex<HashMap<String, Entry>>,
    key: &str,
    now: Instant,
    max: u32,
    lock: Duration,
) {
    let Ok(mut m) = map.lock() else { return };
    let e = m.entry(key.to_string()).or_default();
    e.refresh(now);
    e.fails = e.fails.saturating_add(1);
    e.last = Some(now);
    if e.fails >= max {
        e.locked_until = Some(now + lock);
        log::warn!("登录爆破防护：{key} 连续失败 {} 次，锁定 {} 秒", e.fails, lock.as_secs());
    }
}

/// 用户名/IP 打码：`admin` → `ad***`，`192.0.2.123` → `192.0.*.*`
pub fn mask(s: &str) -> String {
    if s.contains('.') && s.chars().filter(|c| *c == '.').count() == 3 {
        let parts: Vec<&str> = s.split('.').collect();
        return format!("{}.{}.*.*", parts[0], parts[1]);
    }
    let n = s.chars().count();
    if n <= 2 {
        "*".repeat(n.max(1))
    } else {
        let keep: String = s.chars().take(2).collect();
        format!("{keep}{}", "*".repeat(n - 2))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locks_after_user_max_fails() {
        let g = LoginGuard::new();
        for _ in 0..USER_MAX - 1 {
            g.fail("bob", "192.0.2.1");
        }
        assert!(g.check("bob", "192.0.2.1").is_ok(), "差一次还不该锁");
        g.fail("bob", "192.0.2.1");
        let e = g.check("bob", "192.0.2.1").unwrap_err();
        assert_eq!(e.scope, "用户名");
        assert!(e.secs > 0);
    }

    #[test]
    fn success_clears() {
        let g = LoginGuard::new();
        for _ in 0..USER_MAX - 1 {
            g.fail("bob", "192.0.2.1");
        }
        g.ok("bob", "192.0.2.1");
        // 清零后再失败 max 次才会锁
        for _ in 0..USER_MAX { g.fail("bob", "192.0.2.1"); }
        assert!(g.check("bob", "192.0.2.1").is_err());
    }

    #[test]
    fn ip_is_locked_independently() {
        let g = LoginGuard::new();
        // 换用户名也没用：IP 层照样计
        for i in 0..IP_MAX {
            g.fail(&format!("u{i}"), "192.0.2.9");
        }
        let e = g.check("someone-new", "192.0.2.9").unwrap_err();
        assert_eq!(e.scope, "IP");
        // 换个 IP 不受影响
        assert!(g.check("someone-new", "192.0.2.8").is_ok());
    }

    #[test]
    fn mask_hides_details() {
        assert_eq!(mask("admin"), "ad***");
        assert_eq!(mask("192.0.2.123"), "192.0.*.*");
        assert_eq!(mask("ab"), "**");
    }
}
