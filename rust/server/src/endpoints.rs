//! 读接口：课程 / 资源 / 考勤 / 作业 / 执行 / 日志 / 调度。
//!
//! 这些直接查本地缓存表（由 `scan` 任务同步），**不打学校接口** ——
//! 页面刷新不该触发上游请求，那既慢又容易触发风控。
//!
//! 返回形状与 Python 版一致（前端已按它写好）。大多直接给数据库行 ——
//! 用 [`crate::rows::query_json`]，不给每张表写结构体。

use axum::extract::{Path, Query, State};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;

use crate::api::{ApiError, AppState, AuthUser};
use crate::rows::{query_json, query_one_json};
use crate::{db, store};

/// 账号归属校验的快捷方式。
/// 账号归属校验。**传 `&Connection`** —— 见 `require_account_pub` 的注释：
/// 收 `&AppState` 会让「持有锁时再锁一次」变成可能，那是死锁。
fn own(conn: &rusqlite::Connection, user: &AuthUser, id: i64) -> Result<(), ApiError> {
    crate::api::require_account_pub(conn, user, id)
}

#[derive(Deserialize, Default)]
pub struct TermQ {
    #[serde(default)]
    pub term_id: String,
}

/// `GET /api/accounts/:id`
pub async fn get_account(
    State(st): State<Arc<AppState>>,
    user: AuthUser,
    Path(id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    own(&st.conn(), &user, id)?;
    let conn = st.conn();
    let v = query_one_json(
        &conn,
        "SELECT id, label, username, user_id, tenant_id, state, token_expires,
                last_login_at, created_at, updated_at FROM accounts WHERE id=?",
        &[&id],
    )?
    .ok_or_else(|| ApiError::not_found("账号不存在"))?;
    Ok(Json(v))
}

/// `GET /api/accounts/:id/courses`
pub async fn courses(
    State(st): State<Arc<AppState>>,
    user: AuthUser,
    Path(id): Path<i64>,
    Query(q): Query<TermQ>,
) -> Result<Json<Value>, ApiError> {
    own(&st.conn(), &user, id)?;
    let conn = st.conn();
    // ⚠️ 必须带 `res_total` / `res_done` —— 前端课程下拉要显示
    //    「课程名（已完成/总数）」。少了这两个字段会渲染成 `（/）`，
    //    看起来像「课程没数据」，其实是字段没给。
    //    用 LEFT JOIN 一次查完，**不要**对每门课再查一次（N+1）。
    let base = "SELECT c.*,
                   COUNT(r.id) AS res_total,
                   COALESCE(SUM(CASE WHEN r.study_status=2 THEN 1 ELSE 0 END), 0) AS res_done
                FROM courses c LEFT JOIN resources r ON r.course_pk = c.id
                WHERE c.account_id=?";
    let rows = if q.term_id.is_empty() {
        query_json(&conn, &format!("{base} GROUP BY c.id ORDER BY c.id"), &[&id])?
    } else {
        query_json(
            &conn,
            &format!("{base} AND c.term_id=? GROUP BY c.id ORDER BY c.id"),
            &[&id, &q.term_id],
        )?
    };
    Ok(Json(Value::Array(rows)))
}

#[derive(Deserialize, Default)]
pub struct ResQ {
    #[serde(default)]
    pub course_pk: Option<i64>,
    #[serde(default)]
    pub only_todo: bool,
    #[serde(default)]
    pub res_type: Option<i64>,
    #[serde(default)]
    pub term_id: String,
    /// 只看**计入考核**的资源。考核分只由这部分产生。
    #[serde(default)]
    pub only_assessed: bool,
}

/// `GET /api/accounts/:id/resources`
pub async fn resources(
    State(st): State<Arc<AppState>>,
    user: AuthUser,
    Path(id): Path<i64>,
    Query(q): Query<ResQ>,
) -> Result<Json<Value>, ApiError> {
    own(&st.conn(), &user, id)?;
    let conn = st.conn();
    // 逐条拼 WHERE —— 参数个数是动态的，用 Vec<Box<dyn ToSql>> 不好借，
    // 这里条件少、取值又都是本地的整数/字符串，直接内联（**不含用户输入的自由文本**）
    let mut sql = String::from(
        "SELECT r.* FROM resources r JOIN courses c ON c.id = r.course_pk WHERE r.account_id=?",
    );
    if let Some(pk) = q.course_pk {
        sql.push_str(&format!(" AND r.course_pk={pk}"));
    }
    if let Some(t) = q.res_type {
        sql.push_str(&format!(" AND r.res_type={t}"));
    }
    if q.only_todo {
        sql.push_str(" AND r.study_status != 2");
    }
    if q.only_assessed {
        // 计入考核的才影响分数 —— 实测某学期 107 计入 / 382 不计入
        sql.push_str(" AND r.is_assess = 1");
    }
    if !q.term_id.is_empty() {
        // term_id 是外部传入的字符串，**必须参数化**，不能拼进 SQL
        sql.push_str(" AND c.term_id=?");
        let rows = query_json(&conn, &sql, &[&id, &q.term_id])?;
        return Ok(Json(Value::Array(rows)));
    }
    sql.push_str(" ORDER BY r.course_pk, r.id");
    Ok(Json(Value::Array(query_json(&conn, &sql, &[&id])?)))
}

/// `GET /api/accounts/:id/attendance`
///
/// 返回 `{items, summary, pending_items}` —— 与 Python 版一致。
/// 「未签」再分**进行中 / 缺勤 / 未开始**：三者在界面上必须不同形。
pub async fn attendance(
    State(st): State<Arc<AppState>>,
    user: AuthUser,
    Path(id): Path<i64>,
    Query(q): Query<TermQ>,
) -> Result<Json<Value>, ApiError> {
    own(&st.conn(), &user, id)?;
    let conn = st.conn();
    let rows = if q.term_id.is_empty() {
        query_json(&conn, "SELECT * FROM attendance WHERE account_id=? ORDER BY open_time DESC", &[&id])?
    } else {
        query_json(
            &conn,
            "SELECT * FROM attendance WHERE account_id=? AND term_id=? ORDER BY open_time DESC",
            &[&id, &q.term_id],
        )?
    };
    let now = db::now_ms();
    let is_signed = |r: &Value| {
        let role = r.get("sign_role").and_then(Value::as_i64);
        let st = r.get("sign_time").and_then(Value::as_i64);
        matches!(role, Some(3) | Some(4)) || st.is_some()
    };
    let is_open = |r: &Value| {
        let open = r.get("open_time").and_then(Value::as_i64).unwrap_or(0);
        let finish = r.get("finish_time").and_then(Value::as_i64).unwrap_or(0);
        let status = r.get("status").and_then(Value::as_i64);
        if finish != 0 && now > finish {
            return false;
        }
        if open != 0 && now < open {
            return false;
        }
        // status 为 null 也算进行中 —— 新发布的场次服务端就这么返回
        matches!(status, None | Some(1))
    };
    let signed: Vec<&Value> = rows.iter().filter(|r| is_signed(r)).collect();
    let unsigned: Vec<&Value> = rows.iter().filter(|r| !is_signed(r)).collect();
    let pending: Vec<&Value> = unsigned.iter().copied().filter(|r| is_open(r)).collect();
    let upcoming: Vec<&Value> = unsigned
        .iter()
        .copied()
        .filter(|r| {
            let open = r.get("open_time").and_then(Value::as_i64).unwrap_or(0);
            !is_open(r) && open != 0 && open > now
        })
        .collect();
    let missed = unsigned.len() - pending.len() - upcoming.len();
    let total = rows.len();
    let rate = if total == 0 { 0.0 } else { signed.len() as f64 * 100.0 / total as f64 };

    // 类型分布。前端拿它渲染「类型分布：二维码 12 · 教师点名 1」。
    // ⚠️ 缺这个字段前端会抛 `Cannot convert undefined or null to object`
    //    （它对 summary.by_type 做 Object.entries）——
    //    后端自测全绿但页面直接崩，只有前端冒烟能发现。
    let mut by_type = serde_json::Map::new();
    for r in &rows {
        let t = r.get("attendance_type").and_then(Value::as_i64).unwrap_or(-1);
        let label = match t {
            0 => "签到",
            1 => "二维码",
            2 => "教师点名",
            _ => "其他",
        };
        let cur = by_type.get(label).and_then(Value::as_i64).unwrap_or(0);
        by_type.insert(label.to_string(), json!(cur + 1));
    }

    // 该账号有考勤记录的学期（供页面上的学期下拉）
    let terms = query_json(
        &conn,
        "SELECT DISTINCT term_id AS id, '' AS name FROM attendance WHERE account_id=? AND term_id != '' ORDER BY term_id DESC",
        &[&id],
    )?;

    Ok(Json(json!({
        "items": rows,
        "terms": terms,
        "summary": {
            "total": total, "signed": signed.len(), "missed": missed,
            "pending": pending.len(), "upcoming": upcoming.len(),
            "rate": (rate * 10.0).round() / 10.0,
            "by_type": Value::Object(by_type),
        },
        "pending_items": pending,
    })))
}

/// `GET /api/accounts/:id/tasks` —— 学习任务（作业/测验）。
///
/// **实时去拉**（`study-task/page`），不读本地缓存。
///
/// ⚠️ 曾经写成「查本地 resources 表里 res_type=99」—— 而 `run_scan` 只同步资源、
/// **不会带出作业**，所以那个查询**永远是空的**，页面一直显示「没有作业」，
/// 还配了一句我自己编的、跟真实原因无关的提示（「有课程查询失败」）。
/// 作业在**「学习任务」**这条独立线上，必须实时拉。
pub async fn tasks(
    State(st): State<Arc<AppState>>,
    user: AuthUser,
    Path(id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    own(&st.conn(), &user, id)?;
    // 走 worker 串行执行（同账号不并发打上游）
    let ctx = st.tasks.clone();
    let token = crate::worker::CancelToken::new();
    let v = tokio::task::spawn_blocking(move || crate::tasks::run_tasks(&ctx, id, &token))
        .await
        .map_err(|e| ApiError::bad_request(format!("任务执行失败：{e}")))?
        .map_err(|e| ApiError::bad_request(e.to_string()))?;
    Ok(Json(v))
}

/// `GET /api/accounts/:id/exams` —— 已交卷考试列表（实时拉取）。
pub async fn exams(
    State(st): State<Arc<AppState>>,
    user: AuthUser,
    Path(id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    own(&st.conn(), &user, id)?;
    let ctx = st.tasks.clone();
    let token = crate::worker::CancelToken::new();
    let v = tokio::task::spawn_blocking(move || crate::tasks::run_exams(&ctx, id, &token))
        .await
        .map_err(|e| ApiError::bad_request(format!("取考试列表失败：{e}")))?
        .map_err(|e| ApiError::bad_request(e.to_string()))?;
    Ok(Json(v))
}

/// `GET /api/accounts/:id/exams/:exam_id/paper?candidate_id=...`
#[derive(Deserialize, Default)]
pub struct PaperQ {
    #[serde(default)]
    pub candidate_id: Option<String>,
}

pub async fn exam_paper(
    State(st): State<Arc<AppState>>,
    user: AuthUser,
    Path((id, exam_id)): Path<(i64, String)>,
    Query(q): Query<PaperQ>,
) -> Result<Json<Value>, ApiError> {
    own(&st.conn(), &user, id)?;
    let candidate_id = q
        .candidate_id
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| ApiError::bad_request("缺少 candidate_id（试卷详情要 examId + candidateId 两个 id）"))?;
    let ctx = st.tasks.clone();
    let token = crate::worker::CancelToken::new();
    let v = tokio::task::spawn_blocking(move || {
        crate::tasks::run_exam_paper(&ctx, id, &exam_id, &candidate_id, &token)
    })
    .await
    .map_err(|e| ApiError::bad_request(format!("取试卷失败：{e}")))?
    .map_err(|e| ApiError::bad_request(e.to_string()))?;
    Ok(Json(v))
}

/// `GET /api/accounts/:id/homeworks` —— 全部作业/测验（含已交）。
pub async fn homeworks(
    State(st): State<Arc<AppState>>,
    user: AuthUser,
    Path(id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    own(&st.conn(), &user, id)?;
    let ctx = st.tasks.clone();
    let token = crate::worker::CancelToken::new();
    let v = tokio::task::spawn_blocking(move || crate::tasks::run_homeworks(&ctx, id, &token))
        .await
        .map_err(|e| ApiError::bad_request(format!("取作业列表失败：{e}")))?
        .map_err(|e| ApiError::bad_request(e.to_string()))?;
    Ok(Json(v))
}

/// `GET /api/accounts/:id/homeworks/:test_id/detail?candidate_id=` —— 作业详情。
#[derive(Deserialize, Default)]
pub struct HomeworkDetailQ {
    #[serde(default)]
    pub candidate_id: Option<String>,
}

pub async fn homework_detail(
    State(st): State<Arc<AppState>>,
    user: AuthUser,
    Path((id, test_id)): Path<(i64, String)>,
    Query(q): Query<HomeworkDetailQ>,
) -> Result<Json<Value>, ApiError> {
    own(&st.conn(), &user, id)?;
    let cand = q.candidate_id.unwrap_or_default();
    let ctx = st.tasks.clone();
    let v = tokio::task::spawn_blocking(move || {
        crate::tasks::run_homework_detail(&ctx, id, &test_id, &cand)
    })
    .await
    .map_err(|e| ApiError::bad_request(format!("取作业详情失败：{e}")))?
    .map_err(|e| ApiError::bad_request(e.to_string()))?;
    Ok(Json(v))
}

/// `GET /api/accounts/:id/homeworks/:test_id/paper` —— 作业试卷（题目/答案/得分）。
pub async fn homework_paper(
    State(st): State<Arc<AppState>>,
    user: AuthUser,
    Path((id, test_id)): Path<(i64, String)>,
) -> Result<Json<Value>, ApiError> {
    own(&st.conn(), &user, id)?;
    let ctx = st.tasks.clone();
    let v = tokio::task::spawn_blocking(move || crate::tasks::run_homework_paper(&ctx, id, &test_id))
        .await
        .map_err(|e| ApiError::bad_request(format!("取作业试卷失败：{e}")))?
        .map_err(|e| ApiError::bad_request(e.to_string()))?;
    Ok(Json(v))
}

/// `PUT /api/accounts/:id/device-id` —— 开关某账号签到是否带 deviceId。
///
/// body: `{ enabled: bool }`
/// 打开时如果还没有 ID，**顺手生成一个**（避免"开了却没生效"）。
#[derive(Deserialize)]
pub struct DeviceIdIn {
    pub enabled: bool,
}

pub async fn set_device_id(
    State(st): State<Arc<AppState>>,
    user: AuthUser,
    Path(id): Path<i64>,
    Json(body): Json<DeviceIdIn>,
) -> Result<Json<Value>, ApiError> {
    own(&st.conn(), &user, id)?;
    let conn = st.conn();
    let now = db::now_ms();
    let cur: String = conn
        .query_row("SELECT COALESCE(device_id,'') FROM accounts WHERE id=?", rusqlite::params![id], |r| r.get(0))
        .unwrap_or_default();
    // 打开时若没有 ID，自动补一个
    let mut generated = false;
    if body.enabled && cur.trim().is_empty() {
        let fresh = new_uuid_v4();
        conn.execute(
            "UPDATE accounts SET device_id=?, updated_at=? WHERE id=?",
            rusqlite::params![fresh, now, id],
        )?;
        generated = true;
    }
    conn.execute(
        "UPDATE accounts SET device_id_enabled=?, updated_at=? WHERE id=?",
        rusqlite::params![if body.enabled { 1 } else { 0 }, now, id],
    )?;
    let now_id: String = conn
        .query_row("SELECT COALESCE(device_id,'') FROM accounts WHERE id=?", rusqlite::params![id], |r| r.get(0))
        .unwrap_or_default();
    Ok(Json(json!({
        "ok": true,
        "enabled": body.enabled,
        "device_id": now_id,
        "generated": generated,
        "master_on": crate::tasks::device_id_master_on(),
    })))
}

/// `GET /api/device-config` —— 模拟设备 ID 的**全局开关状态**。
///
/// 前端据此显示开关是否生效（关掉时所有账号都不带 deviceId）。
pub async fn device_config(
    // ⚠️ 这个 `AuthUser` 不是摆设 —— 本项目的鉴权**没有全局中间件**，
    // 全靠每个 handler 自己声明 `AuthUser` 来触发 401。
    // 早先这个函数没声明，导致 `GET /api/device-config` **完全无需登录**，
    // 把设备 ID 伪造开关的说明直接给了任何能访问端口的人。
    user: AuthUser,
) -> Result<Json<Value>, ApiError> {
    user.require("user")?;
    Ok(Json(json!({
        "master_on": crate::tasks::device_id_master_on(),
        "env_var": "NEUMOOC_DEVICE_ID",
        "note": "关掉后所有签到都不带 deviceId；每个账号的 device_id 各自独立，\
                 平台只在同一 deviceId 对应多个学号时才判考勤异常。",
    })))
}

/// `GET /api/executions`
#[derive(Deserialize, Default)]
pub struct ExecQ {
    #[serde(default)]
    pub limit: Option<i64>,
    #[serde(default)]
    pub account_id: Option<i64>,
}

pub async fn executions(
    State(st): State<Arc<AppState>>,
    user: AuthUser,
    Query(q): Query<ExecQ>,
) -> Result<Json<Value>, ApiError> {
    let conn = st.conn();
    // 先按权限过滤出可见账号，再查 —— 不要先查全部再过滤，那样容易漏
    let visible: Option<Vec<i64>> = if crate::perm::unrestricted(&user.role) {
        None
    } else {
        let mut stmt = conn.prepare("SELECT account_id FROM user_accounts WHERE username=?")?;
        let rows = stmt.query_map(rusqlite::params![user.username], |r| r.get::<_, i64>(0))?;
        Some(rows.filter_map(|r| r.ok()).collect())
    };
    let limit = q.limit.unwrap_or(50).clamp(1, 500);
    let mut all = query_json(
        &conn,
        "SELECT * FROM executions ORDER BY id DESC LIMIT ?",
        &[&limit],
    )?;
    all.retain(|e| {
        let aid = e.get("account_id").and_then(Value::as_i64).unwrap_or(-1);
        match (&visible, q.account_id) {
            // None = 不受限；Some(vec![]) = 一个都不能访问。别把两者混起来。
            (None, None) => true,
            (None, Some(want)) => aid == want,
            (Some(ids), None) => ids.contains(&aid),
            (Some(ids), Some(want)) => ids.contains(&aid) && aid == want,
        }
    });
    Ok(Json(Value::Array(all)))
}

/// `GET /api/executions/:id/logs`
pub async fn execution_logs(
    State(st): State<Arc<AppState>>,
    user: AuthUser,
    Path(exec_id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    let conn = st.conn();
    let aid: Option<i64> = conn
        .query_row(
            "SELECT account_id FROM executions WHERE id=?",
            rusqlite::params![exec_id],
            |r| r.get(0),
        )
        .ok();
    let aid = aid.ok_or_else(|| ApiError::not_found("执行记录不存在"))?;
    // 用**已有的 conn** 校验，不再取一次锁
    own(&conn, &user, aid)?;
    let rows = query_json(
        &conn,
        "SELECT id, level, message, ts FROM logs WHERE execution_id=? ORDER BY id",
        &[&exec_id],
    )?;
    Ok(Json(Value::Array(rows)))
}

/// `GET /api/schedule`
pub async fn schedule(
    State(st): State<Arc<AppState>>,
    user: AuthUser,
) -> Result<Json<Value>, ApiError> {
    let conn = st.conn();
    let all = store::list_accounts(&conn)?;
    let mut out = Vec::new();
    for a in all {
        if !crate::perm::unrestricted(&user.role) {
            let bound: Option<i64> = conn
                .query_row(
                    "SELECT account_id FROM user_accounts WHERE username=? AND account_id=?",
                    rusqlite::params![user.username, a.id],
                    |r| r.get(0),
                )
                .ok();
            if bound.is_none() {
                continue;
            }
        }
        let cfg = store::read_account_cfg(&conn, a.id, "schedule").unwrap_or_else(|_| json!({}));
        let last: Option<Value> = query_one_json(
            &conn,
            "SELECT id, biz_date, detail, finished_at FROM executions
             WHERE account_id=? AND job_key IN ('check','scan') AND state='done'
             ORDER BY finished_at DESC LIMIT 1",
            &[&a.id],
        )?;
        // ⚠️ 把 `detail` 里的字段**摊平到 last_check 上**。
        // 前端要的是 `last_check.new_resources` / `.todo_resources` /
        // `.pending_tasks` / `.task_error` / `.at`，而 detail 是**字符串** ——
        // 直接给 `{id, biz_date, detail, finished_at}` 的话那几个数字全是
        // undefined，调度页会永远显示「还没检查过」，哪怕检查刚跑完。
        let last = last.map(|v| {
            let det: Value = v
                .get("detail")
                .and_then(Value::as_str)
                .and_then(|x| serde_json::from_str(x).ok())
                .unwrap_or(json!({}));
            json!({
                "at": v.get("finished_at").cloned().unwrap_or(json!(0)),
                "biz_date": v.get("biz_date").cloned().unwrap_or(json!("")),
                "new_resources": det.get("new_resources").cloned().unwrap_or(Value::Null),
                "todo_resources": det.get("todo_resources").cloned().unwrap_or(Value::Null),
                "pending_tasks": det.get("pending_tasks").cloned().unwrap_or(Value::Null),
                "task_error": det.get("task_error").cloned().unwrap_or(json!("")),
            })
        });
        // ⚠️ 必须给 `today_plan` —— 前端拿它显示今天这条计划的**状态徽标和触发时刻**。
        // 少了它页面永远显示「未生成」，哪怕计划已经跑完了。
        let today = db::biz_date_now();
        let today_plan = query_one_json(
            &conn,
            "SELECT id, biz_date, fire_at, state, detail FROM daily_plans
             WHERE account_id=? AND biz_date=? ORDER BY id DESC LIMIT 1",
            &[&a.id, &today],
        )?;
        out.push(json!({
            "account_id": a.id, "label": a.label,
            "enabled": cfg.get("enabled").and_then(Value::as_bool).unwrap_or(false),
            "time": cfg.get("time").and_then(Value::as_str).unwrap_or("08:00"),
            "today_plan": today_plan,
            "last_check": last,
        }));
    }
    Ok(Json(Value::Array(out)))
}

#[derive(Deserialize)]
pub struct SchedIn {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub time: Option<String>,
}

/// `PUT /api/schedule/:id`
pub async fn set_schedule(
    State(st): State<Arc<AppState>>,
    user: AuthUser,
    Path(id): Path<i64>,
    Json(body): Json<SchedIn>,
) -> Result<Json<Value>, ApiError> {
    own(&st.conn(), &user, id)?;
    if let Some(t) = &body.time {
        if crate::scheduler::parse_hhmm(t).is_none() {
            return Err(ApiError::bad_request("时间格式应为 HH:MM（24 小时制）"));
        }
    }
    let conn = st.conn();
    let mut cfg = store::read_account_cfg(&conn, id, "schedule").unwrap_or_else(|_| json!({}));
    let obj = cfg.as_object_mut().expect("配置是对象");
    if let Some(e) = body.enabled {
        obj.insert("enabled".into(), json!(e));
    }
    if let Some(t) = &body.time {
        obj.insert("time".into(), json!(t));
    }
    store::write_account_cfg(&conn, id, "schedule", cfg)
        .map_err(|e| ApiError::bad_request(e.to_string()))?;
    Ok(Json(json!({ "ok": true })))
}

/// `GET /api/logs/stream` 之外的日志查询：按账号最近 N 条。
pub async fn recent_logs(
    State(st): State<Arc<AppState>>,
    user: AuthUser,
    Query(q): Query<ExecQ>,
) -> Result<Json<Value>, ApiError> {
    let conn = st.conn();
    let limit = q.limit.unwrap_or(200).clamp(1, 2000);
    let rows = query_json(
        &conn,
        "SELECT l.*, e.account_id, e.job_key FROM logs l JOIN executions e ON e.id=l.execution_id
         ORDER BY l.id DESC LIMIT ?",
        &[&limit],
    )?;
    let out: Vec<Value> = rows
        .into_iter()
        .filter(|r| {
            if crate::perm::unrestricted(&user.role) {
                return true;
            }
            let aid = r.get("account_id").and_then(Value::as_i64).unwrap_or(-1);
            let bound: Option<i64> = conn
                .query_row(
                    "SELECT account_id FROM user_accounts WHERE username=? AND account_id=?",
                    rusqlite::params![user.username, aid],
                    |x| x.get(0),
                )
                .ok();
            bound.is_some()
        })
        .collect();
    Ok(Json(Value::Array(out)))
}

// ---------------------------------------------------------------- 写接口

#[derive(Deserialize)]
pub struct SignIn {
    pub detail_id: String,
    #[serde(default)]
    pub attendance_id: String,
    /// 定位坐标。**不传就用平台上要求的坐标**（库里已存），所以不需要人到现场。
    #[serde(default)]
    pub longitude: Option<String>,
    #[serde(default)]
    pub latitude: Option<String>,
    #[serde(default)]
    pub address: Option<String>,
}

/// `POST /api/accounts/:id/attendance/sign`
///
/// 交 worker 执行（同账号串行）。**先建执行记录再投递** —— 反过来的话
/// worker 跑得快时日志会写进一条还不存在的 execution，外键失败，
/// 结果就是「签到确实发生了，但界面上什么都看不到」。
pub async fn sign(
    State(st): State<Arc<AppState>>,
    user: AuthUser,
    Path(id): Path<i64>,
    Json(body): Json<SignIn>,
) -> Result<Json<Value>, ApiError> {
    own(&st.conn(), &user, id)?;
    if body.detail_id.trim().is_empty() {
        return Err(ApiError::bad_request("缺少 detail_id"));
    }

    // 从库里补齐场次信息，并**默认用平台要求的坐标**
    let (attendance_id, ty, loc, lon, lat, addr) = {
        let conn = st.conn();
        let r = query_one_json(
            &conn,
            "SELECT attendance_id, attendance_type, location_type, longitude, latitude, address_name
             FROM attendance WHERE account_id=? AND detail_id=?",
            &[&id, &body.detail_id],
        )?
        .ok_or_else(|| ApiError::not_found("考勤场次不存在（先同步一次考勤）"))?;
        let s = |k: &str| r.get(k).and_then(Value::as_str).unwrap_or("").to_string();
        (
            if body.attendance_id.is_empty() { s("attendance_id") } else { body.attendance_id.clone() },
            r.get("attendance_type").and_then(Value::as_i64).unwrap_or(0),
            r.get("location_type").and_then(Value::as_i64),
            // 显式传了就用传的，否则用平台要求的坐标
            body.longitude.clone().or_else(|| Some(s("longitude")).filter(|v| !v.is_empty())),
            body.latitude.clone().or_else(|| Some(s("latitude")).filter(|v| !v.is_empty())),
            body.address.clone().or_else(|| Some(s("address_name")).filter(|v| !v.is_empty())),
        )
    };

    let exec_id = {
        let conn = st.conn();
        crate::store::begin_execution(&conn, id, "sign")?
    };
    let Some(exec_id) = exec_id else {
        return Err(ApiError::bad_request("该账号正在执行其它任务，请稍候"));
    };

    let st2 = st.clone();
    let ctx = st.tasks.clone();
    let detail_id = body.detail_id.clone();
    // ⚠️ 提交必须包在一个**块**里：`w` 是从 `reg`（MutexGuard，!Send）借出来的，
    // 只要它还活着，后面的 `.await` 就会让 future 变成 !Send，
    // axum 报的是「Handler trait 未满足」这种看不出根因的错。
    {
    let mut reg = st.workers.lock().expect("锁中毒");
    let on_done = move |_aid: i64, key: &str, res: &neumooc_core::Result<Value>| {
        if let Ok(conn) = st2.db.lock() {
            let (state, detail) = match res {
                Ok(v) => ("done", v.clone()),
                Err(neumooc_core::error::Error::Invalid(m)) if m == "已取消" => {
                    ("cancelled", json!({ "error": m }))
                }
                Err(e) => ("failed", json!({ "error": e.to_string() })),
            };
            let _ = crate::store::append_log(&conn, exec_id, if state == "done" { "ok" } else { "error" },
                                            &format!("任务 {key} {state}：{detail}"));
            let _ = crate::store::finish_execution(&conn, exec_id, state, detail);
        }
    };
    let w = reg.get_or_spawn(id);
    w.submit(crate::worker::Job::new("sign", move |token| {
        let c = ctx.client(id)?;
        let uid = c.session().user_id.clone();
        token.check()?;
        let req = neumooc_core::attendance::SignRequest {
            detail_id, attendance_id, attendance_type: ty, sign_role: 4,
            sign_user_id: uid,
            status: neumooc_core::attendance::SIGN_STATUS_PRESENT,
            longitude: lon, latitude: lat, address: addr,
            location_type: loc, refresh_seed: None,
            // 预留：账号配了模拟设备 ID 才带（默认空 = 不带）
            device_id: ctx.device_id_for(id),
        };
        c.submit_sign(&req)
    }).on_done(on_done))
    .map_err(|e| ApiError::bad_request(e.to_string()))?;
    } // ← 锁与 w 在这里一起释放

    // **等结果再返回**：前端靠 ok/error 决定是否关面板、是否就地报错。
    // 立刻返回 {ok:true} 会让它把「已受理」当成「签到成功」。
    await_execution(&st, exec_id, 120).await
}

/// `POST /api/accounts/:id/attendance/sync`
///
/// 从平台拉最新考勤落库。这是**唯一会打学校接口的读操作** ——
/// 页面刷新走的是本地缓存，只有用户主动点「同步」才真的去拉。
pub async fn attendance_sync(
    State(st): State<Arc<AppState>>,
    user: AuthUser,
    Path(id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    own(&st.conn(), &user, id)?;
    let exec_id = {
        let conn = st.conn();
        crate::store::begin_execution(&conn, id, "attendance")?
    };
    let Some(exec_id) = exec_id else {
        return Err(ApiError::bad_request("该账号正在执行其它任务，请稍候"));
    };
    let st2 = st.clone();
    let ctx = st.tasks.clone();
    let on_done = move |_aid: i64, key: &str, res: &neumooc_core::Result<Value>| {
        if let Ok(conn) = st2.db.lock() {
            let (state, detail) = match res {
                Ok(v) => ("done", v.clone()),
                Err(e) => ("failed", json!({ "error": e.to_string() })),
            };
            let _ = crate::store::append_log(&conn, exec_id, if state == "done" { "ok" } else { "error" },
                                            &format!("任务 {key} {state}：{detail}"));
            let _ = crate::store::finish_execution(&conn, exec_id, state, detail);
        }
    };
    {
        // ⚠️ 锁必须在块内取、块内放 —— 下面的 `await` 不能占着 worker 注册表
        let mut reg = st.workers.lock().expect("锁中毒");
        let w = reg.get_or_spawn(id);
        w.submit(crate::worker::Job::new("attendance", move |token| {
            crate::tasks::run_attendance(&ctx, id, token)
        }).on_done(on_done))
        .map_err(|e| ApiError::bad_request(e.to_string()))?;
    } // ← 锁在这里释放，下面的 await 不能占着 worker 注册表

    // ⚠️ **必须等结果**。
    //
    // 原先立刻返回 `{ok:true, execution_id}` —— 而前端拿到 `ok` 就 `reload()`，
    // **同步任务还在跑，重读的是旧缓存**。表现是「点了同步考勤，列表还是旧的」。
    //
    // 用户报的就是这个：除了账号1（它的同步恰好很快）以外，
    // 其他账号的考勤列表永远是旧的。
    await_execution(&st, exec_id, 300).await
}

/// `GET /api/users` —— 仅 root。
pub async fn users(
    State(st): State<Arc<AppState>>,
    user: AuthUser,
) -> Result<Json<Value>, ApiError> {
    // 用户管理只有 root 能看 —— admin 也不行
    user.require("root")?;
    let conn = st.conn();
    let rows = query_json(
        &conn,
        "SELECT username, role, created_at, updated_at FROM users ORDER BY username",
        &[],
    )?;
    // ⚠️ 必须补 `role_label` 和 `account_ids` —— 前端要显示中文角色名和
    // 「这个用户绑了哪些账号」。少字段的表现是「页面能开、但那两列是空的」。
    let mut out = Vec::with_capacity(rows.len());
    for mut u in rows {
        let name = u.get("username").and_then(Value::as_str).unwrap_or("").to_string();
        let role = u.get("role").and_then(Value::as_str).unwrap_or("user").to_string();
        let ids = query_json(
            &conn,
            "SELECT account_id FROM user_accounts WHERE username=? ORDER BY account_id",
            &[&name],
        )?;
        let ids: Vec<i64> = ids
            .iter()
            .filter_map(|x| x.get("account_id").and_then(Value::as_i64))
            .collect();
        if let Some(o) = u.as_object_mut() {
            o.insert("role_label".into(), json!(crate::perm::role_label(&role)));
            o.insert("account_ids".into(), json!(ids));
        }
        out.push(u);
    }
    Ok(Json(Value::Array(out)))
}

/// `GET /api/notify/status`
///
/// 一个「配错了会静默不工作」的功能必须能自证状态。
pub async fn notify_status(user: AuthUser) -> Result<Json<Value>, ApiError> {
    user.require("user")?;
    // ⚠️ **不要在这里 `Notifier::from_env()`** —— 它内部会建
    // `reqwest::blocking::Client`，而在 **async 上下文里建 blocking client 会 panic**
    // （reqwest 的 "Cannot drop a runtime in a context where blocking is not allowed"）。
    // 表现是接口返回 HTTP 000 / 连接被重置，日志里只有一行 panic。
    //
    // 状态查询本来也不需要客户端 —— 直接读环境变量即可。
    let bot = std::env::var("NEUMOOC_QQ_BOT_ID").unwrap_or_default();
    let target = std::env::var("NEUMOOC_QQ_TARGET_ID").unwrap_or_default();
    let configured = !bot.trim().is_empty() && !target.trim().is_empty();
    let mode = crate::notify::Mode::parse(
        &std::env::var("NEUMOOC_NOTIFY_MODE").unwrap_or_else(|_| "summary".into()),
    );
    Ok(Json(json!({
        "enabled": configured && mode != crate::notify::Mode::Off,
        "configured": configured,
        "mode": format!("{mode:?}").to_lowercase(),
        // 待汇总条数得问真正在用的那个 Notifier；这里没有全局实例，
        // 所以只报「不适用」而不是编一个 0 出来
        "pending": Value::Null,
        "summary_at": std::env::var("NEUMOOC_NOTIFY_SUMMARY_AT").unwrap_or_else(|_| "22:00".into()),
    })))
}

#[derive(Deserialize)]
pub struct AccountIn {
    pub username: String,
    pub password: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub tenant_id: String,
}

/// `POST /api/accounts` —— 添加账号。仅 admin。
///
/// 密码用 **Fernet 加密**后入库（与 Python 版同一个格式）——
/// 它是能直接登录学校系统的凭据，泄露后果远大于平台自身密码。
/// 注意这与平台用户密码不是一回事：那个用 Argon2（单向），这个必须可逆。
pub async fn create_account(
    State(st): State<Arc<AppState>>,
    user: AuthUser,
    Json(body): Json<AccountIn>,
) -> Result<Json<Value>, ApiError> {
    user.require("admin")?;
    if body.username.trim().is_empty() || body.password.is_empty() {
        return Err(ApiError::bad_request("手机号与密码不能为空"));
    }
    let tenant = if body.tenant_id.trim().is_empty() {
        // 默认租户：与现有账号保持一致
        let conn = st.conn();
        let t: Option<String> = conn
            .query_row("SELECT tenant_id FROM accounts LIMIT 1", [], |r| r.get(0))
            .ok();
        t.unwrap_or_default()
    } else {
        body.tenant_id.clone()
    };
    if tenant.is_empty() {
        return Err(ApiError::bad_request("没有可用的租户 ID，请显式提供 tenant_id"));
    }
    let enc = st
        .tasks
        .cipher
        .encrypt(body.password.as_bytes())
        .map_err(|e| ApiError::bad_request(format!("加密失败：{e}")))?;
    // 锁只覆盖「插库」这一段，后面要 await（验证登录），不能占着
    let id = {
        let conn = st.conn();
        crate::store::create_account(&conn, "", &body.username, enc.as_bytes(), &tenant)
        .map_err(|e| {
            // UNIQUE(tenant_id, username) 撞了要给能看懂的话，不是 500
            let s = e.to_string();
            if s.contains("UNIQUE") {
                ApiError::bad_request("这个账号已经添加过了")
            } else {
                ApiError::bad_request(s)
            }
        })?
    }; // ← 锁在这里释放

    // ⚠️ **加完必须验证登录**，并把备注填成**真实姓名**。
    //
    // 原先这里只加密+插库，备注留空时用**学号**兜底 —— 而其他几个平台
    // 默认都是真名。界面上到处是 `00000000000` 这种，扫一眼认不出是谁。
    //
    // 验证失败的账号要**回滚删掉** —— 留着登不上的空账号比不加更糟
    // （用户以为加好了，实际每次任务都失败）。
    let name = {
        let ctx = st.tasks.clone();
        let probe = tokio::task::spawn_blocking(move || -> neumooc_core::Result<String> {
            let mut c = ctx.client(id)?;
            let _ = c.with_reauth(|c| c.list_terms())?; // 确认登录真的能用
            Ok(c.real_name())
        })
        .await
        .map_err(|e| ApiError::bad_request(format!("验证任务失败：{e}")))?;
        match probe {
            Ok(n) => n,
            Err(e) => {
                // 回滚：不留登不上的账号
                if let Ok(conn) = st.db.lock() {
                    let _ = conn.execute("DELETE FROM accounts WHERE id=?", rusqlite::params![id]);
                }
                return Err(ApiError::bad_request(format!(
                    "登录验证失败，账号未添加：{e}"
                )));
            }
        }
    };
    let final_label = if body.label.trim().is_empty() {
        // 拿不到真名才退回学号（并把这件事说出来，不静默）
        if name.is_empty() { body.username.clone() } else { name.clone() }
    } else {
        body.label.clone()
    };
    {
        let conn = st.conn();
        conn.execute(
            "UPDATE accounts SET label=?, updated_at=? WHERE id=?",
            rusqlite::params![final_label, db::now_ms(), id],
        )?;
    }
    Ok(Json(json!({
        "ok": true, "id": id, "label": final_label,
        "note": if name.is_empty() && body.label.trim().is_empty() {
            "拿不到真实姓名，备注暂用学号"
        } else { "" },
    })))
}

/// `DELETE /api/accounts/:id` —— 删除账号（级联删掉它的课程/资源/考勤/执行）。仅 admin。
pub async fn delete_account(
    State(st): State<Arc<AppState>>,
    user: AuthUser,
    Path(id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    user.require("admin")?;
    let conn = st.conn();
    let n = conn.execute("DELETE FROM accounts WHERE id=?", rusqlite::params![id])?;
    if n == 0 {
        return Err(ApiError::not_found("账号不存在"));
    }
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
pub struct AnswerIn {
    /// 学习任务里的 `ctlId`（= `resCtl.id`，也就是作答页的 testId）
    pub test_id: String,
    /// `[{questionNo, answer}]`
    #[serde(default)]
    pub answers: Vec<Value>,
    /// **默认 false，只存草稿**。true 会真的交卷，不可逆。
    #[serde(default)]
    pub submit: bool,
    #[serde(default)]
    pub dry_run: bool,
}

/// `POST /api/accounts/:id/tasks/answer` —— 作答学习任务。
///
/// **默认只存草稿**。`submit=true` 会真的交卷且不可逆，所以要求 admin，
/// 而且调用方必须显式传。
pub async fn task_answer(
    State(st): State<Arc<AppState>>,
    user: AuthUser,
    Path(id): Path<i64>,
    Json(body): Json<AnswerIn>,
) -> Result<Json<Value>, ApiError> {
    own(&st.conn(), &user, id)?;
    if body.submit {
        user.require("admin")?; // 交卷是不可逆的重操作
    }
    if body.test_id.trim().is_empty() {
        return Err(ApiError::bad_request("缺少 test_id（学习任务里的 ctl_id）"));
    }
    if body.answers.is_empty() {
        return Err(ApiError::bad_request("没有要保存的答案"));
    }

    let exec_id = {
        let conn = st.conn();
        crate::store::begin_execution(&conn, id, "task_answer")?
    };
    let Some(exec_id) = exec_id else {
        return Err(ApiError::bad_request("该账号正在执行其它任务，请稍候"));
    };

    let st2 = st.clone();
    let ctx = st.tasks.clone();
    let (test_id, answers, submit, dry) = (
        body.test_id.clone(), body.answers.clone(), body.submit, body.dry_run,
    );
    let on_done = move |_aid: i64, key: &str, res: &neumooc_core::Result<Value>| {
        if let Ok(conn) = st2.db.lock() {
            let (state, detail) = match res {
                Ok(v) => ("done", v.clone()),
                Err(e) => ("failed", json!({ "error": e.to_string() })),
            };
            let _ = crate::store::append_log(&conn, exec_id, if state == "done" { "ok" } else { "error" },
                                            &format!("任务 {key} {state}：{detail}"));
            let _ = crate::store::finish_execution(&conn, exec_id, state, detail);
        }
    };
    {
        // 锁必须在块内取放 —— 下面要 await，Guard 跨 await 会让 future 不是 Send
        let mut reg = st.workers.lock().expect("锁中毒");
        let w = reg.get_or_spawn(id);
        w.submit(crate::worker::Job::new("task_answer", move |token| {
        let c = ctx.client(id)?;
        token.check()?;
        // enter 拿 candidateId / historyId / 试卷（幂等，不消耗作答次数）
        let e = c.enter_task(&test_id)?;
        // 把「题号 → 答案」映射到 paper 里的题目
        let mut by_no = std::collections::HashMap::new();
        for q in e.paper.questions() {
            by_no.insert(q.question_no.clone(), q);
        }
        let mut out = Vec::new();
        let mut skipped = Vec::new();
        for a in &answers {
            let no = a.get("questionNo").and_then(Value::as_str).unwrap_or("").to_string();
            let Some(q) = by_no.get(&no) else {
                skipped.push(no);
                continue;
            };
            // 前端把文件题的上传结果存成 `file:<eid>|<name>`。
            // 平台要的是**对象**（不是字符串）：
            //   {"stuAnswer":"","attachmentList":[{"eid":"...","filename":"..."}]}
            // 普通题照旧传字符串。
            let raw = a.get("answer").and_then(Value::as_str).unwrap_or("");
            let ans: Value = if let Some(rest) = raw.strip_prefix("file:") {
                let (eid, name) = rest.split_once('|').unwrap_or((rest, ""));
                json!({
                    "stuAnswer": "",
                    "attachmentList": [{
                        "eid": eid,
                        "filename": name,
                        // 平台按 filename 生成预览，eid 决定取哪个文件
                    }]
                })
            } else {
                json!(raw)
            };
            out.push(neumooc_core::homework::Answer {
                paper_id: q.id.clone(),
                question_no: q.question_no.clone(),
                question_type: q.question_type.clone(),
                answer: ans,
            });
        }
        if dry {
            return Ok(json!({ "dry_run": true, "history_id": e.history_id,
                              "would_save": out.len(), "skipped": skipped }));
        }
        if out.is_empty() {
            return Err(neumooc_core::Error::Invalid("题号对不上试卷里的题目".into()));
        }
        let r = c.save_task_answer(&e.candidate_id, &test_id, &e.history_id, &out)?;
        let mut res = json!({ "saved": out.len(), "skipped": skipped,
                              "history_id": e.history_id, "submitted": false });
        if submit {
            c.submit_task(&e.history_id)?;
            res["submitted"] = json!(true);
        }
        let _ = r;
        Ok(res)
        }).on_done(on_done))
        .map_err(|e| ApiError::bad_request(e.to_string()))?;
    }

    // **等结果再返回**：前端要靠 `data.saved` 显示「存了几题」，
    // 靠 `error` 显示失败原因。立刻返回 `{state:"running"}` 会让它
    // 把「已受理」当成「已保存」，失败时也显示成功（就是 401 那次的情况）。
    await_execution(&st, exec_id, 180).await
}

/// `POST /api/accounts/:id/tasks/ai-answer` —— **AI 批量作答**（一次调用答完整份）。
///
/// body：`{ test_id, submit?, dry_run?, only_no?, max? }`
///
/// 与 `/tasks/answer` 的分工：
/// * `/tasks/answer`    —— **手动作答**：前端把用户填的 `{questionNo, answer}` 传进来
/// * `/tasks/ai-answer` —— **AI 批量**：后端自己取题、调模型、算答案
#[derive(Deserialize)]
pub struct AiAnswerIn {
    pub test_id: String,
    #[serde(default)]
    pub submit: bool,
    #[serde(default)]
    pub dry_run: bool,
    #[serde(default)]
    pub only_no: Vec<String>,
    #[serde(default)]
    pub max: Option<u64>,
    /// **只投递不等待**。前端拿 `execution_id` 去订阅 SSE 实时日志，
    /// 算完再自己拉结果。默认 false（等结果一起返回）。
    #[serde(default)]
    pub no_wait: bool,
}

pub async fn task_ai_answer(
    State(st): State<Arc<AppState>>,
    user: AuthUser,
    Path(id): Path<i64>,
    Json(body): Json<AiAnswerIn>,
) -> Result<Json<Value>, ApiError> {
    own(&st.conn(), &user, id)?;
    // 交卷不可逆 → 要求 admin
    if body.submit {
        user.require("admin")?;
    }
    if body.test_id.trim().is_empty() {
        return Err(ApiError::bad_request("缺少 test_id"));
    }
    let exec_id = {
        let conn = st.conn();
        crate::store::begin_execution(&conn, id, "ai_answer")?
    };
    let Some(exec_id) = exec_id else {
        return Err(ApiError::bad_request("该账号正在执行其它任务，请稍候"));
    };

    let st2 = st.clone();
    let ctx = st.tasks.clone();
    let mut args = json!({ "test_id": body.test_id, "submit": body.submit, "dry_run": body.dry_run });
    if !body.only_no.is_empty() {
        args["only_no"] = json!(body.only_no);
    }
    if let Some(m) = body.max {
        args["max"] = json!(m);
    }
    let on_done = move |_aid: i64, key: &str, res: &neumooc_core::Result<Value>| {
        if let Ok(conn) = st2.db.lock() {
            let (state, detail) = match res {
                Ok(v) => ("done", v.clone()),
                Err(e) => ("failed", json!({ "error": e.to_string() })),
            };
            let _ = crate::store::append_log(
                &conn, exec_id,
                if state == "done" { "ok" } else { "error" },
                &format!("任务 {key} {state}"),
            );
            let _ = crate::store::finish_execution(&conn, exec_id, state, detail);
        }
    };
    {
        // 锁必须在块内取放 —— 下面的 await 不能占着 worker 注册表
        let mut reg = st.workers.lock().expect("锁中毒");
        let w = reg.get_or_spawn(id);
        w.submit(crate::worker::Job::new("ai_answer", move |token| {
            crate::tasks::run_ai_answer_bulk_with(&ctx, id, &args, token, Some(exec_id))
        }).on_done(on_done))
        .map_err(|e| ApiError::bad_request(e.to_string()))?;
    }

    // **只投递**：前端要拿 execution_id 去订阅 SSE，实时看思考过程，
    // 算完再自己拉结果（AI 作答可能 30~60 秒，干等体验很差）
    if body.no_wait {
        return Ok(Json(json!({ "ok": true, "execution_id": exec_id, "state": "running" })));
    }

    // 等结果：前端要拿 answers/logs 展示
    await_execution(&st, exec_id, 300).await
}

/// `GET /api/llm/status` —— 只回状态，**绝不回密钥**。
pub async fn llm_status(
    State(st): State<Arc<AppState>>,
    user: AuthUser,
) -> Result<Json<Value>, ApiError> {
    user.require("user")?;
    let conn = st.conn();
    let key = store::setting(&conn, "NEUMOOC_LLM_API_KEY").unwrap_or_default();
    Ok(Json(json!({
        "available": !key.is_empty(),
        "model": store::setting(&conn, "NEUMOOC_LLM_MODEL")
            .unwrap_or_else(|| "glm-4.5-flash".into()),
        "base_url": store::setting(&conn, "NEUMOOC_LLM_BASE_URL")
            .unwrap_or_else(|| "https://api.z.ai/api/paas/v4".into()),
    })))
}

/// `GET /api/login-guard` —— 登录爆破防护的当前状态（仅 root）。
///
/// 只回**打码后**的用户名/IP —— 状态接口不该把完整标识打出去。
pub async fn login_guard_status(
    State(st): State<Arc<AppState>>,
    user: AuthUser,
) -> Result<Json<Value>, ApiError> {
    user.require("root")?;
    Ok(Json(st.login_guard.snapshot()))
}

/// `GET /api/llm/config` —— 读完整配置（**key 打码**）。
pub async fn llm_config_get(
    State(st): State<Arc<AppState>>,
    user: AuthUser,
) -> Result<Json<Value>, ApiError> {
    user.require("admin")?; // 配置里有 key 的存在性，只有管理员能看
    let conn = st.conn();
    let key = store::setting(&conn, "NEUMOOC_LLM_API_KEY").unwrap_or_default();
    Ok(Json(json!({
        // **绝不回明文 key** —— 只告诉前端"有没有配"和前 4 位便于辨认
        "has_key": !key.is_empty(),
        "key_hint": if key.len() > 8 {
            format!("{}…{}", &key[..4], &key[key.len() - 4..])
        } else { String::new() },
        "model": store::setting(&conn, "NEUMOOC_LLM_MODEL")
            .unwrap_or_else(|| "glm-4.5-flash".into()),
        "base_url": store::setting(&conn, "NEUMOOC_LLM_BASE_URL")
            .unwrap_or_else(|| "https://api.z.ai/api/paas/v4".into()),
        // 被环境变量压住时，界面改了也不生效 —— 要明确告诉用户
        "key_from_env": store::setting_from_env("NEUMOOC_LLM_API_KEY"),
        "model_from_env": store::setting_from_env("NEUMOOC_LLM_MODEL"),
        "base_url_from_env": store::setting_from_env("NEUMOOC_LLM_BASE_URL"),
        // 思考强度 / 流式
        "effort": store::setting(&conn, "NEUMOOC_LLM_EFFORT")
            .unwrap_or_else(|| "medium".into()),
        "stream": store::setting(&conn, "NEUMOOC_LLM_STREAM")
            .map(|v| matches!(v.to_ascii_lowercase().as_str(), "1" | "on" | "true" | "yes"))
            .unwrap_or(false),
        "effort_from_env": store::setting_from_env("NEUMOOC_LLM_EFFORT"),
        "stream_from_env": store::setting_from_env("NEUMOOC_LLM_STREAM"),
        // 可选值（前端下拉用，避免两边写死不一致）
        "efforts": ["off", "minimal", "low", "medium", "high", "max"],
    })))
}

/// `PUT /api/llm/config` —— 保存配置。`api_key` 留空 = **不修改**（保留原值）。
#[derive(Deserialize)]
pub struct LlmConfigIn {
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub base_url: Option<String>,
    /// 思考强度：off/minimal/low/medium/high/max
    #[serde(default)]
    pub effort: Option<String>,
    /// 是否流式
    #[serde(default)]
    pub stream: Option<bool>,
    /// 传 true 显式清除已保存的 key
    #[serde(default)]
    pub clear_key: bool,
}

pub async fn llm_config_set(
    State(st): State<Arc<AppState>>,
    user: AuthUser,
    Json(body): Json<LlmConfigIn>,
) -> Result<Json<Value>, ApiError> {
    user.require("admin")?;
    let conn = st.conn();
    if body.clear_key {
        store::set_setting(&conn, "NEUMOOC_LLM_API_KEY", "")
            .map_err(|e| ApiError::bad_request(e.to_string()))?;
    } else if let Some(k) = body.api_key.as_ref() {
        // 空串 = 不改（前端回填的是打码值，不能拿它覆盖真 key）
        if !k.trim().is_empty() {
            store::set_setting(&conn, "NEUMOOC_LLM_API_KEY", k.trim())
                .map_err(|e| ApiError::bad_request(e.to_string()))?;
        }
    }
    if let Some(m) = body.model.as_ref() {
        store::set_setting(&conn, "NEUMOOC_LLM_MODEL", m.trim())
            .map_err(|e| ApiError::bad_request(e.to_string()))?;
    }
    if let Some(b) = body.base_url.as_ref() {
        store::set_setting(&conn, "NEUMOOC_LLM_BASE_URL", b.trim())
            .map_err(|e| ApiError::bad_request(e.to_string()))?;
    }
    if let Some(e) = body.effort.as_ref() {
        // 只收合法值 —— 写进库里的东西要能被 llm_config() 解析出来
        let v = e.trim().to_ascii_lowercase();
        if !v.is_empty() && crate::llm::Effort::parse(&v).is_none() {
            return Err(ApiError::bad_request(format!("未知的思考强度「{e}」")));
        }
        store::set_setting(&conn, "NEUMOOC_LLM_EFFORT", &v)
            .map_err(|e| ApiError::bad_request(e.to_string()))?;
    }
    if let Some(s) = body.stream {
        store::set_setting(&conn, "NEUMOOC_LLM_STREAM", if s { "on" } else { "" })
            .map_err(|e| ApiError::bad_request(e.to_string()))?;
    }
    Ok(Json(json!({ "ok": true })))
}

/// `POST /api/llm/test` —— 用当前配置发一次真实请求，验证 key/base/model 是否可用。
pub async fn llm_test(
    State(st): State<Arc<AppState>>,
    user: AuthUser,
) -> Result<Json<Value>, ApiError> {
    user.require("admin")?;
    let cfg = st
        .tasks
        .llm_config()
        .map_err(|e| ApiError::bad_request(e.to_string()))?;
    // 真发一次最小请求 —— 只验连通性和鉴权，不在界面上假装"配置成功"
    let out = tokio::task::spawn_blocking(move || -> Result<String, String> {
        let url = format!("{}/chat/completions", cfg.base_url.trim_end_matches('/'));
        let http = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .map_err(|e| e.to_string())?;
        let resp = http
            .post(&url)
            .header("Authorization", format!("Bearer {}", cfg.api_key))
            .json(&json!({
                "model": cfg.model,
                "messages": [{ "role": "user", "content": "只回复两个字：可用" }],
                "temperature": 0,
                "max_tokens": 16,
            }))
            .send()
            .map_err(|e| e.to_string())?;
        let status = resp.status();
        let text = resp.text().unwrap_or_default();
        if !status.is_success() {
            return Err(format!("HTTP {status}：{}", text.chars().take(200).collect::<String>()));
        }
        let v: Value = serde_json::from_str(&text).map_err(|_| "响应不是 JSON".to_string())?;
        Ok(v.get("choices").and_then(|a| a.get(0))
            .and_then(|c| c.get("message"))
            .and_then(|m| m.get("content"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string())
    })
    .await
    .map_err(|e| ApiError::bad_request(format!("测试线程失败：{e}")))?;

    match out {
        Ok(s) => Ok(Json(json!({ "ok": true, "reply": s }))),
        Err(e) => Ok(Json(json!({ "ok": false, "error": e }))),
    }
}

// ---------------------------------------------------------------- 最后几个

#[derive(Deserialize)]
pub struct RunNowIn {
    #[serde(default)]
    pub job: Option<String>,
}

/// `POST /api/schedule/:id/run-now` —— 不等计划时刻，立刻跑一次检查。
///
/// 与 `/run` 的区别只是默认任务不同（这里默认 `scan`，也就是「每日检查」）。
pub async fn run_now(
    State(st): State<Arc<AppState>>,
    user: AuthUser,
    Path(id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    own(&st.conn(), &user, id)?;
    let exec_id = {
        let conn = st.conn();
        crate::store::begin_execution(&conn, id, "check")?
    };
    let Some(exec_id) = exec_id else {
        return Err(ApiError::bad_request("该账号正在执行其它任务，请稍候"));
    };
    let st2 = st.clone();
    let ctx = st.tasks.clone();
    let mut reg = st.workers.lock().expect("锁中毒");
    let on_done = move |_aid: i64, key: &str, res: &neumooc_core::Result<Value>| {
        if let Ok(conn) = st2.db.lock() {
            let (state, detail) = match res {
                Ok(v) => ("done", v.clone()),
                Err(e) => ("failed", json!({ "error": e.to_string() })),
            };
            let _ = crate::store::append_log(&conn, exec_id, if state == "done" { "ok" } else { "error" },
                                            &format!("任务 {key} {state}：{detail}"));
            let _ = crate::store::finish_execution(&conn, exec_id, state, detail);
        }
    };
    let w = reg.get_or_spawn(id);
    w.submit(crate::worker::Job::new("check", move |token| {
        crate::tasks::run_scan(&ctx, id, token)
    }).on_done(on_done))
    .map_err(|e| ApiError::bad_request(e.to_string()))?;
    Ok(Json(json!({ "ok": true, "execution_id": exec_id })))
}

/// `POST /api/notify/test` —— 发一条测试消息，**同步**返回是否真的送达。
///
/// 这是唯一能确认「通道确实通」的办法 —— 只看 `enabled` 说明不了对端收得到
/// （botId 失效、目标被删、DSH 没起，都会让它静默失败）。
/// 要求 admin：它会真的往群里发消息。
pub async fn notify_test(user: AuthUser) -> Result<Json<Value>, ApiError> {
    user.require("admin")?;
    // ⚠️ `Notifier` 内部是 blocking client，**必须 spawn_blocking** ——
    // 直接在 async handler 里构造或调用会 panic（见 notify_status 的注释）。
    let r = tokio::task::spawn_blocking(|| {
        let n = crate::notify::Notifier::from_env();
        if !n.enabled() {
            return Err("通知未配置或已关闭。需要设置 NEUMOOC_QQ_BOT_ID 与 NEUMOOC_QQ_TARGET_ID。".to_string());
        }
        let (ok, detail) = n.send_now("🔔 neumooc 测试消息：看到这条说明通知通道已接通。");
        if ok { Ok(()) } else { Err(format!("投递失败：{detail}")) }
    })
    .await
    .map_err(|e| ApiError::bad_request(format!("任务执行失败：{e}")))?;
    match r {
        Ok(()) => Ok(Json(json!({ "ok": true, "sent": true }))),
        Err(msg) => Err(ApiError::bad_request(msg)),
    }
}

#[derive(Deserialize)]
pub struct AskIn {
    pub question: String,
    #[serde(default)]
    pub context: String,
}

/// `POST /api/llm/ask` —— 把题目交给模型生成**参考答案草稿**。
///
/// ⚠️ 是草稿，不是答案。服务端**不做任何自动提交** —— 主观题猜错会留痕、
/// 可能计分，比不交更糟。
pub async fn llm_ask(user: AuthUser, Json(body): Json<AskIn>) -> Result<Json<Value>, ApiError> {
    user.require("user")?;
    if body.question.trim().is_empty() {
        return Err(ApiError::bad_request("题目内容不能为空"));
    }
    let key = std::env::var("NEUMOOC_LLM_API_KEY").unwrap_or_default();
    if key.trim().is_empty() {
        return Err(ApiError::bad_request("AI 辅助未配置。需要设置 NEUMOOC_LLM_API_KEY 后重启后端。"));
    }
    let base = std::env::var("NEUMOOC_LLM_BASE_URL")
        .unwrap_or_else(|_| "https://api.z.ai/api/paas/v4".into());
    let model = std::env::var("NEUMOOC_LLM_MODEL").unwrap_or_else(|_| "glm-4.5-flash".into());
    let model_for_task = model.clone();

    // 同样是 blocking client → spawn_blocking
    let (q, c) = (body.question.clone(), body.context.clone());
    let text = tokio::task::spawn_blocking(move || -> Result<String, String> {
        let http = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(120))
            .build()
            .map_err(|e| e.to_string())?;
        let prompt = if c.trim().is_empty() {
            q.clone()
        } else {
            format!("{c}\n\n---\n{q}")
        };
        let resp = http
            .post(format!("{base}/chat/completions"))
            .bearer_auth(&key)
            .json(&serde_json::json!({
                "model": model_for_task,
                "messages": [
                    // 提示词刻意保守：要求不确定就明说，不编造
                    {"role": "system", "content":
                     "你是课程作业辅助助手。要求：1) 选择题给字母和简短理由，主观题给要点式答案；\
                      2) 不确定就明说，不要编造题面里没有的条件；3) 不要客套话；\
                      4) 用中文，公式用纯文本（如 x^2），不要用 LaTeX；\
                      5) 最后另起一行标注「把握程度：高/中/低」并简述原因。"},
                    {"role": "user", "content": prompt}
                ],
                "temperature": 0.3
            }))
            .send()
            .map_err(|e| format!("请求失败：{e}"))?;
        if !resp.status().is_success() {
            let st = resp.status();
            let t = resp.text().unwrap_or_default();
            return Err(format!("模型返回 HTTP {st}：{}", &t.chars().take(200).collect::<String>()));
        }
        let v: Value = resp.json().map_err(|e| format!("返回非 JSON：{e}"))?;
        v["choices"][0]["message"]["content"]
            .as_str()
            .map(|s| s.to_string())
            .ok_or_else(|| "返回结构异常".to_string())
    })
    .await
    .map_err(|e| ApiError::bad_request(format!("任务执行失败：{e}")))?;

    match text {
        Ok(t) => Ok(Json(json!({
            "ok": true, "answer": t, "model": model,
            "disclaimer": "AI 生成的参考答案，需人工确认后再使用。",
        }))),
        Err(e) => Err(ApiError::bad_request(e)),
    }
}

/// `POST /api/accounts/:id/force-checkin` —— 学生 token 借教师端接口补签。
///
/// 走 worker 串行。**已结束的场次要临时重开**（会让整班在几十秒内看到
/// 「进行中」），所以要求 admin。
pub async fn force_checkin(
    State(st): State<Arc<AppState>>,
    user: AuthUser,
    Path(id): Path<i64>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    own(&st.conn(), &user, id)?;
    user.require("admin")?;
    let exec_id = {
        let conn = st.conn();
        crate::store::begin_execution(&conn, id, "force_checkin")?
    };
    let Some(exec_id) = exec_id else {
        return Err(ApiError::bad_request("该账号正在执行其它任务，请稍候"));
    };
    let st2 = st.clone();
    let ctx = st.tasks.clone();
    let args = body.clone();
    let on_done = move |_aid: i64, key: &str, res: &neumooc_core::Result<Value>| {
        if let Ok(conn) = st2.db.lock() {
            let (state, detail) = match res {
                Ok(v) => ("done", v.clone()),
                Err(e) => ("failed", json!({ "error": e.to_string() })),
            };
            let _ = crate::store::append_log(&conn, exec_id, if state == "done" { "ok" } else { "error" },
                                            &format!("任务 {key} {state}：{detail}"));
            let _ = crate::store::finish_execution(&conn, exec_id, state, detail);
        }
    };
    {
        // 锁必须在块内取放 —— 下面的 await 不能占着 worker 注册表
        let mut reg = st.workers.lock().expect("锁中毒");
        let w = reg.get_or_spawn(id);
        w.submit(crate::worker::Job::new("force_checkin", move |token| {
            crate::tasks::run_force_checkin(&ctx, id, &args, token)
        }).on_done(on_done))
        .map_err(|e| ApiError::bad_request(e.to_string()))?;
    } // ← 锁在这里释放

    // ⚠️ **必须等结果** —— 前端（`MakeupPanel`）读的是响应里的 `data.logs`，
    // 立刻返回的话那个字段根本不存在，用户看到的是「操作成功」但没有任何明细。
    //
    // 这和 `attendance/sync` 是同一类问题（Rust 版有一批端点写成了「投递即返回」，
    // 而前端都是按 Python 版的同步语义写的）。
    await_execution(&st, exec_id, 300).await
}

/// `POST /api/accounts/:id/teacher-makeup` —— 教师代签（signRole=3）。
pub async fn teacher_makeup(
    State(st): State<Arc<AppState>>,
    user: AuthUser,
    Path(id): Path<i64>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    own(&st.conn(), &user, id)?;
    user.require("admin")?;
    let exec_id = {
        let conn = st.conn();
        crate::store::begin_execution(&conn, id, "teacher_makeup")?
    };
    let Some(exec_id) = exec_id else {
        return Err(ApiError::bad_request("该账号正在执行其它任务，请稍候"));
    };
    let st2 = st.clone();
    let ctx = st.tasks.clone();
    let args = body.clone();
    let on_done = move |_aid: i64, key: &str, res: &neumooc_core::Result<Value>| {
        if let Ok(conn) = st2.db.lock() {
            let (state, detail) = match res {
                Ok(v) => ("done", v.clone()),
                Err(e) => ("failed", json!({ "error": e.to_string() })),
            };
            let _ = crate::store::append_log(&conn, exec_id, if state == "done" { "ok" } else { "error" },
                                            &format!("任务 {key} {state}：{detail}"));
            let _ = crate::store::finish_execution(&conn, exec_id, state, detail);
        }
    };
    {
        let mut reg = st.workers.lock().expect("锁中毒");
        let w = reg.get_or_spawn(id);
        w.submit(crate::worker::Job::new("teacher_makeup", move |token| {
            crate::tasks::run_teacher_makeup(&ctx, id, &args, token)
        }).on_done(on_done))
        .map_err(|e| ApiError::bad_request(e.to_string()))?;
    } // ← 锁在这里释放

    // ⚠️ **必须等结果** —— 前端（`MakeupPanel`）读的是响应里的 `data.logs`，
    // 立刻返回的话那个字段根本不存在，用户看到的是「操作成功」但没有任何明细。
    //
    // 这和 `attendance/sync` 是同一类问题（Rust 版有一批端点写成了「投递即返回」，
    // 而前端都是按 Python 版的同步语义写的）。
    await_execution(&st, exec_id, 300).await
}

/// 等一个执行跑完，返回 `{ok, data, error}` —— **与 Python 版 `_run_job(wait=True)` 同形**。
///
/// ## 为什么必须等
///
/// 前端（`SignDialog` 等）靠 `r.ok === false` 判断失败并**保持面板打开**、
/// 就地显示原因。如果后端立刻返回 `{ok:true}`（那只是「任务已受理」），
/// 前端会当成成功、关掉面板 —— **用户以为签上了，实际可能失败**。
///
/// Python 版这些接口都是 `wait=True`，Rust 版一开始写成了立刻返回，
/// 行为不一致。这里补齐。
///
/// 超时不是失败：任务可能还在跑，如实说「还在执行」而不是谎报成功。
pub async fn await_execution(
    st: &AppState,
    exec_id: i64,
    timeout_secs: u64,
) -> Result<Json<Value>, ApiError> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    loop {
        let row: Option<(String, String)> = {
            let conn = st.conn();
            conn.query_row(
                "SELECT state, detail FROM executions WHERE id=?",
                rusqlite::params![exec_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .ok()
        };
        let Some((state, detail)) = row else {
            return Err(ApiError::not_found("执行记录不存在"));
        };
        let data: Value = serde_json::from_str(&detail).unwrap_or(json!({}));
        // **把 `data.logs` 提到顶层** —— 前端 `useCall` 读的是 `r.logs`
        // （Python 版就是把日志放在顶层的）。不提升的话，
        // 补签/自动签到这类「同步调用」的结果日志在界面上永远不显示，
        // 用户只看到一句「补签完成」，不知道到底签了几场、为什么没签。
        let logs = data.get("logs").cloned().unwrap_or_else(|| json!([]));
        match state.as_str() {
            "done" => return Ok(Json(json!({ "ok": true, "data": data, "logs": logs }))),
            "failed" | "cancelled" => {
                let msg = data
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("执行失败")
                    .to_string();
                // 形状与 Python 版一致：前端读 r.ok / r.error
                return Ok(Json(json!({ "ok": false, "error": msg, "data": data, "logs": logs })));
            }
            _ => {}
        }
        if std::time::Instant::now() >= deadline {
            return Ok(Json(json!({
                "ok": false,
                "error": format!("等待超时（{timeout_secs} 秒），任务可能仍在执行，去「运行日志」看进展"),
                "execution_id": exec_id,
            })));
        }
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    }
}

// ---------------------------------------------------------------- 通用执行

#[derive(Deserialize)]
pub struct ExecIn {
    pub cmd: String,
    #[serde(default)]
    pub args: Value,
    /// 等待秒数。默认 180（与 Python 版一致）。
    #[serde(default)]
    pub timeout: Option<u64>,
}

/// `POST /api/accounts/:id/exec` —— **通用命令分发**。
///
/// Python 版是无白名单的透传。Rust 版做成**白名单**：
/// 一是能给出「不支持这个命令」这种明确错误，
/// 二是避免把内部调试命令（`call` / `sms` / `qr_checkin`…）暴露到 HTTP 上。
///
/// **同步等结果** —— 前端（`lib/api.ts` 的 `terms()`、账号页的「重新登录」）
/// 靠返回体判断成败，立刻返回会让它把「已受理」当成「成功了」。
pub async fn exec_cmd(
    State(st): State<Arc<AppState>>,
    user: AuthUser,
    Path(id): Path<i64>,
    Json(body): Json<ExecIn>,
) -> Result<Json<Value>, ApiError> {
    own(&st.conn(), &user, id)?;
    let cmd = body.cmd.trim().to_string();
    if cmd.is_empty() {
        return Err(ApiError::bad_request("缺少 cmd"));
    }

    // 白名单。改动这里 = 改动这个账号能做什么。
    const ALLOWED: &[&str] = &[
        "login", "terms", "scan", "check", "study", "tasks", "task-detail",
        "attendance", "sign", "download", "upload", "profile", "refresh", "tenants",
    ];
    if !ALLOWED.contains(&cmd.as_str()) {
        return Err(ApiError::bad_request(format!(
            "不支持的命令：{cmd}（可用：{}）",
            ALLOWED.join(" / ")
        )));
    }
    // 会真的改上游数据的命令，要求 admin
    if matches!(cmd.as_str(), "study" | "sign" | "upload" | "profile") {
        user.require("admin")?;
    }

    // 「查一下」类命令不建执行记录（Python 版也是 execution_id=None）——
    // 它们不该污染「运行日志」页。
    let readonly = matches!(cmd.as_str(), "login" | "terms" | "tasks" | "task-detail" | "tenants");

    let ctx = st.tasks.clone();
    let args = body.args.clone();
    let timeout = body.timeout.unwrap_or(180);
    let cmd2 = cmd.clone();
    let token = crate::worker::CancelToken::new();
    let run = move || -> neumooc_core::Result<Value> {
        match cmd2.as_str() {
            "login" => crate::tasks::run_login(&ctx, id, &token),
            "terms" => crate::tasks::run_terms(&ctx, id, &token),
            "study" => crate::tasks::run_study(&ctx, id, &args, &token),
            "task-detail" => crate::tasks::run_task_detail(&ctx, id, &args, &token),
            "download" => crate::tasks::run_download(&ctx, id, &args, &token),
            "upload" => crate::tasks::run_upload(&ctx, id, &args, &token),
            "tasks" => crate::tasks::run_tasks(&ctx, id, &token),
            "sign" => crate::tasks::run_sign(&ctx, id, &token),
            _ => crate::tasks::run_scan(&ctx, id, &token),
        }
    };

    if readonly {
        // 不打上游写接口，不需要排队 —— 但**要串行**，否则同账号并发打上游
        let v = tokio::task::spawn_blocking(run)
            .await
            .map_err(|e| ApiError::bad_request(format!("任务执行失败：{e}")))?;
        return match v {
            Ok(d) => Ok(Json(json!({ "ok": true, "data": d }))),
            Err(e) => Ok(Json(json!({ "ok": false, "error": e.to_string() }))),
        };
    }

    // 写命令：先落库再投递，然后等结果
    let exec_id = {
        let conn = st.conn();
        crate::store::begin_execution(&conn, id, &cmd)?
    };
    let Some(exec_id) = exec_id else {
        return Err(ApiError::bad_request("该账号正在执行其它任务，请稍候"));
    };
    let st2 = st.clone();
    {
        let mut reg = st.workers.lock().expect("锁中毒");
        let on_done = move |_aid: i64, key: &str, res: &neumooc_core::Result<Value>| {
            if let Ok(conn) = st2.db.lock() {
                let (state, detail) = match res {
                    Ok(v) => ("done", v.clone()),
                    Err(neumooc_core::error::Error::Invalid(m)) if m == "已取消" => {
                        ("cancelled", json!({ "error": m }))
                    }
                    Err(e) => ("failed", json!({ "error": e.to_string() })),
                };
                let _ = crate::store::append_log(
                    &conn, exec_id,
                    if state == "done" { "ok" } else { "error" },
                    &format!("任务 {key} {state}：{detail}"),
                );
                let _ = crate::store::finish_execution(&conn, exec_id, state, detail);
            }
        };
        let w = reg.get_or_spawn(id);
        w.submit(crate::worker::Job::new(cmd.clone(), move |t| run_with(&t, run)).on_done(on_done))
            .map_err(|e| ApiError::bad_request(e.to_string()))?;
    }
    await_execution(&st, exec_id, timeout).await
}

/// 把 `FnOnce()` 适配成 job 需要的 `FnOnce(&CancelToken)`。
///
/// 单独抽出来是因为闭包不能同时被两个签名使用 —— 只读路径直接调，
/// 写路径要包一层。
fn run_with(
    _t: &crate::worker::CancelToken,
    f: impl FnOnce() -> neumooc_core::Result<Value>,
) -> neumooc_core::Result<Value> {
    f()
}

// ---------------------------------------------------------------- 其余缺口

/// `GET /api/executions/:id`
pub async fn get_execution(
    State(st): State<Arc<AppState>>,
    user: AuthUser,
    Path(exec_id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    let conn = st.conn();
    let v = query_one_json(&conn, "SELECT * FROM executions WHERE id=?", &[&exec_id])?
        .ok_or_else(|| ApiError::not_found("执行记录不存在"))?;
    let aid = v.get("account_id").and_then(Value::as_i64).unwrap_or(-1);
    own(&conn, &user, aid)?;
    // `detail` 在库里是 TEXT（JSON 字符串）。原样返回的后果是前端
    // `detail.answered` 永远是 undefined —— 它拿到的是一个字符串。
    // 这里解析成对象；解析不了就保留原串（有些老记录不是 JSON）。
    let mut out = v;
    if let Some(Value::String(raw)) = out.get("detail").cloned() {
        if let Ok(parsed) = serde_json::from_str::<Value>(&raw) {
            out["detail"] = parsed;
        }
    }
    Ok(Json(out))
}

/// `POST /api/executions/:id/cancel` —— 请求中断一个正在跑的执行。
///
/// ⚠️ 只是**置取消标志**，真正停下来要等 job 自己在循环里检查。
/// 不检查的长循环取消不掉（这是 worker 层的约定，见 `worker::CancelToken`）。
pub async fn cancel_execution(
    State(st): State<Arc<AppState>>,
    user: AuthUser,
    Path(exec_id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    let (aid, state) = {
        let conn = st.conn();
        let r = query_one_json(&conn, "SELECT account_id, state FROM executions WHERE id=?", &[&exec_id])?
            .ok_or_else(|| ApiError::not_found("执行记录不存在"))?;
        (
            r.get("account_id").and_then(Value::as_i64).unwrap_or(-1),
            r.get("state").and_then(Value::as_str).unwrap_or("").to_string(),
        )
    };
    own(&st.conn(), &user, aid)?;
    if state != "running" {
        return Ok(Json(json!({ "ok": false, "error": format!("该执行已结束（{state}），无需取消") })));
    }
    let hit = st
        .workers
        .lock()
        .expect("锁中毒")
        .get(aid)
        .map(|w| w.cancel_current())
        .unwrap_or(false);
    Ok(Json(json!({
        "ok": hit,
        "error": if hit { "" } else { "该账号当前没有在跑的任务" },
    })))
}

/// `GET /api/plans` —— 每日计划。
#[derive(Deserialize, Default)]
pub struct PlanQ {
    #[serde(default)]
    pub account_id: Option<i64>,
    #[serde(default)]
    pub limit: Option<i64>,
}

pub async fn plans(
    State(st): State<Arc<AppState>>,
    user: AuthUser,
    Query(q): Query<PlanQ>,
) -> Result<Json<Value>, ApiError> {
    let conn = st.conn();
    let limit = q.limit.unwrap_or(100).clamp(1, 1000);
    let rows = query_json(
        &conn,
        "SELECT * FROM daily_plans ORDER BY id DESC LIMIT ?",
        &[&limit],
    )?;
    let mut out = Vec::new();
    for r in rows {
        let aid = r.get("account_id").and_then(Value::as_i64).unwrap_or(-1);
        if let Some(want) = q.account_id {
            if aid != want {
                continue;
            }
        }
        if own(&conn, &user, aid).is_err() {
            continue;
        }
        out.push(r);
    }
    Ok(Json(Value::Array(out)))
}

/// `GET /api/tenants` —— 已知租户。
///
/// 没有「租户列表」这种接口，这里返回**库里已出现过的**租户 id ——
/// 添加账号时给个参考，不是权威列表。
pub async fn tenants(
    State(st): State<Arc<AppState>>,
    user: AuthUser,
) -> Result<Json<Value>, ApiError> {
    user.require("admin")?;
    let conn = st.conn();
    let rows = query_json(
        &conn,
        "SELECT DISTINCT tenant_id, COUNT(*) AS accounts FROM accounts
         WHERE tenant_id != '' GROUP BY tenant_id ORDER BY accounts DESC",
        &[],
    )?;
    Ok(Json(Value::Array(rows)))
}

#[derive(Deserialize)]
pub struct NewUserIn {
    pub username: String,
    pub password: String,
    #[serde(default)]
    pub role: String,
}

/// `POST /api/users` —— 建平台用户。仅 root。
pub async fn create_user(
    State(st): State<Arc<AppState>>,
    user: AuthUser,
    Json(body): Json<NewUserIn>,
) -> Result<Json<Value>, ApiError> {
    user.require("root")?;
    let role = if body.role.trim().is_empty() { "user".to_string() } else { body.role.clone() };
    // 角色值必须合法 —— 拼错的值在 perm::rank 里会按最低权限处理，
    // 但那等于静默降级；这里直接拒掉，让人知道写错了。
    if !crate::perm::valid_role(&role) {
        return Err(ApiError::bad_request("角色只能是 root / admin / user"));
    }
    if body.password.len() < 8 {
        return Err(ApiError::bad_request("密码至少 8 位"));
    }
    let hash = crate::auth::hash_password(&body.password)
        .map_err(|e| ApiError::bad_request(format!("哈希失败：{e}")))?;
    let conn = st.conn();
    let now = db::now_ms();
    conn.execute(
        "INSERT INTO users (username, password_hash, role, created_at, updated_at) VALUES (?,?,?,?,?)",
        rusqlite::params![body.username, hash, role, now, now],
    )
    .map_err(|e| {
        if e.to_string().contains("UNIQUE") {
            ApiError::bad_request("这个用户名已存在")
        } else {
            ApiError::bad_request(e.to_string())
        }
    })?;
    Ok(Json(json!({ "ok": true, "username": body.username, "role": role })))
}

/// `DELETE /api/users/:id` —— 删平台用户。仅 root。
///
/// `:id` 是**用户名**（`users` 表以 username 为主键）。
pub async fn delete_user(
    State(st): State<Arc<AppState>>,
    user: AuthUser,
    Path(username): Path<String>,
) -> Result<Json<Value>, ApiError> {
    user.require("root")?;
    // 不让自己删自己 —— 删掉之后立刻失去所有权限，无法恢复
    if username == user.username {
        return Err(ApiError::bad_request("不能删除自己"));
    }
    let conn = st.conn();
    let n = conn.execute("DELETE FROM users WHERE username=?", rusqlite::params![username])?;
    if n == 0 {
        return Err(ApiError::not_found("用户不存在"));
    }
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
pub struct PwIn {
    pub old_password: String,
    pub new_password: String,
}

/// `POST /api/auth/password` —— 改自己的密码。
pub async fn change_password(
    State(st): State<Arc<AppState>>,
    user: AuthUser,
    Json(body): Json<PwIn>,
) -> Result<Json<Value>, ApiError> {
    if body.new_password.len() < 8 {
        return Err(ApiError::bad_request("新密码至少 8 位"));
    }
    let conn = st.conn();
    let hash: String = conn
        .query_row(
            "SELECT password_hash FROM users WHERE username=?",
            rusqlite::params![user.username],
            |r| r.get(0),
        )
        .map_err(|_| ApiError::not_found("用户不存在"))?;
    if !crate::auth::verify_password(&body.old_password, &hash) {
        return Err(ApiError::bad_request("原密码不对"));
    }
    let new_hash = crate::auth::hash_password(&body.new_password)
        .map_err(|e| ApiError::bad_request(format!("哈希失败：{e}")))?;
    conn.execute(
        "UPDATE users SET password_hash=?, updated_at=? WHERE username=?",
        rusqlite::params![new_hash, db::now_ms(), user.username],
    )?;
    // ⚠️ 改密后**清掉该用户的其它会话**（保留当前这个）——
    // 否则旧会话仍然有效，改密就没起到「踢掉别人」的作用。
    let cur = st.conn();
    let _ = cur.execute(
        "DELETE FROM sessions WHERE username=?",
        rusqlite::params![user.username],
    );
    Ok(Json(json!({
        "ok": true,
        "note": "密码已改，其它设备上的登录已失效，请重新登录。",
    })))
}

// ---------------------------------------------------------------- 专用路径（薄包装）
//
// 这些命令 `/exec` 已经能跑（`{cmd:"study"}` 等），但 Python 版有**专用路径**，
// 老客户端 / 脚本可能直接打它们。这里补齐，内部走同一套逻辑 ——
// **不复制实现**，避免两条路径行为不一致。

/// 统一的「跑一个 job 并等结果」。
async fn run_job_wait(
    st: &Arc<AppState>,
    user: &AuthUser,
    id: i64,
    cmd: &str,
    args: Value,
    timeout: u64,
) -> Result<Json<Value>, ApiError> {
    let ctx = st.tasks.clone();
    let cmd_s = cmd.to_string();
    let token = crate::worker::CancelToken::new();
    let run = move || -> neumooc_core::Result<Value> {
        match cmd_s.as_str() {
            "study" => crate::tasks::run_study(&ctx, id, &args, &token),
            "task-detail" => crate::tasks::run_task_detail(&ctx, id, &args, &token),
            "download" => crate::tasks::run_download(&ctx, id, &args, &token),
            "upload" => crate::tasks::run_upload(&ctx, id, &args, &token),
            "publish-attendance" => crate::tasks::run_publish_attendance(&ctx, id, &args, &token),
            "profile" => crate::tasks::run_profile(&ctx, id, &args, &token),
            "refresh" => crate::tasks::run_login(&ctx, id, &token),
            "auto-checkin" => crate::tasks::run_auto_checkin(&ctx, id, &args, &token),
            "sign" => crate::tasks::run_sign(&ctx, id, &token),
            "attendance" => crate::tasks::run_attendance(&ctx, id, &token),
            _ => crate::tasks::run_scan(&ctx, id, &token),
        }
    };

    let exec_id = {
        let conn = st.conn();
        crate::store::begin_execution(&conn, id, cmd)?
    };
    let Some(exec_id) = exec_id else {
        return Err(ApiError::bad_request("该账号正在执行其它任务，请稍候"));
    };
    let st2 = st.clone();
    {
        let mut reg = st.workers.lock().expect("锁中毒");
        let on_done = move |_aid: i64, key: &str, res: &neumooc_core::Result<Value>| {
            if let Ok(conn) = st2.db.lock() {
                let (state, detail) = match res {
                    Ok(v) => ("done", v.clone()),
                    Err(neumooc_core::error::Error::Invalid(m)) if m == "已取消" => {
                        ("cancelled", json!({ "error": m }))
                    }
                    Err(e) => ("failed", json!({ "error": e.to_string() })),
                };
                let _ = crate::store::append_log(
                    &conn, exec_id,
                    if state == "done" { "ok" } else { "error" },
                    &format!("任务 {key} {state}：{detail}"),
                );
                let _ = crate::store::finish_execution(&conn, exec_id, state, detail);
            }
        };
        let w = reg.get_or_spawn(id);
        w.submit(crate::worker::Job::new(cmd.to_string(), move |t| run_with(t, run)).on_done(on_done))
            .map_err(|e| ApiError::bad_request(e.to_string()))?;
    }
    let _ = user;
    await_execution(st, exec_id, timeout).await
}

/// `POST /api/accounts/:id/scan`
pub async fn scan(
    State(st): State<Arc<AppState>>, user: AuthUser, Path(id): Path<i64>,
    Json(a): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    own(&st.conn(), &user, id)?;
    run_job_wait(&st, &user, id, "scan", a, 300).await
}

/// `POST /api/accounts/:id/study`
pub async fn study(
    State(st): State<Arc<AppState>>, user: AuthUser, Path(id): Path<i64>,
    Json(a): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    own(&st.conn(), &user, id)?;
    user.require("admin")?;
    run_job_wait(&st, &user, id, "study", a, 1800).await
}

/// `POST /api/accounts/:id/download`
pub async fn download(
    State(st): State<Arc<AppState>>, user: AuthUser, Path(id): Path<i64>,
    Json(a): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    own(&st.conn(), &user, id)?;
    run_job_wait(&st, &user, id, "download", a, 300).await
}

/// `POST /api/accounts/:id/upload`
pub async fn upload(
    State(st): State<Arc<AppState>>, user: AuthUser, Path(id): Path<i64>,
    Json(a): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    own(&st.conn(), &user, id)?;
    user.require("admin")?;
    run_job_wait(&st, &user, id, "upload", a, 600).await
}

/// `POST /api/accounts/:id/task-detail`
pub async fn task_detail(
    State(st): State<Arc<AppState>>, user: AuthUser, Path(id): Path<i64>,
    Json(a): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    own(&st.conn(), &user, id)?;
    run_job_wait(&st, &user, id, "task-detail", a, 300).await
}

/// `POST /api/accounts/:id/profile`
pub async fn profile(
    State(st): State<Arc<AppState>>, user: AuthUser, Path(id): Path<i64>,
    Json(a): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    own(&st.conn(), &user, id)?;
    user.require("admin")?;
    run_job_wait(&st, &user, id, "profile", a, 300).await
}

/// `POST /api/accounts/:id/refresh`
pub async fn refresh(
    State(st): State<Arc<AppState>>, user: AuthUser, Path(id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    own(&st.conn(), &user, id)?;
    run_job_wait(&st, &user, id, "refresh", json!({}), 120).await
}

/// `POST /api/accounts/:id/publish-attendance`
///
/// ⚠️ **会真的在班级里发布一条考勤** —— 全班学生都会看到。要求 admin。
pub async fn publish_attendance(
    State(st): State<Arc<AppState>>, user: AuthUser, Path(id): Path<i64>,
    Json(a): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    own(&st.conn(), &user, id)?;
    user.require("admin")?;
    run_job_wait(&st, &user, id, "publish-attendance", a, 300).await
}

/// `POST /api/accounts/:id/auto-checkin` —— 自动补签（同 force-checkin 的实现）。
pub async fn auto_checkin(
    State(st): State<Arc<AppState>>, user: AuthUser, Path(id): Path<i64>,
    Json(a): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    own(&st.conn(), &user, id)?;
    user.require("admin")?;
    run_job_wait(&st, &user, id, "auto-checkin", a, 600).await
}

/// `POST /api/attendance/check` —— 对所有可见账号跑一遍考勤检查。
///
/// 返回**每个账号**的结果，不是一个汇总数字 —— 出问题时能看出是哪个账号。
pub async fn attendance_check(
    State(st): State<Arc<AppState>>,
    user: AuthUser,
) -> Result<Json<Value>, ApiError> {
    let accounts = {
        let conn = st.conn();
        store::list_accounts(&conn)?
    };
    let mut out = Vec::new();
    for a in accounts {
        if own(&st.conn(), &user, a.id).is_err() {
            continue;
        }
        let r = run_job_wait(&st, &user, a.id, "attendance", json!({}), 300).await;
        out.push(match r {
            Ok(Json(v)) => json!({ "account_id": a.id, "label": a.label, "result": v }),
            Err(e) => json!({ "account_id": a.id, "label": a.label, "error": e.1 }),
        });
    }
    Ok(Json(Value::Array(out)))
}

/// `POST /api/study/batch` —— 对多个账号批量刷课。
///
/// 入参 `{"account_ids": [1,4,5], "args": {...}}`。
/// **逐个串行投递**（每个账号有自己的队列，投完就返回，不等结果）——
/// 批量刷课可能要几十分钟，同步等会超时。
pub async fn study_batch(
    State(st): State<Arc<AppState>>,
    user: AuthUser,
    Json(body): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    user.require("admin")?;
    let ids: Vec<i64> = body
        .get("account_ids")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_i64).collect())
        .unwrap_or_default();
    if ids.is_empty() {
        return Err(ApiError::bad_request("需要 account_ids"));
    }
    let args = body.get("args").cloned().unwrap_or(json!({}));
    let mut started = Vec::new();
    let mut skipped = Vec::new();
    for id in ids {
        if own(&st.conn(), &user, id).is_err() {
            skipped.push(json!({ "account_id": id, "reason": "无权访问或账号不存在" }));
            continue;
        }
        let exec_id = {
            let conn = st.conn();
            store::begin_execution(&conn, id, "study")?
        };
        let Some(exec_id) = exec_id else {
            skipped.push(json!({ "account_id": id, "reason": "正在执行其它任务" }));
            continue;
        };
        let st2 = st.clone();
        let ctx = st.tasks.clone();
        let a2 = args.clone();
        {
            let mut reg = st.workers.lock().expect("锁中毒");
            let on_done = move |_aid: i64, key: &str, res: &neumooc_core::Result<Value>| {
                if let Ok(conn) = st2.db.lock() {
                    let (state, detail) = match res {
                        Ok(v) => ("done", v.clone()),
                        Err(e) => ("failed", json!({ "error": e.to_string() })),
                    };
                    let _ = store::append_log(&conn, exec_id,
                        if state == "done" { "ok" } else { "error" },
                        &format!("任务 {key} {state}：{detail}"));
                    let _ = store::finish_execution(&conn, exec_id, state, detail);
                }
            };
            let w = reg.get_or_spawn(id);
            w.submit(crate::worker::Job::new("study", move |t| {
                crate::tasks::run_study(&ctx, id, &a2, t)
            }).on_done(on_done))
            .map_err(|e| ApiError::bad_request(e.to_string()))?;
        }
        started.push(json!({ "account_id": id, "execution_id": exec_id }));
    }
    Ok(Json(json!({
        "ok": true, "started": started, "skipped": skipped,
        "note": "已投递，不等结果。去「运行日志」看进展。",
    })))
}

// ---------------------------------------------------------------- PATCH

/// `PATCH /api/accounts/:id` —— 改备注 / 改东软密码。
///
/// ⚠️ Rust 版原先**没有 PATCH 路由**（`grep -rni patch` 零命中），
/// 前端 `AccountsPanel` 的「改密」必然吃 405，而 axum 的 405 body 是空的 ——
/// `useCall` 只能拼出「操作失败（HTTP 405）」，用户完全不知道发生了什么。
#[derive(Deserialize)]
pub struct AccountPatch {
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub note: Option<String>,
    /// 新的东软密码（明文）。传了就用 Fernet 加密后覆盖。
    #[serde(default)]
    pub password: Option<String>,
    /// **模拟设备 ID**。三种情形：
    /// * 不传（`None`）= 不改
    /// * 传空串 `""` = 清空（签到不带 deviceId）
    /// * 传非空 = 设为该值（不校验格式 —— 真 App 就是随机 UUID）
    #[serde(default)]
    pub device_id: Option<String>,
    /// 传 `true` = 随机生成一个新的 UUID 并保存（忽略 `device_id`）
    #[serde(default)]
    pub regenerate_device_id: Option<bool>,
}

pub async fn patch_account(
    State(st): State<Arc<AppState>>,
    user: AuthUser,
    Path(id): Path<i64>,
    Json(body): Json<AccountPatch>,
) -> Result<Json<Value>, ApiError> {
    own(&st.conn(), &user, id)?;
    user.require("admin")?;
    let conn = st.conn();
    let now = db::now_ms();
    if let Some(label) = body.label.as_ref().filter(|s| !s.trim().is_empty()) {
        conn.execute(
            "UPDATE accounts SET label=?, updated_at=? WHERE id=?",
            rusqlite::params![label.trim(), now, id],
        )?;
    }
    if let Some(note) = body.note.as_ref() {
        conn.execute(
            "UPDATE accounts SET note=?, updated_at=? WHERE id=?",
            rusqlite::params![note, now, id],
        )?;
    }
    if let Some(pw) = body.password.as_ref().filter(|s| !s.is_empty()) {
        // 凭据是能直接登录学校系统的东西，加密后入库（与 Python 版同一格式）
        let enc = st
            .tasks
            .cipher
            .encrypt(pw.as_bytes())
            .map_err(|e| ApiError::bad_request(format!("加密失败：{e}")))?;
        conn.execute(
            "UPDATE accounts SET password_enc=?, updated_at=? WHERE id=?",
            rusqlite::params![enc.as_bytes(), now, id],
        )?;
    }
    // ── 模拟设备 ID ──
    //
    // 真 App 的 deviceId 是 `UUID.randomUUID()` 存 SharedPreferences
    // （见 `AppUtil.getDeviceId()`），**与硬件无关**，所以这里生成随机 UUID
    // 在流量上与真机无法区分。
    //
    // 平台只用它判「同一 deviceId 对应多个学号」→ 标考勤异常。
    // 所以**每个账号必须不同**；空值 = 不带该字段，不参与判定。
    if body.regenerate_device_id == Some(true) || body.device_id.is_some() {
        let val = if body.regenerate_device_id == Some(true) {
            new_uuid_v4()
        } else {
            body.device_id.as_ref().map(|s| s.trim().to_string()).unwrap_or_default()
        };
        conn.execute(
            "UPDATE accounts SET device_id=?, updated_at=? WHERE id=?",
            rusqlite::params![val, now, id],
        )?;
    }
    Ok(Json(json!({ "ok": true })))
}

/// `PATCH /api/users/:username` —— 改角色 / 重置密码 / 改账号绑定。仅 root。
#[derive(Deserialize)]
pub struct UserPatch {
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub password: Option<String>,
    /// 该用户能看哪些账号。传了就**整体替换**（不是追加）。
    #[serde(default)]
    pub account_ids: Option<Vec<i64>>,
}

pub async fn patch_user(
    State(st): State<Arc<AppState>>,
    user: AuthUser,
    Path(username): Path<String>,
    Json(body): Json<UserPatch>,
) -> Result<Json<Value>, ApiError> {
    user.require("root")?;
    let conn = st.conn();
    let now = db::now_ms();

    let exists: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM users WHERE username=?",
            rusqlite::params![username],
            |r| r.get(0),
        )
        .ok();
    if exists.is_none() {
        return Err(ApiError::not_found("用户不存在"));
    }

    if let Some(role) = body.role.as_ref() {
        if !crate::perm::valid_role(role) {
            return Err(ApiError::bad_request(format!("角色不合法：{role}")));
        }
        // **不让自己把自己降级** —— 那会立刻失去所有权限且无法恢复。
        // Python 版也有这道判断。
        if username == user.username && role != "root" {
            return Err(ApiError::bad_request("不能把自己降级"));
        }
        conn.execute(
            "UPDATE users SET role=?, updated_at=? WHERE username=?",
            rusqlite::params![role, now, username],
        )?;
    }

    if let Some(pw) = body.password.as_ref().filter(|s| !s.is_empty()) {
        if pw.len() < 8 {
            return Err(ApiError::bad_request("密码至少 8 位"));
        }
        let hash = crate::auth::hash_password(pw)
            .map_err(|e| ApiError::bad_request(format!("哈希失败：{e}")))?;
        conn.execute(
            "UPDATE users SET password_hash=?, updated_at=? WHERE username=?",
            rusqlite::params![hash, now, username],
        )?;
        // 改密后**吊销该用户的全部会话** —— 否则旧会话仍有效，改密就没起作用。
        conn.execute("DELETE FROM sessions WHERE username=?", rusqlite::params![username])?;
    }

    if let Some(ids) = body.account_ids.as_ref() {
        // 整体替换：先清空再插入（用事务，避免中途失败留下半套绑定）
        conn.execute("DELETE FROM user_accounts WHERE username=?", rusqlite::params![username])?;
        for aid in ids {
            // ⚠️ `created_at` 是 **NOT NULL**，必须给。
            //
            // 而且**不能用 `INSERT OR IGNORE`** —— 它连 NOT NULL 违反也一起吞掉，
            // 于是接口返回 `{"ok":true}` 而一行都没插进去。
            // 这个坑是实测「PATCH 返回 200 但绑定表还是空的」才发现的。
            conn.execute(
                "INSERT INTO user_accounts (username, account_id, created_at) VALUES (?,?,?)",
                rusqlite::params![username, aid, now],
            )
            .map_err(|e| {
                if e.to_string().contains("UNIQUE") {
                    ApiError::bad_request(format!("账号 {aid} 已经绑过了"))
                } else {
                    ApiError::bad_request(format!("绑定账号 {aid} 失败：{e}"))
                }
            })?;
        }
    }
    Ok(Json(json!({ "ok": true })))
}

/// 生成一个 v4 UUID（RFC 4122），格式与 App 的 `UUID.randomUUID().toString()` 一致。
///
/// 不引入 `uuid` crate —— 只需要 16 字节随机数 + 设置 version/variant 位。
pub fn new_uuid_v4() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    // 用系统时间 + 地址熵混一个种子；够用（不是密码学场景，只需唯一）
    let mut seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let addr = &seed as *const u64 as u64;
    seed ^= addr;
    let mut b = [0u8; 16];
    let mut x = seed | 1;
    for i in 0..16 {
        // xorshift64*
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        b[i] = ((x.wrapping_mul(0x2545F4914F6CDD1D)) >> 32) as u8;
    }
    // version 4 + variant 10xx（与 UUID.randomUUID() 一致）
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]
    )
}

// ───────────────────── 附件 / 文件题 ─────────────────────

/// `GET /api/accounts/:id/files/:eid` —— **下载附件**（任务附件、我的作答附件）。
///
/// 直接返回二进制流，前端用 `<a download>` 或 fetch+blob 存。
///
/// ## 为什么要走后端
///
/// 平台的下载接口**要 `Authorization`**，浏览器直接开链接会 401/拿不到。
/// 而且它在路径写错时**不报 404**，而是回一段 45 字节的 JSON 错误（HTTP 200）——
/// 那样的"文件"存下来是坏的，所以这里也要把错误外壳变成真错误。
pub async fn download_file(
    State(st): State<Arc<AppState>>,
    user: AuthUser,
    Path((id, eid)): Path<(i64, String)>,
) -> Result<axum::response::Response, ApiError> {
    own(&st.conn(), &user, id)?;
    let ctx = st.tasks.clone();
    let eid2 = eid.clone();
    let (bytes, name) = tokio::task::spawn_blocking(move || -> neumooc_core::Result<(Vec<u8>, String)> {
        let c = ctx.client(id)?;
        let meta = c.file_meta(&eid2).unwrap_or(Value::Null);
        let name = meta
            .get("objectKey")
            .and_then(Value::as_str)
            .map(|s| s.to_string())
            .unwrap_or_else(|| format!("{eid2}.bin"));
        let b = c.file_download(&eid2)?;
        Ok((b, name))
    })
    .await
    .map_err(|e| ApiError::bad_request(format!("下载线程失败：{e}")))?
    .map_err(|e| ApiError::bad_request(e.to_string()))?;

    // 文件名要编码 —— 中文名直接塞头里会被 HTTP 规则切断
    let encoded = urlencoding::encode(&name).to_string();
    axum::response::Response::builder()
        .header("content-type", "application/octet-stream")
        .header(
            "content-disposition",
            format!("attachment; filename*=UTF-8''{encoded}"),
        )
        .body(axum::body::Body::from(bytes))
        .map_err(|e| ApiError::bad_request(format!("构造响应失败：{e}")))
}

/// `POST /api/accounts/:id/files` —— **上传文件**（multipart，字段名 `file`）。
///
/// 返回平台给的 `eid`，后续把它填进题目的 `answerEid` 就能当作业附件。
pub async fn upload_file(
    State(st): State<Arc<AppState>>,
    user: AuthUser,
    Path(id): Path<i64>,
    mut multipart: axum::extract::Multipart,
) -> Result<Json<Value>, ApiError> {
    own(&st.conn(), &user, id)?;

    let mut got: Option<(String, Vec<u8>)> = None;
    while let Ok(Some(field)) = multipart.next_field().await {
        // 只认名为 `file` 的字段（与网页端一致）
        if field.name() != Some("file") && field.name() != Some("files") {
            continue;
        }
        let name = field.file_name().unwrap_or("upload.bin").to_string();
        let data = field
            .bytes()
            .await
            .map_err(|e| ApiError::bad_request(format!("读上传内容失败：{e}")))?;
        got = Some((name, data.to_vec()));
        break;
    }
    let Some((name, data)) = got else {
        return Err(ApiError::bad_request("没有收到文件（字段名应为 file）"));
    };
    if data.is_empty() {
        return Err(ApiError::bad_request("文件是空的"));
    }
    if data.len() > 300 * 1024 * 1024 {
        return Err(ApiError::bad_request("文件超过 300MB"));
    }

    let ctx = st.tasks.clone();
    let mime = neumooc_core::file::mime_of(&name).to_string();
    let name_for_upload = name.clone();
    let out = tokio::task::spawn_blocking(move || -> neumooc_core::Result<Value> {
        let c = ctx.client(id)?;
        c.file_put_object(&name_for_upload, data, &mime)
    })
    .await
    .map_err(|e| ApiError::bad_request(format!("上传线程失败：{e}")))?
    .map_err(|e| ApiError::bad_request(e.to_string()))?;

    Ok(Json(json!({
        "ok": true,
        "eid": out.get("eid").and_then(Value::as_str).unwrap_or(""),
        "name": out.get("objectKey").and_then(Value::as_str).unwrap_or(&name),
        "size": out.get("fileSize").and_then(Value::as_str).unwrap_or(""),
        "md5": out.get("fileMd5").and_then(Value::as_str).unwrap_or(""),
        "raw": out,
    })))
}

/// `GET /api/accounts/:id/asset?src=<相对路径或 URL>` —— **带上 token 转发一个资源**。
///
/// 用于题目里的图片（`/content/s3/getObjectIO?eid=...`）、以及任何需要
/// `Authorization` 才能取的静态资源。
///
/// ## 为什么由前端传 `src`
///
/// 早先的写法是 `.../questions/:no/image?idx=N` —— 后端每次都要
/// `enter_task(test_id)` 拿到试卷、再数题图。**一道题两张图就是两次 enter**，
/// 而 `enter` 很重（还可能在 token 过期时触发重新登录）。
///
/// 实测症状：一张作业的两张图同时加载 → 两次并发重登互相踢掉 token →
/// 第二张图 400。前端本来就有题干 HTML，直接从里面抽 src 更省事。
///
/// ## 安全
///
/// 只放行 `/content/` 开头的路径和 `http(s)` 绝对 URL —— 否则这个接口
/// 就成了"用服务端 token 打任意内部地址"的代理。
pub async fn proxy_asset(
    State(st): State<Arc<AppState>>,
    user: AuthUser,
    Path(id): Path<i64>,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Result<axum::response::Response, ApiError> {
    own(&st.conn(), &user, id)?;
    let src = q.get("src").cloned().unwrap_or_default();
    if src.is_empty() {
        return Err(ApiError::bad_request("缺少 src"));
    }
    let ok = src.starts_with("/content/")
        || src.starts_with("https://")
        || src.starts_with("http://");
    if !ok {
        return Err(ApiError::bad_request("只允许 /content/ 开头的路径或 http(s) URL"));
    }

    let ctx = st.tasks.clone();
    let out = tokio::task::spawn_blocking(move || -> neumooc_core::Result<Vec<u8>> {
        let c = ctx.client(id)?;
        c.fetch_bytes(&src)
    })
    .await
    .map_err(|e| ApiError::bad_request(format!("取资源线程失败：{e}")))?
    .map_err(|e| ApiError::bad_request(e.to_string()))?;

    let mime = mime_of_bytes(&out);
    axum::response::Response::builder()
        .header("content-type", mime)
        // 图片不会变，让浏览器缓存 —— 翻来翻去不该重复打后端
        .header("cache-control", "private, max-age=3600")
        .body(axum::body::Body::from(out))
        .map_err(|e| ApiError::bad_request(format!("构造响应失败：{e}")))
}

/// 按魔数判 MIME（不信 Content-Type —— 平台可能回 JSON 错误）。
fn mime_of_bytes(b: &[u8]) -> &'static str {
    if b.starts_with(&[0x89, b'P', b'N', b'G']) {
        "image/png"
    } else if b.starts_with(&[0xFF, 0xD8, 0xFF]) {
        "image/jpeg"
    } else if b.starts_with(b"GIF8") {
        "image/gif"
    } else if b.len() > 12 && &b[0..4] == b"RIFF" && &b[8..12] == b"WEBP" {
        "image/webp"
    } else if b.first() == Some(&b'{') {
        "application/json"
    } else {
        "application/octet-stream"
    }
}
