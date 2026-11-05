//! **端到端验收**：用 Rust 跑一次真实任务。
//!
//! 走完整条链路：从库里取凭据 → Fernet 解密 → 登录东软 → 拉学期/课程/资源
//! → 落库。这是「协议层 + store + 加密 + 任务编排」四层一起的真实验证。
//!
//! ```bash
//! cargo run -p neumooc-server --example run_scan -- <库> <secret.key> <账号id>
//! ```

use std::sync::{Arc, Mutex};

use neumooc_server::crypto::CredentialCipher;
use neumooc_server::db;
use neumooc_server::tasks::{run_scan, run_sign, TaskCtx};
use neumooc_server::worker::CancelToken;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut a = std::env::args().skip(1);
    let dbp = a.next().expect("需要库路径");
    let keyp = a.next().expect("需要 secret.key 路径");
    let account_id: i64 = a.next().unwrap_or_else(|| "1".into()).parse()?;
    let job = a.next().unwrap_or_else(|| "scan".into());

    let cipher = Arc::new(CredentialCipher::from_file(&keyp)?);
    let conn = db::open(&dbp)?;
    let ctx = TaskCtx::new(Arc::new(Mutex::new(conn)), cipher);

    let token = CancelToken::new();
    println!("▶ 账号 {account_id} 执行任务「{job}」…");
    let t0 = std::time::Instant::now();
    let out = match job.as_str() {
        "sign" => run_sign(&ctx, account_id, &token)?,
        _ => run_scan(&ctx, account_id, &token)?,
    };
    println!("✅ 完成（{:.1}s）：{}", t0.elapsed().as_secs_f64(), out);

    // 落库结果核对
    let c = ctx.conn.lock().unwrap();
    for (label, sql) in [
        ("courses", "SELECT count(*) FROM courses WHERE account_id=?"),
        ("resources", "SELECT count(*) FROM resources WHERE account_id=?"),
    ] {
        let n: i64 = c.query_row(sql, rusqlite::params![account_id], |r| r.get(0))?;
        println!("   {label:10} {n:>5} 行");
    }
    let st: String = c.query_row(
        "SELECT state FROM accounts WHERE id=?",
        rusqlite::params![account_id],
        |r| r.get(0),
    )?;
    println!("   账号状态   {st}");
    Ok(())
}
