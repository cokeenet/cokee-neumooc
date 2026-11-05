//! SQLite 存储层。
//!
//! ## 与现有库的兼容是**硬要求**
//!
//! 这份 schema 是**逐字照抄** Python 版线上库的 —— 目标是 Rust 版能**直接打开
//! 那个 `.db` 文件**，不需要导出导入。所以：
//!
//! * 列名、类型、`NOT NULL`、`DEFAULT`、`UNIQUE` 都必须一致
//! * `CREATE TABLE IF NOT EXISTS` **不会修改已存在的表** ——
//!   加列必须单独写 `ALTER TABLE` 迁移（本项目为此踩过两次：
//!   `executions` 的 UNIQUE 约束、`attendance` 的定位坐标三列）
//!
//! ## 连接是线程局部的
//!
//! `rusqlite::Connection` 不是 `Sync`。每个 worker 线程各自持有一个连接
//! （对应 Python 版的 `threading.local`）。跨线程共享要用通道，不要共享连接。

use std::path::Path;
use std::sync::Mutex;

use rusqlite::Connection;

pub type Result<T> = std::result::Result<T, rusqlite::Error>;

/// 建表语句。**与 Python 版逐字一致**，改这里等于改线上库的契约。
pub const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS users (
  username      TEXT PRIMARY KEY,
  password_hash TEXT NOT NULL,
  role          TEXT NOT NULL DEFAULT 'user',
  created_at    INTEGER NOT NULL,
  updated_at    INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS sessions (
  token_hash TEXT PRIMARY KEY,
  username   TEXT NOT NULL REFERENCES users(username) ON DELETE CASCADE,
  expires_at INTEGER NOT NULL,
  created_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_sessions_exp ON sessions(expires_at);

CREATE TABLE IF NOT EXISTS accounts (
  id            INTEGER PRIMARY KEY AUTOINCREMENT,
  label         TEXT NOT NULL,
  username      TEXT NOT NULL,
  password_enc  BLOB NOT NULL,
  tenant_id     TEXT NOT NULL,
  user_id       TEXT,
  access_token  TEXT,
  refresh_token TEXT,
  token_expires INTEGER NOT NULL DEFAULT 0,
  state         TEXT NOT NULL DEFAULT 'unknown',
  note          TEXT NOT NULL DEFAULT '',
  -- 班级（来自上游的 deptName）。界面上按它分组。
  dept          TEXT NOT NULL DEFAULT '',
  -- **是否在签到里带 device_id**（0=不带，默认）。
  -- 与 device_id 分开：ID 可以留着备查，带不带由这个开关决定，
  -- 免得为了"暂时不带"把 ID 清掉、以后又要重新生成。
  device_id_enabled INTEGER NOT NULL DEFAULT 0,
  -- 模拟设备 ID（UUID）。留空表示还没生成过。
  -- 平台只在「同一 deviceId 对应多个学号」时判异常，空值不参与该判定。
  -- 真 App 的 deviceId 就是随机 UUID（`AppUtil.getDeviceId()`），
  -- 所以填一个随机值在流量上与真机无法区分。
  device_id     TEXT NOT NULL DEFAULT '',
  last_login_at INTEGER NOT NULL DEFAULT 0,
  created_at    INTEGER NOT NULL,
  updated_at    INTEGER NOT NULL,
  UNIQUE(tenant_id, username)
);

CREATE TABLE IF NOT EXISTS user_accounts (
  username   TEXT NOT NULL REFERENCES users(username) ON DELETE CASCADE,
  account_id INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
  created_at INTEGER NOT NULL,
  PRIMARY KEY (username, account_id)
);
CREATE INDEX IF NOT EXISTS idx_ua_account ON user_accounts(account_id);

CREATE TABLE IF NOT EXISTS courses (
  id              INTEGER PRIMARY KEY AUTOINCREMENT,
  account_id      INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
  term_id         TEXT NOT NULL,
  term_name       TEXT NOT NULL DEFAULT '',
  teach_course_id TEXT NOT NULL,
  teach_class_id  TEXT NOT NULL,
  course_name     TEXT NOT NULL DEFAULT '',
  progress        TEXT NOT NULL DEFAULT '',
  scanned_at      INTEGER NOT NULL,
  UNIQUE(account_id, term_id, teach_course_id)
);
CREATE INDEX IF NOT EXISTS idx_courses_acc ON courses(account_id, term_id);

CREATE TABLE IF NOT EXISTS resources (
  id            INTEGER PRIMARY KEY AUTOINCREMENT,
  account_id    INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
  course_pk     INTEGER NOT NULL REFERENCES courses(id) ON DELETE CASCADE,
  res_id        TEXT NOT NULL,
  name          TEXT NOT NULL DEFAULT '',
  res_type      INTEGER NOT NULL DEFAULT 0,
  res_ext       TEXT NOT NULL DEFAULT '',
  study_status  INTEGER NOT NULL DEFAULT 0,
  play_progress INTEGER,
  is_drag       INTEGER,
  updated_at    INTEGER NOT NULL,
  UNIQUE(account_id, res_id)
);
CREATE INDEX IF NOT EXISTS idx_res_course ON resources(course_pk, study_status);

CREATE TABLE IF NOT EXISTS attendance (
  id               INTEGER PRIMARY KEY AUTOINCREMENT,
  account_id       INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
  term_id          TEXT NOT NULL,
  detail_id        TEXT NOT NULL,
  attendance_id    TEXT NOT NULL DEFAULT '',
  title            TEXT NOT NULL DEFAULT '',
  course_name      TEXT NOT NULL DEFAULT '',
  attendance_type  INTEGER NOT NULL DEFAULT 0,
  status           INTEGER,
  open_time        INTEGER NOT NULL DEFAULT 0,
  finish_time      INTEGER NOT NULL DEFAULT 0,
  sign_role        INTEGER,
  sign_time        INTEGER,
  location_type    INTEGER,
  teach_class_id   TEXT NOT NULL DEFAULT '',
  address_name     TEXT NOT NULL DEFAULT '',
  teach_course_id  TEXT NOT NULL DEFAULT '',
  longitude        TEXT NOT NULL DEFAULT '',
  latitude         TEXT NOT NULL DEFAULT '',
  radius           INTEGER,
  scanned_at       INTEGER NOT NULL,
  UNIQUE(account_id, detail_id)
);
CREATE INDEX IF NOT EXISTS idx_att_acc ON attendance(account_id, term_id);
CREATE INDEX IF NOT EXISTS idx_att_time ON attendance(account_id, open_time DESC);

CREATE TABLE IF NOT EXISTS executions (
  id          INTEGER PRIMARY KEY AUTOINCREMENT,
  account_id  INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
  job_key     TEXT NOT NULL,
  biz_date    TEXT NOT NULL,
  state       TEXT NOT NULL,
  detail      TEXT NOT NULL DEFAULT '{}',
  started_at  INTEGER NOT NULL DEFAULT 0,
  finished_at INTEGER NOT NULL DEFAULT 0,
  created_at  INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_exec_acc_date ON executions(account_id, biz_date);

CREATE TABLE IF NOT EXISTS logs (
  id           INTEGER PRIMARY KEY AUTOINCREMENT,
  execution_id INTEGER NOT NULL REFERENCES executions(id) ON DELETE CASCADE,
  level        TEXT NOT NULL DEFAULT 'info',
  message      TEXT NOT NULL,
  ts           INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_logs_exec ON logs(execution_id, id);

CREATE TABLE IF NOT EXISTS daily_plans (
  id         INTEGER PRIMARY KEY AUTOINCREMENT,
  account_id INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
  biz_date   TEXT NOT NULL,
  fire_at    INTEGER NOT NULL,
  action     TEXT NOT NULL DEFAULT '{}',
  state      TEXT NOT NULL,
  detail     TEXT NOT NULL DEFAULT '',
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  UNIQUE(account_id, biz_date)
);
CREATE INDEX IF NOT EXISTS idx_plans_due ON daily_plans(state, fire_at);
"#;

/// 打开（必要时创建）数据库，并跑迁移。
pub fn open(path: impl AsRef<Path>) -> Result<Connection> {
    let conn = Connection::open(path)?;
    init(&conn)?;
    Ok(conn)
}

/// 打开一个**内存库**（测试用）。
pub fn open_memory() -> Result<Connection> {
    let conn = Connection::open_in_memory()?;
    init(&conn)?;
    Ok(conn)
}

/// 建表 + 迁移 + 打开必要 pragma。
pub fn init(conn: &Connection) -> Result<()> {
    // WAL：读不阻塞写，配合多 worker 线程
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.pragma_update(None, "busy_timeout", 5000)?;
    conn.execute_batch(SCHEMA)?;
    migrate(conn)?;
    Ok(())
}

/// 迁移：补 `CREATE TABLE IF NOT EXISTS` 覆盖不到的列。
///
/// ⚠️ 这一层不是可选的 —— 老库里的表已经存在，`IF NOT EXISTS` 会整段跳过，
/// 新加的列**永远不会出现**。本项目为此踩过两次，都是「测试全绿但线上是坏的」。
pub fn migrate(conn: &Connection) -> Result<usize> {
    let mut added = 0;
    // attendance 的定位坐标三列（Python 版补过同样的一次）
    //
    // ⚠️ 表不存在时不能 PRAGMA 也不能 ALTER —— 半初始化的库会遇到。
    let has_attendance: bool = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name='attendance'",
            [],
            |_| Ok(true),
        )
        .unwrap_or(false);
    let cols: Vec<String> = if has_attendance {
        let mut stmt = conn.prepare("PRAGMA table_info(attendance)")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(1))?;
        rows.filter_map(|r| r.ok()).collect()
    } else {
        Vec::new()
    };
    if has_attendance {
        for (name, decl) in [
            ("longitude", "TEXT NOT NULL DEFAULT ''"),
            ("latitude", "TEXT NOT NULL DEFAULT ''"),
            ("radius", "INTEGER"),
        ] {
            if !cols.iter().any(|c| c == name) {
                conn.execute(&format!("ALTER TABLE attendance ADD COLUMN {name} {decl}"), [])?;
                added += 1;
            }
        }
    }

    // accounts 的班级列。
    //
    // ⚠️ **必须单独 ALTER** —— `CREATE TABLE IF NOT EXISTS` 不改已存在的表。
    let has_accounts: bool = conn
        .query_row("SELECT 1 FROM sqlite_master WHERE type='table' AND name='accounts'", [], |_| Ok(true))
        .unwrap_or(false);
    if has_accounts {
        let acols: Vec<String> = {
            let mut stmt = conn.prepare("PRAGMA table_info(accounts)")?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(1))?;
            rows.filter_map(|r| r.ok()).collect()
        };
        if !acols.iter().any(|c| c == "dept") {
            conn.execute("ALTER TABLE accounts ADD COLUMN dept TEXT NOT NULL DEFAULT ''", [])?;
            added += 1;
        }
    }

    // accounts 的 device_id 列（模拟设备 ID，可选）
    let has_accounts_dev: bool = conn
        .query_row("SELECT 1 FROM sqlite_master WHERE type='table' AND name='accounts'", [], |_| Ok(true))
        .unwrap_or(false);
    if has_accounts_dev {
        let acols: Vec<String> = {
            let mut stmt = conn.prepare("PRAGMA table_info(accounts)")?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(1))?;
            rows.filter_map(|r| r.ok()).collect()
        };
        if !acols.iter().any(|c| c == "device_id") {
            conn.execute("ALTER TABLE accounts ADD COLUMN device_id TEXT NOT NULL DEFAULT ''", [])?;
            added += 1;
        }
    }

    // accounts.device_id_enabled 列（是否带模拟设备 ID）
    if conn
        .query_row("SELECT 1 FROM sqlite_master WHERE type='table' AND name='accounts'", [], |_| Ok(true))
        .unwrap_or(false)
    {
        let acols: Vec<String> = {
            let mut stmt = conn.prepare("PRAGMA table_info(accounts)")?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(1))?;
            rows.filter_map(|r| r.ok()).collect()
        };
        if !acols.iter().any(|c| c == "device_id_enabled") {
            conn.execute(
                "ALTER TABLE accounts ADD COLUMN device_id_enabled INTEGER NOT NULL DEFAULT 0",
                [],
            )?;
            added += 1;
        }
    }

    // resources 的考核相关列。
    //
    // ⚠️ **必须单独 ALTER** —— `CREATE TABLE IF NOT EXISTS` 不改已存在的表。
    //    本项目为此踩过两次，两次都是「测试全绿但线上是坏的」。
    // ⚠️ 表可能**不存在**（半初始化的库）。`PRAGMA table_info(不存在的表)`
    // 返回空结果集而不是报错，但 `prepare` 之后再 `query` 会失败 ——
    // 所以先确认表在不在。原先没这一步，迁移会直接 panic。
    let has_resources: bool = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name='resources'",
            [],
            |_| Ok(true),
        )
        .unwrap_or(false);
    let rcols: Vec<String> = if has_resources {
        let mut stmt = conn.prepare("PRAGMA table_info(resources)")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(1))?;
        rows.filter_map(|r| r.ok()).collect()
    } else {
        Vec::new()
    };
    // ⚠️ 表不存在时**不能**去 ALTER —— 会报 "no such table"。
    // 光判断「列不在 rcols 里」是不够的：空表时所有列都不在。
    if has_resources {
        for (name, decl) in [
            ("is_assess", "INTEGER NOT NULL DEFAULT 0"),
            ("open_time", "INTEGER NOT NULL DEFAULT 0"),
            ("close_time", "INTEGER"),
            ("res_desc", "TEXT NOT NULL DEFAULT ''"),
        ] {
            if !rcols.iter().any(|c| c == name) {
                conn.execute(&format!("ALTER TABLE resources ADD COLUMN {name} {decl}"), [])?;
                added += 1;
            }
        }
    }
    Ok(added)
}

/// 事务包装：`BEGIN IMMEDIATE`。
///
/// 用 IMMEDIATE 而不是默认的 DEFERRED —— 后者要等第一次写才拿锁，
/// 多线程下容易在「读后写」的间隙里撞上 `SQLITE_BUSY`。
pub fn with_tx<T>(conn: &Mutex<Connection>, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
    let guard = conn.lock().expect("连接锁中毒");
    guard.execute_batch("BEGIN IMMEDIATE")?;
    match f(&guard) {
        Ok(v) => {
            guard.execute_batch("COMMIT")?;
            Ok(v)
        }
        Err(e) => {
            let _ = guard.execute_batch("ROLLBACK");
            Err(e)
        }
    }
}

/// 当前毫秒时间戳。
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// 北京时间当天（YYYY-MM-DD）。
///
/// ⚠️ 容器时区是 UTC，**必须显式偏移** —— 直接用本地时间会差 8 小时，
/// 「当日」的边界就错了（幂等键、每日计划全受影响）。
pub fn biz_date_now() -> String {
    biz_date_of(now_ms())
}

pub fn biz_date_of(ms: i64) -> String {
    const CST_OFFSET_SEC: i64 = 8 * 3600;
    let secs = ms / 1000 + CST_OFFSET_SEC;
    let days = secs.div_euclid(86400);
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}")
}

/// 天数 → (年, 月, 日)。Howard Hinnant 的 `civil_from_days`。
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_applies_cleanly() {
        let c = open_memory().expect("内存库应能建起来");
        let n: i64 = c
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 10, "应与 Python 版一样有 10 张表");
    }

    /// **回归**：`resources` 的考核相关列也要能迁移到老库上。
    ///
    /// 这一类踩过三次（`executions` 的 UNIQUE、`attendance` 的定位三列、
    /// 现在的 `resources.is_assess`）—— 每次都是「测试全绿但线上是坏的」，
    /// 因为 `CREATE TABLE IF NOT EXISTS` **不改已存在的表**。
    #[test]
    fn migrate_adds_resources_assess_columns() {
        let c = Connection::open_in_memory().unwrap();
        // 模拟加列之前的老 resources 表
        c.execute_batch(
            "CREATE TABLE resources (
               id INTEGER PRIMARY KEY AUTOINCREMENT,
               account_id INTEGER NOT NULL, course_pk INTEGER NOT NULL,
               res_id TEXT NOT NULL, name TEXT NOT NULL DEFAULT '',
               res_type INTEGER NOT NULL DEFAULT 0, res_ext TEXT NOT NULL DEFAULT '',
               study_status INTEGER NOT NULL DEFAULT 0, play_progress INTEGER,
               updated_at INTEGER NOT NULL DEFAULT 0,
               UNIQUE(account_id, res_id))",
        )
        .unwrap();
        let added = migrate(&c).unwrap();
        assert_eq!(added, 4, "应补上 is_assess / open_time / close_time / res_desc");
        let cols: Vec<String> = {
            let mut s = c.prepare("PRAGMA table_info(resources)").unwrap();
            let rows = s.query_map([], |r| r.get::<_, String>(1)).unwrap();
            rows.filter_map(|r| r.ok()).collect()
        };
        for want in ["is_assess", "open_time", "close_time", "res_desc"] {
            assert!(cols.iter().any(|c| c == want), "缺列 {want}");
        }
        // 再跑一次应该是幂等的（加 0 列）
        assert_eq!(migrate(&c).unwrap(), 0, "迁移必须幂等");
    }

    /// 表不存在时迁移不能崩 —— 半初始化的库会遇到。
    #[test]
    fn migrate_tolerates_missing_tables() {
        let c = Connection::open_in_memory().unwrap();
        assert!(migrate(&c).is_ok(), "空库里跑迁移不该报错");
    }

    #[test]
    fn migrate_adds_missing_columns() {
        // **回归**：CREATE TABLE IF NOT EXISTS 不改已存在的表。
        // 模拟一张缺列的老表，确认迁移把它补齐。
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(
            "CREATE TABLE attendance (
               id INTEGER PRIMARY KEY AUTOINCREMENT,
               account_id INTEGER NOT NULL, term_id TEXT NOT NULL,
               detail_id TEXT NOT NULL, attendance_id TEXT NOT NULL DEFAULT '',
               title TEXT NOT NULL DEFAULT '', course_name TEXT NOT NULL DEFAULT '',
               attendance_type INTEGER NOT NULL DEFAULT 0, status INTEGER,
               open_time INTEGER NOT NULL DEFAULT 0, finish_time INTEGER NOT NULL DEFAULT 0,
               sign_role INTEGER, sign_time INTEGER, location_type INTEGER,
               teach_class_id TEXT NOT NULL DEFAULT '', address_name TEXT NOT NULL DEFAULT '',
               teach_course_id TEXT NOT NULL DEFAULT '', scanned_at INTEGER NOT NULL,
               UNIQUE(account_id, detail_id))",
        )
        .unwrap();
        let added = migrate(&c).unwrap();
        assert_eq!(added, 3, "应补上 longitude / latitude / radius 三列");
        let cols: Vec<String> = {
            let mut s = c.prepare("PRAGMA table_info(attendance)").unwrap();
            let rows = s.query_map([], |r| r.get::<_, String>(1)).unwrap();
            rows.filter_map(|r| r.ok()).collect()
        };
        for want in ["longitude", "latitude", "radius"] {
            assert!(cols.iter().any(|c| c == want), "缺列 {want}");
        }
        // 幂等：再跑一次不该重复加
        assert_eq!(migrate(&c).unwrap(), 0);
    }

    #[test]
    fn biz_date_uses_beijing_not_utc() {
        // 2026-09-28T16:30:00Z —— UTC 还是 28 号，北京已是 29 号凌晨
        let ms = 1_790_680_200_000;
        assert_eq!(biz_date_of(ms), "2026-09-29", "必须按北京时间算当日");
        // 同一时刻按 UTC 会是 28 号
        assert_ne!(biz_date_of(ms), "2026-09-28");
    }

    #[test]
    fn biz_date_boundaries() {
        // 北京 00:00:00 == UTC 前一天 16:00
        assert_eq!(biz_date_of(0), "1970-01-01");
        // 北京 1970-01-01 07:59:59 == UTC 前一天 23:59:59
        assert_eq!(biz_date_of(8 * 3600 * 1000 - 1000), "1970-01-01");
        assert_eq!(biz_date_of(8 * 3600 * 1000), "1970-01-01");
    }
}
