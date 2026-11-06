//! 调度器 + 考勤监控。
//!
//! ## 两件事
//!
//! | | 干什么 | 频率 |
//! | --- | --- | --- |
//! | [`Scheduler`] | 到点给各账号生成「每日检查」计划并执行 | 每 30 秒看一次 |
//! | [`AttendanceWatcher`] | 发现「进行中且未签」的考勤就**自动签到** | 每 60 秒 |
//!
//! ## 考勤监控是「签到」不是「提醒」
//!
//! 只发提醒等于没做：签到窗口常只开几分钟，等人看到消息、打开平台、赶到教室，
//! 窗口早关了。所以发现未签就**直接提交签到**，通知里说的是「正在自动签到」。
//!
//! **顺序必须是先签再通知** —— 反过来的话通知只能说「请立刻到场扫码」，
//! 而那正是这个功能要消灭的动作。
//!
//! ## 开关按账号，且默认关
//!
//! 这项功能会**真的替学生提交考勤记录**，必须由人在平台上明确打开，
//! 不能默认替谁做决定。配置存在 `accounts.note` 的 `auto_sign` 段。

use std::sync::{Arc, Mutex};

use rusqlite::Connection;
use serde_json::json;

use crate::db;
use crate::notify::Notifier;
use crate::store;

/// 考勤监控轮询间隔（秒）。
///
/// ## 为什么默认改成 180 而不是 60
///
/// 平台侧的考勤场次通常**持续 10 分钟**（`openTime` → `finishTime`），
/// 60 秒轮询等于一场考勤要查 10 次。10 个账号就是 **每分钟 10 次请求**、
/// 每天 14400 次 —— 这个量级极易被平台判定为爬虫。
///
/// 实际后果：学校封了我们的出口 IP（平台所在网段整段不通，
/// 从 VPS 访问正常），整个平台都用不了了。
///
/// 180 秒 = 10 分钟内仍能覆盖到，但请求量降到 1/3（每天 4800 次）。
/// 可用 `NEUMOOC_ATTENDANCE_TICK` 环境变量覆盖。
pub const ATTENDANCE_TICK_SECS: u64 = 180;

/// 从环境变量取轮询间隔（秒），范围夹在 [60, 1800]。
///
/// 下限 60 是安全的（再快没有收益，一场考勤已经能覆盖 10 次）；
/// 上限 1800 防止配错成"一天只查一次"。
pub fn attendance_tick_secs() -> u64 {
    std::env::var("NEUMOOC_ATTENDANCE_TICK")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(|v| v.clamp(60, 1800))
        .unwrap_or(ATTENDANCE_TICK_SECS)
}
/// 调度器轮询间隔（秒）。
pub const SCHEDULER_TICK_SECS: u64 = 30;

/// 一条「进行中且未签」的考勤。
#[derive(Debug, Clone)]
pub struct PendingSign {
    pub account_id: i64,
    pub account_label: String,
    pub detail_id: String,
    pub attendance_id: String,
    pub title: String,
    pub course_name: String,
    pub seconds_left: i64,
    pub needs_location: bool,
    pub attendance_type: i64,
    pub location_type: Option<i64>,
    pub longitude: Option<String>,
    pub latitude: Option<String>,
    pub address: Option<String>,
}

impl PendingSign {
    /// 通知文案。签到时说「正在自动签到」，只提醒时才说「请立刻到场」。
    pub fn message(&self, auto: bool) -> String {
        let left = format!("剩余 {} 分 {} 秒", self.seconds_left / 60, self.seconds_left % 60);
        if auto {
            format!("✅ 正在自动签到：{}（{}）{left}", self.title, self.course_name)
        } else {
            let loc = if self.needs_location { "，需现场定位" } else { "" };
            format!(
                "⚠️ 正在签到：{}（{}）{left}{loc}。请立刻到场扫码，或找老师手动补签。",
                self.title, self.course_name
            )
        }
    }
}

/// 考勤监控。
pub struct AttendanceWatcher {
    pub notifier: Notifier,
    /// 已处理过的场次（`{account_id}:{detail_id}`），避免每分钟重复提交。
    alerted: Arc<Mutex<std::collections::HashSet<String>>>,
}

impl AttendanceWatcher {
    pub fn new(notifier: Notifier) -> Self {
        Self { notifier, alerted: Default::default() }
    }

    /// 查一遍缓存的考勤，找出「进行中且未签」的。
    ///
    /// 读的是**库里已缓存的数据**（由 `scan` 任务同步），这样监控本身不需要
    /// 频繁打学校接口，也不会因为频繁轮询触发风控。
    pub fn find_pending(conn: &Connection) -> rusqlite::Result<Vec<PendingSign>> {
        let now = db::now_ms();
        let mut stmt = conn.prepare(
            "SELECT a.id, a.label, t.detail_id, t.attendance_id, t.title, t.course_name,
                    t.finish_time, t.location_type, t.attendance_type,
                    t.longitude, t.latitude, t.address_name
             FROM attendance t JOIN accounts a ON a.id = t.account_id
             WHERE (t.sign_role IS NULL OR t.sign_role NOT IN (3,4))
               AND t.sign_time IS NULL
               AND (t.finish_time = 0 OR t.finish_time >= ?)
               AND (t.open_time = 0 OR t.open_time <= ?)
               AND (t.status IS NULL OR t.status = 1)
               AND a.state != 'disabled'
             ORDER BY t.finish_time",
        )?;
        let rows = stmt.query_map(rusqlite::params![now, now], |r| {
            let finish: i64 = r.get(6)?;
            let loc: Option<i64> = r.get(7)?;
            let addr: String = r.get(11)?;
            Ok(PendingSign {
                account_id: r.get(0)?,
                account_label: r.get(1)?,
                detail_id: r.get(2)?,
                attendance_id: r.get(3)?,
                title: r.get(4)?,
                course_name: r.get(5)?,
                seconds_left: if finish == 0 { 0 } else { ((finish - now) / 1000).max(0) },
                needs_location: loc == Some(1),
                attendance_type: r.get(8)?,
                location_type: loc,
                longitude: r.get(9)?,
                latitude: r.get(10)?,
                address: if addr.is_empty() { None } else { Some(addr) },
            })
        })?;
        rows.collect()
    }

    /// **总开关**：环境变量 `NEUMOOC_AUTO_SIGN`（默认开，`0/off/false/no` 为关）。
    ///
    /// 这是一个**急停开关** —— 出事时运维改一个环境变量就该让所有自动签到停下来。
    /// 原先它只用于接口返回的 `available` 字段，派发逻辑**根本没查**，
    /// 等于"界面上关了，后台照签"。
    pub fn auto_master_on() -> bool {
        std::env::var("NEUMOOC_AUTO_SIGN")
            .map(|v| {
                !matches!(
                    v.trim().to_ascii_lowercase().as_str(),
                    "0" | "off" | "false" | "no"
                )
            })
            .unwrap_or(true)
    }

    /// 所有**开着自动签到**的账号 id（`state != 'disabled'`）。
    ///
    /// 监控线程每轮按这个列表派发「同步 + 签」任务 —— 不能只读缓存，
    /// 因为缓存只有手动同步才会更新（见 `run_auto_sync_sign` 的注释）。
    pub fn auto_sign_accounts(conn: &Connection) -> rusqlite::Result<Vec<i64>> {
        if !Self::auto_master_on() {
            return Ok(Vec::new()); // 急停：总开关关了就一个都不派
        }
        let mut stmt = conn.prepare("SELECT id FROM accounts WHERE state != 'disabled' ORDER BY id")?;
        let ids = stmt
            .query_map([], |r| r.get::<_, i64>(0))?
            .filter_map(|r| r.ok())
            .filter(|id| Self::auto_sign_enabled(conn, *id))
            .collect();
        Ok(ids)
    }

    /// 某账号是否开了自动签到。**默认关。**
    pub fn auto_sign_enabled(conn: &Connection, account_id: i64) -> bool {
        store::read_account_cfg(conn, account_id, "auto_sign")
            .ok()
            .and_then(|v| v.get("enabled").and_then(|b| b.as_bool()))
            .unwrap_or(false)
    }

    /// 跑一轮。返回本轮新发现的场次（供调用方投递签到任务）。
    ///
    /// 会**立即发通知** —— 签到时说「正在自动签到」，未开启时回退成提醒。
    pub fn tick(&self, conn: &Connection) -> Vec<PendingSign> {
        let found = Self::find_pending(conn).unwrap_or_default();
        let mut fresh = Vec::new();
        {
            let mut seen = self.alerted.lock().expect("锁中毒");
            for p in &found {
                let key = format!("{}:{}", p.account_id, p.detail_id);
                if !seen.insert(key) {
                    continue; // 这一场本轮之前已经处理过
                }
                let auto = Self::auto_sign_enabled(conn, p.account_id);
                self.notifier.urgent(&format!("{}\n（账号 #{}）", p.message(auto), p.account_id));
                log::info!(
                    "账号 {} {}",
                    p.account_id,
                    if auto { "自动签到" } else { "发现未签（未开启自动签到）" }
                );
                if auto {
                    fresh.push(p.clone());
                }
            }
            // 清理很早以前的记录，避免无限增长
            if seen.len() > 5000 {
                seen.clear();
            }
        }
        fresh
    }
}

/// 每日检查计划。
pub struct Scheduler;

impl Scheduler {
    /// 为每个开了调度的账号生成今天的检查计划（幂等）。
    pub fn ensure_today_plans(conn: &Connection) -> rusqlite::Result<usize> {
        let today = db::biz_date_now();
        let accounts = store::list_accounts(conn)?;
        let mut created = 0;
        for a in accounts {
            let cfg = store::read_account_cfg(conn, a.id, "schedule")
                .unwrap_or_else(|_| json!({}));
            if cfg.get("enabled").and_then(|v| v.as_bool()) != Some(true) {
                continue;
            }
            let exists: Option<i64> = conn
                .query_row(
                    "SELECT id FROM daily_plans WHERE account_id=? AND biz_date=?",
                    rusqlite::params![a.id, today],
                    |r| r.get(0),
                )
                .ok();
            if exists.is_some() {
                continue;
            }
            let time = cfg.get("time").and_then(|v| v.as_str()).unwrap_or("08:00");
            let fire_at = fire_at_for(time, a.id, &today);
            conn.execute(
                "INSERT INTO daily_plans (account_id, biz_date, fire_at, action, state, created_at, updated_at)
                 VALUES (?,?,?,?,'pending',?,?)",
                rusqlite::params![
                    a.id, today, fire_at, json!({ "job": "check" }).to_string(),
                    db::now_ms(), db::now_ms()
                ],
            )?;
            created += 1;
        }
        Ok(created)
    }

    /// 取出到点且未执行的计划。
    pub fn due_plans(conn: &Connection) -> rusqlite::Result<Vec<(i64, i64)>> {
        let now = db::now_ms();
        let mut stmt = conn.prepare(
            "SELECT id, account_id FROM daily_plans
             WHERE state='pending' AND fire_at <= ? ORDER BY fire_at",
        )?;
        let rows = stmt.query_map(rusqlite::params![now], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.collect()
    }
}

/// 触发时刻 = 北京时间当天 `HH:MM` + 账号错峰。
///
/// 错峰是必要的：多账号同时打学校接口容易触发风控。
pub fn fire_at_for(hhmm: &str, account_id: i64, biz_date: &str) -> i64 {
    let (h, m) = parse_hhmm(hhmm).unwrap_or((8, 0));
    // biz_date 是北京日期；换算成 UTC 毫秒时间戳
    let days = days_from_civil(biz_date);
    let secs = days * 86400 + (h as i64) * 3600 + (m as i64) * 60 - 8 * 3600;
    // 每个账号往后推 7 秒，避免同时请求
    (secs + (account_id % 100) * 7) * 1000
}

/// 解析 `HH:MM`。**0 点是合法值**，不能用 `or` 兜底。
pub fn parse_hhmm(s: &str) -> Option<(u32, u32)> {
    let (h, m) = s.trim().split_once(':')?;
    let h: u32 = h.trim().parse().ok()?;
    let m: u32 = m.trim().parse().ok()?;
    (h < 24 && m < 60).then_some((h, m))
}

/// `YYYY-MM-DD` → 距 1970-01-01 的天数（Howard Hinnant 的 days_from_civil）。
fn days_from_civil(date: &str) -> i64 {
    let mut it = date.split('-');
    let y: i64 = it.next().and_then(|s| s.parse().ok()).unwrap_or(1970);
    let m: i64 = it.next().and_then(|s| s.parse().ok()).unwrap_or(1);
    let d: i64 = it.next().and_then(|s| s.parse().ok()).unwrap_or(1);
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;


    fn setup() -> (Connection, i64) {
        let c = db::open_memory().unwrap();
        let aid = store::create_account(&c, "测试", "u", b"e", "t").unwrap();
        (c, aid)
    }

    #[test]
    fn hhmm_parsing() {
        assert_eq!(parse_hhmm("08:00"), Some((8, 0)));
        assert_eq!(parse_hhmm("00:00"), Some((0, 0)), "0 点是合法值");
        assert_eq!(parse_hhmm("23:59"), Some((23, 59)));
        assert_eq!(parse_hhmm("24:00"), None);
        assert_eq!(parse_hhmm("8:0"), Some((8, 0)));
        assert_eq!(parse_hhmm("abc"), None);
    }

    #[test]
    fn midnight_is_not_replaced_by_default() {
        // **回归**：曾经写 `parse_hhmm(s).or(Some((8,0)))`，
        // 于是 00:00 被静默改成 08:00。
        let midnight = fire_at_for("00:00", 1, "2026-09-29");
        let eight = fire_at_for("08:00", 1, "2026-09-29");
        assert_ne!(midnight, eight, "00:00 不能被当成 08:00");
        assert!(midnight < eight);
    }

    #[test]
    fn stagger_by_account() {
        let a = fire_at_for("08:00", 1, "2026-09-29");
        let b = fire_at_for("08:00", 2, "2026-09-29");
        assert_ne!(a, b, "多账号必须错峰");
        assert!(b > a);
    }

    #[test]
    fn auto_sign_defaults_off() {
        // 这项功能会真的替学生提交考勤，默认必须关
        let (c, aid) = setup();
        assert!(!AttendanceWatcher::auto_sign_enabled(&c, aid));
        store::write_account_cfg(&c, aid, "auto_sign", json!({"enabled": true})).unwrap();
        assert!(AttendanceWatcher::auto_sign_enabled(&c, aid));
    }

    #[test]
    fn finds_only_open_unsigned() {
        let (c, aid) = setup();
        let now = db::now_ms();
        let insert = |did: &str, open: i64, finish: i64, role: Option<i64>, status: Option<i64>| {
            c.execute(
                "INSERT INTO attendance (account_id, term_id, detail_id, attendance_id, title,
                   course_name, attendance_type, status, open_time, finish_time, sign_role, scanned_at)
                 VALUES (?,?,?,?,?,?,1,?,?,?,?,?)",
                rusqlite::params![aid, "t", did, "a", "课", "课程", status, open, finish, role, now],
            )
            .unwrap();
        };
        insert("open", now - 1000, now + 60_000, None, Some(1));       // ✓ 进行中未签
        insert("signed", now - 1000, now + 60_000, Some(4), Some(1));  // ✗ 已签
        insert("ended", now - 90_000, now - 1000, None, Some(2));      // ✗ 已结束
        insert("future", now + 60_000, now + 120_000, None, Some(0));  // ✗ 未开始
        insert("nullstatus", now - 1000, now + 60_000, None, None);    // ✓ null 也算进行中

        let got = AttendanceWatcher::find_pending(&c).unwrap();
        let ids: Vec<&str> = got.iter().map(|p| p.detail_id.as_str()).collect();
        assert!(ids.contains(&"open"), "进行中未签应被发现");
        assert!(ids.contains(&"nullstatus"), "status=null 也算进行中");
        assert!(!ids.contains(&"signed"));
        assert!(!ids.contains(&"ended"));
        assert!(!ids.contains(&"future"));
    }

    #[test]
    fn tick_reports_once_per_session() {
        let (c, aid) = setup();
        let now = db::now_ms();
        c.execute(
            "INSERT INTO attendance (account_id, term_id, detail_id, attendance_id, title,
               course_name, attendance_type, status, open_time, finish_time, scanned_at)
             VALUES (?,?,?,?,?,?,1,1,?,?,?)",
            rusqlite::params![aid, "t", "d1", "a1", "课", "课程", now - 1000, now + 60_000, now],
        )
        .unwrap();
        let w = AttendanceWatcher::new(Notifier::from_env());
        // 未开启自动签到 → 不返回待签，但每场只报一次
        assert!(w.tick(&c).is_empty(), "默认关，不该返回待签");
        assert!(w.tick(&c).is_empty());
        // 打开后应返回待签
        store::write_account_cfg(&c, aid, "auto_sign", json!({"enabled": true})).unwrap();
        let w2 = AttendanceWatcher::new(Notifier::from_env());
        assert_eq!(w2.tick(&c).len(), 1, "开启后应返回待签");
        assert_eq!(w2.tick(&c).len(), 0, "同一场只处理一次");
    }

    #[test]
    fn message_says_auto_when_signing() {
        let p = PendingSign {
            account_id: 1, account_label: "甲".into(), detail_id: "d".into(),
            attendance_id: "a".into(), title: "签到".into(), course_name: "课".into(),
            seconds_left: 125, needs_location: true, attendance_type: 1,
            location_type: Some(1), longitude: None, latitude: None, address: None,
        };
        let auto = p.message(true);
        assert!(auto.contains("自动签到"));
        assert!(!auto.contains("请立刻到场"), "自动签到时不该再让人跑一趟");
        let manual = p.message(false);
        assert!(manual.contains("请立刻到场"));
    }
}
