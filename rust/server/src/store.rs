//! 业务查询层。
//!
//! 从 Python 版 `server/store.py` 移植。几条贯穿全层的规矩：
//!
//! * **写操作走事务**（[`crate::db::with_tx`]，`BEGIN IMMEDIATE`）
//! * **幂等靠 UNIQUE + `ON CONFLICT`**，不靠「先查再写」——后者在并发下会漏
//! * 时间戳一律 `now_ms()`（毫秒），与 Python 版一致

use rusqlite::{params, Connection, OptionalExtension, Row};

use crate::db::{self, Result};

/// 账号行。
#[derive(Debug, Clone)]
pub struct AccountRow {
    pub id: i64,
    pub label: String,
    pub username: String,
    pub tenant_id: String,
    pub user_id: String,
    pub state: String,
    pub note: String,
    /// 班级（来自上游 `deptName`）
    pub dept: String,
    /// 模拟设备 ID（UUID）。留空 = 还没生成过。
    pub device_id: String,
    /// **是否在签到里带 device_id**（与 device_id 解耦）。
    pub device_id_enabled: bool,
    pub token_expires: i64,
}

fn row_to_account(r: &Row<'_>) -> rusqlite::Result<AccountRow> {
    Ok(AccountRow {
        id: r.get("id")?,
        label: r.get("label")?,
        username: r.get("username")?,
        tenant_id: r.get("tenant_id")?,
        user_id: r.get::<_, Option<String>>("user_id")?.unwrap_or_default(),
        state: r.get("state")?,
        note: r.get("note")?,
        dept: r.get::<_, Option<String>>("dept")?.unwrap_or_default(),
        device_id: r.get::<_, Option<String>>("device_id")?.unwrap_or_default(),
        device_id_enabled: r.get::<_, Option<i64>>("device_id_enabled")?.unwrap_or(0) != 0,
        token_expires: r.get("token_expires")?,
    })
}

const ACC_COLS: &str = "id, label, username, tenant_id, user_id, state, note, dept, device_id, device_id_enabled, token_expires";

pub fn list_accounts(conn: &Connection) -> Result<Vec<AccountRow>> {
    let mut stmt = conn.prepare(&format!("SELECT {ACC_COLS} FROM accounts ORDER BY id"))?;
    let rows = stmt.query_map([], row_to_account)?;
    rows.collect()
}

pub fn get_account(conn: &Connection, id: i64) -> Result<Option<AccountRow>> {
    conn.query_row(
        &format!("SELECT {ACC_COLS} FROM accounts WHERE id=?"),
        params![id],
        row_to_account,
    )
    .optional()
}

/// 新建账号，返回 id。
pub fn create_account(
    conn: &Connection,
    label: &str,
    username: &str,
    password_enc: &[u8],
    tenant_id: &str,
) -> Result<i64> {
    let now = db::now_ms();
    conn.execute(
        "INSERT INTO accounts (label, username, password_enc, tenant_id, note, created_at, updated_at)
         VALUES (?,?,?,?,'',?,?)",
        params![label, username, password_enc, tenant_id, now, now],
    )?;
    Ok(conn.last_insert_rowid())
}

/// 取账号凭据与 token（登录用）。
pub fn account_tokens(conn: &Connection, id: i64) -> Result<(String, String, String, i64)> {
    conn.query_row(
        "SELECT user_id, tenant_id, access_token, token_expires FROM accounts WHERE id=?",
        params![id],
        |r| {
            Ok((
                r.get::<_, Option<String>>(0)?.unwrap_or_default(),
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?.unwrap_or_default(),
                r.get::<_, i64>(3)?,
            ))
        },
    )
}

/// 写回登录后的 token。
pub fn save_session(
    conn: &Connection,
    id: i64,
    user_id: &str,
    tenant_id: &str,
    access_token: &str,
    refresh_token: &str,
    expires: i64,
) -> Result<()> {
    // 🔴 **绝不能把空的 tenant_id / user_id 写回去** —— 那是**删数据**。
    //
    // 本项目实测栽过一次：4 个账号的 tenant_id 被清空，症状是
    // 「请求的租户标识未传递」(400)，而接口报的是**参数错**，
    // 看起来像上游的毛病，实际是本地库被自己写坏了。
    //
    // 触发路径：`persist_session` 把会话里的 tenant_id 写回账号，
    // 而会话在某些路径下 tenant_id 为空（例如只 load 了 token 没重新登录）。
    //
    // 用 `NULLIF(?, '')` + `COALESCE` 表达「空值就保持原样」。
    conn.execute(
        "UPDATE accounts SET
           user_id      = COALESCE(NULLIF(?, ''), user_id),
           tenant_id    = COALESCE(NULLIF(?, ''), tenant_id),
           access_token = COALESCE(NULLIF(?, ''), access_token),
           refresh_token= COALESCE(NULLIF(?, ''), refresh_token),
           token_expires= CASE WHEN ?>0 THEN ? ELSE token_expires END,
           state='active', last_login_at=?, updated_at=? WHERE id=?",
        params![
            user_id, tenant_id, access_token, refresh_token,
            expires, expires, db::now_ms(), db::now_ms(), id
        ],
    )?;
    Ok(())
}

/// 账号密码密文（解密后交给协议层）。
pub fn account_password_enc(conn: &Connection, id: i64) -> Result<Vec<u8>> {
    conn.query_row("SELECT password_enc FROM accounts WHERE id=?", params![id], |r| r.get(0))
}

// ---------------------------------------------------------------- 账号配置

/// 读 `accounts.note` 里的某段 JSON 配置。
pub fn read_account_cfg(conn: &Connection, id: i64, key: &str) -> Result<serde_json::Value> {
    let note: String = conn
        .query_row("SELECT note FROM accounts WHERE id=?", params![id], |r| r.get(0))
        .optional()?
        .unwrap_or_default();
    if !note.starts_with('{') {
        return Ok(serde_json::json!({}));
    }
    let v: serde_json::Value = serde_json::from_str(&note).unwrap_or(serde_json::json!({}));
    Ok(v.get(key).cloned().unwrap_or(serde_json::json!({})))
}

/// 写 `accounts.note` 里的某段配置，**不动其它段**。
pub fn write_account_cfg(
    conn: &Connection,
    id: i64,
    key: &str,
    cfg: serde_json::Value,
) -> Result<()> {
    let note: String = conn
        .query_row("SELECT note FROM accounts WHERE id=?", params![id], |r| r.get(0))
        .optional()?
        .unwrap_or_default();
    let mut data: serde_json::Map<String, serde_json::Value> = if note.starts_with('{') {
        serde_json::from_str(&note).unwrap_or_default()
    } else {
        let mut m = serde_json::Map::new();
        if !note.is_empty() {
            m.insert("text".into(), serde_json::Value::String(note));
        }
        m
    };
    data.insert(key.to_string(), cfg);
    conn.execute(
        "UPDATE accounts SET note=?, updated_at=? WHERE id=?",
        params![
            serde_json::Value::Object(data).to_string(),
            db::now_ms(),
            id
        ],
    )?;
    Ok(())
}

// ---------------------------------------------------------------- 执行

/// 开始一次执行。返回 `None` 表示该账号**同一 job 已在跑**（不是「当天跑过」）。
///
/// ⚠️ 判据是「同账号 + 同 job + 处于 running」，**不含 biz_date** ——
/// 曾经把「每天一次」写进这里，结果刷课被误当成打卡，同一天第二次直接被挡。
pub fn begin_execution(conn: &Connection, account_id: i64, job_key: &str) -> Result<Option<i64>> {
    let running: Option<i64> = conn
        .query_row(
            "SELECT id FROM executions WHERE account_id=? AND job_key=? AND state='running' LIMIT 1",
            params![account_id, job_key],
            |r| r.get(0),
        )
        .optional()?;
    if running.is_some() {
        return Ok(None);
    }
    let now = db::now_ms();
    conn.execute(
        "INSERT INTO executions (account_id, job_key, biz_date, state, detail, started_at, created_at)
         VALUES (?,?,?,'running','{}',?,?)",
        params![account_id, job_key, db::biz_date_now(), now, now],
    )?;
    Ok(Some(conn.last_insert_rowid()))
}

/// 收尾一次执行。**失败也保留记录** —— 删掉会让失败彻底不可见。
pub fn finish_execution(
    conn: &Connection,
    exec_id: i64,
    state: &str,
    detail: serde_json::Value,
) -> Result<()> {
    conn.execute(
        "UPDATE executions SET state=?, detail=?, finished_at=? WHERE id=?",
        params![state, detail.to_string(), db::now_ms(), exec_id],
    )?;
    Ok(())
}

/// 把上次进程退出时残留的 `running` 标成 `failed`。
///
/// ⚠️ 必须标 `failed` 而不是 `pending` —— 标成 `pending` 会继续占着那个 job 的
/// 「已在跑」名额，于是重启后该账号再也发不起同类任务。
pub fn reset_stale_running(conn: &Connection) -> Result<usize> {
    Ok(conn.execute(
        "UPDATE executions SET state='failed', detail='{\"error\":\"进程重启，执行中断\"}',
         finished_at=? WHERE state='running'",
        params![db::now_ms()],
    )?)
}

/// 追加一条执行日志。
pub fn append_log(conn: &Connection, exec_id: i64, level: &str, message: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO logs (execution_id, level, message, ts) VALUES (?,?,?,?)",
        params![exec_id, level, message, db::now_ms()],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> Connection {
        let c = db::open_memory().unwrap();
        create_account(&c, "甲", "u1", b"enc", "t1").unwrap();
        c
    }

    /// **回归**：`append_log` 的第一参是 execution_id，不是 account_id。
    ///
    /// 传成 account_id 时外键会失败 —— 如果调用方用 `let _ =` 吞掉错误，
    /// 表现就是「执行有结果、日志表却是空的」，很难查。
    #[test]
    fn append_log_uses_execution_id_not_account_id() {
        let c = setup();
        // 账号 id 是 1，但 execution id 从 1 开始也是 1 —— 造一个错位的场景：
        // 先建一条属于账号 1 的执行，拿到它的 id
        let exec = begin_execution(&c, 1, "scan").unwrap().unwrap();
        append_log(&c, exec, "ok", "正常写入").unwrap();

        // 再建一个账号，其 account_id 与任何 execution id 都不同
        let a2 = create_account(&c, "乙", "u2", b"enc", "t1").unwrap();
        assert_ne!(a2, exec);
        // 用 account_id 当 execution_id 传 → 必须报错，而不是静默成功
        assert!(
            append_log(&c, a2, "ok", "错位").is_err(),
            "外键应挡住错位的 id"
        );

        let n: i64 = c
            .query_row("SELECT count(*) FROM logs WHERE execution_id=?", params![exec], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1, "正确的 execution_id 应写入一行");
    }

    #[test]
    fn begin_execution_blocks_same_job_only() {
        let c = setup();
        let a = begin_execution(&c, 1, "study").unwrap();
        assert!(a.is_some());
        // 同账号同 job 再开 → 被挡（防重复点击）
        assert!(begin_execution(&c, 1, "study").unwrap().is_none());
        // **不同 job 不受影响**
        assert!(begin_execution(&c, 1, "sign").unwrap().is_some());
        // 收尾后可以再开 —— **没有「每天一次」的限制**
        finish_execution(&c, a.unwrap(), "done", serde_json::json!({})).unwrap();
        assert!(begin_execution(&c, 1, "study").unwrap().is_some(), "刷课随时可以再跑");
    }

    #[test]
    fn reset_stale_marks_failed_not_pending() {
        let c = setup();
        begin_execution(&c, 1, "study").unwrap();
        assert_eq!(reset_stale_running(&c).unwrap(), 1);
        let st: String = c
            .query_row("SELECT state FROM executions WHERE account_id=1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(st, "failed", "标成 pending 会继续占着名额，该账号再也发不起任务");
        // 且现在能重新发起
        assert!(begin_execution(&c, 1, "study").unwrap().is_some());
    }

    #[test]
    fn account_cfg_round_trip_keeps_other_keys() {
        let c = setup();
        write_account_cfg(&c, 1, "schedule", serde_json::json!({"enabled": true, "time": "08:00"}))
            .unwrap();
        write_account_cfg(&c, 1, "auto_sign", serde_json::json!({"enabled": true})).unwrap();
        // 写第二段不能冲掉第一段
        let s = read_account_cfg(&c, 1, "schedule").unwrap();
        assert_eq!(s["time"], "08:00");
        let a = read_account_cfg(&c, 1, "auto_sign").unwrap();
        assert_eq!(a["enabled"], true);
    }

    /// **回归**：写会话时**不能把空的 tenant_id / user_id 覆盖掉原值**。
    ///
    /// 这是**删数据**级别的 bug：本项目实测把 4 个账号的 tenant_id 清空，
    /// 症状是「请求的租户标识未传递」(400) —— 报的是参数错，
    /// 看起来像上游的毛病，实际是本地库被自己写坏了。
    #[test]
    fn save_session_never_wipes_tenant_or_user() {
        let c = setup();
        save_session(&c, 1, "u9", "TENANT-9", "AT", "RT", 9_999_999_999_999).unwrap();
        // 再写一次「空会话」（模拟只 load 了 token 没重新登录的路径）
        save_session(&c, 1, "", "", "AT2", "", 0).unwrap();
        let (uid, tid, at, exp) = account_tokens(&c, 1).unwrap();
        assert_eq!(tid, "TENANT-9", "空的 tenant_id 不能覆盖原值");
        assert_eq!(uid, "u9", "空的 user_id 不能覆盖原值");
        assert_eq!(at, "AT2", "非空的 token 应正常更新");
        assert!(exp > 0, "expires<=0 不该覆盖原过期时间");
    }

    #[test]
    fn session_round_trip() {
        let c = setup();
        save_session(&c, 1, "u9", "t9", "AT", "RT", 9_999_999_999_999).unwrap();
        let (uid, tid, at, exp) = account_tokens(&c, 1).unwrap();
        assert_eq!((uid.as_str(), tid.as_str(), at.as_str()), ("u9", "t9", "AT"));
        assert!(exp > 0);
        let a = get_account(&c, 1).unwrap().unwrap();
        assert_eq!(a.state, "active");
    }
}
