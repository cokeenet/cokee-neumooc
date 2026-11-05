//! 每账号一个 worker 线程，**命令串行**执行。
//!
//! ## 为什么要串行
//!
//! 同一个账号的请求必须按顺序发：并发跑「刷课」和「签到」会让上游看到
//! 同一账号的请求交错，轻则状态错乱，重则触发风控。**不同账号之间**才是并行的
//! —— 每个账号一个线程，互不阻塞。
//!
//! ## 为什么用「闭包 + 通道」而不是命令枚举
//!
//! Python 版是 `Job(cmd=..., args=...)` 再 `match` 到 `_cmd_*`。Rust 里那样写
//! 会让 worker 依赖所有业务模块（协议层、store、通知…），编译期就耦合死了。
//! 这里让**调用方把要做的事包成闭包**递进来，worker 只负责「排队 + 串行 + 回传结果」。
//! 加新命令不用动这个文件。
//!
//! ## 取消
//!
//! 每个 job 带一个 [`CancelToken`]。长时间跑的循环（比如刷课逐个资源）要**定期
//! 检查**它；不检查的话取消请求要等整个 job 跑完才生效，界面上的「中断」就是摆设。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use std::thread::JoinHandle;

use serde_json::Value;

use neumooc_core::error::{Error, Result};

/// 取消令牌。克隆出来共享给正在跑的 job。
#[derive(Clone, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }
    /// 请求取消。
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
    /// 已取消就返回错误 —— 循环里直接 `self.token.check()?` 即可。
    pub fn check(&self) -> Result<()> {
        if self.is_cancelled() {
            // 用专门的文案：调用方据此把执行标成 cancelled 而不是 failed
            return Err(Error::Invalid("已取消".into()));
        }
        Ok(())
    }
}

/// 交给 worker 执行的一件事。
/// 任务结束后的回调（写日志、落执行状态、回填计划…）。
pub type OnDone = Box<dyn Fn(i64, &str, &Result<Value>) + Send>;

pub struct Job {
    /// 用于日志与排查（如 `study` / `sign`）
    pub key: String,
    pub token: CancelToken,
    /// 真正要做的事。**在 worker 线程里跑**，所以不能借用调用方的栈。
    pub run: Box<dyn FnOnce(&CancelToken) -> Result<Value> + Send>,
    /// 收尾回调。**跟着 job 走，不跟着 worker 走。**
    ///
    /// ⚠️ 原先是 `AccountWorker::spawn(account_id, on_done)` —— 回调绑在
    /// **worker 的生命周期**上，而 `get_or_spawn` 用的是 `or_insert_with`：
    /// worker 已存在时**传入的回调被静默丢弃**。
    ///
    /// 后果是**账号被永久堵死**：调度器/考勤监控先建 worker 时传的是空回调，
    /// 之后用户点「扫描」拿到的还是那个旧 worker → 任务真跑了，
    /// 但 `finish_execution` 永不调用 → 该 job 永久 `running` →
    /// `begin_execution` 永久返回 None → 用户只看到「该账号正在执行其它任务」。
    ///
    /// 实测复现过：exec#64（账号1 scan）跑了 115 秒仍 `running`，
    /// 而账号1 正是唯一开了调度、worker 被后台先建起来的账号。
    pub on_done: Option<OnDone>,
}

impl Job {
    pub fn new(
        key: impl Into<String>,
        run: impl FnOnce(&CancelToken) -> Result<Value> + Send + 'static,
    ) -> Self {
        Self { key: key.into(), token: CancelToken::new(), run: Box::new(run), on_done: None }
    }

    /// 挂上收尾回调（链式）。
    pub fn on_done(
        mut self,
        f: impl Fn(i64, &str, &Result<Value>) + Send + 'static,
    ) -> Self {
        self.on_done = Some(Box::new(f));
        self
    }
}

/// 每个账号一个。
pub struct AccountWorker {
    account_id: i64,
    tx: Sender<Job>,
    handle: Option<JoinHandle<()>>,
    /// 当前正在跑的 job 的取消令牌（供外部中断）
    current: Arc<std::sync::Mutex<Option<CancelToken>>>,
}

impl AccountWorker {
    /// 起一个 worker 线程。`on_done` 在**每次 job 结束后**被调用（记日志、写库）。
    pub fn spawn(account_id: i64) -> Self {
        let (tx, rx): (Sender<Job>, Receiver<Job>) = channel();
        let current: Arc<std::sync::Mutex<Option<CancelToken>>> = Default::default();
        let cur = current.clone();
        let handle = std::thread::Builder::new()
            .name(format!("worker-{account_id}"))
            .spawn(move || {
                // 串行：一条一条收，收完才收下一条
                for job in rx {
                    // 排队期间已被取消的，直接跳过 —— 不要白跑一遍再报取消
                    let finish = |key: &str, r: &Result<Value>| {
                        if let Some(f) = job.on_done.as_ref() {
                            f(account_id, key, r);
                        }
                    };
                    if job.token.is_cancelled() {
                        finish(&job.key, &Err(Error::Invalid("已取消".into())));
                        continue;
                    }
                    *cur.lock().expect("锁中毒") = Some(job.token.clone());
                    let r = (job.run)(&job.token);
                    *cur.lock().expect("锁中毒") = None;
                    finish(&job.key, &r);
                }
            })
            .expect("起 worker 线程失败");
        Self { account_id, tx, handle: Some(handle), current }
    }

    pub fn account_id(&self) -> i64 {
        self.account_id
    }

    /// 投递一件事。worker 已停时返回错误（不静默丢弃）。
    pub fn submit(&self, job: Job) -> Result<()> {
        self.tx
            .send(job)
            .map_err(|_| Error::Invalid("worker 已停止".into()))
    }

    /// 取消当前正在跑的事（若有）。
    ///
    /// ⚠️ 只是**置标志**，真正停下来要等 job 自己在循环里检查
    /// [`CancelToken::check`]。不检查的长循环取消不掉。
    pub fn cancel_current(&self) -> bool {
        match self.current.lock().expect("锁中毒").as_ref() {
            Some(t) => {
                t.cancel();
                true
            }
            None => false,
        }
    }

    /// 停止 worker（丢弃队列，等线程退出）。
    pub fn shutdown(&mut self) {
        // 换一个已断开的 sender，让 `for job in rx` 结束
        let (tx, _rx) = channel();
        let old = std::mem::replace(&mut self.tx, tx);
        drop(old);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for AccountWorker {
    fn drop(&mut self) {
        if self.handle.is_some() {
            self.shutdown();
        }
    }
}

/// 一组 worker，按账号 id 索引。
#[derive(Default)]
pub struct WorkerRegistry {
    workers: std::collections::HashMap<i64, AccountWorker>,
}

impl WorkerRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// 取（必要时创建）某账号的 worker。
    /// 取（必要时创建）某账号的 worker。
    ///
    /// **不再收 `on_done`** —— 收尾回调跟着 `Job` 走（见 [`Job::on_done`]）。
    /// 收在这里必然被 `or_insert_with` 丢弃，那是「账号被永久堵死」的根因。
    pub fn get_or_spawn(&mut self, account_id: i64) -> &AccountWorker {
        self.workers
            .entry(account_id)
            .or_insert_with(|| AccountWorker::spawn(account_id))
    }

    pub fn get(&self, account_id: i64) -> Option<&AccountWorker> {
        self.workers.get(&account_id)
    }

    /// 当前有多少个 worker 活着（概览页显示用）。
    pub fn len(&self) -> usize {
        self.workers.len()
    }
    pub fn is_empty(&self) -> bool {
        self.workers.is_empty()
    }

    pub fn shutdown_all(&mut self) {
        for (_, w) in self.workers.iter_mut() {
            w.shutdown();
        }
        self.workers.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::channel as std_channel;

    #[test]
    fn jobs_run_serially_in_order() {
        let (tx, rx) = std_channel::<i64>();
        let mut w = AccountWorker::spawn(1);
        for i in 0..5 {
            let t = tx.clone();
            w.submit(Job::new("t", move |_| {
                t.send(i).unwrap();
                Ok(Value::Null)
            }))
            .unwrap();
        }
        drop(tx);
        let got: Vec<i64> = rx.iter().collect();
        assert_eq!(got, vec![0, 1, 2, 3, 4], "必须按投递顺序串行执行");
        w.shutdown();
    }

    #[test]
    fn result_is_reported_back() {
        let (tx, rx) = std_channel();
        let mut w = AccountWorker::spawn(1);
        // 回调**跟着 job 走**（不是跟着 worker）—— 这正是修复「账号被永久堵死」
        // 那个 bug 的核心：同一账号的两次投递各自带自己的收尾逻辑。
        let t1 = tx.clone();
        w.submit(Job::new("ok", |_| Ok(Value::Null)).on_done(move |_id, key, r| {
            let _ = t1.send((key.to_string(), r.is_ok()));
        }))
        .unwrap();
        w.submit(
            Job::new("bad", |_| Err(Error::Invalid("炸了".into()))).on_done(move |_id, key, r| {
                let _ = tx.send((key.to_string(), r.is_ok()));
            }),
        )
        .unwrap();
        drop(w);
        let got: Vec<_> = rx.iter().collect();
        assert_eq!(got, vec![("ok".to_string(), true), ("bad".to_string(), false)]);
    }

    #[test]
    fn pre_cancelled_job_is_skipped() {
        // 排队期间被取消的，不该白跑一遍
        let ran = Arc::new(AtomicBool::new(false));
        let r2 = ran.clone();
        let mut w = AccountWorker::spawn(1);
        let job = Job::new("x", move |_| {
            r2.store(true, Ordering::SeqCst);
            Ok(Value::Null)
        });
        job.token.cancel();
        w.submit(job).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(80));
        assert!(!ran.load(Ordering::SeqCst), "预取消的 job 不该执行");
        w.shutdown();
    }

    #[test]
    fn cancel_token_stops_a_loop() {
        let (tx, rx) = std_channel();
        let mut w = AccountWorker::spawn(1);
        let job = Job::new("loop", move |token| {
            let mut n = 0;
            for _ in 0..1000 {
                token.check()?; // 循环里必须检查，否则取消不掉
                n += 1;
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            tx.send(n).unwrap();
            Ok(Value::Null)
        });
        let token = job.token.clone();
        w.submit(job).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(30));
        token.cancel();
        std::thread::sleep(std::time::Duration::from_millis(80));
        assert!(rx.try_recv().is_err(), "取消后不该跑满 1000 次");
        w.shutdown();
    }

    #[test]
    fn registry_reuses_worker_per_account() {
        let mut reg = WorkerRegistry::new();
        reg.get_or_spawn(1);
        reg.get_or_spawn(1);
        reg.get_or_spawn(2);
        assert_eq!(reg.len(), 2, "同账号复用，不同账号各自一个");
        reg.shutdown_all();
        assert_eq!(reg.len(), 0);
    }
}
