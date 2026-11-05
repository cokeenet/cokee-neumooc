//! 执行日志的 SSE 流。
//!
//! ## 为什么要带游标（`Last-Event-ID`）
//!
//! 浏览器断线重连时会自动带上 `Last-Event-ID`。用它从**上次收到的那条之后**
//! 继续推，而不是从头重放 —— 否则长任务的日志会重复刷屏。
//!
//! ## 为什么靠「轮询数据库」而不是「事件广播」
//!
//! worker 在**另一个线程**里跑，日志是它写进库的。做事件广播要给每个执行
//! 维护一份订阅表，还要处理「订阅建立前就已经写进去的日志」这个竞态。
//! 轮询日志表 + 游标天然没有这个问题：**库是唯一事实来源**，晚连上来也能补齐。
//!
//! 代价是最多 `POLL_MS` 的延迟，对日志来说完全够用。

use std::convert::Infallible;
use std::time::Duration;

use axum::extract::{Path, State};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::IntoResponse;

use crate::api::{ApiError, AppState, AuthUser};

/// 轮询间隔。
const POLL_MS: u64 = 400;
/// 最长推送时长（防止连接永久占着）。
const MAX_SECS: u64 = 30 * 60;

/// 执行是否已到终态。
fn is_terminal(state: &str) -> bool {
    matches!(state, "done" | "failed" | "cancelled")
}

/// `GET /api/executions/:id/stream`
///
/// 事件类型：
/// * `log`  —— `{"id":..,"level":..,"message":..,"ts":..}`
/// * `state` —— 执行状态变化
/// * `end`  —— 流结束（执行到终态）
pub async fn stream(
    State(st): State<std::sync::Arc<AppState>>,
    user: AuthUser,
    Path(exec_id): Path<i64>,
    headers: axum::http::HeaderMap,
) -> Result<impl IntoResponse, ApiError> {
    // 从 `Last-Event-ID` 续传；没有就从头
    let cursor: i64 = headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);

    // 先做归属校验 —— SSE 也是接口，不能因为「只是读日志」就跳过。
    //
    // ⚠️ `st.conn()` 是 **std::sync::Mutex**，同一线程锁两次会**死锁**
    //    （不是报错，是整个请求永远挂住）。所以取 account_id 的锁必须
    //    先释放，再调 `require_account_pub` —— 它内部还会再锁一次。
    let account_id = {
        let conn = st.conn();
        conn.query_row(
            "SELECT account_id FROM executions WHERE id=?",
            rusqlite::params![exec_id],
            |r| r.get::<_, i64>(0),
        )
        .ok()
    }; // ← 锁在这里释放
    let Some(account_id) = account_id else {
        return Err(ApiError::not_found("执行记录不存在"));
    };
    crate::api::require_account_pub(&st.conn(), &user, account_id)?;

    let st2 = st.clone();
    let body = async_stream::stream! {
        let mut last = cursor;
        let started = std::time::Instant::now();
        loop {
            let (rows, state) = {
                let conn = st2.conn();
                let mut stmt = conn
                    .prepare("SELECT id, level, message, ts FROM logs WHERE execution_id=? AND id>? ORDER BY id")
                    .expect("prepare 失败");
                let rows: Vec<(i64, String, String, i64)> = stmt
                    .query_map(rusqlite::params![exec_id, last], |r| {
                        Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
                    })
                    .map(|it| it.filter_map(|r| r.ok()).collect())
                    .unwrap_or_default();
                let state: String = conn
                    .query_row("SELECT state FROM executions WHERE id=?", rusqlite::params![exec_id], |r| r.get(0))
                    .unwrap_or_else(|_| "unknown".into());
                (rows, state)
            };

            for (id, level, message, ts) in rows {
                last = id;
                let data = serde_json::json!({
                    "id": id, "level": level, "message": message, "ts": ts
                })
                .to_string();
                // id 同时作为 SSE 的事件 id —— 重连时浏览器会带回来
                yield Ok::<_, Infallible>(Event::default().event("log").id(id.to_string()).data(data));
            }

            let data = serde_json::json!({ "state": state }).to_string();
            yield Ok(Event::default().event("state").data(data));

            if is_terminal(&state) {
                yield Ok(Event::default().event("end").data("{}"));
                break;
            }
            if started.elapsed().as_secs() > MAX_SECS {
                // 超时不报错，只停 —— 前端可以重连并靠 Last-Event-ID 续上
                yield Ok(Event::default().event("end").data(r#"{"reason":"timeout"}"#));
                break;
            }
            tokio::time::sleep(Duration::from_millis(POLL_MS)).await;
        }
    };

    Ok(Sse::new(body).keep_alive(KeepAlive::default()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_states() {
        for s in ["done", "failed", "cancelled"] {
            assert!(is_terminal(s), "{s} 应是终态");
        }
        for s in ["running", "pending", "unknown", ""] {
            assert!(!is_terminal(s), "{s} 不是终态");
        }
    }
}

/// `GET /api/logs/stream` —— **全局**日志流（跨执行）。
///
/// 与 [`stream`] 的区别：那个盯一次执行，这个盯整张 `logs` 表 ——
/// 概览页要「实时看到所有账号在干什么」，就得用这个。
///
/// 同样带游标（`Last-Event-ID`）：从上次之后继续推，不从头重放。
pub async fn logs_stream(
    State(st): State<std::sync::Arc<AppState>>,
    user: AuthUser,
    headers: axum::http::HeaderMap,
) -> Result<impl IntoResponse, ApiError> {
    let cursor: i64 = headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);

    // 普通用户只看绑定账号的日志。**先在流外面算好可见账号集合** ——
    // 在流里面每轮查一次权限既慢又容易写漏。
    let visible: Option<Vec<i64>> = {
        let conn = st.conn();
        if crate::perm::unrestricted(&user.role) {
            None
        } else {
            let mut stmt = conn
                .prepare("SELECT account_id FROM user_accounts WHERE username=?")
                .map_err(|e| ApiError::bad_request(e.to_string()))?;
            let rows = stmt
                .query_map(rusqlite::params![user.username], |r| r.get::<_, i64>(0))
                .map_err(|e| ApiError::bad_request(e.to_string()))?;
            Some(rows.filter_map(|r| r.ok()).collect())
        }
    };

    let st2 = st.clone();
    let body = async_stream::stream! {
        let mut last = cursor;
        let started = std::time::Instant::now();
        loop {
            let rows = {
                let conn = st2.conn();
                let mut stmt = conn
                    .prepare(
                        "SELECT l.id, l.execution_id, e.account_id, e.job_key, l.level, l.message, l.ts
                         FROM logs l JOIN executions e ON e.id = l.execution_id
                         WHERE l.id > ? ORDER BY l.id LIMIT 200",
                    )
                    .expect("prepare 失败");
                let it = stmt
                    .query_map(rusqlite::params![last], |r| {
                        Ok((
                            r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?,
                            r.get::<_, String>(3)?, r.get::<_, String>(4)?,
                            r.get::<_, String>(5)?, r.get::<_, i64>(6)?,
                        ))
                    })
                    .map(|it| it.filter_map(|r| r.ok()).collect::<Vec<_>>())
                    .unwrap_or_default();
                it
            };
            for (id, exec_id, account_id, job_key, level, message, ts) in rows {
                last = id;
                // 权限：不受限的看全部；受限的只看绑定账号
                let allowed = match &visible {
                    None => true,
                    Some(ids) => ids.contains(&account_id),
                };
                if !allowed {
                    continue;
                }
                let data = serde_json::json!({
                    "id": id, "execution_id": exec_id, "account_id": account_id,
                    "job_key": job_key, "level": level, "message": message, "ts": ts
                })
                .to_string();
                yield Ok::<_, Infallible>(
                    Event::default().event("log").id(id.to_string()).data(data),
                );
            }
            if started.elapsed().as_secs() > MAX_SECS {
                yield Ok(Event::default().event("end").data(r#"{"reason":"timeout"}"#));
                break;
            }
            tokio::time::sleep(Duration::from_millis(POLL_MS)).await;
        }
    };

    Ok(Sse::new(body).keep_alive(KeepAlive::default()))
}
