//! Rust 版服务入口。
//!
//! 环境变量与 Python 版保持一致，便于直接替换：
//!
//! | 变量 | 默认 | 说明 |
//! | --- | --- | --- |
//! | `NEUMOOC_DATA` | `data` | 数据目录（放 `neumooc.db` 与 `secret.key`） |
//! | `NEUMOOC_HOST` | `127.0.0.1` | 监听地址 |
//! | `NEUMOOC_PORT` | `8788` | 监听端口 |

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use neumooc_server::{api, db};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // ⚠️ 必须初始化。没有它，所有 `log::info!` 都被**静默丢弃** ——
    // 表现是「后台循环明明在跑，日志里却一个字都没有」，非常难查。
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let data = std::env::var("NEUMOOC_DATA").unwrap_or_else(|_| "data".into());
    let host = std::env::var("NEUMOOC_HOST").unwrap_or_else(|_| "127.0.0.1".into());
    let port: u16 = std::env::var("NEUMOOC_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(8788);

    let db_path = std::path::Path::new(&data).join("neumooc.db");
    if let Some(dir) = db_path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let conn = db::open(&db_path)?;
    println!("[neumooc] 数据库：{}", db_path.display());

    // 启动时把上次进程留下的 running 标成 failed —— 否则那些 job 的
    // 「已在跑」名额永远占着，该账号再也发不起同类任务
    let n = neumooc_server::store::reset_stale_running(&conn)?;
    if n > 0 {
        println!("[neumooc] 已把 {n} 条残留执行标为 failed");
    }

    let key_path = std::path::Path::new(&data).join("secret.key");
    let cipher = std::sync::Arc::new(
        neumooc_server::crypto::CredentialCipher::from_file(&key_path)?,
    );
    println!("[neumooc] 凭据密钥：{}", key_path.display());

    let state = api::AppState::new(conn, cipher);

    // 后台循环：调度器 + 考勤监控。没有这两条，库里那些配置就只是死数据。
    spawn_background(state.clone());
    let app = api::router(state.clone());

    let addr: SocketAddr = format!("{host}:{port}").parse()?;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    println!("[neumooc] 监听 http://{addr}");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    println!("[neumooc] 已停止");
    Ok(())
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
    println!("[neumooc] 收到停止信号…");
}

/// 起后台循环。
///
/// 两个循环各跑各的：
/// * 调度器每 30 秒看一次有没有到点的「每日检查」计划
/// * 考勤监控每 60 秒看一次有没有「进行中且未签」的考勤，开着自动签到的账号直接签
///
/// 用**阻塞线程**而不是 tokio task：这两件事本质是「定时 + 打阻塞式 HTTP」，
/// 塞进 async 里反而要用 spawn_blocking 绕一圈，还容易把运行时线程占满。
fn spawn_background(state: Arc<api::AppState>) {
    // 调度器
    {
        let st = state.clone();
        std::thread::Builder::new()
            .name("scheduler".into())
            .spawn(move || loop {
                {
                    let conn = st.conn();
                    if let Err(e) = neumooc_server::scheduler::Scheduler::ensure_today_plans(&conn) {
                        eprintln!("[scheduler] 生成计划失败：{e}");
                    }
                    match neumooc_server::scheduler::Scheduler::due_plans(&conn) {
                        Ok(plans) => {
                            for (plan_id, account_id) in plans {
                                // 交给该账号的 worker 串行执行
                                let ctx = st.tasks.clone();
                                let mut reg = st.workers.lock().expect("锁中毒");
                                let w = reg.get_or_spawn(account_id);
                                let _ = w.submit(neumooc_server::worker::Job::new(
                                    "check",
                                    move |token| neumooc_server::tasks::run_scan(&ctx, account_id, token),
                                ));
                                let _ = conn.execute(
                                    "UPDATE daily_plans SET state='done', updated_at=? WHERE id=?",
                                    rusqlite::params![neumooc_server::db::now_ms(), plan_id],
                                );
                            }
                        }
                        Err(e) => eprintln!("[scheduler] 取计划失败：{e}"),
                    }
                }
                std::thread::sleep(std::time::Duration::from_secs(
                    neumooc_server::scheduler::SCHEDULER_TICK_SECS,
                ));
            })
            .expect("起调度器线程失败");
    }

    // 考勤监控
    {
        let st = state.clone();
        std::thread::Builder::new()
            .name("attendance".into())
            .spawn(move || {
                let watcher = neumooc_server::scheduler::AttendanceWatcher::new(
                    neumooc_server::notify::Notifier::from_env(),
                );
                // 正在跑的账号（内存闸门）。不用 `begin_execution` 当闸门了 ——
                // 那会在没有可签场次时也每分钟插一条执行记录
                // （3 个账号 = 4320 行/天，运行日志会被刷满）。
                let auto_inflight: Arc<Mutex<HashSet<i64>>> = Default::default();
                // 硬错误（登录失效/网络）的**限频**记录：每账号 15 分钟最多一条
                let auto_last_err: Arc<Mutex<HashMap<i64, i64>>> = Default::default();
                loop {
                    // ── ① 开着自动签到的账号：**每分钟**同步 + 立即签 ──
                    //
                    // 为什么不复用缓存：`attendance` 表只有手动「同步考勤」才会更新，
                    // 实测 9 个账号里 7 个的缓存是 140 小时前的 —— 新考勤根本不在库里，
                    // 「开着自动签到」等于没开。所以这里自己做「同步→挑→签」。
                    //
                    // 为什么合在一个任务里：监控一轮就是 60 秒（ATTENDANCE_TICK_SECS），
                    // 拆成「本轮同步、下轮签」最坏要等 2 分钟，而考勤窗口常常只有几分钟。
                    let auto_ids = {
                        let conn = st.conn();
                        neumooc_server::scheduler::AttendanceWatcher::auto_sign_accounts(&conn)
                            .unwrap_or_default()
                    };
                    for aid in auto_ids {
                        // 内存闸门：上一轮还没跑完就跳过（不会堵住别的账号）
                        {
                            let mut g = auto_inflight.lock().expect("锁中毒");
                            if !g.insert(aid) {
                                continue;
                            }
                        }
                        let ctx = st.tasks.clone();
                        let st3 = st.clone();
                        let inflight = auto_inflight.clone();
                        let last_err = auto_last_err.clone();
                        let mut reg = st.workers.lock().expect("锁中毒");
                        let w = reg.get_or_spawn(aid);
                        let _ = w.submit(neumooc_server::worker::Job::new(
                            "auto-checkin",
                            move |token| {
                                // 无论正常返回还是 panic 展开，离开作用域都会放行
                                struct Release(Arc<Mutex<HashSet<i64>>>, i64);
                                impl Drop for Release {
                                    fn drop(&mut self) {
                                        if let Ok(mut g) = self.0.lock() {
                                            g.remove(&self.1);
                                        }
                                    }
                                }
                                let _release = Release(inflight, aid);

                                let r = neumooc_server::tasks::run_auto_sync_sign(&ctx, aid, token);

                                // ── **只有真签了 / 有失败，才落执行记录** ──
                                // 每分钟一条「同步 0 场」的空记录没有信息量，
                                // 只会污染「运行日志」页、并让 executions 表每天涨几千行。
                                let signed = r
                                    .as_ref()
                                    .ok()
                                    .and_then(|v| v.get("signed"))
                                    .and_then(|v| v.as_i64())
                                    .unwrap_or(0);
                                let failed_n = r
                                    .as_ref()
                                    .ok()
                                    .and_then(|v| v.get("failed"))
                                    .and_then(|v| v.as_array())
                                    .map(|a| a.len())
                                    .unwrap_or(0);
                                let hard_err = r.is_err();
                                if signed == 0 && failed_n == 0 && !hard_err {
                                    return r;
                                }
                                // 硬错误限频：密码失效时别每分钟刷一条
                                if hard_err {
                                    let now = neumooc_server::db::now_ms();
                                    let mut m = last_err.lock().expect("锁中毒");
                                    let last = m.get(&aid).copied().unwrap_or(0);
                                    if now - last < 15 * 60 * 1000 {
                                        return r;
                                    }
                                    m.insert(aid, now);
                                }
                                if let Ok(conn) = st3.db.lock() {
                                    let (state, detail) = match &r {
                                        Ok(v) => ("done", v.clone()),
                                        Err(e) => (
                                            "failed",
                                            serde_json::json!({ "error": e.to_string() }),
                                        ),
                                    };
                                    if let Ok(Some(exec_id)) =
                                        neumooc_server::store::begin_execution(&conn, aid, "auto-checkin")
                                    {
                                        let msg = if state == "done" {
                                            format!(
                                                "自动签到：签了 {signed} 场{}",
                                                if failed_n > 0 {
                                                    format!("，{failed_n} 场失败")
                                                } else {
                                                    String::new()
                                                }
                                            )
                                        } else {
                                            format!("自动签到 failed：{detail}")
                                        };
                                        let _ = neumooc_server::store::append_log(
                                            &conn,
                                            exec_id,
                                            if state == "done" && failed_n == 0 { "ok" } else { "error" },
                                            &msg,
                                        );
                                        let _ = neumooc_server::store::finish_execution(
                                            &conn, exec_id, state, detail,
                                        );
                                    }
                                }
                                r
                            },
                        ));
                    }

                    // ── ② 提醒：所有账号读缓存，发现「进行中未签」就发通知 ──
                    // （没开自动签到的账号只提醒不签；开了的上面已经处理）
                    {
                        let conn = st.conn();
                        let _ = watcher.tick(&conn);
                    }

                    // ── 决定下一轮等多久 ──
                    //
                    // 优先级：
                    //   ① 有课表 → 严格按课表窗口（有课才查，没课最长等 30 分钟）
                    //   ② 没课表 → 退回「按时段降频」的启发式（poll_policy）
                    //
                    // 课表从 `timetable.json` 读（由 `tools/jw-fetch.py` 抓，
                    // systemd timer 每天跑 2 次）。教务接口校外可访问，
                    // 不受校园网封禁影响。
                    let base = neumooc_server::scheduler::attendance_tick_secs();
                    let wait_secs = {
                        let tt = neumooc_server::timetable::load_from_file(
                            // 路径由 `NEUMOOC_TIMETABLE` 指定；没设就用数据目录下的约定名
                            &std::env::var("NEUMOOC_TIMETABLE").unwrap_or_else(|_| {
                                format!(
                                    "{}/timetable.json",
                                    std::env::var("NEUMOOC_DATA")
                                        .unwrap_or_else(|_| ".".into())
                                        .trim_end_matches('/')
                                )
                            }),
                        );
                        let (h, m, wd) = local_hm_weekday();
                        let now_min = h * 60 + m;

                        match tt {
                            Some(t) if !t.by_weekday.is_empty() => {
                                if t.in_window(wd, now_min).is_some() {
                                    // 窗口内：按基准间隔（180 秒），加抖动
                                    let j = base as f64 * 0.2 * (rand_jitter() - 0.5);
                                    ((base as f64 + j) as u64).max(60)
                                } else if let Some(gap_min) = t.minutes_to_window(wd, now_min) {
                                    // 还没到下一节课：睡到窗口前再起，最长 30 分钟一段。
                                    //
                                    // 分段而不是一觉睡到点：万一课表过期（临时加课），
                                    // 30 分钟内能自愈。
                                    //
                                    // ⚠️ `minutes_to_window` 返回的是**分钟**，
                                    // 而 `sleep` 要**秒** —— 早先直接把分钟传给
                                    // `from_secs`，结果"睡 266 分钟"变成"睡 266 秒"，
                                    // 窗口外每 4 分钟醒一次。不影响签到正确性，
                                    // 但白烧 CPU、白读磁盘。
                                    let target_min = (gap_min as u64).saturating_sub(
                                        neumooc_server::timetable::MARGIN_MIN as u64,
                                    );
                                    target_min.clamp(1, 30) * 60
                                } else {
                                    // 今天没课了 → 30 分钟后再看（跨天/临时课兜底）
                                    30 * 60
                                }
                            }
                            _ => {
                                // 没课表：老的启发式
                                let factor =
                                    neumooc_server::poll_policy::slow_factor(std::time::SystemTime::now());
                                let scaled = (base * factor) as f64;
                                let jitter = scaled * 0.2 * (rand_jitter() - 0.5);
                                ((scaled + jitter) as i64).max(30) as u64
                            }
                        }
                    };
                    std::thread::sleep(std::time::Duration::from_secs(wait_secs));
                }
            })
            .expect("起考勤监控线程失败");
    }
    println!("[neumooc] 调度器与考勤监控已启动");
}

/// 返回 [0,1) 的伪随机数（不需要密码学强度，只要够散）。
///
/// 用纳秒时间戳的低位 —— 每轮调用间隔几秒，低位足够随机。
/// 不引入 `rand` crate 是为了不给这个纯定时用途增加依赖。
fn rand_jitter() -> f64 {
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    // xorshift 再打散一次
    let mut x = n as u64 | 1;
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    (x % 1_000_000) as f64 / 1_000_000.0
}

/// 本地时间的 (时, 分, 星期几 1-7)。
///
/// 用 `localtime_r` 而不是手算 UTC+8 —— 这台机器有夏令时/时区配置，
/// 硬编码会在换时区后错位（错位 = 有课时按没课处理 = 漏签）。
#[cfg(unix)]
fn local_hm_weekday() -> (i32, i32, i32) {
    #[repr(C)]
    struct Tm {
        tm_sec: i32,
        tm_min: i32,
        tm_hour: i32,
        tm_mday: i32,
        tm_mon: i32,
        tm_year: i32,
        tm_wday: i32,
        tm_yday: i32,
        tm_isdst: i32,
        tm_gmtoff: i64,
        tm_zone: *const i8,
    }
    extern "C" {
        fn localtime_r(timep: *const i64, result: *mut Tm) -> *mut Tm;
    }
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    unsafe {
        let mut tm = std::mem::zeroed::<Tm>();
        if localtime_r(&secs, &mut tm).is_null() {
            return (12, 0, 1); // 取不到按中午，保守
        }
        // tm_wday: 0=周日 … 6=周六 → 转成 1=周一 … 7=周日
        let wd = if tm.tm_wday == 0 { 7 } else { tm.tm_wday };
        (tm.tm_hour, tm.tm_min, wd)
    }
}

#[cfg(not(unix))]
fn local_hm_weekday() -> (i32, i32, i32) {
    (12, 0, 1)
}
