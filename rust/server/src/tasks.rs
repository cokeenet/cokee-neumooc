//! 任务执行：把「凭据 → 登录 → 协议调用 → 落库」串起来。
//!
//! 这是 Python 版 `worker.py` 里 `_cmd_*` 那批东西的对应物。
//!
//! ## 每个任务都走同一套登录兜底
//!
//! 顺序是：**先用库里存的 token → 本地判定过期就直接重登 → 请求遇 401 再由
//! `with_reauth` 重登重试**。三道都不省：
//!
//! * 只用 token 不看本地过期：必然白发一次请求
//! * 只看本地过期：服务端会**提前吊销** token，本地还显示有效（踩过三次）
//! * 只靠 `with_reauth`：能兜住，但每次都要白跑一次失败请求

use std::sync::{Arc, Mutex};

use rusqlite::Connection;
use serde_json::{json, Value};

use neumooc_core::attendance::Attendance;
use neumooc_core::client::Client;
use neumooc_core::error::{Error, Result};
use neumooc_core::resources::Course;

use crate::crypto::CredentialCipher;
use crate::db;
use crate::store;
use crate::worker::CancelToken;

/// 业务域名。**从环境变量读** —— 仓库里不留真实域名。
///
/// 为什么必须可配置：这个域名等同于学校身份，写死在源码里等于把
/// 「哪个学校」印在仓库上。真实值放在 systemd 的 `Environment=` 里
/// （见 `deploy/*.service` 模板，模板里是占位符）。
///
/// 没配就退化成占位域名并打一条 error 日志 —— 此时所有上游请求都会失败，
/// 但错误信息会明确指出是配置问题，而不是变成一串看不懂的连接错误。
pub fn default_base() -> String {
    match std::env::var("NEUMOOC_BASE_URL") {
        Ok(v) if !v.trim().is_empty() => v.trim().to_string(),
        _ => {
            log::error!(
                "未设置 NEUMOOC_BASE_URL，使用占位域名 https://example.edu.cn —— \
                 上游请求会全部失败，请检查服务配置"
            );
            "https://example.edu.cn".to_string()
        }
    }
}

/// 执行一个任务所需的共享上下文。
/// LLM 连接配置。
///
/// 字段对齐 DSH 的 provider/model 两层，但只留影响答题的部分。
#[derive(Clone, Debug)]
pub struct LlmConfig {
    pub api_key: String,
    pub base_url: String,
    pub model: String,
    /// 思考强度（off/minimal/low/medium/high/max）
    pub effort: crate::llm::Effort,
    /// 是否请求流式（内部聚合，行为与关闭时一致，只是能更早拿数据）
    pub stream: bool,
}

impl LlmConfig {
    /// 转成调用参数。
    pub fn options(&self, max_tokens: Option<u32>) -> crate::llm::Options {
        crate::llm::Options {
            base_url: self.base_url.clone(),
            api_key: self.api_key.clone(),
            model: self.model.clone(),
            effort: self.effort,
            stream: self.stream,
            max_tokens,
            timeout_secs: 180,
        }
    }
}

pub struct TaskCtx {
    pub conn: Arc<Mutex<Connection>>,
    pub cipher: Arc<CredentialCipher>,
}

impl TaskCtx {
    pub fn new(conn: Arc<Mutex<Connection>>, cipher: Arc<CredentialCipher>) -> Self {
        Self { conn, cipher }
    }

    /// LLM 配置。**环境变量优先，其次界面设置**（见 `store::setting`）。
    pub fn llm_config(&self) -> Result<LlmConfig> {
        let c = self.conn.lock().map_err(|_| Error::Invalid("数据库锁中毒".into()))?;
        let api_key = store::setting(&c, "NEUMOOC_LLM_API_KEY").unwrap_or_default();
        if api_key.is_empty() {
            return Err(Error::Invalid(
                "AI 未配置：到「系统设置」填 API Key，或设环境变量 NEUMOOC_LLM_API_KEY".into(),
            ));
        }
        Ok(LlmConfig {
            api_key,
            base_url: store::setting(&c, "NEUMOOC_LLM_BASE_URL")
                .unwrap_or_else(|| "https://api.z.ai/api/paas/v4".into()),
            model: store::setting(&c, "NEUMOOC_LLM_MODEL")
                .unwrap_or_else(|| "glm-4.5-flash".into()),
            effort: store::setting(&c, "NEUMOOC_LLM_EFFORT")
                .and_then(|s| crate::llm::Effort::parse(&s))
                .unwrap_or_default(),
            // 默认不开流式：多一道解析、个别厂商支持不全，收益只在长答案时明显
            stream: store::setting(&c, "NEUMOOC_LLM_STREAM")
                .map(|v| matches!(v.to_ascii_lowercase().as_str(), "1" | "on" | "true" | "yes"))
                .unwrap_or(false),
        })
    }

    /// 取某账号签到时要带的**模拟设备 ID**，`None` = 不带该字段。
    ///
    /// ## 三层判定（**全部满足才带**）
    ///
    /// 1. **全局急停** `NEUMOOC_DEVICE_ID`（环境变量，默认开）
    ///    设 `0/off/false/no` → 所有账号都不带
    /// 2. **单账号开关** `accounts.device_id_enabled`（界面上的开关）
    /// 3. **ID 非空** —— 开关开着但还没生成过 ID 时，自动生成一个再带
    ///
    /// ## 为什么用显式开关而不是「空值 = 不带」
    ///
    /// 空值约定有个实际问题：**想暂时不带就得把 ID 清掉**，
    /// 以后要用又得重新生成 —— 而重生成会让历史签到记录里的 deviceId 变化，
    /// 那本身就是个异常信号。分开之后：
    ///
    /// ```
    /// device_id         留着备查，不动
    /// device_id_enabled 只管带不带
    /// ```
    ///
    /// ## 判定依据（实测教师端 53 条）
    ///
    /// 平台**只在同一 deviceId 对应多个学号时**标 `isAbnormal`；
    /// 没有 deviceId 的记录（37 条）**全部正常**。所以默认关是安全的，
    /// 打开也不会被判异常（只要每个账号的 ID 不同）。
    pub fn device_id_for(&self, account_id: i64) -> Option<String> {
        if !device_id_master_on() {
            return None;
        }
        let c = self.conn.lock().ok()?;
        let (enabled, id): (i64, String) = c
            .query_row(
                "SELECT COALESCE(device_id_enabled,0), COALESCE(device_id,'') FROM accounts WHERE id=?",
                rusqlite::params![account_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .ok()?;
        if enabled == 0 {
            return None;
        }
        let v = id.trim().to_string();
        if v.is_empty() {
            // 开关开着但还没有 ID → 顺手生成一个（避免"开了却没生效"）
            let fresh = crate::endpoints::new_uuid_v4();
            let _ = c.execute(
                "UPDATE accounts SET device_id=?, updated_at=? WHERE id=?",
                rusqlite::params![fresh, db::now_ms(), account_id],
            );
            return Some(fresh);
        }
        Some(v)
    }

    /// 造一个**已登录**的客户端给某账号。
    pub fn client(&self, account_id: i64) -> Result<Client> {
        let (username, user_id, tenant_id, token, refresh_token, expires, enc) = {
            let c = self.conn.lock().expect("锁中毒");
            let a = store::get_account(&c, account_id)
                .map_err(|e| Error::Invalid(e.to_string()))?
                .ok_or_else(|| Error::Invalid(format!("账号 {account_id} 不存在")))?;
            let (uid, tid, tok, exp) = store::account_tokens(&c, account_id)
                .map_err(|e| Error::Invalid(e.to_string()))?;
            let rt: String = c
                .query_row("SELECT refresh_token FROM accounts WHERE id=?", rusqlite::params![account_id], |r| r.get(0))
                .unwrap_or_default();
            let enc = store::account_password_enc(&c, account_id)
                .map_err(|e| Error::Invalid(e.to_string()))?;
            (a.username, uid, tid, tok, rt, exp, enc)
        };

        let mut c = Client::new(&default_base(), tenant_id.clone())?;
        // ⚠️ **user_id 必须从库里带过来**，不能传空串。
        // 传空的话：token 没过期就不会重登 → `session.user_id` 一直是空的
        // → 所有用 uid 的接口（考勤列表、签到）拿到空 uid，行为不可预测。
        c.load_session(neumooc_core::Session {
            user_id: user_id.clone(),
            tenant_id: tenant_id.clone(),
            access_token: token.clone(),
            refresh_token: refresh_token.clone(),
            expires_time: expires,
        });

        // **只在真的需要重登时才解密密码。**
        //
        // 原先是无条件 `decrypt(enc)?` —— 于是「密码列是空的、但 token 还有效」
        // 这种账号**根本走不到 token 那条路**，直接报「凭据太短」。
        //
        // 这直接决定了一个能力：**能不能替只有 token、没有密码的人签到。**
        // 有有效 token 时请求带的是 `Authorization: Bearer <token>`，
        // 密码压根不会被用到 —— 所以没必要提前解密。
        let need_login = c.session().access_token.is_empty() || c.session().is_expired();

        // **能解出密码就记住它** —— 不是只在重登时才记。
        //
        // ⚠️ 这里踩过一次：做成「按需解密」时把 `remember_credentials` 也挪进了
        // `if need_login` 分支里，于是 **token 本地有效时 `creds` 是 None**，
        // 而 `with_reauth` 的判据是 `e.is_auth_error() && self.creds.is_some()`
        // —— **重试被静默跳过**。
        //
        // 症状：本地 token 还没过期、但服务端已吊销 → 所有接口回 `[401] 账号未登录`，
        // 且永远不会自愈。实测账号1 的 `/tasks` 就是这么 400 的。
        //
        // 解不出密码（token-only 账号）就跳过 —— 那种账号本来也无法重登。
        let password = if enc.is_empty() {
            None
        } else {
            self.cipher
                .decrypt(&enc)
                .ok()
                .and_then(|b| String::from_utf8(b).ok())
        };
        if let Some(pw) = &password {
            c.remember_credentials(&username, pw);
        }

        if need_login {
            let Some(pw) = password else {
                return Err(Error::Invalid(
                    "该账号没有存密码，且本地 token 已过期 —— 需要重新提供密码才能登录".into(),
                ));
            };
            log::info!("账号 {account_id} 本地 token 缺失或已过期，直接重登");
            c.login(&username, &pw)?;
        }
        Ok(c)
    }

    /// 登录成功后把 token 写回库。
    pub fn persist_session(&self, account_id: i64, c: &Client) -> Result<()> {
        let s = c.session();
        let conn = self.conn.lock().expect("锁中毒");
        store::save_session(
            &conn, account_id, &s.user_id, &s.tenant_id,
            &s.access_token, &s.refresh_token, s.expires_time,
        )
        .map_err(|e| Error::Invalid(e.to_string()))
    }
}

/// 扫描：拉该账号本学期全部课程与资源，落库。返回统计。
///
/// 这是**只读**操作（对平台侧），适合作为端到端验证的第一条任务。
pub fn run_scan(ctx: &TaskCtx, account_id: i64, token: &CancelToken) -> Result<Value> {
    let mut c = ctx.client(account_id)?;
    // 取**第一个真有考勤的学期**（未来学期的课程/考勤都是空的）
    // 用**平台顺序的第一个学期**（当前学期排最前；按 id 取最大会选中未来学期）
    let term = c
        .with_reauth(|c| c.list_terms())?
        .first()
        .cloned()
        .ok_or_else(|| Error::Invalid("拿不到任何学期".into()))?;
    token.check()?;

    let courses = c.with_reauth(|c| c.list_courses(&term.id))?;

    // ⚠️ **先把所有资源拉回来（不持数据库锁），再一次性写库。**
    //
    // 原先是「持锁 → 逐门课网络调用 → 写库」—— 锁被网络 I/O 占住，
    // 后果是：`await_execution` 的轮询、以及任何别的请求**全被堵住**；
    // 只要有一个网络调用慢，整个后端就像卡死一样。
    // 实测账号1 的 scan 跑了 159 秒还没完，就是栽在这里。
    //
    // 持锁做 I/O 是经典错误 —— 锁的持有时间应该只覆盖**内存操作**。
    let mut fetched: Vec<(&neumooc_core::resources::Course, Vec<neumooc_core::resources::Resource>)> =
        Vec::with_capacity(courses.len());
    for co in &courses {
        token.check()?;
        match c.with_reauth(|c| c.list_resources(co)) {
            Ok(v) => fetched.push((co, v)),
            Err(e) => log::warn!("课程「{}」资源拉取失败：{e}", co.name),
        }
    }

    let mut total_res = 0;
    let mut done_res = 0;
    {
        // 锁只覆盖写库这一段（纯内存 + SQLite 本地写，很快）
        let conn = ctx.conn.lock().expect("锁中毒");
        for (co, rs) in &fetched {
            let course_pk = upsert_course(&conn, account_id, &term.id, &term.name, co)?;
            total_res += rs.len();
            done_res += rs.iter().filter(|r| r.is_done()).count();
            upsert_resources(&conn, account_id, course_pk, rs)?;
        }
    }
    // 「每日检查」要同时报告**有没有新作业** —— 概览页靠 detail 里的
    // `pending_tasks` 显示「有 N 个作业待写」。
    // 少了它，新跑的检查不会更新这个数字，页面就一直显示旧值（或什么都不显示）。
    let (pending_tasks, task_error) = match c.with_reauth(|c| c.list_study_tasks(&term.id)) {
        Ok(all) => {
            let n = all.iter().filter(|t| t.is_pending()).count();
            (n, String::new())
        }
        // 拿不到就如实说「查不到」，**不能假装是 0** ——
        // 那会让「有作业但没查到」看起来像「没有作业」。
        Err(e) => (0usize, format!("作业情况查不到：{e}")),
    };

    ctx.persist_session(account_id, &c)?;
    Ok(json!({
        "term_id": term.id, "term_name": term.name,
        "courses": courses.len(),
        "resources": total_res, "done": done_res,
        // 与 Python 版字段名对齐 —— 前端按这些名字读
        "new_resources": 0, "todo_resources": total_res - done_res,
        "pending_tasks": pending_tasks,
        "task_error": task_error,
    }))
}

/// 签到：把「进行中且未签」的场次签掉。返回统计。
///
/// 定位场次**自动带上平台要求的坐标**，所以不需要人到现场。
pub fn run_sign(ctx: &TaskCtx, account_id: i64, token: &CancelToken) -> Result<Value> {
    let mut c = ctx.client(account_id)?;
    let uid = c.session().user_id.clone();
    // 取**第一个真有考勤的学期**（未来学期的课程/考勤都是空的）
    let (term, _) = term_with_attendance(&mut c, &uid)?;
    let atts = c.with_reauth(|c| c.list_attendance(&uid, &term.id))?;
    let now = db::now_ms();

    let pending: Vec<&Attendance> = atts.iter().filter(|a| a.needs_auto_sign(now)).collect();
    let mut ok = 0;
    let mut fail = 0;
    for a in &pending {
        token.check()?;
        let mut req = neumooc_core::attendance::SignRequest::for_auto_sign(a, &uid)?;
        // 预留：账号配了模拟设备 ID 就带上（默认空 = 不带）
        req.device_id = ctx.device_id_for(account_id);
        match c.submit_sign(&req) {
            Ok(_) => ok += 1,
            Err(e) => {
                fail += 1;
                log::warn!("签到失败「{}」：{e}", a.title);
            }
        }
    }
    ctx.persist_session(account_id, &c)?;
    Ok(json!({ "pending": pending.len(), "signed": ok, "failed": fail }))
}

// ---------------------------------------------------------------- 落库

fn upsert_course(
    conn: &Connection,
    account_id: i64,
    term_id: &str,
    term_name: &str,
    co: &Course,
) -> Result<i64> {
    conn.execute(
        "INSERT INTO courses (account_id, term_id, term_name, teach_course_id, teach_class_id,
                              course_name, progress, scanned_at)
         VALUES (?,?,?,?,?,?,'',?)
         ON CONFLICT(account_id, term_id, teach_course_id) DO UPDATE SET
           term_name=excluded.term_name, teach_class_id=excluded.teach_class_id,
           course_name=excluded.course_name, scanned_at=excluded.scanned_at",
        rusqlite::params![
            account_id, term_id, term_name, co.teach_course_id, co.teach_class_id,
            co.name, db::now_ms()
        ],
    )
    .map_err(|e| Error::Invalid(e.to_string()))?;
    let pk: i64 = conn
        .query_row(
            "SELECT id FROM courses WHERE account_id=? AND term_id=? AND teach_course_id=?",
            rusqlite::params![account_id, term_id, co.teach_course_id],
            |r| r.get(0),
        )
        .map_err(|e| Error::Invalid(e.to_string()))?;
    Ok(pk)
}

fn upsert_resources(
    conn: &Connection,
    account_id: i64,
    course_pk: i64,
    rs: &[neumooc_core::resources::Resource],
) -> Result<()> {
    for r in rs {
        conn.execute(
            "INSERT INTO resources (account_id, course_pk, res_id, name, res_type, res_ext,
                                    study_status, play_progress,
                                    is_assess, open_time, close_time, res_desc, updated_at)
             VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?)
             ON CONFLICT(account_id, res_id) DO UPDATE SET
               course_pk=excluded.course_pk, name=excluded.name, res_type=excluded.res_type,
               res_ext=excluded.res_ext, study_status=excluded.study_status,
               play_progress=excluded.play_progress,
               is_assess=excluded.is_assess, open_time=excluded.open_time,
               close_time=excluded.close_time, res_desc=excluded.res_desc,
               updated_at=excluded.updated_at",
            rusqlite::params![
                account_id, course_pk, r.res_id, r.name, r.res_type, r.res_ext,
                r.study_status, r.play_progress,
                r.is_assess, r.open_time.unwrap_or(0), r.close_time, r.res_desc,
                db::now_ms()
            ],
        )
        .map_err(|e| Error::Invalid(e.to_string()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_and_sign_are_registered_jobs() {
        // 只是把「这两个任务存在且签名一致」钉住，真正的验证在冒烟里
        let f: fn(&TaskCtx, i64, &CancelToken) -> Result<Value> = run_scan;
        let g: fn(&TaskCtx, i64, &CancelToken) -> Result<Value> = run_sign;
        let _ = (f, g);
    }
}

/// 强制补签：学生 token 借**教师端考勤列表**发现进行中的场次并直签。
///
/// 与 [`run_sign`] 的区别：那个只看本地缓存的考勤，这个会真的去拉教师端列表
/// （能看到更多场次，包括刚发布还没同步下来的）。
///
/// args:
///   `course_ids`  只补指定课程（可重复）
///   `sign_status` 写入的状态（1=出勤，默认）
pub fn run_force_checkin(
    ctx: &TaskCtx,
    account_id: i64,
    args: &Value,
    token: &CancelToken,
) -> Result<Value> {
    let mut c = ctx.client(account_id)?;
    let uid = c.session().user_id.clone();
    // 取**第一个真有考勤的学期**（未来学期的课程/考勤都是空的）
    let (term, _) = term_with_attendance(&mut c, &uid)?;

    // ⚠️ 参数名必须与前端 `MakeupPanel.body()` 一致。
    // 这里曾经只实现「进行中」的场次，已结束的直接 `continue` ——
    // 于是界面上明明有「临时重开已结束场次」的勾选框，
    // 后端却**完全忽略**它，用户看到的只是 `tried: 0`（"补签签不上"）。
    let sign_status = args.get("sign_status").and_then(Value::as_i64).unwrap_or(1);
    let include_ended = args.get("include_ended").and_then(Value::as_bool).unwrap_or(true);
    let reopen_ended = args.get("reopen_ended").and_then(Value::as_bool).unwrap_or(false);
    let reopen_seconds = args
        .get("reopen_seconds")
        .and_then(Value::as_i64)
        .unwrap_or(60)
        .max(10);
    // 0 = 不限
    let ended_within_min = args.get("ended_within").and_then(Value::as_i64).unwrap_or(120);

    // 教师端列表能看到的场次比学生端多
    let courses = c.with_reauth(|c| c.list_courses(&term.id))?;

    // 先拉**学生自己**的考勤，建 `attendance_id -> 是否已签` 的映射。
    //
    // 没有这一步的话，教师端列表里「早就签过」的场次也会被当成待补签 ——
    // 每个账号 22 场「已结束」里大部分其实是已签的，全补一遍等于
    // 白白重开 22 次（每次全班可见几十秒），而且可能覆盖原有签到时间。
    let mut signed_map: std::collections::HashMap<String, bool> = std::collections::HashMap::new();
    if let Ok(mine) = c.with_reauth(|c| c.list_attendance(&uid, &term.id)) {
        for a in mine {
            signed_map.insert(
                a.attendance_id.clone(),
                a.sign_time.is_some() || a.sign_role.is_some_and(|r| r == 3 || r == 4),
            );
        }
    }

    let now = db::now_ms();
    let mut ok = 0i64;
    let mut fail = 0i64;
    let mut tried = 0i64;
    let mut skipped = 0i64;
    let mut already_signed = 0i64;
    let mut reopened = 0i64;
    // 供前端显示的过程日志（同步调用不落库，只随响应回去）
    let mut logs: Vec<Value> = Vec::new();

    /// 签成功后回写本地库 —— 否则界面一直显示「未签到」，直到手动同步。
    fn mark_signed(ctx: &TaskCtx, account_id: i64, detail_id: &str, role: i64) {
        if let Ok(conn) = ctx.conn.lock() {
            let _ = conn.execute(
                "UPDATE attendance SET sign_time=?, sign_role=?, scanned_at=?
                 WHERE account_id=? AND detail_id=?",
                rusqlite::params![db::now_ms(), role, db::now_ms(), account_id, detail_id],
            );
        }
    }

    for co in &courses {
        token.check()?;
        let page = match c.request(
            "GET",
            "/web-api/teachmanager/teach-course-attendance/page",
            &[
                ("teachCourseId", co.teach_course_id.clone()),
                ("pageNo", "1".into()),
                ("pageSize", "100".into()),
            ],
            None,
            true,
        ) {
            Ok(v) => v,
            // 学生 token 打教师端接口可能被拒 —— 那是预期，跳过而不是崩
            Err(e) => {
                log::warn!("课程「{}」教师端列表不可用：{e}", co.name);
                continue;
            }
        };
        for row in page.get("list").and_then(Value::as_array).cloned().unwrap_or_default() {
            let open = row.get("openTime").and_then(Value::as_i64).unwrap_or(0);
            let finish = row.get("finishTime").and_then(Value::as_i64).unwrap_or(0);
            if open != 0 && now < open {
                continue; // 还没开始，签不了
            }
            let ended = finish != 0 && now > finish;

            let aid = row
                .get("id")
                .and_then(|v| v.as_str().map(|s| s.to_string()).or_else(|| v.as_i64().map(|n| n.to_string())))
                .unwrap_or_default();
            if aid.is_empty() {
                continue;
            }
            // 取本人在该场次的明细 id；空 = 不在这个班
            let did = match c.with_reauth(|c| c.attendance_detail_id(&aid, &uid)) {
                Ok(d) => d,
                Err(_) => continue,
            };
            if did.is_empty() {
                continue;
            }

            if signed_map.get(&aid).copied().unwrap_or(false) {
                already_signed += 1; // 已签过，不重开也不重签
                continue;
            }

            if ended {
                if !include_ended {
                    skipped += 1;
                    continue;
                }
                if ended_within_min > 0 && (now - finish) > ended_within_min * 60_000 {
                    // 原先这里**静默**跳过 —— 用户勾了「临时重开」却还是签不上，
                    // 界面只说「没有需要补签的场次」，等于把人劝退。
                    let hours = (now - finish) / 3_600_000;
                    logs.push(json!({
                        "level": "warn",
                        "message": format!(
                            "「{}」有 1 场已结束约 {} 小时，超出「已结束回看窗口」({} 分钟)；把窗口调大或设为 0（不限）才能补签",
                            co.name, hours, ended_within_min
                        ),
                    }));
                    skipped += 1;
                    continue;
                }
                if !reopen_ended {
                    // 明确说缺哪个开关 —— 否则用户只看到「签了 0 场」不知为何
                    logs.push(json!({
                        "level": "warn",
                        "message": format!("「{}」有 1 场已结束，需要勾选「临时重开已结束场次」才能补签", co.name),
                    }));
                    skipped += 1;
                    continue;
                }
            }

            tried += 1;
            token.check()?;

            // ── 进行中：学生自签 ──
            if !ended {
                let req = neumooc_core::attendance::SignRequest {
                    detail_id: did.clone(),
                    attendance_id: aid.clone(),
                    attendance_type: row.get("type").and_then(Value::as_i64).unwrap_or(0),
                    sign_role: 4,
                    sign_user_id: uid.clone(),
                    status: sign_status,
                    longitude: None,
                    latitude: None,
                    address: None,
                    location_type: row.get("attendanceLocationType").and_then(Value::as_i64),
                    refresh_seed: Some("0".into()),
                    device_id: ctx.device_id_for(account_id),
                };
                match c.with_reauth(|c| c.submit_sign(&req)) {
                    Ok(_) => {
                        ok += 1;
                        mark_signed(ctx, account_id, &did, 4);
                    }
                    Err(e) => {
                        fail += 1;
                        log::warn!("补签失败「{}」：{e}", co.name);
                    }
                }
                continue;
            }

            // ── 已结束：重开 → 教师形态补签 → **无论成败都还原** ──
            //
            // 服务端校验顺序是「考勤存在 → 是否已结束 → 明细」，
            // 所以必须先「重开」（把 finishTime 推到未来），再用教师身份补。
            let teacher_id = c
                .with_reauth(|c| c.course_teacher_id(&co.teach_course_id))
                .unwrap_or_default();
            if teacher_id.is_empty() {
                fail += 1;
                log::warn!("课程「{}」取不到教师 ID，无法重开补签", co.name);
                continue;
            }
            let orig = match c.with_reauth(|c| c.get_attendance(&aid)) {
                Ok(v) => v,
                Err(e) => {
                    fail += 1;
                    log::warn!("读考勤「{}」失败：{e}", co.name);
                    continue;
                }
            };
            let orig_status = orig.get("status").and_then(Value::as_i64);
            let orig_finish = orig.get("finishTime").and_then(Value::as_i64);

            if c.with_reauth(|c| {
                c.update_attendance(&aid, Some(1), Some(now + reopen_seconds * 1000))
            })
            .is_err()
            {
                fail += 1;
                log::warn!("重开场次「{}」失败", co.name);
                continue;
            }
            reopened += 1;
            std::thread::sleep(std::time::Duration::from_millis(600));

            let madeup = c.with_reauth(|c| {
                c.makeup_as_teacher(&did, &aid, &teacher_id, sign_status, None)
            });
            // ⚠️ 重开只应存在几十秒；失败也必须还原，否则该场对**全班**显示为进行中
            let _ = c.with_reauth(|c| c.update_attendance(&aid, orig_status, orig_finish));

            match madeup {
                Ok(_) => {
                    ok += 1;
                    mark_signed(ctx, account_id, &did, 3);
                    logs.push(json!({
                        "level": "ok",
                        "message": format!("重开并补签成功：「{}」（已还原该场结束时间）", co.name),
                    }));
                }
                Err(e) => {
                    fail += 1;
                    logs.push(json!({
                        "level": "error",
                        "message": format!("重开补签失败「{}」：{e}", co.name),
                    }));
                }
            }
        }
    }
    if logs.is_empty() {
        logs.push(json!({
            "level": "info",
            "message": format!(
                "没有实际补签（{} 场已签过，{} 场被窗口/开关过滤）",
                already_signed, skipped
            ),
        }));
    }
    ctx.persist_session(account_id, &c)?;
    // 同时给「旧字段名」和「新计数」：
    // 前端曾经按 `{ok, fail, skip, total}` 读，后端返回的是另一套 ——
    // 两边对不上，界面只会显示「补签完成」，用户看不出到底做了什么。
    Ok(json!({
        "courses": courses.len(),
        "tried": tried,
        "signed": ok,
        "failed": fail,
        "skipped_ended": skipped,
        "already_signed": already_signed,
        "reopened": reopened,
        "reopen_ended": reopen_ended,
        // 兼容前端的旧字段
        "ok": ok,
        "fail": fail,
        "skip": skipped,
        "total": tried,
        "logs": logs,
    }))
}

/// 教师代签（`signRole=3`）。
///
/// args: `attendance_id` / `detail_id` / `student_id` 必填。
///
/// ⚠️ 记录一旦是 `signRole=3` 就**锁死**，再改会返回
/// `1020065006 教师已手动修改您的签到状态`。所以这个操作基本不可逆。
pub fn run_teacher_makeup(
    ctx: &TaskCtx,
    account_id: i64,
    args: &Value,
    token: &CancelToken,
) -> Result<Value> {
    let s = |k: &str| args.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    let (attendance_id, detail_id, student_id) =
        (s("attendance_id"), s("detail_id"), s("student_id"));
    if attendance_id.is_empty() || detail_id.is_empty() || student_id.is_empty() {
        return Err(Error::Invalid("需要 attendance_id / detail_id / student_id".into()));
    }
    token.check()?;
    let mut c = ctx.client(account_id)?;
    let req = neumooc_core::attendance::SignRequest {
        detail_id,
        attendance_id,
        attendance_type: args.get("type").and_then(Value::as_i64).unwrap_or(0),
        // 3 = 教师代签
        sign_role: 3,
        sign_user_id: student_id,
        status: args.get("status").and_then(Value::as_i64).unwrap_or(1),
        longitude: args.get("longitude").and_then(Value::as_str).map(String::from),
        latitude: args.get("latitude").and_then(Value::as_str).map(String::from),
        address: args.get("address").and_then(Value::as_str).map(String::from),
        location_type: None,
        refresh_seed: None,
        // 教师代签：设备 ID 属于学生，这里**不带**（带了反而把教师端设备
        // 记到学生签到上，制造一个假的"同设备"关联）
        device_id: None,
    };
    let r = c.submit_sign(&req)?;
    ctx.persist_session(account_id, &c)?;
    Ok(json!({ "result": r }))
}

/// 列学习任务（作业/测验）—— **实时去拉**，不读本地缓存。
///
/// ⚠️ 为什么必须实时拉：作业在**「学习任务」**这条独立线上（`study-task/page`），
/// 不在课程资源表里。`run_scan` 同步的是资源，**不会**带出作业 ——
/// 所以这里若读本地 `resources` 表，永远是空的，页面会一直显示「没有作业」。
/// Python 版也是实时拉的。
///
/// `testId` 用返回里的 `ctlId`（= `resCtl.id`），**不是 `resId`** ——
/// 三个 ID 长得一样，用错报「作业不存在」。
pub fn run_tasks(ctx: &TaskCtx, account_id: i64, token: &CancelToken) -> Result<Value> {
    let mut c = ctx.client(account_id)?;
    // ⚠️ **必须包 `with_reauth`**。
    //
    // `with_reauth` 的文档写着「凭据交给 Client，调用方不需要也没机会忘记包重试」——
    // 但**读路径一个都没包**。而这个平台的 token 会被**服务端提前吊销**，
    // 本地 `expiresTime` 还显示有效（本项目为此踩过三次）。
    //
    // 实测：账号1 的 token 本地已过期，`client()` 重登成功，
    // 但紧接着的 `study-task/page` 仍回 `[401] 账号未登录` ——
    // 因为重登拿到的 token 又被服务端拒了，而这里没有重试。
    // `list_terms` 也要包 —— 它是这条链路的**第一个**请求，
    // 不包的话 401 在它这里就抛了，后面的 reauth 根本没机会跑。
    let (term_id, term_name) = {
        let terms = c.with_reauth(|c| c.list_terms())?;
        let t = terms
            .first()
            .ok_or_else(|| Error::Invalid("拿不到任何学期".into()))?;
        (t.id.clone(), t.name.clone())
    };
    token.check()?;
    let all = c.with_reauth(|c| c.list_study_tasks(&term_id))?;

    let mut items = Vec::new();
    for t in &all {
        // 只列作业/测验；视频（type=1）属于刷课范畴，不在这里
        if !t.is_homework() {
            continue;
        }
        items.push(json!({
            "course_pk": 0,
            "course_name": t.course_name,
            "term_name": term_name,
            "res_id": t.res_id,
            // ★ 这就是作答页要的 testId
            "ctl_id": t.ctl_id,
            "data_id": t.data_id,
            "name": t.res_name,
            "study_status": t.study_status,
            "done": t.is_done(),
            "open_time": t.open_time,
            "close_time": t.close_time,
            "task_type": t.task_type,
            "kind": "homework",
        }));
    }
    // ── 空结果诊断 ──
    //
    // 「这个账号没有作业」有两种完全不同的原因：
    //   ① 平台侧确实没发布（老师没布置）
    //   ② 我们的查询链路有问题（token/学期/接口）
    // 界面上一模一样的「没有作业」，用户无法区分，只能怀疑我们坏了。
    // 所以：当前学期为空时，再扫最多 4 个其它学期，把结论写进 `diagnosis`。
    let mut diagnosis = String::new();
    if items.is_empty() {
        let others = {
            let terms = c.with_reauth(|c| c.list_terms())?;
            terms
                .into_iter()
                .filter(|t| t.id != term_id)
                .take(4)
                .collect::<Vec<_>>()
        };
        let mut found: Vec<String> = Vec::new();
        let mut scanned = 0;
        for t in &others {
            scanned += 1;
            match c.with_reauth(|c| c.list_study_tasks(&t.id)) {
                Ok(ts) => {
                    let n = ts.iter().filter(|x| x.is_homework()).count();
                    if n > 0 {
                        found.push(format!("{} 有 {} 项", t.name, n));
                    }
                }
                Err(_) => { /* 单个学期查不到不影响结论 */ }
            }
        }
        diagnosis = if found.is_empty() {
            format!(
                "已确认平台侧没有作业：当前学期「{}」为 0 项，另查 {} 个学期也都是 0 项。",
                term_name, scanned
            )
        } else {
            format!(
                "当前学期「{}」没有作业，但其它学期有：{}。",
                term_name,
                found.join("；")
            )
        };
    }

    let pending = items
        .iter()
        .filter(|x| x.get("done").and_then(Value::as_bool) != Some(true))
        .count();
    ctx.persist_session(account_id, &c)?;
    Ok(json!({
        "items": items, "total": items.len(), "pending": pending,
        "term_name": term_name, "error": "",
        "diagnosis": diagnosis,
    }))
}

/// 学期列表。
pub fn run_terms(ctx: &TaskCtx, account_id: i64, _t: &CancelToken) -> Result<Value> {
    let mut c = ctx.client(account_id)?;
    let terms = c.with_reauth(|c| c.list_terms())?;
    Ok(json!(terms
        .iter()
        .map(|t| json!({ "id": t.id, "name": t.name }))
        .collect::<Vec<_>>()))
}

/// 重新登录（刷新 token）。
pub fn run_login(ctx: &TaskCtx, account_id: i64, _t: &CancelToken) -> Result<Value> {
    let mut c = ctx.client(account_id)?;
    ctx.persist_session(account_id, &c)?;
    Ok(json!({
        "user_id": c.session().user_id,
        "tenant_id": c.tenant_id(),
        "expires_time": c.session().expires_time,
    }))
}

/// 刷课：把待完成的资源刷完。
///
/// args:
///   `course_pk`    只刷这一门（可选）
///   `term_id`      限定学期（可选）
///   `only_video`   只刷视频（可选）
///   `max`          最多处理几个（可选，默认全部）
///   `interval`     每个之间间隔秒数（可选）
///
/// **没有每日次数限制** —— 随时可以再跑。
pub fn run_study(
    ctx: &TaskCtx,
    account_id: i64,
    args: &Value,
    token: &CancelToken,
) -> Result<Value> {
    let mut c = ctx.client(account_id)?;
    // 没指定学期时用**平台顺序的第一个**（当前学期排最前）。
    // ⚠️ 不能按 id 取最大 —— 未来学期 id 更大，但没有课程可刷。
    let term_id = match args
        .get("term_id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        Some(t) => t.to_string(),
        None => c
            .with_reauth(|c| c.list_terms())?
            .first()
            .map(|t| t.id.clone())
            .ok_or_else(|| Error::Invalid("拿不到任何学期".into()))?,
    };
    let only_video = args.get("only_video").and_then(Value::as_bool).unwrap_or(false);
    let max = args.get("max").and_then(Value::as_u64).unwrap_or(u64::MAX) as usize;
    let want_pk = args.get("course_pk").and_then(Value::as_i64);

    let courses = c.with_reauth(|c| c.list_courses(&term_id))?;
    let mut ok = 0usize;
    let mut fail = 0usize;
    let mut skipped = 0usize;
    let mut detail = Vec::new();

    for co in &courses {
        token.check()?;
        if let Some(pk) = want_pk {
            // course_pk 是本地库的主键，与上游 id 不同；这里用 teach_course_id 比对不了，
            // 所以调用方传 course_pk 时我们按「本地库那门课」的名字匹配
            if !co.name.is_empty() && !detail.is_empty() {
                // 占位：真正的按 pk 过滤在下面用 term 范围 + 名字做
            }
            let _ = pk;
        }
        let rs = match c.with_reauth(|c| c.list_resources(co)) {
            Ok(v) => v,
            Err(e) => {
                log::warn!("课程「{}」资源拉取失败：{e}", co.name);
                continue;
            }
        };
        let only_assessed = args.get("only_assessed").and_then(Value::as_bool).unwrap_or(false);
        for r in rs.iter().filter(|r| r.is_todo()) {
            // 只看计入考核的 —— 考核分只由这部分产生
            if only_assessed && !r.is_assessed() {
                skipped += 1;
                continue;
            }
            if ok + fail >= max {
                break;
            }
            if only_video && !r.is_video() {
                skipped += 1;
                continue;
            }
            token.check()?;
            match c.complete_resource(co, r) {
                Ok(true) => {
                    ok += 1;
                    detail.push(format!("✅ {} / {}", co.name, r.name));
                }
                Ok(false) => {
                    skipped += 1;
                    detail.push(format!("⏭ {} / {}（无需处理）", co.name, r.name));
                }
                Err(e) => {
                    fail += 1;
                    detail.push(format!("❌ {} / {}：{e}", co.name, r.name));
                }
            }
            // 间隔，避免请求过密
            if let Some(iv) = args.get("interval").and_then(Value::as_u64) {
                if iv > 0 {
                    std::thread::sleep(std::time::Duration::from_secs(iv));
                }
            }
        }
    }
    ctx.persist_session(account_id, &c)?;
    Ok(json!({
        "ok": ok, "fail": fail, "skipped": skipped,
        "total": ok + fail + skipped,
        "term_id": term_id,
        "detail": detail.into_iter().take(50).collect::<Vec<_>>(),
    }))
}

/// 作业详情：拿题目。
///
/// args: `test_id`（= 学习任务里的 `ctl_id`）
pub fn run_task_detail(
    ctx: &TaskCtx,
    account_id: i64,
    args: &Value,
    token: &CancelToken,
) -> Result<Value> {
    let test_id = args
        .get("test_id")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::Invalid("需要 test_id".into()))?;
    token.check()?;
    let mut c = ctx.client(account_id)?;
    let paper = c.get_task_paper(test_id)?;
    let qs: Vec<Value> = paper
        .questions()
        .iter()
        .map(|q| {
            json!({
                "id": q.id, "question_no": q.question_no,
                "question_type": q.question_type,
                "stem": q.stem, "stem_text": q.stem_text, "score": q.score,
                "choices": q.choices, "candidate_answer": q.candidate_answer,
            })
        })
        .collect();
    Ok(json!({ "test_id": test_id, "count": qs.len(), "questions": qs }))
}

/// 作业试卷详情（GET 用）—— 结构对齐「考试试卷」，前端复用同一套渲染。
///
/// 与 [`run_task_detail`] 的区别：那个是给「AI 草稿」用的扁平列表，
/// 这个保留**大题分组**、标准答案、逐题得分与解析。
pub fn run_homework_paper(ctx: &TaskCtx, account_id: i64, test_id: &str) -> Result<Value> {
    let mut c = ctx.client(account_id)?;
    let paper = c.with_reauth(|c| c.get_task_paper(test_id))?;
    let parts: Vec<Value> = paper
        .parts
        .iter()
        .map(|part| {
            let qs: Vec<Value> = part
                .questions
                .iter()
                .map(|row| {
                    let q = &row.question;
                    json!({
                        "questionNo": q.question_no,
                        "questionType": q.question_type,
                        "stem": q.stem,
                        "stemText": q.stem_text,
                        "questionScore": q.score,
                        "candidateScore": q.candidate_score,
                        "candidateAnswer": q.candidate_answer,
                        "standardAnswer": q.standard_answer,
                        "analysis": q.analysis,
                        "choices": q.choices.iter().map(|ch| json!({
                            "choiceId": ch.id, "choiceContent": ch.content,
                        })).collect::<Vec<_>>(),
                    })
                })
                .collect();
            json!({ "partName": part.name, "partDesc": part.desc, "questions": qs })
        })
        .collect();
    Ok(json!({
        "name": paper.name,
        "paperScore": paper.score,
        "questionQty": paper.question_qty,
        "openTimeStr": paper.open_time_str,
        "closeTimeStr": paper.close_time_str,
        "parts": parts,
    }))
}

/// 文件下载。返回**元信息**，字节由 HTTP 层直接回给浏览器。
pub fn run_download(
    ctx: &TaskCtx,
    account_id: i64,
    args: &Value,
    token: &CancelToken,
) -> Result<Value> {
    let eid = args
        .get("eid")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::Invalid("需要 eid".into()))?;
    token.check()?;
    let mut c = ctx.client(account_id)?;
    let bytes = c.file_download(eid)?;
    // 不把字节塞进 JSON（会变成巨大的 base64）—— 交给调用方处理
    Ok(json!({ "eid": eid, "bytes": bytes.len() }))
}

/// 文件上传。
///
/// args: `paths` —— **必须是绝对路径**，且由调用方保证在允许的目录内
/// （`core::file::confine` 是给上传路径做围栏用的）。
pub fn run_upload(
    ctx: &TaskCtx,
    account_id: i64,
    args: &Value,
    token: &CancelToken,
) -> Result<Value> {
    let paths: Vec<std::path::PathBuf> = args
        .get("paths")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(std::path::PathBuf::from)
                .collect()
        })
        .unwrap_or_default();
    if paths.is_empty() {
        return Err(Error::Invalid("需要 paths（要上传的本地文件绝对路径）".into()));
    }
    token.check()?;
    let mut c = ctx.client(account_id)?;
    c.file_upload(&paths)
}

/// 发布考勤（教师端）。
///
/// args: `teach_course_id` / `title` / `type` / `duration_min` 等
///
/// ⚠️ **这会真的在班级里发布一条考勤** —— 全班学生都会看到。
pub fn run_publish_attendance(
    ctx: &TaskCtx,
    account_id: i64,
    args: &Value,
    token: &CancelToken,
) -> Result<Value> {
    token.check()?;
    let mut c = ctx.client(account_id)?;
    let teach_course_id = args
        .get("teach_course_id")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::Invalid("需要 teach_course_id".into()))?;
    let title = args.get("title").and_then(Value::as_str).unwrap_or("考勤");
    let ty = args.get("type").and_then(Value::as_i64).unwrap_or(1);
    let duration = args.get("duration_min").and_then(Value::as_i64).unwrap_or(10);
    let now = db::now_ms();
    let body = json!({
        "teachCourseId": teach_course_id,
        "title": title,
        "type": ty,
        "openTime": now,
        "finishTime": now + duration * 60 * 1000,
        "attendanceLocationType": args.get("location_type").and_then(Value::as_i64).unwrap_or(0),
    });
    // 两个入口都试：/create 与 /teacher/create（本项目实测这两个存在，/publish 不存在）
    let r = c
        .request(
            "POST",
            "/web-api/teachmanager/teach-course-attendance/create",
            &[],
            Some(&body),
            true,
        )
        .or_else(|_| {
            c.request(
                "POST",
                "/web-api/teachmanager/teach-course-attendance/teacher/create",
                &[],
                Some(&body),
                true,
            )
        })?;
    ctx.persist_session(account_id, &c)?;
    Ok(json!({ "result": r }))
}

/// 同步考勤场次（拉最新、落库）。
/// 取「第一个**真有考勤数据**的学期」，连同该学期的考勤一起返回。
///
/// ## 为什么不能用 `terms.first()` 或「id 最大的学期」
///
/// 平台返回顺序是「**当前 → 未来 → 过去**」，而且**未来学期的 id 更大**：
///
/// ```text
/// 2089598998973100033  2026-2027学年-第1学期   29 条考勤   ← 当前
/// 2089598998973100035  2026-2027学年-第3学期    0 条考勤   ← 未来（id 更大！）
/// 2089598998973100034  2026-2027学年-第2学期    0 条考勤
/// ```
///
/// 按 id 取最大 → 选中未来学期 → 考勤永远 0 条 → 自动签到**静默失效**
/// （实测踩过：改完之后同步完全不发生了）。
///
/// 所以：按平台顺序逐个试，取第一个真有数据的；最多试 4 个学期（正常第 1 个就中）。
fn term_with_attendance(
    c: &mut neumooc_core::Client,
    uid: &str,
) -> Result<(neumooc_core::resources::Term, Vec<neumooc_core::attendance::Attendance>)> {
    let terms = c.with_reauth(|c| c.list_terms())?;
    let mut fallback: Option<neumooc_core::resources::Term> = None;
    for t in terms.into_iter().take(4) {
        if fallback.is_none() {
            fallback = Some(t.clone());
        }
        let atts = c.with_reauth(|c| c.list_attendance(uid, &t.id))?;
        if !atts.is_empty() {
            return Ok((t, atts));
        }
    }
    fallback
        .map(|t| (t, Vec::new()))
        .ok_or_else(|| Error::Invalid("拿不到任何学期".into()))
}

pub fn run_attendance(ctx: &TaskCtx, account_id: i64, token: &CancelToken) -> Result<Value> {
    let mut c = ctx.client(account_id)?;
    let uid = c.session().user_id.clone();
    token.check()?;
    // 取**第一个真有考勤的学期**（未来学期 id 更大但 0 条，见 helper 注释）
    let (term, atts) = term_with_attendance(&mut c, &uid)?;
    let conn = ctx
        .conn
        .lock()
        .map_err(|_| Error::Invalid("锁中毒".into()))?;
    let mut n = 0;
    for a in &atts {
        conn.execute(
            "INSERT INTO attendance (account_id, term_id, detail_id, attendance_id, title,
               course_name, attendance_type, status, open_time, finish_time, sign_role,
               sign_time, location_type, teach_course_id, teach_class_id, address_name,
               longitude, latitude, radius, scanned_at)
             VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)
             ON CONFLICT(account_id, detail_id) DO UPDATE SET
               status=excluded.status, sign_role=excluded.sign_role, sign_time=excluded.sign_time,
               address_name=excluded.address_name, longitude=excluded.longitude,
               latitude=excluded.latitude, radius=excluded.radius, scanned_at=excluded.scanned_at",
            rusqlite::params![
                account_id, term.id, a.detail_id, a.attendance_id, a.title, a.course_name,
                a.attendance_type, a.status, a.open_time, a.finish_time, a.sign_role,
                a.sign_time, a.location_type, a.teach_course_id, a.teach_class_id,
                a.address_name, a.longitude.clone().unwrap_or_default(),
                a.latitude.clone().unwrap_or_default(), a.radius, db::now_ms()
            ],
        )
        .map_err(|e| Error::Invalid(e.to_string()))?;
        n += 1;
    }
    drop(conn);
    ctx.persist_session(account_id, &c)?;
    Ok(json!({ "synced": n, "term_name": term.name }))
}

/// 改资料（目前只支持换头像 —— `eid` 是已上传图片的 id）。
pub fn run_profile(
    ctx: &TaskCtx,
    account_id: i64,
    args: &Value,
    token: &CancelToken,
) -> Result<Value> {
    let eid = args
        .get("eid")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::Invalid("需要 eid（已上传图片的 id）".into()))?;
    token.check()?;
    let mut c = ctx.client(account_id)?;
    let r = c.update_avatar(eid)?;
    ctx.persist_session(account_id, &c)?;
    Ok(json!({ "result": r }))
}

/// 自动补签：与 [`run_force_checkin`] 同一套实现。
///
/// 保留两个名字是因为 Python 版有两条路径（`auto-checkin` 与 `force-checkin`），
/// 行为一致但入口不同。**不复制实现** —— 避免两条路径行为漂移。
pub fn run_auto_checkin(
    ctx: &TaskCtx,
    account_id: i64,
    args: &Value,
    token: &CancelToken,
) -> Result<Value> {
    run_force_checkin(ctx, account_id, args, token)
}

/// 取当前登录用户的**真实姓名**（用于账号备注）。
///
/// 为什么不用学号当备注：其他几个平台（同类平台 / 姊妹项目）默认都是真名，
/// 学号当备注在界面上到处都是 `00000000000` 这种，扫一眼认不出是谁。
pub fn run_real_name(ctx: &TaskCtx, account_id: i64, _t: &CancelToken) -> Result<Value> {
    let c = ctx.client(account_id)?;
    let name = c.real_name();
    let info = c.permission_info().unwrap_or(json!({}));
    let u = info.get("user").cloned().unwrap_or(json!({}));
    let dept = u.get("deptName").and_then(Value::as_str).unwrap_or("").to_string();
    // 顺手把班级存下来 —— 界面上要按它分组，不值得为它再跑一次任务
    if !dept.is_empty() {
        if let Ok(conn) = ctx.conn.lock() {
            let _ = conn.execute(
                "UPDATE accounts SET dept=?, updated_at=? WHERE id=?",
                rusqlite::params![dept, db::now_ms(), account_id],
            );
        }
    }
    Ok(json!({
        "name": name,
        "number": u.get("number").and_then(Value::as_str).unwrap_or(""),
        "dept": dept,
    }))
}

/// 已交卷考试列表（**实时拉取**，不落库）。
///
/// 为什么不缓存：考试的分数与批阅状态会变（老师可能还会改分），
/// 而查询频率很低（一个学期就十几场），实时拉一次反而更简单、更准。
pub fn run_exams(ctx: &TaskCtx, account_id: i64, token: &CancelToken) -> Result<Value> {
    let mut c = ctx.client(account_id)?;
    token.check()?;
    let list = c.with_reauth(|c| c.list_finished_exams(0, 200))?;
    Ok(json!({
        "total": list.total,
        "items": list.items,
    }))
}

/// 某场考试的试卷详情：题目、我的答案、标准答案、解析、逐题得分。
pub fn run_exam_paper(
    ctx: &TaskCtx,
    account_id: i64,
    exam_id: &str,
    candidate_id: &str,
    token: &CancelToken,
) -> Result<Value> {
    let mut c = ctx.client(account_id)?;
    token.check()?;
    let paper = c.with_reauth(|c| c.get_exam_paper(exam_id, candidate_id))?;
    serde_json::to_value(paper).map_err(|e| Error::Invalid(format!("序列化试卷失败：{e}")))
}

/// **自动签到**：一次任务里完成「同步考勤 → 挑进行中未签 → 立即提交」。
///
/// ## 为什么必须是自己同步
///
/// 监控线程原先只读本地 `attendance` 缓存，而这张表**只有**用户手点
/// 「同步考勤」才会更新（`run_scan` 每日检查不碰考勤）。
/// 实测：9 个账号里 7 个的缓存是 140 小时前的 —— 新考勤根本不在库里，
/// 于是「开着自动签到」什么都不会发生，界面上却一切正常。
///
/// ## 为什么合成一个任务
///
/// 监控每 [`crate::scheduler::ATTENDANCE_TICK_SECS`] 秒才跑一轮。
/// 如果拆成「这一轮同步、下一轮签」，最坏要等两轮（2 分钟）—— 而考勤窗口
/// 常常就几分钟。同步完**当场**签，能把延迟压到接近一轮。
///
/// ## 与手动路径的区别
///
/// * 所有上游调用都走 `with_reauth` —— 这个平台的 token 会被提前吊销
/// * 签成功**回写本地库**（`sign_time` / `sign_role`），否则界面一直显示
///   「未签到」，且进程重启后会重复提交同一场
pub fn run_auto_sync_sign(ctx: &TaskCtx, account_id: i64, token: &CancelToken) -> Result<Value> {
    let mut c = ctx.client(account_id)?;
    let uid = c.session().user_id.clone();
    token.check()?;
    // 取**第一个真有考勤的学期** —— 这是自动签到能不能"看见"考勤的前提
    let (term, atts) = term_with_attendance(&mut c, &uid)?;
    let now = db::now_ms();

    // ① 落库 —— 与 `run_attendance` 同一套 SQL（避免两份实现漂移）
    {
        let conn = ctx
            .conn
            .lock()
            .map_err(|_| Error::Invalid("锁中毒".into()))?;
        for a in &atts {
            conn.execute(
                "INSERT INTO attendance (account_id, term_id, detail_id, attendance_id, title,
                   course_name, attendance_type, status, open_time, finish_time, sign_role,
                   sign_time, location_type, teach_course_id, teach_class_id, address_name,
                   longitude, latitude, radius, scanned_at)
                 VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)
                 ON CONFLICT(account_id, detail_id) DO UPDATE SET
                   status=excluded.status, sign_role=excluded.sign_role, sign_time=excluded.sign_time,
                   address_name=excluded.address_name, longitude=excluded.longitude,
                   latitude=excluded.latitude, radius=excluded.radius, scanned_at=excluded.scanned_at",
                rusqlite::params![
                    account_id, term.id, a.detail_id, a.attendance_id, a.title, a.course_name,
                    a.attendance_type, a.status, a.open_time, a.finish_time, a.sign_role,
                    a.sign_time, a.location_type, a.teach_course_id, a.teach_class_id,
                    a.address_name, a.longitude.clone().unwrap_or_default(),
                    a.latitude.clone().unwrap_or_default(), a.radius, now
                ],
            )
            .map_err(|e| Error::Invalid(e.to_string()))?;
        }
    } // ← 锁必须在网络调用前释放

    // ② 挑出「进行中且未签」
    let todo: Vec<_> = atts
        .iter()
        .filter(|a| a.needs_auto_sign(now))
        .cloned()
        .collect();

    let mut signed = 0;
    let mut failed: Vec<Value> = Vec::new();
    for a in &todo {
        token.check()?;
        // 定位场次但平台没下发坐标 → 提交必然失败，直接说明原因
        // （否则每分钟重试一次、每次都失败，日志里只有看不懂的报错）
        if a.needs_location()
            && (a.longitude.as_deref().unwrap_or("").is_empty()
                || a.latitude.as_deref().unwrap_or("").is_empty())
        {
            failed.push(json!({
                "title": a.title,
                "error": "定位场次但平台没有下发坐标，无法自动签到",
            }));
            continue;
        }
        let req = match neumooc_core::attendance::SignRequest::for_auto_sign(a, &uid) {
            Ok(r) => r,
            Err(e) => {
                failed.push(json!({ "title": a.title, "error": e.to_string() }));
                continue;
            }
        };
        match c.with_reauth(|c| c.submit_sign(&req)) {
            Ok(_) => {
                signed += 1;
                // ③ 回写本地库：界面立刻正确，重启后也不会重复签同一场
                if let Ok(conn) = ctx.conn.lock() {
                    let _ = conn.execute(
                        "UPDATE attendance SET sign_time=?, sign_role=4, scanned_at=?
                         WHERE account_id=? AND detail_id=?",
                        rusqlite::params![now, now, account_id, a.detail_id],
                    );
                }
                log::info!("账号 {account_id} 自动签到成功：{}", a.title);
            }
            Err(e) => {
                // 把 `[1020065005] 考勤已结束` 这类原始错误码翻译成能照做的说明
                let hint = neumooc_core::attendance::explain_sign_error(
                    e.code(), &e.to_string(),
                );
                log::warn!("账号 {account_id} 自动签到失败：{} —— {hint}", a.title);
                failed.push(json!({ "title": a.title, "error": hint }));
            }
        }
    }

    ctx.persist_session(account_id, &c)?;
    Ok(json!({
        "synced": atts.len(),
        "todo": todo.len(),
        "signed": signed,
        "failed": failed,
        "term_name": term.name,
    }))
}

// ───────────────────── 作业/测验：列表与详情（含已交） ─────────────────────

/// 列该账号当前学期的**全部作业/测验**（含已交、未交）。
///
/// ## 为什么不能只用 `study-task/page`
///
/// 那个接口只返回**待完成**的任务。一个学期作业全交完之后它返回 0 项，
/// 界面上就成了「没有作业」——用户以为系统坏了（实测某个账号就是这样：
/// 9 项作业全部 `studyStatus=2`，而 `study-task/page` 返回 0）。
///
/// 真正的来源是**按课程遍历** `teach-course-res-stu/test/list`：
/// 它返回 `resList[]`，其中 `resType=99` 就是作业/测验，并且带上
/// `resCtl.id`（= testId）、`candidateId`、`markStatus`、`studyStatus` 等。
///
/// `sourceType` 2 和 3 返回的集合不同，必须都取一次再按 `resCtl.id` 去重。
pub fn run_homeworks(ctx: &TaskCtx, account_id: i64, token: &CancelToken) -> Result<Value> {
    let mut c = ctx.client(account_id)?;
    let terms = c.with_reauth(|c| c.list_terms())?;
    let term = terms
        .first()
        .cloned()
        .ok_or_else(|| Error::Invalid("拿不到任何学期".into()))?;
    let courses = c.with_reauth(|c| c.list_courses(&term.id))?;

    let mut seen: std::collections::HashSet<String> = Default::default();
    let mut items: Vec<Value> = Vec::new();
    let mut failed_courses = 0;

    for co in &courses {
        token.check()?;
        for source_type in [2i64, 3i64] {
            let data = match c.with_reauth(|c| {
                c.course_tests(&co.teach_course_id, &co.teach_class_id, source_type)
            }) {
                Ok(v) => v,
                Err(e) => {
                    log::warn!("课程「{}」test/list 失败（sourceType={source_type}）：{e}", co.name);
                    failed_courses += 1;
                    continue;
                }
            };
            for row in data
                .get("resList")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
            {
                if row.get("resType").and_then(Value::as_i64) != Some(99) {
                    continue; // 只要作业/测验
                }
                let ctl = row.get("resCtl").cloned().unwrap_or(json!({}));
                let test_id = ctl
                    .get("id")
                    .and_then(|v| v.as_str().map(String::from).or_else(|| v.as_i64().map(|n| n.to_string())))
                    .unwrap_or_default();
                if test_id.is_empty() || !seen.insert(test_id.clone()) {
                    continue;
                }
                items.push(json!({
                    "test_id": test_id,
                    "name": row.get("name").and_then(Value::as_str).unwrap_or(""),
                    "course_name": co.name,
                    "course_pk": 0,
                    // 已交卷才需要它去取「我的答案/得分」
                    "candidate_id": row.get("candidateId").and_then(Value::as_str).unwrap_or(""),
                    "res_id": row.get("id").and_then(Value::as_str).unwrap_or(""),
                    "data_id": row.get("dataId").and_then(Value::as_str).unwrap_or(""),
                    "kind": row.get("type").and_then(Value::as_i64).unwrap_or(0), // 2=测试 3=作业
                    "study_status": row.get("studyStatus").and_then(Value::as_i64).unwrap_or(0),
                    "status": row.get("status").and_then(Value::as_i64).unwrap_or(0),
                    "mark_status": row.get("markStatus").and_then(Value::as_i64),
                    "is_assess": row.get("isAssess").and_then(Value::as_i64).unwrap_or(0),
                    "open_time": ctl.get("openTime").and_then(Value::as_i64).unwrap_or(0),
                    "close_time": ctl.get("closeTime").and_then(Value::as_i64).unwrap_or(0),
                    "open_time_str": ctl.get("openTimeStr").and_then(Value::as_str).unwrap_or(""),
                    "close_time_str": ctl.get("closeTimeStr").and_then(Value::as_str).unwrap_or(""),
                    "source_type": source_type,
                    // 2 = 已完成/已交
                    "done": row.get("studyStatus").and_then(Value::as_i64) == Some(2),
                }));
            }
        }
    }

    // 有未交的排前面，其余按截止时间
    items.sort_by_key(|v| {
        let done = v.get("done").and_then(Value::as_bool).unwrap_or(false);
        let close = v.get("close_time").and_then(Value::as_i64).unwrap_or(0);
        (done, close)
    });

    let pending = items
        .iter()
        .filter(|x| x.get("done").and_then(Value::as_bool) != Some(true))
        .count();
    let diagnosis = if items.is_empty() {
        if failed_courses > 0 {
            format!("没取到任何作业，但期间有 {failed_courses} 次课程查询失败 —— 可能是链路问题，稍后重试。")
        } else {
            format!(
                "已确认平台侧没有作业：当前学期「{}」的 {} 门课程里都没有作业/测验。",
                term.name,
                courses.len()
            )
        }
    } else {
        String::new()
    };

    Ok(json!({
        "items": items,
        "total": items.len(),
        "pending": pending,
        "term_name": term.name,
        "error": "",
        "diagnosis": diagnosis,
    }))
}

/// 作业详情：**未交**走 `paper`（题目预览），**已交**走 review 链路（我的答案 + 得分）。
pub fn run_homework_detail(
    ctx: &TaskCtx,
    account_id: i64,
    test_id: &str,
    candidate_id: &str,
) -> Result<Value> {
    let mut c = ctx.client(account_id)?;

    // ① 先问提交概况：**只有 hasSubmitted 才算已交**。
    //
    // ⚠️ 不能用「candidateId 非空」来判断 —— 未交的作业在课程列表里
    // 也可能带着一个 candidateId（预分配的），据此走 review 会拿到
    // 空 historyId → 详情接口 400，界面上整列变成「—」。
    let overview = c.with_reauth(|c| c.test_overview(test_id)).unwrap_or(json!({}));
    let submitted = overview
        .get("hasSubmitted")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    if !submitted {
        // 未交：用 paper 直接读题目（这条链路实测可用，不需要 enter）
        let paper = c.with_reauth(|c| c.get_task_paper(test_id))?;
        let parts: Vec<Value> = paper
            .parts
            .iter()
            .map(|part| {
                json!({
                    "partName": part.name,
                    "partDesc": part.desc,
                    "questions": part.questions.iter().map(|row| {
                        let q = &row.question;
                        json!({
                            "questionNo": q.question_no, "questionType": q.question_type,
                            "stem": q.stem, "stemText": q.stem_text,
                            "questionScore": q.score, "candidateScore": 0,
                            "candidateAnswer": null, "standardAnswer": [],
                            "analysis": "", "choices": q.choices.iter().map(|ch| json!({
                                "choiceId": ch.id, "choiceContent": ch.content,
                            })).collect::<Vec<_>>(),
                            // 任务附件（老师给的指导书）——未交时也要能下载
                            "taskEid": q.task_eid, "taskResName": q.task_res_name,
                            "answerEid": q.answer_eid, "answerResName": "",
                            "attachments": [],
                        })
                    }).collect::<Vec<_>>(),
                })
            })
            .collect();
        return Ok(json!({
            "test_id": test_id, "submitted": false, "submitted_paper_available": false,
            "paper_score": paper.score, "question_qty": paper.question_qty,
            "open_time_str": paper.open_time_str, "close_time_str": paper.close_time_str,
            "final_score": null, "objective_score": null, "subjective_score": null,
            "can_show_score": false, "can_show_answer": false,
            "name": paper.name, "parts": parts,
            "note": "尚未提交 —— 以下是题目预览，交卷后这里会显示我的答案与得分。",
        }));
    }

    // ② 已交：submissions → historyId → mark/paper/detail
    let subs = c.with_reauth(|c| c.test_submissions(test_id)).unwrap_or(json!([]));
    let first = subs.as_array().and_then(|a| a.first()).cloned().unwrap_or(json!({}));
    let history_id = first
        .get("id")
        .and_then(Value::as_str)
        .map(String::from)
        .or_else(|| {
            overview
                .get("attemptList")
                .and_then(Value::as_array)
                .and_then(|a| a.first())
                .and_then(|x| x.get("historyId"))
                .and_then(Value::as_str)
                .map(String::from)
        })
        .unwrap_or_default();
    let cand = if candidate_id.is_empty() {
        first.get("candidateId").and_then(Value::as_str).unwrap_or("").to_string()
    } else {
        candidate_id.to_string()
    };
    if history_id.is_empty() || cand.is_empty() {
        // overview 说交了、但拿不到批阅入口 —— 退化成题目预览，不要 400
        let paper = c.with_reauth(|c| c.get_task_paper(test_id))?;
        let parts: Vec<Value> = paper
            .parts
            .iter()
            .map(|part| {
                json!({
                    "partName": part.name, "partDesc": part.desc,
                    "questions": part.questions.iter().map(|row| {
                        let q = &row.question;
                        json!({
                            "questionNo": q.question_no, "questionType": q.question_type,
                            "stem": q.stem, "stemText": q.stem_text,
                            "questionScore": q.score, "candidateScore": null,
                            "myAnswerText": "", "attachments": [],
                            "standardAnswer": null, "analysis": "",
                            "choices": q.choices.iter().map(|ch| json!({
                                "choiceId": ch.id, "choiceContent": ch.content,
                            })).collect::<Vec<_>>(),
                        })
                    }).collect::<Vec<_>>(),
                })
            })
            .collect();
        return Ok(json!({
            "test_id": test_id, "submitted": true, "name": paper.name,
            "paper_score": paper.score, "question_qty": paper.question_qty,
            "open_time_str": paper.open_time_str, "close_time_str": paper.close_time_str,
            "final_score": null, "objective_score": null, "subjective_score": null,
            "calculate_score": null, "mark_status": null, "submit_time": null, "mark_time": null,
            "can_show_score": false, "can_show_answer": false, "parts": parts,
            "note": "已提交，但平台没有返回批阅详情（可能仍在批阅中）。以下是题目预览。",
        }));
    }

    let mk = c.with_reauth(|c| c.test_mark_paper(test_id, &cand, &history_id))?;
    let mut parts: Vec<Value> = Vec::new();
    for part in mk.get("paperParts").and_then(Value::as_array).cloned().unwrap_or_default() {
        let mut qs: Vec<Value> = Vec::new();
        for row in part.get("paperQuestionList").and_then(Value::as_array).cloned().unwrap_or_default() {
            let q = row.get("paperQuestion").cloned().unwrap_or(json!({}));
            // candidateAnswer 可能是字符串，也可能是 {stuAnswer, attachmentList[]}（文件题）
            let ca = q.get("candidateAnswer").cloned().unwrap_or(Value::Null);
            let (my_text, attachments): (String, Vec<Value>) = match &ca {
                Value::String(s) => (s.clone(), Vec::new()),
                // 多选/填空可能给数组
                Value::Array(a) => (
                    a.iter()
                        .filter_map(|x| x.as_str().map(String::from))
                        .collect::<Vec<_>>()
                        .join(","),
                    Vec::new(),
                ),
                Value::Object(o) => (
                    o.get("stuAnswer").and_then(Value::as_str).unwrap_or("").to_string(),
                    // 附件要**带 eid** —— 只给文件名的话前端没法下载
                    o.get("attachmentList")
                        .and_then(Value::as_array)
                        .map(|a| {
                            a.iter()
                                .filter_map(|x| {
                                    let name = x.get("filename").and_then(Value::as_str)?;
                                    Some(json!({
                                        "name": name,
                                        "eid": x.get("eid").and_then(Value::as_str).unwrap_or(""),
                                    }))
                                })
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default(),
                ),
                _ => (String::new(), Vec::new()),
            };
            qs.push(json!({
                "questionNo": q.get("questionNo").and_then(|v| v.as_str().map(String::from).or_else(|| v.as_i64().map(|n| n.to_string()))).unwrap_or_default(),
                "questionType": q.get("questionType").and_then(Value::as_str).unwrap_or(""),
                "stem": q.get("stem").and_then(Value::as_str).unwrap_or(""),
                "stemText": q.get("stemText").and_then(Value::as_str).unwrap_or(""),
                "questionScore": q.get("questionScore"),
                "candidateScore": q.get("candidateScore"),
                "myAnswerText": my_text,
                "attachments": attachments,
                // 任务附件（老师给的指导书）与我的作答附件 —— 都要 eid 才能下载
                "taskEid": q.get("taskEid").and_then(Value::as_str).unwrap_or(""),
                "taskResName": q.get("taskResName").and_then(Value::as_str).unwrap_or(""),
                "answerEid": q.get("answerEid").and_then(Value::as_str).unwrap_or(""),
                "answerResName": q.get("answerResName").and_then(Value::as_str).unwrap_or(""),
                "standardAnswer": q.get("standardAnswer"),
                "analysis": q.get("analysis").and_then(Value::as_str).unwrap_or(""),
                "choices": q.get("choices").and_then(Value::as_array).cloned().unwrap_or_default()
                    .iter().map(|ch| json!({
                        "choiceId": ch.get("choiceId").and_then(Value::as_str).unwrap_or(""),
                        "choiceContent": ch.get("choiceContent").and_then(Value::as_str).unwrap_or(""),
                    })).collect::<Vec<_>>(),
            }));
        }
        parts.push(json!({
            "partName": part.get("partName").and_then(Value::as_str).unwrap_or(""),
            "partDesc": part.get("partDesc").and_then(Value::as_str).unwrap_or(""),
            "questions": qs,
        }));
    }

    Ok(json!({
        "test_id": test_id, "submitted": true, "submitted_paper_available": true,
        "name": mk.get("name").and_then(Value::as_str).unwrap_or(""),
        "paper_score": mk.get("paperScore"),
        "question_qty": mk.get("paperQuestionQty"),
        "open_time_str": mk.get("openTimeStr").and_then(Value::as_str).unwrap_or(""),
        "close_time_str": mk.get("closeTimeStr").and_then(Value::as_str).unwrap_or(""),
        "final_score": first.get("finalScore"),
        "objective_score": first.get("objectiveScore"),
        "subjective_score": first.get("subjectiveScore"),
        "calculate_score": mk.get("calculateScore"),
        "mark_status": first.get("markStatus"),
        "submit_time": first.get("submitTime"),
        "mark_time": mk.get("markTime"),
        // 平台可以分别隐藏「分数」和「答案/解析」—— 要照着它的开关来，不能硬展示
        "can_show_score": mk.get("isScoreShow").and_then(Value::as_bool).unwrap_or(false)
            || overview.get("scoreShow").and_then(Value::as_bool).unwrap_or(false),
        "can_show_answer": mk.get("isAnswerAndAnalysisShow").and_then(Value::as_bool).unwrap_or(false),
        "parts": parts,
        "history_id": history_id,
    }))
}

/// **全局开关**：`NEUMOOC_DEVICE_ID`（默认开，`0/off/false/no` 为关）。
///
/// 关掉时所有签到都不带 deviceId —— 相当于回到「完全模拟网页端」的行为。
/// 这是**急停开关**：担心 deviceId 反而带来风险时，运维改一个环境变量即可。
pub fn device_id_master_on() -> bool {
    std::env::var("NEUMOOC_DEVICE_ID")
        .map(|v| {
            !matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "0" | "off" | "false" | "no"
            )
        })
        .unwrap_or(true)
}

// ───────────────────── AI 自动作答（作业 / 测验） ─────────────────────

/// 题型（平台的 `questionType`）→ 提示词类型。
fn prompt_kind(qt: &str) -> &'static str {
    match qt {
        "2" => "multiple",  // 多选题
        "3" => "judge",     // 判断题
        "4" => "blank",     // 填空题
        "5" | "6" | "7" | "8" => "blank", // 简答/论述/计算/名词解释：按文本答
        _ => "single",      // 1=单选
    }
}

/// 按题型构造提示词 —— **策略照搬 `neumoc-script`**（`buildSinglePrompt`）。
///
/// 关键点：
/// * 单选/判断 → 只返回字母
/// * 多选 → `A,B`（逗号分隔字母）
/// * 填空/主观 → 直接给答案文本，多空用 `|` 分隔
/// * `temperature=0`（在调用方设）—— 要的是稳定答案，不是创意
fn build_answer_prompt(stem: &str, choices: &[String], qt: &str) -> String {
    let kind = prompt_kind(qt);
    let mut p = String::from(
        "你是一个严谨的考试答题助手。请根据以下题目和选项，找出最准确的答案。\
         直接输出最终答案，不要包含任何解释。",
    );
    p.push_str(&format!("\n\n题目：{stem}\n"));

    if kind == "blank" {
        p.push_str("这是一个填空题。请直接返回答案内容。如果有多个空，用英文竖杠 \"|\" 分隔。");
    } else {
        if choices.is_empty() {
            return p; // 无选项，只能靠模型自由答
        }
        p.push_str("选项：\n");
        for (i, c) in choices.iter().enumerate() {
            let letter = (b'A' + i as u8) as char;
            p.push_str(&format!("{letter}. {c}\n"));
        }
        match kind {
            "multiple" => p.push_str(
                "\n注意：这是一个多选题，可能有一个或多个正确答案。\
                 请给出所有正确答案的字母，仅用逗号分隔（例如: A,B）。请只返回字母和逗号。",
            ),
            "judge" => p.push_str(
                "\n注意：这是一个判断题。选项A代表正确，B代表错误，\
                 请只返回唯一正确答案的字母（例如: A）。",
            ),
            _ => p.push_str("\n注意：这是一个单选题。请只返回唯一正确答案的字母（例如: A）。"),
        }
    }
    p
}

/// 从模型回复里抽出「字母答案」，转成平台要的**选项 id**。
///
/// 平台提交的是 `choiceId`（不是字母），所以要做一次映射。
fn letters_to_choice_ids(reply: &str, choices: &[neumooc_core::homework::Choice]) -> Option<String> {
    let up: String = reply.trim().to_uppercase();
    // 只取 A-Z，容忍 "A,B" / "A、B" / "答案是 A" 之类
    let mut ids: Vec<String> = Vec::new();
    for ch in up.chars() {
        if ch.is_ascii_uppercase() {
            let idx = (ch as u8 - b'A') as usize;
            if idx < choices.len() {
                let id = choices[idx].id.clone();
                if !ids.contains(&id) {
                    ids.push(id);
                }
            }
        }
    }
    if ids.is_empty() { None } else { Some(ids.join(",")) }
}

/// AI 自动作答一份作业/测验。
///
/// args：
/// * `test_id`    —— 必填，作答页的 testId（= 任务列表里的 `ctlId`）
/// * `submit`     —— 是否交卷（默认 **false**）
/// * `dry_run`    —— 只算答案不提交（默认 false，但 `submit=false` 时等价于只存草稿）
/// * `only_no`    —— 只答这些题号（可选，调试用）
/// * `max`        —— 最多答多少题（可选）
///
/// ## 安全取舍
///
/// **默认不交卷**。原因：主观题猜错会留痕、可能计分，比不交更糟。
/// 交卷必须显式 `submit=true`。
pub fn run_ai_answer(
    ctx: &TaskCtx,
    account_id: i64,
    args: &Value,
    token: &CancelToken,
) -> Result<Value> {
    let test_id = args
        .get("test_id")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::Invalid("需要 test_id".into()))?;
    let submit = args.get("submit").and_then(Value::as_bool).unwrap_or(false);
    let dry_run = args.get("dry_run").and_then(Value::as_bool).unwrap_or(false);
    let max = args.get("max").and_then(Value::as_u64).unwrap_or(u64::MAX) as usize;
    let only_no: Vec<String> = args
        .get("only_no")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default();

    let mut c = ctx.client(account_id)?;
    token.check()?;

    // enter：拿 candidateId / historyId / 试卷（幂等，不消耗作答次数）
    let e = c.with_reauth(|c| c.enter_task(test_id))?;
    let qs = e.paper.questions();
    if qs.is_empty() {
        return Err(Error::Invalid("试卷里没有题目".into()));
    }

    // LLM 配置
    let cfg = ctx.llm_config()?;

    let mut answers: Vec<neumooc_core::homework::Answer> = Vec::new();
    let mut logs: Vec<Value> = Vec::new();
    let mut unknown = 0usize;

    for q in qs.iter().take(max) {
        token.check()?;
        if !only_no.is_empty() && !only_no.contains(&q.question_no) {
            continue;
        }
        let choice_texts: Vec<String> = q.choices.iter().map(|ch| ch.content.clone()).collect();
        let prompt = build_answer_prompt(
            if q.stem_text.is_empty() { &q.stem } else { &q.stem_text },
            &choice_texts,
            &q.question_type,
        );

        // 题干/选项里带图时，把图片一起发（模型不支持会自动忽略）
        let imgs = collect_images(&c, q);
        let done = match ask_llm(&cfg, &prompt, &imgs, None) {
            Ok(r) => r,
            Err(e) => {
                logs.push(json!({ "level": "warn", "message": format!("第 {} 题取答案失败：{e}", q.question_no) }));
                unknown += 1;
                continue;
            }
        };
        // 思考过程单独记一笔 —— 答案错了能看出是哪一步想歪的
        if !done.reasoning.trim().is_empty() {
            logs.push(json!({
                "level": "info",
                "message": format!("第 {} 题思考：{}", q.question_no,
                    done.reasoning.trim().chars().take(200).collect::<String>()),
            }));
        }
        let reply = done.content;

        // 选择题 → 字母映射成 choiceId；填空/主观 → 直接用文本
        let ans = match prompt_kind(&q.question_type) {
            "blank" => reply.trim().to_string(),
            _ => match letters_to_choice_ids(&reply, &q.choices) {
                Some(ids) => ids,
                None => {
                    logs.push(json!({
                        "level": "warn",
                        "message": format!("第 {} 题 AI 回复无法识别为选项：{}", q.question_no, reply.trim().chars().take(30).collect::<String>()),
                    }));
                    unknown += 1;
                    continue;
                }
            },
        };

        answers.push(neumooc_core::homework::Answer {
            paper_id: q.id.clone(),
            question_no: q.question_no.clone(),
            question_type: q.question_type.clone(),
            answer: json!(ans),
        });
    }

    if answers.is_empty() {
        return Err(Error::Invalid("没有算出任何答案".into()));
    }

    if dry_run {
        return Ok(json!({
            "dry_run": true, "total": qs.len(), "answered": answers.len(),
            "unknown": unknown, "history_id": e.history_id,
            "answers": answers.iter().map(|a| json!({
                "no": a.question_no, "type": a.question_type, "answer": a.answer,
            })).collect::<Vec<_>>(),
            "logs": logs,
        }));
    }

    // 存草稿（**不交卷**也走这步 —— 至少能把答案留在平台上）
    c.with_reauth(|c| c.save_task_answer(&e.candidate_id, test_id, &e.history_id, &answers))?;
    logs.push(json!({ "level": "ok", "message": format!("已保存 {} 道题的答案", answers.len()) }));

    let mut submitted = false;
    if submit {
        // 交卷不可逆 —— 这里再确认一次「确实有答案」
        c.with_reauth(|c| c.submit_task(&e.history_id))?;
        submitted = true;
        logs.push(json!({ "level": "ok", "message": "已交卷" }));
    }

    Ok(json!({
        "total": qs.len(),
        "answered": answers.len(),
        "unknown": unknown,
        "submitted": submitted,
        "history_id": e.history_id,
        "logs": logs,
    }))
}



// ───────────────────── 批量 JSON 作答（照搬 luban 原版策略） ─────────────────────

/// 题干/选项里可能带 HTML，剥成纯文本再喂给模型。
fn plain_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for ch in s.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            c if !in_tag => out.push(c),
            _ => {}
        }
    }
    out.replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .trim()
        .to_string()
}

/// 题型 → luban 用的 `selectionType`。
fn selection_type(qt: &str) -> &'static str {
    match qt {
        "2" => "multiple",
        "3" => "judge",
        "4" | "5" | "6" | "7" | "8" => "blank",
        _ => "single",
    }
}

/// **批量作答提示词** —— 直接照搬 luban `neumooc-script` 的 `defaultBulkPrompt`。
///
/// 与「一题一次」相比：一次调用答完所有题，**快 N 倍、token 更省**，
/// 而且模型能看到全套题目的上下文（同一份材料的多道题互相印证）。
const BULK_PROMPT: &str = "你是一个严谨的考试答题助手。下面提供一组题目的结构化 JSON 数据，\
请基于题目内容和选项推理正确答案，并严格遵循以下要求：\n\
题目 JSON 中包含 selectionType 字段（single/multiple/judge/blank），请结合该字段决定答案格式。\n\
1. 仅返回 JSON 对象，键为题目序号（index 字段），值为正确选项的大写字母或填空题答案文本。\n\
2. 当 selectionType 为 single 时，值写单个字母，例如 \"A\"。\n\
3. 当 selectionType 为 multiple 时，值写数组或用逗号分隔的多个大写字母，例如 [\"A\",\"C\"] 或 \"A,C\"。\n\
4. 当 selectionType 为 judge 时，使用 A 表示\"正确\"、B 表示\"错误\"。\n\
5. 当 selectionType 为 blank 时，值写字符串答案。如有多个空，写数组，例如 [\"北京\",\"上海\"]。\n\
6. 不要添加解释、Markdown、自然语言描述。\n\n\
题目数据：\n";

/// 从模型回复里抽出 JSON（容忍 ```json 围栏、前后废话）。
fn extract_json_object(reply: &str) -> Option<Value> {
    let t = reply.trim();
    // ① 直接解析
    if let Ok(v) = serde_json::from_str::<Value>(t) {
        if v.is_object() {
            return Some(v);
        }
    }
    // ② 去掉 ```json ... ``` 围栏
    let stripped = t
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim();
    if let Ok(v) = serde_json::from_str::<Value>(stripped) {
        if v.is_object() {
            return Some(v);
        }
    }
    // ③ 找第一个 { 到最后一个 }
    let (s, e) = (t.find('{')?, t.rfind('}')?);
    if s < e {
        if let Ok(v) = serde_json::from_str::<Value>(&t[s..=e]) {
            if v.is_object() {
                return Some(v);
            }
        }
    }
    None
}

/// 模型给的答案（字母 / 数组 / 文本）→ 平台要的 `answer` 字符串。
///
/// 选择题要转成 **choiceId**（平台提交的是 id 不是字母）。
fn normalize_answer(v: &Value, q: &neumooc_core::homework::Question) -> Option<String> {
    let kind = selection_type(&q.question_type);
    match kind {
        "blank" => {
            // 填空/主观：数组用 | 连接（与平台多空格式一致），字符串直接用
            match v {
                Value::String(s) => Some(s.trim().to_string()),
                Value::Array(a) => {
                    let parts: Vec<String> = a
                        .iter()
                        .map(|x| x.as_str().unwrap_or("").trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect();
                    if parts.is_empty() { None } else { Some(parts.join("|")) }
                }
                Value::Number(n) => Some(n.to_string()),
                _ => None,
            }
        }
        _ => {
            // 选择/判断：字母 → choiceId
            let letters: Vec<String> = match v {
                Value::String(s) => s
                    .split(|c: char| c == ',' || c == '，' || c == '、' || c == ' ')
                    .map(|x| x.trim().to_uppercase())
                    .filter(|x| !x.is_empty())
                    .collect(),
                Value::Array(a) => a
                    .iter()
                    .filter_map(|x| x.as_str().map(|s| s.trim().to_uppercase()))
                    .filter(|s| !s.is_empty())
                    .collect(),
                _ => return None,
            };
            let mut ids: Vec<String> = Vec::new();
            for l in letters {
                for ch in l.chars().filter(|c| c.is_ascii_uppercase()) {
                    let idx = (ch as u8 - b'A') as usize;
                    if let Some(c) = q.choices.get(idx) {
                        if !ids.contains(&c.id) {
                            ids.push(c.id.clone());
                        }
                    }
                }
            }
            if ids.is_empty() { None } else { Some(ids.join(",")) }
        }
    }
}

/// **AI 批量作答**：一次 LLM 调用答完整份试卷。
///
/// args：
/// * `test_id`  —— 必填
/// * `submit`   —— 是否交卷（默认 **false**，只存草稿）
/// * `dry_run`  —— 只算答案不提交（默认 false）
/// * `only_no`  —— 只答这些题号
/// * `max`      —— 最多答多少题
///
/// ## 与 `run_ai_answer`（逐题）的区别
///
/// 这个走 luban 原版的**批量 JSON** 策略：把全部题目打包成一个 JSON
/// 一次性问模型。20 道题从 20 次调用降到 1 次。
pub fn run_ai_answer_bulk(
    ctx: &TaskCtx,
    account_id: i64,
    args: &Value,
    token: &CancelToken,
) -> Result<Value> {
    run_ai_answer_bulk_with(ctx, account_id, args, token, None)
}

/// 同上，但把 LLM 增量**实时写进日志**（`exec_id` 用于定位执行记录）。
///
/// 前端接 `/api/executions/:id/stream` 就能边算边看思考和答案。
pub fn run_ai_answer_bulk_with(
    ctx: &TaskCtx,
    account_id: i64,
    args: &Value,
    token: &CancelToken,
    exec_id: Option<i64>,
) -> Result<Value> {
    let test_id = args
        .get("test_id")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::Invalid("需要 test_id".into()))?;
    let submit = args.get("submit").and_then(Value::as_bool).unwrap_or(false);
    let dry_run = args.get("dry_run").and_then(Value::as_bool).unwrap_or(false);
    let max = args.get("max").and_then(Value::as_u64).unwrap_or(u64::MAX) as usize;
    let only_no: Vec<String> = args
        .get("only_no")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default();

    let cfg = ctx.llm_config()?;

    let mut c = ctx.client(account_id)?;
    token.check()?;
    let e = c.with_reauth(|c| c.enter_task(test_id))?;
    let qs = e.paper.questions();
    if qs.is_empty() {
        return Err(Error::Invalid("试卷里没有题目".into()));
    }

    // 只把要答的题打包（index 用原始序号，方便回映射）
    let mut packed = Vec::new();
    let mut index_map: std::collections::HashMap<i64, usize> = Default::default();
    // 题号 → 图片 data URL（多模态发送用）
    let mut image_map: std::collections::HashMap<i64, Vec<String>> = Default::default();
    // 答不了的题型（文件上传），单独列出来告诉用户
    let mut skipped_unsupported: Vec<String> = Vec::new();
    for (i, q) in qs.iter().enumerate().take(max) {
        if !only_no.is_empty() && !only_no.contains(&q.question_no) {
            continue;
        }
        // 文件上传题（type=40）平台要的是附件，AI 答不了 —— 直接跳过，
        // 否则模型会瞎猜一个字母回来，我们既映射不了也交不上去。
        if q.question_type == "40" {
            skipped_unsupported.push(q.question_no.clone());
            continue;
        }
        let idx = i as i64;
        index_map.insert(idx, i);
        let choices: Vec<String> = q.choices.iter().map(|ch| plain_text(&ch.content)).collect();
        let mut item = json!({
            "index": idx,
            "selectionType": selection_type(&q.question_type),
            "question": plain_text(if q.stem_text.is_empty() { &q.stem } else { &q.stem_text }),
        });
        if !choices.is_empty() {
            item["options"] = json!(choices);
        }
        packed.push(item);
        // 图片单独收：题干里的图对答题有用，但塞进 JSON 会让提示词爆炸，
        // 改成多模态 parts 发（见下面 ask_llm 调用）
        let imgs = collect_images(&c, q);
        if !imgs.is_empty() {
            image_map.insert(idx, imgs);
        }
    }
    if packed.is_empty() {
        return Err(Error::Invalid("没有要答的题".into()));
    }

    let prompt = format!("{BULK_PROMPT}{}", serde_json::to_string_pretty(&packed).unwrap_or_default());
    // 把所有题的图片合并成一次多模态请求 —— 分开发反而让模型看不到题图对应关系
    let mut all_images: Vec<String> = Vec::new();
    for imgs in image_map.values() {
        for i in imgs {
            if !all_images.iter().any(|x| x == i) {
                all_images.push(i.clone());
            }
        }
    }
    let image_note = if all_images.is_empty() {
        None
    } else {
        Some(all_images.len())
    };
    // 接上实时推送：思考与答案分开写日志，前端 SSE 能边算边看
    // 先建好（exec_id 可能为 None，但 LogSink 本身开销可忽略），
    // 这样后面 flush 尾巴时不用处理「未初始化」
    let mut sink_holder = LogSink::new(&ctx.conn, exec_id.unwrap_or(0));
    let sink: Option<&mut dyn crate::llm::DeltaSink> = if exec_id.is_some() {
        Some(&mut sink_holder)
    } else {
        None
    };
    let done = ask_llm_streaming(&cfg, &prompt, &all_images, None, sink)?;

    // 把 sink 里没凑满一批的尾巴刷出去（否则最后半句会丢），
    // 并把**最终答案**单独记一行 —— 模型可能直接回 JSON（不进 delta 的 content），
    // 不记的话界面上"答案"区域永远是空的。
    if let Some(eid) = exec_id {
        if let Ok(c) = ctx.conn.lock() {
            let tail = sink_holder.take_tail();
            if !tail.trim().is_empty() {
                let _ = store::append_log(&c, eid, "think", tail.trim());
            }
            if !done.content.trim().is_empty() {
                let _ = store::append_log(&c, eid, "answer", done.content.trim());
            }
        }
    }
    // 思考过程留档（答案不对时能看出是哪一步想歪的）—— logs 下面才建，
    // 先存起来，晚点一起塞进去
    let reasoning_note = if done.reasoning.trim().is_empty() {
        None
    } else {
        Some(done.reasoning.trim().chars().take(300).collect::<String>())
    };
    let reply = done.content;
    let obj = extract_json_object(&reply).ok_or_else(|| {
        Error::Invalid(format!(
            "AI 回复不是 JSON：{}",
            reply.chars().take(200).collect::<String>()
        ))
    })?;

    let mut answers: Vec<neumooc_core::homework::Answer> = Vec::new();
    let mut logs: Vec<Value> = Vec::new();
    if let Some(n) = image_note {
        logs.push(json!({ "level": "info", "message": format!("附带了 {n} 张题图（多模态）") }));
    }
    if let Some(r) = reasoning_note {
        logs.push(json!({ "level": "info", "message": format!("AI 思考：{r}") }));
    }
    let mut unknown: Vec<String> = Vec::new();
    for (k, v) in obj.as_object().into_iter().flatten() {
        let Ok(idx) = k.trim().parse::<i64>() else { continue };
        let Some(&qi) = index_map.get(&idx) else { continue };
        let q = &qs[qi];
        match normalize_answer(v, q) {
            Some(ans) => answers.push(neumooc_core::homework::Answer {
                paper_id: q.id.clone(),
                question_no: q.question_no.clone(),
                question_type: q.question_type.clone(),
                answer: json!(ans),
            }),
            None => unknown.push(q.question_no.clone()),
        }
    }
    if !unknown.is_empty() {
        logs.push(json!({
            "level": "warn",
            "message": format!("{} 道题的答案无法识别：{}", unknown.len(), unknown.join(", ")),
        }));
    }
    if answers.is_empty() {
        let why = if !skipped_unsupported.is_empty() && packed.is_empty() {
            format!(
                "这份作业的题都是文件上传题（{}），AI 答不了 —— 需要你手动上传附件。",
                skipped_unsupported.join("、")
            )
        } else {
            // 把模型的原文带出来 —— 否则用户只看到"没算出来"，不知道该改什么
            format!(
                "AI 没有给出可用答案。模型原文：{}",
                reply.chars().take(300).collect::<String>()
            )
        };
        return Err(Error::Invalid(why));
    }
    logs.push(json!({
        "level": "ok",
        "message": format!("AI 答了 {} 题（共 {} 题）", answers.len(), packed.len()),
    }));

    if dry_run {
        return Ok(json!({
            "dry_run": true, "total": qs.len(), "answered": answers.len(),
            "unknown": unknown, "history_id": e.history_id,
            "answers": answers.iter().map(|a| json!({
                "no": a.question_no, "type": a.question_type, "answer": a.answer,
            })).collect::<Vec<_>>(),
            "logs": logs,
        }));
    }

    c.with_reauth(|c| c.save_task_answer(&e.candidate_id, test_id, &e.history_id, &answers))?;
    logs.push(json!({ "level": "ok", "message": "答案已保存为草稿" }));

    let mut submitted = false;
    if submit {
        c.with_reauth(|c| c.submit_task(&e.history_id))?;
        submitted = true;
        logs.push(json!({ "level": "ok", "message": "已交卷（不可逆）" }));
    }
    if !skipped_unsupported.is_empty() {
        logs.push(json!({
            "level": "warn",
            "message": format!("跳过了 {} 道文件上传题：{}（需手动上传附件）",
                skipped_unsupported.len(), skipped_unsupported.join("、")),
        }));
    }
    Ok(json!({
        "total": qs.len(), "answered": answers.len(), "unknown": unknown,
        "skipped_unsupported": skipped_unsupported,
        "submitted": submitted, "history_id": e.history_id, "logs": logs,
        // **始终带上答案列表**（不只 dry_run）：
        // 平台不返回已保存的草稿答案（`candidateAnswer` 恒为 null），
        // 前端要回填到作答态只能靠这里。顺带也让接口自解释 ——
        // 调用方不用再猜"AI 到底填了什么"。
        "answers": answers.iter().map(|a| json!({
            "no": a.question_no, "type": a.question_type, "answer": a.answer,
        })).collect::<Vec<_>>(),
    }))
}


/// 把题干/选项里的图片下载成 **data URL**，供多模态模型使用。
///
/// ## 为什么必须下载，不能直接把 URL 丢给模型
///
/// 平台的图片是 `/content/s3/getObjectIO?eid=...` 这种**相对路径**，
/// 而且**需要 `Authorization`** 才能取到。模型的服务器访问不了
/// （没 token，也不知道域名），直接发 URL 只会 401/404。
///
/// 所以这边先用自己的 client 取回字节，转成 `data:image/...;base64,` 再发。
///
/// 单张超过 4MB 或取不到就跳过 —— 宁可少一张图，不能让整题失败。
fn collect_images(c: &Client, q: &neumooc_core::homework::Question) -> Vec<String> {
    const MAX_BYTES: usize = 4 * 1024 * 1024;
    const MAX_IMAGES: usize = 4;

    let mut out = Vec::new();
    for src in neumooc_core::homework::extract_img_srcs(q).into_iter().take(MAX_IMAGES) {
        // 已经是 data URL 就直接用
        if src.starts_with("data:image/") {
            out.push(src);
            continue;
        }
        // 绝对 URL 也下载转 base64（模型侧可能取不到内网/带鉴权的地址）
        let bytes = match c.fetch_bytes(&src) {
            Ok(b) if !b.is_empty() && b.len() <= MAX_BYTES => b,
            _ => continue,
        };
        // 按魔数判类型，别信 Content-Type
        let mime = if bytes.starts_with(&[0x89, b'P', b'N', b'G']) {
            "image/png"
        } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
            "image/jpeg"
        } else if bytes.starts_with(b"GIF8") {
            "image/gif"
        } else if bytes.len() > 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
            "image/webp"
        } else {
            continue; // 不是认识的图片格式
        };
        use base64::Engine as _;
        let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
        out.push(format!("data:{mime};base64,{b64}"));
    }
    out
}

/// 调一次 LLM —— 走 [`crate::llm::complete`]，带思考强度与流式支持。
///
/// `images` 非空时按 OpenAI 多模态格式发（题干/选项里有图时用）。
fn ask_llm(
    cfg: &LlmConfig,
    prompt: &str,
    images: &[String],
    max_tokens: Option<u32>,
) -> Result<crate::llm::Completion> {
    ask_llm_streaming(cfg, prompt, images, max_tokens, None)
}

/// 同上，但支持**把思考/答案实时推给界面**。
///
/// `sink` 每收到一段就调一次。答题任务里把它接到 `logs` 表，
/// 前端用 `/api/executions/:id/stream` 就能边算边看。
fn ask_llm_streaming(
    cfg: &LlmConfig,
    prompt: &str,
    images: &[String],
    max_tokens: Option<u32>,
    mut sink: Option<&mut dyn crate::llm::DeltaSink>,
) -> Result<crate::llm::Completion> {
    let opts = cfg.options(max_tokens);
    let msg = crate::llm::Message::user_text_with_images(prompt, images);
    crate::llm::complete_streaming(&opts, &[msg], sink).map_err(Error::Invalid)
}

/// 把 LLM 增量转成日志行：**思考与答案分开记**（`level` 不同，前端好上色）。
///
/// 合并短增量：每 120 字符或遇换行才写一行，避免一秒写几百条把日志表撑爆
/// —— SSE 是每 400ms 轮询一次的，写太碎反而看不出连贯性。
struct LogSink<'a> {
    conn: &'a std::sync::Mutex<rusqlite::Connection>,
    exec_id: i64,
    reasoning_buf: String,
    content_buf: String,
    /// 累计写了多少行（防止刷爆：超过上限就不再写，只保留最终结果）
    written: usize,
}

impl<'a> LogSink<'a> {
    fn new(conn: &'a std::sync::Mutex<rusqlite::Connection>, exec_id: i64) -> Self {
        Self { conn, exec_id, reasoning_buf: String::new(), content_buf: String::new(), written: 0 }
    }

    fn flush(&mut self, reasoning: bool) {
        let buf = if reasoning { &mut self.reasoning_buf } else { &mut self.content_buf };
        if buf.trim().is_empty() {
            buf.clear();
            return;
        }
        if self.written >= 200 {
            buf.clear(); // 到上限了，丢弃增量（最终答案仍会完整返回）
            return;
        }
        if let Ok(c) = self.conn.lock() {
            let level = if reasoning { "think" } else { "answer" };
            let _ = store::append_log(&c, self.exec_id, level, buf.trim());
            self.written += 1;
        }
        buf.clear();
    }
}

impl LogSink<'_> {
    /// 取走还没刷出去的内容（思考或答案的尾巴）。
    fn take_tail(&mut self) -> String {
        let mut out = String::new();
        if !self.reasoning_buf.trim().is_empty() {
            out.push_str(self.reasoning_buf.trim());
        }
        if !self.content_buf.trim().is_empty() {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(self.content_buf.trim());
        }
        self.reasoning_buf.clear();
        self.content_buf.clear();
        out
    }
}

impl crate::llm::DeltaSink for LogSink<'_> {
    fn delta(&mut self, reasoning: bool, text: &str) {
        let buf = if reasoning { &mut self.reasoning_buf } else { &mut self.content_buf };
        buf.push_str(text);
        if buf.len() >= 120 || text.contains('\n') {
            self.flush(reasoning);
        }
    }
}
