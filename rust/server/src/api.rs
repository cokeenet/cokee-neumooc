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
    /// 登录爆破防护（内存计数，见 `login_guard` 模块）
    pub login_guard: Arc<crate::login_guard::LoginGuard>,
}

impl AppState {
    pub fn new(conn: Connection, cipher: std::sync::Arc<crate::crypto::CredentialCipher>) -> Arc<Self> {
        let db = Arc::new(Mutex::new(conn));
        Arc::new(Self {
            db: db.clone(),
            workers: Arc::new(Mutex::new(WorkerRegistry::new())),
            tasks: Arc::new(crate::tasks::TaskCtx::new(db, cipher)),
            secure_cookie: false,
            login_guard: Arc::new(crate::login_guard::LoginGuard::new()),
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

/// 429 的专属返回：消息 + `Retry-After` 头。
///
/// 单独一个类型是因为 `ApiError` 是 `(StatusCode, String)` 的 tuple struct，
/// 加不了响应头。而 `Retry-After` 是标准头，比在消息里拼 `|899` 规矩得多
/// —— 早先那么干，前端把 `|899` 直接显示给用户了。
pub struct TooMany {
    pub msg: String,
    pub retry_secs: u64,
}

impl IntoResponse for TooMany {
    fn into_response(self) -> Response {
        let mut resp = (
            StatusCode::TOO_MANY_REQUESTS,
            Json(json!({ "detail": self.msg, "retry_after": self.retry_secs })),
        )
            .into_response();
        if let Ok(v) = self.retry_secs.to_string().parse() {
            resp.headers_mut().insert("retry-after", v);
        }
        resp
    }
}

impl ApiError {
    pub fn unauthorized() -> Self {
        Self(StatusCode::UNAUTHORIZED, "未登录".into())
    }

    /// 429 —— 请求过于频繁。
    ///
    /// 和 `bad_request` 分开是有意的：前端要能区分「密码错了」和
    /// 「你先歇会」。混成 400 的话，用户会以为密码记错了，越试越锁。
    ///
    /// 剩余秒数走 `Retry-After` 头（标准做法），不塞进消息体 ——
    /// 早先塞成 `msg|899`，前端直接把 `|899` 显示给用户了。
    pub fn too_many(msg: impl Into<String>) -> Self {
        Self(StatusCode::TOO_MANY_REQUESTS, msg.into())
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
        .route("/api/login-guard", get(crate::endpoints::login_guard_status))
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
    headers: axum::http::HeaderMap,
    Json(body): Json<LoginIn>,
) -> Result<Response, ApiError> {
    // 真实客户端 IP —— 后端只监听回环，直连地址永远是 127.0.0.1，
    // 拿它限流等于全体用户共用一个桶（见 `login_guard::client_ip` 的注释）
    let ip = crate::login_guard::client_ip(&headers, "unknown");

    // **先查锁再验密码** —— 爆破时连 Argon2 都不该算（它一次 150ms，是攻击者的免费算力）
    if let Err(lock) = st.login_guard.check(&body.username, &ip) {
        // 日志里**不写完整用户名**：这行可能反复出现，而 log 是长期留存的
        log::warn!(
            "登录被拒（{} 锁定中）：{} ip={} 剩余 {}s",
            lock.scope,
            crate::login_guard::mask(&body.username),
            crate::login_guard::mask(&ip),
            lock.secs
        );
        return Ok(TooMany {
            msg: format!("尝试过于频繁，请 {} 分钟后再试", (lock.secs + 59) / 60),
            retry_secs: lock.secs,
        }
        .into_response());
    }

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
        st.login_guard.fail(&body.username, &ip);
        return Err(ApiError::bad_request("用户名或密码错误"));
    };
    if !auth::verify_password(&body.password, &hash) {
        st.login_guard.fail(&body.username, &ip);
        return Err(ApiError::bad_request("用户名或密码错误"));
    }
    // 成功 → 清零计数（正常用户打错一两次不该被罚）
    st.login_guard.ok(&body.username, &ip);

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
    // `Secure` 按请求实际协议决定（代理层是 HTTPS 就带）——
    // 硬编码会漏掉 `Secure`，会话 cookie 明文也会发（见 `auth::want_secure_cookie`）
    let secure = auth::want_secure_cookie(&headers);
    resp.headers_mut().insert(
        axum::http::header::SET_COOKIE,
        auth::set_cookie(&token, auth::SESSION_TTL_SECS, secure)
            .parse()
            .expect("cookie 值合法"),
    );
    Ok(resp)
}

async fn logout(State(st): State<Arc<AppState>>, headers: axum::http::HeaderMap) -> Response {
    // 即使没带 token 也返回 200 —— 登出失败不该把用户卡住。
    // 这里**不碰数据库**：只清 cookie 就够了，服务端会话靠过期自然清理。
    let mut resp = Json(json!({ "ok": true })).into_response();
    resp.headers_mut().insert(
        axum::http::header::SET_COOKIE,
        auth::clear_cookie(auth::want_secure_cookie(&headers))
            .parse()
            .expect("cookie 值合法"),
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
        "interval_seconds": crate::scheduler::attendance_tick_secs(),
        "hint": if !master {
            "平台已全局关闭自动签到（NEUMOOC_AUTO_SIGN=0）—— 单账号开关无效".to_string()
        } else if on {
            "按课表在课堂时段内每 180 秒同步一次考勤并立即签到（无课时段不查）；定位场次用平台要求的坐标，不需要人到现场。".to_string()
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
    /// **回归护栏**：每个路由的处理函数必须声明 `AuthUser`。
    ///
    /// ## 为什么需要这个测试
    ///
    /// 本项目的鉴权**没有全局中间件**（`is_public` 只在测试里用），
    /// 保护完全靠每个 handler 自己声明 `AuthUser` 参数 —— 声明了，
    /// 未登录请求才会在提取阶段被拒（401）；没声明，那个接口就是**公开的**。
    ///
    /// 实际踩过：`GET /api/device-config` 漏了 `AuthUser`，
    /// 于是无需登录就能读到设备 ID 伪造开关的说明（实测直接 200）。
    ///
    /// 这个测试直接扫源码，把「漏声明」变成构建期失败。
    #[test]
    fn every_route_handler_requires_auth() {
        const API_SRC: &str = include_str!("api.rs");
        const EPS_SRC: &str = include_str!("endpoints.rs");
        const SSE_SRC: &str = include_str!("sse.rs");

        // 公开路由：登录/登出/健康检查 —— 本来就该匿名可访问
        const PUBLIC: &[&str] = &[
            "/api/auth/login",
            "/api/auth/logout",
            "/api/health",
        ];

        // 从路由表里抽 (路径, 处理函数名)
        let mut routes: Vec<(String, String)> = Vec::new();
        for line in API_SRC.lines() {
            let Some(i) = line.find(".route(") else { continue };
            let rest = &line[i + 7..];
            let Some(q1) = rest.find('"') else { continue };
            let Some(q2) = rest[q1 + 1..].find('"') else { continue };
            let path = &rest[q1 + 1..q1 + 1 + q2];
            if !path.starts_with("/api/") || PUBLIC.contains(&path) {
                continue;
            }
            // 处理函数：取 `(get|post|put|delete|patch)(xxx)` 里的 xxx
            for kw in ["get(", "post(", "put(", "delete(", "patch("] {
                if let Some(j) = rest.find(kw) {
                    let after = &rest[j + kw.len()..];
                    if let Some(k) = after.find(')') {
                        let name = after[..k]
                            .rsplit("::")
                            .next()
                            .unwrap_or("")
                            .trim()
                            .to_string();
                        if !name.is_empty() {
                            routes.push((path.to_string(), name));
                        }
                    }
                }
            }
        }
        assert!(!routes.is_empty(), "没解析出任何路由，正则/格式变了？");

        let mut bad: Vec<String> = Vec::new();
        for (path, fn_name) in &routes {
            // 在 endpoints.rs / api.rs 里找该函数的签名（到第一个 `{` 之前）
            let mut sig = None;
            // 签名可能是 `pub async fn`（endpoints.rs）或 `async fn`（api.rs 内私有）
            for src in [EPS_SRC, API_SRC, SSE_SRC] {
                for pat in [
                    format!("pub async fn {fn_name}"),
                    format!("async fn {fn_name}"),
                ] {
                    if let Some(i) = src.find(&pat) {
                        let end = src[i..].find('{').map(|k| i + k).unwrap_or(src.len());
                        sig = Some(&src[i..end]);
                        break;
                    }
                }
                if sig.is_some() {
                    break;
                }
            }
            match sig {
                None => bad.push(format!("{path} → {fn_name}（找不到函数）")),
                Some(s) if !s.contains("AuthUser") => {
                    bad.push(format!("{path} → {fn_name}（缺 AuthUser）"))
                }
                _ => {}
            }
        }

        assert!(
            bad.is_empty(),
            "以下路由的处理函数没声明 AuthUser，等于未授权可访问：\n  {}",
            bad.join("\n  ")
        );
    }

}
