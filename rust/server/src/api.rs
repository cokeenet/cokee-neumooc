//! HTTP 层（axum）。
//!
//! ## 鉴权做成「默认拒绝」
//!
//! 中间件**默认要求登录**，只放行明确公开的路径（`/api/health`、`/api/auth/*`）。
//! 反过来（默认放行、逐个加守卫）漏一个就是一个洞 —— 本项目为此修过一次
//! 水平越权（8 条路由没做账号归属校验）。
//!
//! ## `/api/health` 是公开的，所以它只能回答「活没活着」
//!
//! 曾经它返回账号数量与 worker 状态 —— 无需登录即可访问，等于公开泄露内部信息。

use std::sync::{Arc, Mutex};

use axum::extract::{FromRequestParts, Path, State};
use axum::http::request::Parts;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use rusqlite::Connection;
use serde_json::{json, Value};

use crate::auth;
use crate::db;
use crate::perm;
use crate::store;
use crate::worker::WorkerRegistry;

/// 放行的前缀 / 精确路径。**改动这里等于改动安全边界。**
pub const PUBLIC_PREFIXES: &[&str] = &["/api/auth/"];
pub const PUBLIC_EXACT: &[&str] = &["/api/health"];

pub fn is_public(path: &str) -> bool {
    PUBLIC_PREFIXES.iter().any(|p| path.starts_with(p)) || PUBLIC_EXACT.contains(&path)
}

/// 共享状态。
pub struct AppState {
    pub db: Arc<Mutex<Connection>>,
    pub workers: Arc<Mutex<WorkerRegistry>>,
    /// 凭据解密 + 任务编排（HTTP 层只转发，不自己讲协议）
    pub tasks: Arc<crate::tasks::TaskCtx>,
    /// 站点走 HTTPS 时置 true（决定 cookie 带不带 `Secure`）
    pub secure_cookie: bool,
}

impl AppState {
    pub fn new(conn: Connection, cipher: std::sync::Arc<crate::crypto::CredentialCipher>) -> Arc<Self> {
        let db = Arc::new(Mutex::new(conn));
        Arc::new(Self {
            db: db.clone(),
            workers: Arc::new(Mutex::new(WorkerRegistry::new())),
            tasks: Arc::new(crate::tasks::TaskCtx::new(db, cipher)),
            secure_cookie: false,
        })
    }

    pub fn conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.db.lock().expect("数据库锁中毒")
    }
}

/// 当前登录用户。
#[derive(Debug, Clone)]
pub struct AuthUser {
    pub username: String,
    pub role: String,
}

impl AuthUser {
    pub fn is_root(&self) -> bool {
        self.role == "root"
    }
    pub fn rank(&self) -> u8 {
        perm::rank(&self.role)
    }
    /// 要求至少某角色。
    pub fn require(&self, minimum: &str) -> Result<(), ApiError> {
        if self.rank() < perm::rank(minimum) {
            return Err(ApiError::forbidden(perm::deny_message(minimum)));
        }
        Ok(())
    }
}

/// 统一的错误响应。
pub struct ApiError(pub StatusCode, pub String);

impl ApiError {
    pub fn unauthorized() -> Self {
        Self(StatusCode::UNAUTHORIZED, "未登录".into())
    }
    pub fn forbidden(msg: &str) -> Self {
        Self(StatusCode::FORBIDDEN, msg.into())
    }
    pub fn bad_request(msg: impl Into<String>) -> Self {
        Self(StatusCode::BAD_REQUEST, msg.into())
    }
    pub fn not_found(msg: &str) -> Self {
        Self(StatusCode::NOT_FOUND, msg.into())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({ "detail": self.1 }))).into_response()
    }
}

impl From<rusqlite::Error> for ApiError {
    fn from(e: rusqlite::Error) -> Self {
        log::error!("数据库错误：{e}");
        Self(StatusCode::INTERNAL_SERVER_ERROR, "服务内部错误".into())
    }
}

impl From<neumooc_core::error::Error> for ApiError {
    fn from(e: neumooc_core::error::Error) -> Self {
        // 业务错误原样透出（它们本来就是给用户看的），内部错误不外泄
        match e {
            neumooc_core::error::Error::Biz { msg, .. } => ApiError::bad_request(msg),
            neumooc_core::error::Error::Invalid(m) => ApiError::bad_request(m),
            other => {
                log::error!("内部错误：{other}");
                ApiError(StatusCode::INTERNAL_SERVER_ERROR, "服务内部错误".into())
            }
        }
    }
}

/// 从会话 cookie 解析当前用户。
///
/// 实现成 axum 的提取器：**handler 签名里写了 `AuthUser` 就等于要求登录**，
/// 忘了加守卫的后果是编译不过，而不是静默放行。
#[async_trait::async_trait]
impl FromRequestParts<Arc<AppState>> for AuthUser {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &Arc<AppState>,
    ) -> Result<Self, Self::Rejection> {
        let header = parts
            .headers
            .get(axum::http::header::COOKIE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        let token = auth::token_from_cookie(header).ok_or_else(ApiError::unauthorized)?;
        let th = auth::token_hash(&token);
        let conn = state.conn();
        let row: Option<(String, String, i64)> = conn
            .query_row(
                "SELECT s.username, u.role, s.expires_at FROM sessions s
                 JOIN users u ON u.username = s.username WHERE s.token_hash=?",
                rusqlite::params![th],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .ok();
        let (username, role, expires) = row.ok_or_else(ApiError::unauthorized)?;
        if expires < db::now_ms() {
            // 过期就顺手清掉，别让死会话一直堆在表里
            let _ = conn.execute("DELETE FROM sessions WHERE token_hash=?", rusqlite::params![th]);
            return Err(ApiError::unauthorized());
        }
        Ok(AuthUser { username, role })
    }
}

// ---------------------------------------------------------------- 路由

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/api/health", get(health))
        .route("/api/auth/login", post(login))
        .route("/api/auth/logout", post(logout))
        .route("/api/auth/me", get(me))
        .route("/api/overview", get(overview))
        .route("/api/accounts/:id/auto-sign", get(get_auto_sign).put(set_auto_sign))
        .route("/api/accounts/:id/executions", get(list_executions))
        .route("/api/accounts/:id/run", post(run_job))
        .route("/api/executions/:id/stream", get(crate::sse::stream))
        // ---- 读接口（查本地缓存，不打学校接口）----
        .route("/api/accounts/:id/courses", get(crate::endpoints::courses))
        .route("/api/accounts/:id/resources", get(crate::endpoints::resources))
        .route("/api/accounts/:id/attendance", get(crate::endpoints::attendance))
        .route("/api/accounts/:id/tasks", get(crate::endpoints::tasks))
        .route("/api/accounts/:id/exams", get(crate::endpoints::exams))
        .route("/api/accounts/:id/exams/:exam_id/paper", get(crate::endpoints::exam_paper))
        .route("/api/accounts/:id/homeworks/:test_id/paper", get(crate::endpoints::homework_paper))
        .route("/api/accounts/:id/homeworks", get(crate::endpoints::homeworks))
        .route("/api/accounts/:id/homeworks/:test_id/detail", get(crate::endpoints::homework_detail))
        .route("/api/device-config", get(crate::endpoints::device_config))

        .route(
            "/api/accounts/:id/device-id",
            axum::routing::put(crate::endpoints::set_device_id),
        )
        .route("/api/executions", get(crate::endpoints::executions))
        .route("/api/executions/:id/logs", get(crate::endpoints::execution_logs))
        .route("/api/logs", get(crate::endpoints::recent_logs))
        .route("/api/schedule", get(crate::endpoints::schedule))
        .route("/api/schedule/:id", put(crate::endpoints::set_schedule))
        // ---- 写接口 ----
        .route("/api/accounts/:id/attendance/sign", post(crate::endpoints::sign))
        .route("/api/accounts/:id/attendance/sync", post(crate::endpoints::attendance_sync))
        .route("/api/notify/status", get(crate::endpoints::notify_status))
        .route("/api/accounts", get(list_accounts).post(crate::endpoints::create_account))
        .route("/api/accounts/:id", get(crate::endpoints::get_account).delete(crate::endpoints::delete_account))
        .route("/api/accounts/:id/tasks/answer", post(crate::endpoints::task_answer))
        .route("/api/accounts/:id/tasks/ai-answer", post(crate::endpoints::task_ai_answer))
        // 附件 / 文件题
        .route("/api/accounts/:id/files", post(crate::endpoints::upload_file))
        .route("/api/accounts/:id/files/:eid", get(crate::endpoints::download_file))
        .route("/api/accounts/:id/asset", get(crate::endpoints::proxy_asset))
        .route("/api/llm/status", get(crate::endpoints::llm_status))
        .route(
            "/api/llm/config",
            get(crate::endpoints::llm_config_get).put(crate::endpoints::llm_config_set),
        )
        .route("/api/llm/test", post(crate::endpoints::llm_test))
        .route("/api/llm/ask", post(crate::endpoints::llm_ask))
        .route("/api/notify/test", post(crate::endpoints::notify_test))
        .route("/api/schedule/:id/run-now", post(crate::endpoints::run_now))
        .route("/api/accounts/:id/force-checkin", post(crate::endpoints::force_checkin))
        .route("/api/accounts/:id/teacher-makeup", post(crate::endpoints::teacher_makeup))
        .route("/api/logs/stream", get(crate::sse::logs_stream))
        // ---- 补齐 Python 版有、Rust 版缺的 ----
        .route("/api/accounts/:id/exec", post(crate::endpoints::exec_cmd))
        .route("/api/executions/:id", get(crate::endpoints::get_execution))
        .route("/api/executions/:id/cancel", post(crate::endpoints::cancel_execution))
        .route("/api/plans", get(crate::endpoints::plans))
        .route("/api/tenants", get(crate::endpoints::tenants))
        .route("/api/users", get(crate::endpoints::users).post(crate::endpoints::create_user))
        .route("/api/users/:id", delete(crate::endpoints::delete_user))
        .route("/api/users/:id", axum::routing::patch(crate::endpoints::patch_user))
        .route("/api/accounts/:id", axum::routing::patch(crate::endpoints::patch_account))
        .route("/api/auth/password", post(crate::endpoints::change_password))
        .route("/api/accounts/:id/scan", post(crate::endpoints::scan))
        .route("/api/accounts/:id/study", post(crate::endpoints::study))
        .route("/api/accounts/:id/download", post(crate::endpoints::download))
        .route("/api/accounts/:id/upload", post(crate::endpoints::upload))
        .route("/api/accounts/:id/task-detail", post(crate::endpoints::task_detail))
        .route("/api/accounts/:id/profile", post(crate::endpoints::profile))
        .route("/api/accounts/:id/refresh", post(crate::endpoints::refresh))
        .route("/api/accounts/:id/publish-attendance", post(crate::endpoints::publish_attendance))
        .route("/api/accounts/:id/auto-checkin", post(crate::endpoints::auto_checkin))
        .route("/api/attendance/check", post(crate::endpoints::attendance_check))
        .route("/api/study/batch", post(crate::endpoints::study_batch))
        .with_state(state)
}

/// 健康检查。**公开路径** —— 只能回答「活没活着」。
async fn health() -> Json<Value> {
    Json(json!({ "ok": true, "ts": db::now_ms() }))
}

#[derive(serde::Deserialize)]
struct LoginIn {
    username: String,
    password: String,
}

async fn login(
    State(st): State<Arc<AppState>>,
    Json(body): Json<LoginIn>,
) -> Result<Response, ApiError> {
    let conn = st.conn();
    let row: Option<(String, String)> = conn
        .query_row(
            "SELECT password_hash, role FROM users WHERE username=?",
            rusqlite::params![body.username],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .ok();
    // 用户不存在与密码错误给**同一个**提示 —— 否则可以拿来枚举用户名
    let Some((hash, role)) = row else {
        return Err(ApiError::bad_request("用户名或密码错误"));
    };
    if !auth::verify_password(&body.password, &hash) {
        return Err(ApiError::bad_request("用户名或密码错误"));
    }

    let token = auth::new_token();
    let now = db::now_ms();
    conn.execute(
        "INSERT INTO sessions (token_hash, username, expires_at, created_at) VALUES (?,?,?,?)",
        rusqlite::params![
            auth::token_hash(&token),
            body.username,
            now + auth::SESSION_TTL_SECS * 1000,
            now
        ],
    )?;
    // 顺手清掉过期会话
    let _ = conn.execute("DELETE FROM sessions WHERE expires_at < ?", rusqlite::params![now]);

    let mut resp = Json(json!({ "ok": true, "username": body.username, "role": role }))
        .into_response();
    resp.headers_mut().insert(
        axum::http::header::SET_COOKIE,
        auth::set_cookie(&token, auth::SESSION_TTL_SECS, st.secure_cookie)
            .parse()
            .expect("cookie 值合法"),
    );
    Ok(resp)
}

async fn logout(State(st): State<Arc<AppState>>) -> Response {
    // 即使没带 token 也返回 200 —— 登出失败不该把用户卡住。
    // 这里**不碰数据库**：只清 cookie 就够了，服务端会话靠过期自然清理。
    let mut resp = Json(json!({ "ok": true })).into_response();
    resp.headers_mut().insert(
        axum::http::header::SET_COOKIE,
        auth::clear_cookie(st.secure_cookie).parse().expect("cookie 值合法"),
    );
    resp
}

async fn me(user: AuthUser) -> Json<Value> {
    Json(json!({
        "username": user.username,
        "role": user.role,
        "role_label": perm::role_label(&user.role),
        "is_root": user.is_root(),
    }))
}

async fn list_accounts(
    State(st): State<Arc<AppState>>,
    user: AuthUser,
) -> Result<Json<Value>, ApiError> {
    let conn = st.conn();
    let all = store::list_accounts(&conn)?;
    // 普通用户只看绑定给自己的账号
    let allowed: Option<Vec<i64>> = if perm::unrestricted(&user.role) {
        None
    } else {
        let mut stmt = conn.prepare("SELECT account_id FROM user_accounts WHERE username=?")?;
        let rows = stmt.query_map(rusqlite::params![user.username], |r| r.get::<_, i64>(0))?;
        Some(rows.filter_map(|r| r.ok()).collect())
    };
    let out: Vec<Value> = all
        .into_iter()
        .filter(|a| match &allowed {
            // ⚠️ None = 不受限；Some(vec![]) = 一个都不能访问。别把两者混起来。
            None => true,
            Some(ids) => ids.contains(&a.id),
        })
        .map(|a| {
            json!({
                "id": a.id, "label": a.label, "username": a.username,
                "state": a.state, "tenant_id": a.tenant_id,
                "dept": a.dept,
                // 模拟设备 ID（空串 = 签到不带 deviceId）
                "device_id": a.device_id,
                "device_id_enabled": a.device_id_enabled,
            })
        })
        .collect();
    Ok(Json(Value::Array(out)))
}

async fn overview(
    State(st): State<Arc<AppState>>,
    user: AuthUser,
) -> Result<Json<Value>, ApiError> {
    let conn = st.conn();
    let accounts = store::list_accounts(&conn)?;
    let visible: Vec<&store::AccountRow> = if perm::unrestricted(&user.role) {
        accounts.iter().collect()
    } else {
        let mut stmt = conn.prepare("SELECT account_id FROM user_accounts WHERE username=?")?;
        let rows = stmt.query_map(rusqlite::params![user.username], |r| r.get::<_, i64>(0))?;
        let ids: Vec<i64> = rows.filter_map(|r| r.ok()).collect();
        accounts.iter().filter(|a| ids.contains(&a.id)).collect()
    };
    let workers = st.workers.lock().expect("锁中毒").len();
    Ok(Json(json!({
        "accounts": visible.len(),
        "workers": workers,
    })))
}

async fn get_auto_sign(
    State(st): State<Arc<AppState>>,
    user: AuthUser,
    Path(id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    require_account(&st.conn(), &user, id)?;
    let conn = st.conn();
    let cfg = store::read_account_cfg(&conn, id, "auto_sign")?;
    let on = cfg.get("enabled").and_then(Value::as_bool).unwrap_or(false);
    // ⚠️ 必须返回 `available` —— 前端按钮写的是 `disabled={!autoSign.available}`。
    // 少这个字段 → undefined → **按钮永久禁用**，用户根本打不开自动签到。
    // 这是「接口少一个字段导致功能完全不可用」，而页面看起来完全正常。
    //
    // `available` 是**总开关**（环境变量 NEUMOOC_AUTO_SIGN），`enabled` 是
    // **单账号开关**；两者都开才会真的自动签。总开关默认开。
    // 与派发逻辑共用**同一份**判断（`scheduler::AttendanceWatcher::auto_master_on`），
    // 否则界面显示「可用」而实际不派发，或反过来。
    let master = crate::scheduler::AttendanceWatcher::auto_master_on();
    Ok(Json(json!({
        "enabled": on,
        "available": master,
        "interval_seconds": crate::scheduler::ATTENDANCE_TICK_SECS,
        "hint": if !master {
            "平台已全局关闭自动签到（NEUMOOC_AUTO_SIGN=0）—— 单账号开关无效".to_string()
        } else if on {
            "每分钟自动同步一次考勤并立即签到；定位场次用平台要求的坐标，不需要人到现场。".to_string()
        } else {
            "已关闭：只发提醒，不会自动签到。".to_string()
        },
    })))
}

#[derive(serde::Deserialize)]
struct AutoSignIn {
    enabled: bool,
}

async fn set_auto_sign(
    State(st): State<Arc<AppState>>,
    user: AuthUser,
    Path(id): Path<i64>,
    Json(body): Json<AutoSignIn>,
) -> Result<Json<Value>, ApiError> {
    require_account(&st.conn(), &user, id)?;
    // 打开会**真的替学生提交考勤**，所以要求 admin
    user.require("admin")?;
    let conn = st.conn();
    store::write_account_cfg(&conn, id, "auto_sign", json!({ "enabled": body.enabled }))?;
    Ok(Json(json!({ "ok": true, "enabled": body.enabled })))
}

async fn list_executions(
    State(st): State<Arc<AppState>>,
    user: AuthUser,
    Path(id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    require_account(&st.conn(), &user, id)?;
    let conn = st.conn();
    let mut stmt = conn.prepare(
        "SELECT id, job_key, state, biz_date, started_at, finished_at FROM executions
         WHERE account_id=? ORDER BY id DESC LIMIT 50",
    )?;
    let rows = stmt.query_map(rusqlite::params![id], |r| {
        Ok(json!({
            "id": r.get::<_, i64>(0)?, "job_key": r.get::<_, String>(1)?,
            "state": r.get::<_, String>(2)?, "biz_date": r.get::<_, String>(3)?,
            "started_at": r.get::<_, i64>(4)?, "finished_at": r.get::<_, i64>(5)?,
        }))
    })?;
    Ok(Json(Value::Array(rows.filter_map(|r| r.ok()).collect())))
}

#[derive(serde::Deserialize)]
struct RunIn {
    /// `scan` | `sign`
    job: String,
}

/// 触发一个任务。
///
/// 执行记录**先落库再投递** —— 反过来的话，worker 跑得快时日志会写进一条
/// 还不存在的 execution（外键失败）。
async fn run_job(
    State(st): State<Arc<AppState>>,
    user: AuthUser,
    Path(id): Path<i64>,
    Json(body): Json<RunIn>,
) -> Result<Json<Value>, ApiError> {
    require_account(&st.conn(), &user, id)?;
    if !matches!(body.job.as_str(), "scan" | "sign") {
        return Err(ApiError::bad_request(format!("不支持的任务：{}", body.job)));
    }
    // 先占坑：同一账号同 job 已在跑时直接挡掉（防重复点击）
    let exec_id = {
        let conn = st.conn();
        store::begin_execution(&conn, id, &body.job)?
    };
    let Some(exec_id) = exec_id else {
        return Err(ApiError::bad_request("该账号的这个任务正在执行，请稍候"));
    };

    let st2 = st.clone();
    let job = body.job.clone();
    let mut reg = st.workers.lock().expect("锁中毒");
    let on_done = move |_aid: i64, key: &str, res: &neumooc_core::Result<Value>| {
        // worker 线程里收尾：写日志 + 落执行结果
        let conn = match st2.db.lock() {
            Ok(c) => c,
            Err(_) => return,
        };
        let (state, detail) = match res {
            Ok(v) => ("done", v.clone()),
            // 取消不是失败 —— 记成 failed 会让人以为是 bug
            Err(neumooc_core::error::Error::Invalid(m)) if m == "已取消" => {
                ("cancelled", json!({ "error": m }))
            }
            Err(e) => ("failed", json!({ "error": e.to_string() })),
        };
        // ⚠️ 第一参是 **execution_id**，不是 account_id。
        // 传错的话外键直接失败 —— 而下面若用 `let _ =` 就会**静默丢掉日志**，
        // 表现是「执行有结果但日志表空的」。这里改成把错误记出来。
        if let Err(e) = store::append_log(
            &conn,
            exec_id,
            if state == "done" { "ok" } else { "error" },
            &format!("任务 {key} {state}：{}", detail),
        ) {
            log::error!("写日志失败（exec={exec_id}）：{e}");
        }
        if let Err(e) = store::finish_execution(&conn, exec_id, state, detail) {
            log::error!("收尾执行失败（exec={exec_id}）：{e}");
        }
    };
    let w = reg.get_or_spawn(id);

    let ctx = st.tasks.clone();
    w.submit(crate::worker::Job::new(body.job, move |token| match job.as_str() {
        "sign" => crate::tasks::run_sign(&ctx, id, token),
        _ => crate::tasks::run_scan(&ctx, id, token),
    }).on_done(on_done))
    .map_err(|e| ApiError::bad_request(e.to_string()))?;

    Ok(Json(json!({ "ok": true, "execution_id": exec_id, "state": "running" })))
}

/// 供其它模块复用的归属校验。
///
/// ⚠️ 参数是 **`&Connection` 而不是 `&AppState`** —— 这是刻意的。
/// 收 `&AppState` 的话，调用方若已经持有 `st.conn()`，这里会**再锁一次同一个
/// `std::sync::Mutex`，直接死锁**（不报错，整个请求永远挂住）。
/// 本项目为此栽过两次（SSE 日志流、执行日志）。
/// 收 `&Connection` 之后，「持有锁时再锁一次」在类型上就写不出来了。
pub fn require_account_pub(
    conn: &Connection,
    user: &AuthUser,
    id: i64,
) -> Result<(), ApiError> {
    require_account(conn, user, id)
}

/// 账号归属校验。**每条按账号取数的路由都必须调它。**
///
/// 曾经有 8 条路由漏了这一步 —— 普通用户能读到别人的账号数据（水平越权）。
fn require_account(conn: &Connection, user: &AuthUser, id: i64) -> Result<(), ApiError> {
    if store::get_account(conn, id)?.is_none() {
        return Err(ApiError::not_found("账号不存在"));
    }
    if perm::unrestricted(&user.role) {
        return Ok(());
    }
    let bound: Option<i64> = conn
        .query_row(
            "SELECT account_id FROM user_accounts WHERE username=? AND account_id=?",
            rusqlite::params![user.username, id],
            |r| r.get(0),
        )
        .ok();
    if bound.is_none() {
        // 不告诉他「这个账号存在但不是你的」—— 那等于确认了账号 id 有效
        return Err(ApiError::not_found("账号不存在"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_paths_are_minimal() {
        // 放行面越小越好；每加一条都要想清楚「未登录也能访问」意味着什么
        assert!(is_public("/api/health"));
        assert!(is_public("/api/auth/login"));
        assert!(is_public("/api/auth/logout"));
        // 这些必须要求登录
        for p in [
            "/api/accounts",
            "/api/overview",
            "/api/accounts/1/auto-sign",
            "/api/accounts/1/executions",
            "/api/users",
        ] {
            assert!(!is_public(p), "{p} 不该公开");
        }
    }

    #[test]
    fn health_prefix_does_not_leak_siblings() {
        // `/api/healthz` 不该因为 `/api/health` 是精确匹配而被放行
        assert!(!is_public("/api/healthz"));
        assert!(!is_public("/api/health/secret"));
    }

    #[test]
    fn auth_prefix_covers_subpaths() {
        assert!(is_public("/api/auth/login"));
        assert!(is_public("/api/auth/me"));
        // 但别的前缀不能沾光
        assert!(!is_public("/api/authorization"));
    }

    #[test]
    fn require_role_uses_rank() {
        let u = AuthUser { username: "a".into(), role: "user".into() };
        assert!(u.require("user").is_ok());
        assert!(u.require("admin").is_err());
        assert!(u.require("root").is_err());
        let a = AuthUser { username: "a".into(), role: "admin".into() };
        assert!(a.require("admin").is_ok());
        assert!(a.require("root").is_err());
        let r = AuthUser { username: "a".into(), role: "root".into() };
        assert!(r.require("root").is_ok() && r.is_root());
    }

    #[test]
    fn unknown_role_gets_lowest_privilege() {
        let weird = AuthUser { username: "a".into(), role: "ADMIN".into() };
        assert!(weird.require("admin").is_err(), "拼错的角色不能提权");
    }
}
